//! Root parsing and long paths (plan.md §6.4 "Root paths", backlog E5-S2, §11.3 path golden
//! values). Pure string logic: it never touches the file system and **never consults the
//! process current directory**, so it behaves the same on every platform.

use core_base::{Error, Result};

use crate::path::VPath;

/// What kind of root a path names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RootKind {
    /// `C:\`
    Drive(char),
    /// `\\server\share\`
    Unc { server: String, share: String },
    /// `\\?\C:\`, `\\?\UNC\server\share\`, `\\?\Volume{GUID}\`: kept as given.
    Verbatim,
    /// `/` on Unix-like systems.
    Posix,
}

/// A parsed absolute path: a root plus the components below it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VfsRoot {
    kind: RootKind,
    /// The root prefix in display form with a trailing separator, e.g. `C:\`,
    /// `\\server\share\`, `\\?\C:\`.
    root: String,
    /// Components below the root, lexically resolved.
    parts: Vec<String>,
}

fn invalid(input: &str, why: &str) -> Error {
    Error::invalid_argument(format!("{why}: `{input}`"))
}

/// Resolves `.` and `..` lexically. `..` above the root is an error.
fn resolve(input: &str, comps: &str) -> Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for c in comps.split(['\\', '/']) {
        match c {
            "" | "." => {}
            ".." => {
                if out.pop().is_none() {
                    return Err(invalid(input, "`..` goes above the root"));
                }
            }
            other => out.push(other.to_string()),
        }
    }
    Ok(out)
}

impl VfsRoot {
    /// Parses a path the way the platform understands it: Windows forms on Windows, absolute
    /// POSIX paths elsewhere.
    pub fn parse(input: &str) -> Result<VfsRoot> {
        if cfg!(windows) {
            Self::parse_windows(input)
        } else {
            Self::parse_posix(input)
        }
    }

    /// An absolute POSIX path (`/a/b`). Anything else is rejected.
    pub fn parse_posix(input: &str) -> Result<VfsRoot> {
        let Some(rest) = input.strip_prefix('/') else {
            return Err(invalid(input, "not an absolute path"));
        };
        Ok(VfsRoot {
            kind: RootKind::Posix,
            root: "/".to_string(),
            parts: resolve(input, rest)?,
        })
    }

    /// Windows path forms (accepted on every platform so the rules can be tested anywhere):
    ///
    /// * `C:\x`, `C:/x`: both separators; `C:` alone is rejected (it means the *current
    ///   directory* of drive C), `C:foo` is rejected (drive-relative);
    /// * `\\server\share\x`, `//server/share/x`: the root is server **and** share;
    ///   `\\server` alone is rejected;
    /// * `\\?\...`: verbatim, kept; `\\.\...`: device paths are rejected;
    /// * relative (`foo\bar`) and rooted-without-drive (`\foo`) paths are rejected;
    /// * `.` and `..` are resolved lexically; `..` above the root is an error.
    pub fn parse_windows(input: &str) -> Result<VfsRoot> {
        if input.is_empty() {
            return Err(invalid(input, "empty path"));
        }
        let b = input.as_bytes();
        let sep = |c: u8| c == b'\\' || c == b'/';
        // verbatim and device prefixes
        if input.len() >= 4 && sep(b[0]) && sep(b[1]) && (b[2] == b'?' || b[2] == b'.') && sep(b[3])
        {
            if b[2] == b'.' {
                return Err(invalid(input, "device paths are not allowed"));
            }
            return Self::parse_verbatim(input);
        }
        // UNC
        if input.len() >= 2 && sep(b[0]) && sep(b[1]) {
            let rest = &input[2..];
            let mut it = rest.split(['\\', '/']).filter(|s| !s.is_empty());
            let (Some(server), Some(share)) = (it.next(), it.next()) else {
                return Err(invalid(input, "a UNC path needs a server and a share"));
            };
            if server == "." || server == ".." || share == "." || share == ".." {
                return Err(invalid(input, "bad UNC server or share"));
            }
            // components after the share
            let after: String = {
                // skip server and share in the original text
                let mut seen = 0;
                let mut idx = 0;
                let bytes = rest.as_bytes();
                while idx < bytes.len() && seen < 2 {
                    while idx < bytes.len() && sep(bytes[idx]) {
                        idx += 1;
                    }
                    while idx < bytes.len() && !sep(bytes[idx]) {
                        idx += 1;
                    }
                    seen += 1;
                }
                rest[idx..].to_string()
            };
            return Ok(VfsRoot {
                kind: RootKind::Unc {
                    server: server.to_string(),
                    share: share.to_string(),
                },
                root: format!("\\\\{server}\\{share}\\"),
                parts: resolve(input, &after)?,
            });
        }
        // drive letter
        if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
            let letter = (b[0] as char).to_ascii_uppercase();
            if b.len() == 2 {
                return Err(invalid(
                    input,
                    "`C:` means the current directory of the drive; use `C:\\`",
                ));
            }
            if !sep(b[2]) {
                return Err(invalid(input, "drive-relative path"));
            }
            return Ok(VfsRoot {
                kind: RootKind::Drive(letter),
                root: format!("{letter}:\\"),
                parts: resolve(input, &input[3..])?,
            });
        }
        if sep(b[0]) {
            return Err(invalid(input, "rooted path without a drive"));
        }
        Err(invalid(
            input,
            "relative paths depend on the current directory and are not allowed",
        ))
    }

    fn parse_verbatim(input: &str) -> Result<VfsRoot> {
        // `\\?\` + rest, where rest is `C:\...`, `UNC\server\share\...` or `Volume{...}\...`.
        // Verbatim paths are not normalised by Windows, so `.`/`..` are not resolved either.
        let rest = &input[4..];
        let norm = rest.replace('/', "\\");
        let (root_len, valid) = if norm.len() >= 3
            && norm.as_bytes()[0].is_ascii_alphabetic()
            && norm.get(1..3) == Some(":\\")
        {
            (3, true)
        } else if norm
            .get(..4)
            .is_some_and(|p| p.eq_ignore_ascii_case("UNC\\"))
        {
            let after = &norm[4..];
            let mut it = after.splitn(3, '\\');
            match (it.next(), it.next()) {
                (Some(s), Some(h)) if !s.is_empty() && !h.is_empty() => (
                    4 + s.len() + 1 + h.len() + usize::from(after.len() > s.len() + 1 + h.len()),
                    true,
                ),
                _ => (0, false),
            }
        } else if norm.to_ascii_lowercase().starts_with("volume{") {
            (norm.find('\\').map_or(norm.len(), |i| i + 1), true)
        } else {
            (0, false)
        };
        if !valid {
            return Err(invalid(input, "unsupported verbatim path"));
        }
        if norm.split('\\').any(|c| c == "." || c == "..") {
            return Err(invalid(
                input,
                "verbatim paths must not contain `.` or `..`",
            ));
        }
        let mut root = format!(
            "\\\\?\\{}",
            norm.get(..root_len.min(norm.len())).unwrap_or("")
        );
        if !root.ends_with('\\') {
            root.push('\\');
        }
        let parts: Vec<String> = norm[root_len.min(norm.len())..]
            .split('\\')
            .filter(|s| !s.is_empty())
            .map(ToString::to_string)
            .collect();
        Ok(VfsRoot {
            kind: RootKind::Verbatim,
            root,
            parts,
        })
    }

    /// The root one level up (the last component removed), or `None` at the volume root.
    pub fn parent(&self) -> Option<VfsRoot> {
        if self.parts.is_empty() {
            return None;
        }
        let mut p = self.clone();
        p.parts.pop();
        Some(p)
    }

    pub fn kind(&self) -> &RootKind {
        &self.kind
    }

    /// The root prefix with a trailing separator (`C:\`, `\\server\share\`).
    pub fn root_string(&self) -> &str {
        &self.root
    }

    /// The components below the root.
    pub fn parts(&self) -> &[String] {
        &self.parts
    }

    /// The path below the root as a `VPath`.
    pub fn vpath(&self) -> VPath {
        VPath::from_parts(&self.parts)
    }

    /// The long form `\\?\C:\a\b`, `\\?\UNC\server\share\a` (Windows kinds) or the POSIX path.
    pub fn long_form(&self) -> String {
        self.long_form_of(&self.vpath())
    }

    /// The long form of `rel` below this root's *root prefix* (the stored parts are not added).
    pub fn long_form_of(&self, rel: &VPath) -> String {
        let tail: Vec<String> = rel
            .components()
            .iter()
            .map(crate::path::Name::to_string_lossy)
            .collect();
        match &self.kind {
            RootKind::Drive(c) => join_win(format!("\\\\?\\{c}:\\"), &tail),
            RootKind::Unc { server, share } => {
                join_win(format!("\\\\?\\UNC\\{server}\\{share}\\"), &tail)
            }
            RootKind::Verbatim => join_win(self.root.clone(), &tail),
            RootKind::Posix => format!("/{}", tail.join("/")),
        }
    }

    /// The long form as NUL-terminated UTF-16, ready for a wide Win32 call. Components keep
    /// unpaired surrogates (`rel` is used as code units, not as text).
    pub fn to_long_wide(&self, rel: &VPath) -> Vec<u16> {
        let (prefix, sep): (String, u16) = match &self.kind {
            RootKind::Drive(c) => (format!("\\\\?\\{c}:\\"), u16::from(b'\\')),
            RootKind::Unc { server, share } => {
                (format!("\\\\?\\UNC\\{server}\\{share}\\"), u16::from(b'\\'))
            }
            RootKind::Verbatim => (self.root.clone(), u16::from(b'\\')),
            RootKind::Posix => ("/".to_string(), u16::from(b'/')),
        };
        let mut out: Vec<u16> = prefix.encode_utf16().collect();
        for (i, n) in rel.components().iter().enumerate() {
            if i > 0 {
                out.push(sep);
            }
            out.extend_from_slice(n.units());
        }
        out.push(0);
        out
    }
}

fn join_win(mut prefix: String, tail: &[String]) -> String {
    prefix.push_str(&tail.join("\\"));
    prefix
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_base::ErrorKind;

    fn ok(s: &str) -> VfsRoot {
        VfsRoot::parse_windows(s).unwrap_or_else(|e| panic!("{s}: {e}"))
    }

    fn rejected(s: &str) {
        let e = VfsRoot::parse_windows(s).expect_err(s);
        assert_eq!(e.kind(), ErrorKind::InvalidArgument, "{s}");
    }

    /// The path golden table of plan.md §11.3.
    #[test]
    fn path_golden_values() {
        rejected("C:");
        assert_eq!(ok("C:\\").root_string(), "C:\\");
        for bad in ["C:foo", "foo\\bar", "\\foo"] {
            rejected(bad);
        }
        let unc = ok("//server/share/x");
        assert_eq!(unc.root_string(), "\\\\server\\share\\");
        assert_eq!(unc.long_form(), "\\\\?\\UNC\\server\\share\\x");
        rejected("\\\\server");
        rejected("\\\\.\\PhysicalDrive0");
        assert_eq!(ok("C:\\a\\..\\b").long_form(), "\\\\?\\C:\\b");
    }

    #[test]
    fn both_separators_and_drive_case() {
        let r = ok("c:/Users/x\\y");
        assert_eq!(r.root_string(), "C:\\");
        assert_eq!(r.parts(), ["Users", "x", "y"]);
        assert_eq!(r.long_form(), "\\\\?\\C:\\Users\\x\\y");
        assert!(ok("D:\\").parts().is_empty());
        assert_eq!(ok("D:\\").long_form(), "\\\\?\\D:\\");
    }

    #[test]
    fn dot_and_dotdot_are_resolved_before_the_prefix() {
        assert_eq!(ok("C:\\a\\.\\b\\..\\c").parts(), ["a", "c"]);
        assert_eq!(ok("C:\\a\\b\\..\\..").parts().len(), 0);
        rejected("C:\\..");
        rejected("C:\\a\\..\\..\\b");
        rejected("\\\\server\\share\\..\\..");
    }

    #[test]
    fn verbatim_forms_are_kept() {
        let r = ok("\\\\?\\C:\\x\\y");
        assert_eq!(r.kind(), &RootKind::Verbatim);
        assert_eq!(r.long_form(), "\\\\?\\C:\\x\\y");
        let u = ok("\\\\?\\UNC\\srv\\shr\\dir");
        assert_eq!(u.long_form(), "\\\\?\\UNC\\srv\\shr\\dir");
        let v = ok("\\\\?\\Volume{01234567-89ab-cdef-0123-456789abcdef}\\data");
        assert_eq!(v.parts(), ["data"]);
        rejected("\\\\?\\bogus");
        rejected("\\\\?\\C:\\a\\..\\b");
    }

    #[test]
    fn unc_needs_server_and_share() {
        rejected("\\\\");
        rejected("\\\\server\\");
        assert_eq!(ok("\\\\server\\share").root_string(), "\\\\server\\share\\");
        assert_eq!(ok("\\\\server\\share\\a\\b").parts(), ["a", "b"]);
        assert_eq!(ok("//server//share//a").parts(), ["a"]);
    }

    #[test]
    fn non_ascii_verbatim_input_is_an_error_not_a_panic() {
        for bad in [
            "\\\\?\\a\u{20ac}x",
            "\\\\?\\\u{65e5}\u{672c}\u{8a9e}\\x",
            "\\\\?\\\u{20ac}\u{20ac}\u{20ac}\u{20ac}",
        ] {
            rejected(bad);
        }
        assert_eq!(
            ok("\\\\?\\C:\\\u{65e5}\u{672c}").parts(),
            ["\u{65e5}\u{672c}"]
        );
    }

    #[test]
    fn parent_roots() {
        let r = ok("C:\\a\\b");
        assert_eq!(r.parent().unwrap().parts(), ["a"]);
        assert!(ok("C:\\").parent().is_none());
    }

    #[test]
    fn empty_and_relative_are_rejected() {
        rejected("");
        rejected(".");
        rejected("..\\x");
        rejected("a");
    }

    #[test]
    fn posix_roots() {
        let r = VfsRoot::parse_posix("/a/./b/../c").unwrap();
        assert_eq!(r.parts(), ["a", "c"]);
        assert!(VfsRoot::parse_posix("a/b").is_err());
        assert!(VfsRoot::parse_posix("/..").is_err());
    }

    #[test]
    fn long_wide_keeps_surrogates_and_is_nul_terminated() {
        let r = ok("C:\\base");
        let rel = VPath::root()
            .join(crate::path::Name::from_units(&[0x61, 0xD800]))
            .join("c");
        let w = r.to_long_wide(&rel);
        let text: Vec<u16> = "\\\\?\\C:\\"
            .encode_utf16()
            .chain([0x61, 0xD800, u16::from(b'\\'), u16::from(b'c'), 0])
            .collect();
        assert_eq!(w, text);
    }

    /// Backlog E5-S2.2: a 300-character path converts without touching the file system.
    #[test]
    fn a_300_character_path_converts_to_the_long_form() {
        let long = format!("C:\\{}\\{}", "a".repeat(150), "b".repeat(150));
        let r = ok(&long);
        assert!(r.long_form().starts_with("\\\\?\\C:\\aaaa"));
        assert_eq!(r.long_form().len(), long.len() + 4);
    }
}
