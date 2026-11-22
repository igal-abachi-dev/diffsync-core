//! CRC-32 in the two flavours the engine needs (plan.md §3, spec ALIGN-1.2).
//!
//! * **Line hash** (aligner): reflected table, polynomial `EDB88320`, initial value
//!   `FFFFFFFF`, **no final xor**, fed with the UTF-16LE bytes of the text. The empty text
//!   hashes to `FFFFFFFF`.
//! * **File CRC** (folder compare "CRC32" mode): the ordinary CRC-32 (final xor), computed
//!   with `crc32fast`.

/// Reflected CRC-32 polynomial (`EDB88320`).
const POLY: u32 = 0xEDB8_8320;

const fn build_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { (c >> 1) ^ POLY } else { c >> 1 };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
}

static TABLE: [u32; 256] = build_table();

#[inline]
fn step(crc: u32, byte: u8) -> u32 {
    TABLE[((crc ^ u32::from(byte)) & 0xFF) as usize] ^ (crc >> 8)
}

/// Raw table update: no start value and no final xor. `update(0xFFFF_FFFF, data)` is the
/// line-hash form; `!update(!0, data)` is the standard CRC-32.
pub fn update(crc: u32, data: &[u8]) -> u32 {
    data.iter().fold(crc, |c, &b| step(c, b))
}

/// Line hash over a stream of UTF-16 code units, each fed little-endian.
pub fn line_hash_utf16le(units: impl Iterator<Item = u16>) -> u32 {
    let mut crc = 0xFFFF_FFFF;
    for u in units {
        crc = step(crc, u as u8);
        crc = step(crc, (u >> 8) as u8);
    }
    crc
}

/// Line hash of a UTF-8 string, hashed as its UTF-16LE form (supplementary characters as
/// surrogate pairs). `""` gives `FFFFFFFF`. Allocation free.
pub fn line_hash_str(s: &str) -> u32 {
    let mut crc = 0xFFFF_FFFF;
    if s.is_ascii() {
        // ASCII fast path: the high byte of every unit is 0.
        for &b in s.as_bytes() {
            crc = step(crc, b);
            crc = step(crc, 0);
        }
        return crc;
    }
    let mut buf = [0u16; 2];
    for ch in s.chars() {
        for &u in ch.encode_utf16(&mut buf).iter() {
            crc = step(crc, u as u8);
            crc = step(crc, (u >> 8) as u8);
        }
    }
    crc
}

/// Standard CRC-32 of a whole buffer (final xor applied).
pub fn file_crc32(data: &[u8]) -> u32 {
    crc32fast::hash(data)
}

/// Streaming standard CRC-32 for file content, fed block by block.
#[derive(Clone, Default)]
pub struct FileCrc(crc32fast::Hasher);

impl std::fmt::Debug for FileCrc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FileCrc(..)")
    }
}

impl FileCrc {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn update(&mut self, data: &[u8]) {
        self.0.update(data);
    }

    pub fn finalize(self) -> u32 {
        self.0.finalize()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Vectors from plan.md §11.3, each checked against
    /// `python -I .claude/skills/golden-case/scripts/refmodel.py hash ...`.
    const LINE_VECTORS: &[(&str, u32)] = &[
        ("", 0xFFFF_FFFF),
        ("a", 0xC2C0_B7E6),
        ("abc", 0x526A_854F),
        ("123456789", 0x5D6F_1788),
        ("  x  ", 0x7899_F23F),
        ("שלום", 0xAADB_A6C2),
        ("\u{1F600}", 0x3E0B_9BC4),
    ];

    #[test]
    fn line_hash_vectors() {
        for &(text, want) in LINE_VECTORS {
            assert_eq!(line_hash_str(text), want, "line_hash_str({text:?})");
            assert_eq!(
                line_hash_utf16le(text.encode_utf16()),
                want,
                "line_hash_utf16le({text:?})"
            );
        }
    }

    #[test]
    fn ascii_fast_path_equals_generic_path() {
        // Force the generic path by mixing in one non-ASCII char and compare prefixes.
        for s in [
            "",
            "x",
            "hello world",
            "\t tabs and   spaces ",
            "0123456789abcdef",
        ] {
            assert_eq!(
                line_hash_str(s),
                line_hash_utf16le(s.encode_utf16()),
                "{s:?}"
            );
        }
    }

    #[test]
    fn surrogate_pairs_hash_as_two_units() {
        // U+1F600 = D83D DE00. Hashing the two units by hand must give the same result.
        assert_eq!(
            line_hash_utf16le([0xD83D_u16, 0xDE00].into_iter()),
            0x3E0B_9BC4
        );
    }

    #[test]
    fn standard_crc_check_value() {
        assert_eq!(file_crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(file_crc32(b""), 0);
    }

    #[test]
    fn raw_update_with_final_xor_is_the_standard_crc() {
        let data = b"The quick brown fox jumps over the lazy dog";
        assert_eq!(!update(0xFFFF_FFFF, data), file_crc32(data));
        assert_eq!(file_crc32(data), 0x414F_A339);
    }

    #[test]
    fn update_is_incremental() {
        let (a, b) = (b"12345".as_slice(), b"6789".as_slice());
        assert_eq!(
            update(update(0xFFFF_FFFF, a), b),
            update(0xFFFF_FFFF, b"123456789")
        );
    }

    #[test]
    fn streaming_file_crc_matches_one_shot() {
        let mut h = FileCrc::new();
        h.update(b"1234");
        h.update(b"56789");
        assert_eq!(h.finalize(), 0xCBF4_3926);
    }

    #[test]
    fn table_has_the_known_first_entries() {
        // Well-known entries of the reflected EDB88320 table.
        assert_eq!(TABLE[0], 0);
        assert_eq!(TABLE[1], 0x7707_3096);
        assert_eq!(TABLE[255], 0x2D02_EF8D);
    }
}
