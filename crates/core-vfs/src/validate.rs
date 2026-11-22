//! Destination name validation (plan.md §6.4 "Names inside a tree", §9.8, backlog E5-S9).
//!
//! Problem names become plan errors in the preview, before anything is written.

use std::collections::HashMap;

use crate::vfs::VfsCaps;

/// The outcome of checking one name against a target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameCheck {
    Ok,
    /// Contains a character the target cannot store (`< > " / \ | ? *`, control characters).
    InvalidChars,
    /// A Windows device name (`CON`, `aux.c`, `COM1`, ...): reachable only through `\\?\`, so
    /// it is a **warning**, not an error.
    ReservedName,
    /// Longer than the target allows.
    TooLong,
    /// Contains `:` on a Windows target: writing it would create an NTFS alternate data
    /// stream on the part before the colon. Never written.
    ColonStream,
    /// Empty, `.` or `..`.
    InvalidName,
}

impl NameCheck {
    /// True for outcomes that must stop the copy (everything except `Ok` and the warning).
    pub fn is_error(self) -> bool {
        !matches!(self, NameCheck::Ok | NameCheck::ReservedName)
    }
}

/// Windows device names: `CON PRN AUX NUL`, `COM0-9`, `LPT0-9` (also with the superscript
/// digits `¹ ² ³`), with or without an extension, case-insensitive.
pub fn is_reserved_windows_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or("").trim_end_matches(' ');
    let upper = stem.to_uppercase();
    match upper.as_str() {
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$" => true,
        _ => {
            let chars: Vec<char> = upper.chars().collect();
            matches!(chars.as_slice(), [a, b, c, d]
                if matches!(String::from_iter([*a, *b, *c]).as_str(), "COM" | "LPT")
                    && (d.is_ascii_digit() || matches!(d, '\u{B9}' | '\u{B2}' | '\u{B3}')))
        }
    }
}

/// Checks `name` against the target's rules.
pub fn validate_name(target: &VfsCaps, name: &str) -> NameCheck {
    if name.is_empty() || name == "." || name == ".." {
        return NameCheck::InvalidName;
    }
    if name.encode_utf16().count() > target.max_name_units {
        return NameCheck::TooLong;
    }
    if name.contains('\0') || (target.windows_names && name.chars().any(|c| (c as u32) < 0x20)) {
        return NameCheck::InvalidChars;
    }
    if !target.windows_names {
        return if name.contains('/') {
            NameCheck::InvalidChars
        } else {
            NameCheck::Ok
        };
    }
    if name.contains(':') {
        return NameCheck::ColonStream;
    }
    if name
        .chars()
        .any(|c| matches!(c, '<' | '>' | '"' | '/' | '\\' | '|' | '?' | '*'))
    {
        return NameCheck::InvalidChars;
    }
    if is_reserved_windows_name(name) {
        return NameCheck::ReservedName;
    }
    NameCheck::Ok
}

/// Groups of names that differ only by case. Meaningful for a case-insensitive target: names
/// that are distinct on a case-sensitive source would be silently merged there. Returns the
/// indexes of every group with more than one member (nothing when the target is case
/// sensitive). Folding is per-character uppercase; the NTFS rules of D6 refine it in `core-folder`.
pub fn case_collisions(names: &[&str], target_case_sensitive: bool) -> Vec<Vec<usize>> {
    if target_case_sensitive {
        return Vec::new();
    }
    let mut groups: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, n) in names.iter().enumerate() {
        let key: String = n
            .chars()
            .map(|c| {
                let mut up = c.to_uppercase();
                match (up.next(), up.next()) {
                    (Some(u), None) => u,
                    _ => c, // multi-character uppercase (ß): leave as is, like NTFS
                }
            })
            .collect();
        groups.entry(key).or_default().push(i);
    }
    let mut out: Vec<Vec<usize>> = groups.into_values().filter(|g| g.len() > 1).collect();
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win() -> VfsCaps {
        VfsCaps {
            windows_names: true,
            case_sensitive: false,
            ..VfsCaps::default()
        }
    }

    fn posix() -> VfsCaps {
        VfsCaps::default()
    }

    /// The path golden rows for destination names (plan.md §11.3).
    #[test]
    fn golden_rows() {
        assert_eq!(
            validate_name(&win(), "aux.c"),
            NameCheck::ReservedName,
            "aux.c: warning, still written via \\\\?\\"
        );
        assert!(!NameCheck::ReservedName.is_error());
        assert_eq!(validate_name(&win(), "a:b"), NameCheck::ColonStream);
        assert!(NameCheck::ColonStream.is_error());
        assert_eq!(case_collisions(&["A", "a"], false), vec![vec![0, 1]]);
        assert!(case_collisions(&["A", "a"], true).is_empty());
    }

    #[test]
    fn invalid_characters() {
        for n in [
            "a<b", "a>b", "a\"b", "a|b", "a?b", "a*b", "a\\b", "a/b", "a\u{1}b",
        ] {
            assert_eq!(validate_name(&win(), n), NameCheck::InvalidChars, "{n:?}");
        }
        assert_eq!(validate_name(&win(), "ok name.txt"), NameCheck::Ok);
        assert_eq!(validate_name(&win(), "שלום"), NameCheck::Ok);
    }

    #[test]
    fn reserved_names_with_and_without_extension() {
        for n in [
            "CON",
            "con",
            "Prn.txt",
            "AUX",
            "nul.tar.gz",
            "COM1",
            "lpt9.log",
            "COM\u{B9}",
            "COM0",
        ] {
            assert_eq!(validate_name(&win(), n), NameCheck::ReservedName, "{n}");
        }
        for n in ["CONSOLE", "COM10", "COMX", "aux1", "xcon", "COM"] {
            assert_eq!(validate_name(&win(), n), NameCheck::Ok, "{n}");
        }
    }

    #[test]
    fn length_limit_counts_utf16_units() {
        let ok = "a".repeat(255);
        assert_eq!(validate_name(&win(), &ok), NameCheck::Ok);
        assert_eq!(validate_name(&win(), &format!("{ok}b")), NameCheck::TooLong);
        // 128 emoji are 256 UTF-16 units
        assert_eq!(validate_name(&win(), &"😀".repeat(128)), NameCheck::TooLong);
        assert_eq!(validate_name(&win(), &"😀".repeat(127)), NameCheck::Ok);
    }

    #[test]
    fn posix_targets_allow_colons_and_backslashes_but_not_slashes() {
        assert_eq!(validate_name(&posix(), "a:b"), NameCheck::Ok);
        assert_eq!(validate_name(&posix(), "a\\b"), NameCheck::Ok);
        assert_eq!(validate_name(&posix(), "a/b"), NameCheck::InvalidChars);
        assert_eq!(validate_name(&posix(), "aux.c"), NameCheck::Ok);
    }

    #[test]
    fn odd_but_legal_names() {
        assert_eq!(
            validate_name(&win(), "foo."),
            NameCheck::Ok,
            "trailing dots exist and are kept"
        );
        assert_eq!(validate_name(&win(), "foo "), NameCheck::Ok);
        assert_eq!(validate_name(&win(), ""), NameCheck::InvalidName);
        assert_eq!(validate_name(&win(), ".."), NameCheck::InvalidName);
    }

    #[test]
    fn collisions_group_by_case_and_ignore_sharp_s() {
        let g = case_collisions(
            &["Readme", "README", "other", "readme", "Straße", "STRASSE"],
            false,
        );
        assert_eq!(g, vec![vec![0, 1, 3]]);
        assert!(case_collisions(&["a", "b"], false).is_empty());
    }
}
