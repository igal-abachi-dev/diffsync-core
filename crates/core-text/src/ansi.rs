//! The system ANSI code page as an `encoding_rs` encoding (plan.md §4.1).
//!
//! On Windows the code page is `GetACP()`; elsewhere (and for code pages `encoding_rs` does
//! not know) it is windows-1252. Callers can always override it with
//! `DecodeOptions::ansi`, which is what tests do so results never depend on the machine.

use encoding_rs::Encoding;

/// Maps a Windows code page number to an encoding. Unknown pages, and 65001 (UTF-8, which
/// is not an "ANSI" fallback because UTF-8 is tried first), give windows-1252.
pub fn encoding_for_codepage(cp: u32) -> &'static Encoding {
    use encoding_rs::{
        BIG5, EUC_JP, EUC_KR, GB18030, GBK, ISO_2022_JP, ISO_8859_2, ISO_8859_3, ISO_8859_4,
        ISO_8859_5, ISO_8859_6, ISO_8859_7, ISO_8859_8, ISO_8859_10, ISO_8859_13, ISO_8859_14,
        ISO_8859_15, ISO_8859_16, KOI8_R, KOI8_U, MACINTOSH, SHIFT_JIS, WINDOWS_874, WINDOWS_1250,
        WINDOWS_1251, WINDOWS_1252, WINDOWS_1253, WINDOWS_1254, WINDOWS_1255, WINDOWS_1256,
        WINDOWS_1257, WINDOWS_1258, X_MAC_CYRILLIC,
    };
    match cp {
        874 => WINDOWS_874,
        932 => SHIFT_JIS,
        936 => GBK,
        949 => EUC_KR,
        950 => BIG5,
        1250 => WINDOWS_1250,
        1251 => WINDOWS_1251,
        1253 => WINDOWS_1253,
        1254 => WINDOWS_1254,
        1255 => WINDOWS_1255,
        1256 => WINDOWS_1256,
        1257 => WINDOWS_1257,
        1258 => WINDOWS_1258,
        10000 => MACINTOSH,
        10007 => X_MAC_CYRILLIC,
        20866 => KOI8_R,
        21866 => KOI8_U,
        28592 => ISO_8859_2,
        28593 => ISO_8859_3,
        28594 => ISO_8859_4,
        28595 => ISO_8859_5,
        28596 => ISO_8859_6,
        28597 => ISO_8859_7,
        28598 => ISO_8859_8,
        28600 => ISO_8859_10,
        28603 => ISO_8859_13,
        28604 => ISO_8859_14,
        28605 => ISO_8859_15,
        28606 => ISO_8859_16,
        50220..=50222 => ISO_2022_JP,
        51932 => EUC_JP,
        54936 => GB18030,
        _ => WINDOWS_1252,
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod sys {
    // SAFETY: `GetACP` is a plain kernel32 export with no arguments and no preconditions;
    // it only reads a process-wide setting. Declaring it `safe` is sound for that reason.
    unsafe extern "system" {
        pub safe fn GetACP() -> u32;
    }
}

/// The active ANSI code page of this machine (`GetACP` on Windows, 1252 elsewhere).
pub fn system_codepage() -> u32 {
    #[cfg(windows)]
    {
        sys::GetACP()
    }
    #[cfg(not(windows))]
    {
        1252
    }
}

/// The system ANSI encoding, resolved once.
pub fn system_ansi_encoding() -> &'static Encoding {
    use std::sync::OnceLock;
    static ENC: OnceLock<&'static Encoding> = OnceLock::new();
    ENC.get_or_init(|| encoding_for_codepage(system_codepage()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn well_known_pages() {
        assert_eq!(encoding_for_codepage(1252), encoding_rs::WINDOWS_1252);
        assert_eq!(encoding_for_codepage(1255), encoding_rs::WINDOWS_1255);
        assert_eq!(encoding_for_codepage(1251), encoding_rs::WINDOWS_1251);
        assert_eq!(encoding_for_codepage(932), encoding_rs::SHIFT_JIS);
        assert_eq!(encoding_for_codepage(936), encoding_rs::GBK);
    }

    #[test]
    fn utf8_and_unknown_pages_fall_back_to_1252() {
        assert_eq!(encoding_for_codepage(65001), encoding_rs::WINDOWS_1252);
        assert_eq!(encoding_for_codepage(0), encoding_rs::WINDOWS_1252);
        assert_eq!(encoding_for_codepage(99999), encoding_rs::WINDOWS_1252);
    }

    #[test]
    fn the_system_encoding_resolves_and_is_never_utf8() {
        let e = system_ansi_encoding();
        assert_ne!(e, encoding_rs::UTF_8);
        assert!(system_codepage() > 0);
    }
}
