//! `Name` and `VPath` (plan.md §6.1, §6.4, backlog E5-S1.2).
//!
//! A `Name` is one path component stored as WTF-16 code units, so Windows names with unpaired
//! surrogates round-trip losslessly. A `VPath` is a sequence of names relative to the root of a
//! `Vfs`. Paths are never built by string concatenation, and names that came from a listing are
//! never normalised (trailing dots and spaces are real).

use std::fmt;

/// One path component, as UTF-16 code units (unpaired surrogates allowed).
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct Name(Vec<u16>);

impl Name {
    pub fn new(s: &str) -> Self {
        Name(s.encode_utf16().collect())
    }

    /// From raw code units; keeps unpaired surrogates.
    pub fn from_units(units: &[u16]) -> Self {
        Name(units.to_vec())
    }

    pub fn units(&self) -> &[u16] {
        &self.0
    }

    pub fn len_units(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The name as a `String` when it is valid UTF-16.
    pub fn to_string_exact(&self) -> Option<String> {
        String::from_utf16(&self.0).ok()
    }

    /// The name with unpaired surrogates replaced by U+FFFD (for display only).
    pub fn to_string_lossy(&self) -> String {
        String::from_utf16_lossy(&self.0)
    }

    /// Display form that never loses information: unpaired surrogates are shown as `\uD800`.
    pub fn display_escaped(&self) -> String {
        let mut out = String::new();
        for r in char::decode_utf16(self.0.iter().copied()) {
            match r {
                Ok(c) => out.push(c),
                Err(e) => out.push_str(&format!("\\u{:04X}", e.unpaired_surrogate())),
            }
        }
        out
    }

    /// Converts to an `OsString` (lossless on Windows; lossy on other platforms).
    pub fn to_os_string(&self) -> std::ffi::OsString {
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStringExt;
            std::ffi::OsString::from_wide(&self.0)
        }
        #[cfg(not(windows))]
        {
            std::ffi::OsString::from(self.to_string_lossy())
        }
    }

    pub fn from_os_str(s: &std::ffi::OsStr) -> Self {
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;
            Name(s.encode_wide().collect())
        }
        #[cfg(not(windows))]
        {
            Name::new(&s.to_string_lossy())
        }
    }
}

impl fmt::Debug for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Name({:?})", self.display_escaped())
    }
}

impl fmt::Display for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.display_escaped())
    }
}

impl From<&str> for Name {
    fn from(s: &str) -> Self {
        Name::new(s)
    }
}

/// A root-relative path: a list of names. The empty path is the root itself.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct VPath(Vec<Name>);

impl VPath {
    pub fn root() -> Self {
        VPath(Vec::new())
    }

    /// From UTF-8 components (`"a"`, `"b"`). Empty components are rejected by `join`.
    pub fn from_parts<I, S>(parts: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        VPath(parts.into_iter().map(|p| Name::new(p.as_ref())).collect())
    }

    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }

    pub fn components(&self) -> &[Name] {
        &self.0
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// This path with one more component.
    #[must_use]
    pub fn join(&self, name: impl Into<Name>) -> VPath {
        let mut v = self.0.clone();
        v.push(name.into());
        VPath(v)
    }

    pub fn parent(&self) -> Option<VPath> {
        (!self.0.is_empty()).then(|| VPath(self.0[..self.0.len() - 1].to_vec()))
    }

    pub fn file_name(&self) -> Option<&Name> {
        self.0.last()
    }

    /// True when `self` is `other` or lies below it.
    pub fn starts_with(&self, other: &VPath) -> bool {
        self.0.len() >= other.0.len() && self.0[..other.0.len()] == other.0[..]
    }

    /// `a/b/c` for display and logs (escaped, never used to build OS paths).
    pub fn display(&self) -> String {
        if self.0.is_empty() {
            return ".".to_string();
        }
        self.0
            .iter()
            .map(Name::display_escaped)
            .collect::<Vec<_>>()
            .join("/")
    }
}

impl fmt::Debug for VPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "VPath({})", self.display())
    }
}

impl fmt::Display for VPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.display())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unpaired_surrogates_round_trip() {
        let units = [0x61u16, 0xD800, 0x62, 0xDC00, 0xD83D, 0xDE00];
        let n = Name::from_units(&units);
        assert_eq!(n.units(), units);
        assert_eq!(n.to_string_exact(), None);
        assert_eq!(n.display_escaped(), "a\\uD800b\\uDC00\u{1F600}");
        assert!(n.to_string_lossy().contains('\u{FFFD}'));
        #[cfg(windows)]
        assert_eq!(
            Name::from_os_str(&n.to_os_string()),
            n,
            "lossless through OsString on Windows"
        );
    }

    #[test]
    fn names_are_never_normalised() {
        for s in ["foo.", "foo ", " foo", "a  b", "CON", "x."] {
            assert_eq!(Name::new(s).to_string_exact().as_deref(), Some(s));
        }
    }

    #[test]
    fn vpath_join_parent_and_prefix() {
        let p = VPath::root().join("a").join("b");
        assert_eq!(p.display(), "a/b");
        assert_eq!(p.parent().unwrap().display(), "a");
        assert_eq!(p.parent().unwrap().parent().unwrap(), VPath::root());
        assert_eq!(VPath::root().parent(), None);
        assert!(p.starts_with(&VPath::from_parts(["a"])));
        assert!(!VPath::from_parts(["a"]).starts_with(&p));
        assert!(p.starts_with(&p) && p.starts_with(&VPath::root()));
        assert_eq!(p.file_name().unwrap().to_string_exact().unwrap(), "b");
        assert_eq!(VPath::root().display(), ".");
    }

    #[test]
    fn case_is_significant_in_equality() {
        assert_ne!(Name::new("A"), Name::new("a"));
    }
}
