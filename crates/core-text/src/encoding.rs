//! Encoding detection, decoding and encoding (plan.md §4.1, backlog E1-S1, E1-S7).
//!
//! Detection order, as in `Dc.TextLines`, with the additions of the plan:
//!
//! 1. a BOM: `EF BB BF` UTF-8, `FF FE` UTF-16 LE, `FE FF` UTF-16 BE;
//! 2. without a BOM, a NUL pattern that looks like UTF-16 (decision D20);
//! 3. any other NUL byte: the file is **binary** (decision D19: the whole file is scanned,
//!    not only the first 64 KiB);
//! 4. valid UTF-8 → UTF-8 (decision D21: strict `std` validation);
//! 5. otherwise the ANSI code page.
//!
//! Everything is transcoded once into one UTF-8 buffer. Never one `String` per line.

use core_base::{Error, ErrorKind, Result};
use encoding_rs::Encoding as RsEncoding;

use crate::ansi;

/// Number of leading bytes inspected by the UTF-16-without-BOM heuristic.
pub const SNIFF_BYTES: usize = 64 * 1024;

/// The encoding a file was loaded with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    /// A legacy code page (system ANSI or an override).
    Ansi(&'static RsEncoding),
    Utf8,
    Utf8Bom,
    Utf16Le,
    Utf16Be,
}

impl Encoding {
    /// A stable display name (`"UTF-8"`, `"UTF-16LE"`, `"windows-1255"`, …).
    pub fn name(self) -> &'static str {
        match self {
            Encoding::Ansi(e) => e.name(),
            Encoding::Utf8 | Encoding::Utf8Bom => "UTF-8",
            Encoding::Utf16Le => "UTF-16LE",
            Encoding::Utf16Be => "UTF-16BE",
        }
    }
}

/// Options for [`sniff`] and the loaders.
#[derive(Debug, Clone, Copy, Default)]
pub struct DecodeOptions {
    /// Code page used when a file is not valid UTF-8. `None` = the system ANSI code page.
    pub ansi: Option<&'static RsEncoding>,
}

impl DecodeOptions {
    pub fn ansi_encoding(&self) -> &'static RsEncoding {
        self.ansi.unwrap_or_else(ansi::system_ansi_encoding)
    }
}

/// What [`sniff`] decided about a buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sniff {
    pub encoding: Encoding,
    /// Bytes of BOM to skip (0 when there is none).
    pub bom_len: usize,
    /// NUL bytes outside a UTF-16 pattern. Binary buffers are not decoded.
    pub is_binary: bool,
}

/// Which UTF-16 byte order the NUL pattern of `sample` suggests, if any (decision D20).
///
/// Counts, per 2-byte unit, a zero high byte (little endian: `'a'` is `61 00`) and a zero low
/// byte (big endian: `'a'` is `00 61`). It is UTF-16 when
///
/// * one order shows zeros and the other at most a quarter as many,
/// * no unit is `00 00` (a NUL character means binary), and
/// * the units read in that order look like text: no control characters other than TAB, LF,
///   VT, FF and CR, no C1 controls, and well-formed surrogate pairs. This keeps a short binary
///   blob with a stray NUL from being mistaken for UTF-16.
pub fn utf16_pattern(sample: &[u8]) -> Option<Encoding> {
    let pairs = sample.len() / 2;
    if pairs == 0 {
        return None;
    }
    let (mut le_nul, mut be_nul) = (0usize, 0usize);
    for unit in sample.chunks_exact(2) {
        match (unit[0] == 0, unit[1] == 0) {
            (true, true) => return None,
            (false, true) => le_nul += 1,
            (true, false) => be_nul += 1,
            (false, false) => {}
        }
    }
    let (major, minor) = (le_nul.max(be_nul), le_nul.min(be_nul));
    if major == 0 || minor * 4 > major {
        return None;
    }
    let le = le_nul > be_nul;
    let units = sample.chunks_exact(2).map(|u| {
        if le {
            u16::from_le_bytes([u[0], u[1]])
        } else {
            u16::from_be_bytes([u[0], u[1]])
        }
    });
    looks_like_text(units).then_some(if le {
        Encoding::Utf16Le
    } else {
        Encoding::Utf16Be
    })
}

fn looks_like_text(units: impl Iterator<Item = u16>) -> bool {
    let mut pending_high = false;
    for u in units {
        let is_high = (0xD800..0xDC00).contains(&u);
        let is_low = (0xDC00..0xE000).contains(&u);
        if pending_high != is_low {
            return false; // lone low surrogate, or a high one not followed by a low one
        }
        pending_high = is_high;
        let bad_control = u < 0x20 && !matches!(u, 0x09..=0x0D) || (0x7F..=0x9F).contains(&u);
        if bad_control {
            return false;
        }
    }
    true // a high surrogate cut off by the end of the sample is fine
}

/// Decides how `bytes` should be read. Pure: nothing is decoded and nothing is allocated.
pub fn sniff(bytes: &[u8], opts: &DecodeOptions) -> Sniff {
    let text = |encoding, bom_len| Sniff {
        encoding,
        bom_len,
        is_binary: false,
    };
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return text(Encoding::Utf8Bom, 3);
    }
    if bytes.starts_with(&[0xFF, 0xFE]) {
        return text(Encoding::Utf16Le, 2);
    }
    if bytes.starts_with(&[0xFE, 0xFF]) {
        return text(Encoding::Utf16Be, 2);
    }
    let has_nul = memchr::memchr(0, bytes).is_some();
    if has_nul {
        let sample = &bytes[..bytes.len().min(SNIFF_BYTES) & !1];
        return match utf16_pattern(sample) {
            Some(enc) => text(enc, 0),
            None => Sniff {
                encoding: Encoding::Utf8,
                bom_len: 0,
                is_binary: true,
            },
        };
    }
    if std::str::from_utf8(bytes).is_ok() {
        text(Encoding::Utf8, 0)
    } else {
        text(Encoding::Ansi(opts.ansi_encoding()), 0)
    }
}

/// A decoded buffer.
#[derive(Debug)]
pub struct Decoded {
    /// The UTF-8 text. UTF-16 BOMs are not part of it; a UTF-8 BOM stays as a leading U+FEFF
    /// (the document skips it with `text_start`, so no copy is needed).
    pub text: String,
    /// Malformed input was replaced with U+FFFD, so the original bytes cannot be rebuilt.
    pub had_errors: bool,
}

/// Decodes `bytes` (BOM included) according to `sniff`. UTF-8 input is validated, not
/// copied: the `String` takes over the allocation. Malformed input is replaced with U+FFFD
/// and reported through `had_errors`.
pub fn decode(bytes: Vec<u8>, sniff: &Sniff) -> Decoded {
    match sniff.encoding {
        Encoding::Utf8 | Encoding::Utf8Bom => match String::from_utf8(bytes) {
            Ok(text) => Decoded {
                text,
                had_errors: false,
            },
            Err(e) => {
                let lossy = String::from_utf8_lossy(e.as_bytes()).into_owned();
                Decoded {
                    text: lossy,
                    had_errors: true,
                }
            }
        },
        Encoding::Utf16Le => decode_with(encoding_rs::UTF_16LE, &bytes[sniff.bom_len..]),
        Encoding::Utf16Be => decode_with(encoding_rs::UTF_16BE, &bytes[sniff.bom_len..]),
        Encoding::Ansi(e) => decode_with(e, &bytes),
    }
}

fn decode_with(enc: &'static RsEncoding, bytes: &[u8]) -> Decoded {
    let (text, had_errors) = enc.decode_without_bom_handling(bytes);
    Decoded {
        text: text.into_owned(),
        had_errors,
    }
}

/// Encodes `text` back to `enc`, writing a BOM when `bom` is set. Fails with
/// `ErrorKind::Encoding` when a code point has no representation in a legacy code page.
pub fn encode(text: &str, enc: Encoding, bom: bool) -> Result<Vec<u8>> {
    match enc {
        Encoding::Utf8 | Encoding::Utf8Bom => {
            let with_bom = bom || enc == Encoding::Utf8Bom;
            let mut out = Vec::with_capacity(text.len() + 3);
            if with_bom {
                out.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
            }
            out.extend_from_slice(text.as_bytes());
            Ok(out)
        }
        Encoding::Utf16Le | Encoding::Utf16Be => {
            let le = enc == Encoding::Utf16Le;
            let mut out = Vec::with_capacity(text.len() * 2 + 2);
            if bom {
                out.extend_from_slice(if le { &[0xFF, 0xFE] } else { &[0xFE, 0xFF] });
            }
            for u in text.encode_utf16() {
                out.extend_from_slice(&if le { u.to_le_bytes() } else { u.to_be_bytes() });
            }
            Ok(out)
        }
        Encoding::Ansi(e) => {
            let (bytes, _, had_unmappable) = e.encode(text);
            if had_unmappable {
                return Err(Error::new(
                    ErrorKind::Encoding,
                    format!("the text has characters that {} cannot represent", e.name()),
                ));
            }
            Ok(bytes.into_owned())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use encoding_rs::{WINDOWS_1252, WINDOWS_1255};

    fn opts() -> DecodeOptions {
        DecodeOptions {
            ansi: Some(WINDOWS_1252),
        }
    }

    fn utf16(s: &str, le: bool) -> Vec<u8> {
        s.encode_utf16()
            .flat_map(|u| if le { u.to_le_bytes() } else { u.to_be_bytes() })
            .collect()
    }

    #[test]
    fn each_bom_is_detected_and_skipped() {
        let s = sniff(b"\xEF\xBB\xBFhi", &opts());
        assert_eq!(
            (s.encoding, s.bom_len, s.is_binary),
            (Encoding::Utf8Bom, 3, false)
        );
        let s = sniff(b"\xFF\xFEh\0i\0", &opts());
        assert_eq!((s.encoding, s.bom_len), (Encoding::Utf16Le, 2));
        let s = sniff(b"\xFE\xFF\0h\0i", &opts());
        assert_eq!((s.encoding, s.bom_len), (Encoding::Utf16Be, 2));
    }

    #[test]
    fn bom_alone_is_an_empty_text() {
        for (bytes, enc) in [
            (&b"\xEF\xBB\xBF"[..], Encoding::Utf8Bom),
            (&b"\xFF\xFE"[..], Encoding::Utf16Le),
            (&b"\xFE\xFF"[..], Encoding::Utf16Be),
        ] {
            let s = sniff(bytes, &opts());
            assert_eq!(s.encoding, enc);
            assert_eq!(
                decode(bytes.to_vec(), &s)
                    .text
                    .trim_start_matches('\u{FEFF}'),
                ""
            );
        }
    }

    #[test]
    fn plain_ascii_and_valid_utf8_are_utf8() {
        assert_eq!(sniff(b"hello\r\nworld", &opts()).encoding, Encoding::Utf8);
        assert_eq!(
            sniff("שלום עולם".as_bytes(), &opts()).encoding,
            Encoding::Utf8
        );
        assert_eq!(sniff(b"", &opts()).encoding, Encoding::Utf8);
        assert!(!sniff(b"", &opts()).is_binary);
    }

    #[test]
    fn invalid_utf8_falls_back_to_the_ansi_code_page() {
        let s = sniff(b"caf\xE9", &opts());
        assert_eq!(s.encoding, Encoding::Ansi(WINDOWS_1252));
        let s = sniff(
            b"caf\xE9",
            &DecodeOptions {
                ansi: Some(WINDOWS_1255),
            },
        );
        assert_eq!(s.encoding, Encoding::Ansi(WINDOWS_1255));
    }

    #[test]
    fn utf16_without_bom_is_found_by_the_nul_pattern() {
        let le = utf16("hello world\r\nline two\r\n", true);
        assert_eq!(sniff(&le, &opts()).encoding, Encoding::Utf16Le);
        let be = utf16("hello world\r\nline two\r\n", false);
        assert_eq!(sniff(&be, &opts()).encoding, Encoding::Utf16Be);
        assert!(!sniff(&le, &opts()).is_binary);
    }

    #[test]
    fn utf16_heuristic_handles_short_files() {
        // One ASCII character is enough: "a\0" / "\0a".
        assert_eq!(sniff(b"a\0", &opts()).encoding, Encoding::Utf16Le);
        assert_eq!(sniff(b"\0a", &opts()).encoding, Encoding::Utf16Be);
        // A single byte cannot be UTF-16: a lone NUL is binary.
        assert!(sniff(b"\0", &opts()).is_binary);
        // Odd length: the trailing byte is ignored by the heuristic.
        assert_eq!(sniff(b"a\0b\0c", &opts()).encoding, Encoding::Utf16Le);
    }

    #[test]
    fn utf16_text_with_a_few_cross_order_zeros_still_counts() {
        // 'Ā' (U+0100) is `00 01` in LE: one zero low byte among many zero high bytes.
        let mut s = String::from("a mostly ascii line of text with one Ā in it\r\n");
        s.push_str("and another ascii line\r\n");
        assert_eq!(sniff(&utf16(&s, true), &opts()).encoding, Encoding::Utf16Le);
    }

    #[test]
    fn nul_characters_make_a_file_binary() {
        let s = sniff(b"abc\0\0def", &opts());
        assert!(s.is_binary);
        // NUL pairs inside otherwise UTF-16 looking data are NUL characters: binary.
        let mut data = utf16("hello", true);
        data.extend_from_slice(&[0, 0]);
        assert!(sniff(&data, &opts()).is_binary);
        // Random binary with NULs in both positions.
        assert!(sniff(b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\x01\0", &opts()).is_binary);
    }

    #[test]
    fn nul_after_the_first_64k_still_makes_utf8_binary() {
        // D19: the whole file is scanned.
        let mut data = vec![b'a'; SNIFF_BYTES + 100];
        data[SNIFF_BYTES + 50] = 0;
        assert!(sniff(&data, &opts()).is_binary);
    }

    #[test]
    fn decode_utf8_keeps_the_allocation_and_the_text() {
        let bytes = "שלום 😀".as_bytes().to_vec();
        let s = sniff(&bytes, &opts());
        let d = decode(bytes, &s);
        assert_eq!(d.text, "שלום 😀");
        assert!(!d.had_errors);
    }

    #[test]
    fn decode_utf16_both_orders_with_and_without_bom() {
        let text = "שלום\r\nemoji 😀\r\n";
        for le in [true, false] {
            let mut with_bom = if le {
                vec![0xFF, 0xFE]
            } else {
                vec![0xFE, 0xFF]
            };
            with_bom.extend(utf16(text, le));
            let s = sniff(&with_bom, &opts());
            assert_eq!(decode(with_bom, &s).text, text, "le={le} bom");
            let no_bom = utf16(text, le);
            let s = sniff(&no_bom, &opts());
            assert_eq!(
                s.encoding,
                if le {
                    Encoding::Utf16Le
                } else {
                    Encoding::Utf16Be
                }
            );
            assert_eq!(decode(no_bom, &s).text, text, "le={le} no bom");
        }
    }

    #[test]
    fn decode_ansi_uses_the_chosen_code_page() {
        let bytes = vec![0xF9, 0xEC, 0xE5, 0xED]; // שלום in windows-1255
        let s = sniff(
            &bytes,
            &DecodeOptions {
                ansi: Some(WINDOWS_1255),
            },
        );
        assert_eq!(decode(bytes.clone(), &s).text, "שלום");
        let s = sniff(&bytes, &opts());
        assert_eq!(decode(bytes, &s).text, "ùìåí"); // the same bytes read as windows-1252
    }

    #[test]
    fn utf16_with_a_lone_surrogate_reports_errors() {
        let bytes = vec![0xFF, 0xFE, 0x00, 0xD8]; // lone high surrogate
        let s = sniff(&bytes, &opts());
        let d = decode(bytes, &s);
        assert!(d.had_errors);
        assert!(d.text.contains('\u{FFFD}'));
    }

    #[test]
    fn invalid_utf8_after_a_bom_is_decoded_lossily() {
        let bytes = b"\xEF\xBB\xBFab\xFFcd".to_vec();
        let s = sniff(&bytes, &opts());
        let d = decode(bytes, &s);
        assert!(d.had_errors);
        assert!(d.text.contains('\u{FFFD}'));
    }

    #[test]
    fn encode_round_trips_every_encoding() {
        let text = "a\r\nשלום\n😀";
        for (enc, bom) in [
            (Encoding::Utf8, false),
            (Encoding::Utf8Bom, true),
            (Encoding::Utf16Le, true),
            (Encoding::Utf16Le, false),
            (Encoding::Utf16Be, true),
            (Encoding::Utf16Be, false),
        ] {
            let bytes = encode(text, enc, bom).unwrap();
            let s = sniff(&bytes, &opts());
            let d = decode(bytes, &s);
            assert_eq!(
                d.text.trim_start_matches('\u{FEFF}'),
                text,
                "{enc:?} bom={bom}"
            );
        }
        let ansi = encode("שלום", Encoding::Ansi(WINDOWS_1255), false).unwrap();
        assert_eq!(ansi, [0xF9, 0xEC, 0xE5, 0xED]);
    }

    #[test]
    fn encode_refuses_unmappable_characters() {
        let err = encode("שלום", Encoding::Ansi(WINDOWS_1252), false).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Encoding);
    }

    #[test]
    fn encoding_names() {
        assert_eq!(Encoding::Utf8.name(), "UTF-8");
        assert_eq!(Encoding::Utf8Bom.name(), "UTF-8");
        assert_eq!(Encoding::Utf16Le.name(), "UTF-16LE");
        assert_eq!(Encoding::Ansi(WINDOWS_1255).name(), "windows-1255");
    }
}
