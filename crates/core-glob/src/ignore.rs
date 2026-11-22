//! Ignore lists for source-control tools (backlog E11.5-S3): the files a diff tool is told to
//! skip, such as `*package-lock.json,*gemfile.lock,*podfile.lock,*cartfile.resolved` (the pattern
//! list SourceTree and Beyond Compare users put in their tool settings).
//!
//! The list is separated by commas, semicolons or line breaks (a comma inside `{a,b}` belongs to
//! the brace block). Each entry is an ordinary exclude pattern of this crate: a pattern without a
//! `/` matches the file name at any depth, `*` and `?` and `[..]` work as in the folder filters,
//! and `!pattern` re-includes. Matching ignores case on Windows.

use crate::rules::{Decision, MatchOptions, RuleSet};

/// The lock files that make noisy diffs: the list from the tool settings this feature is modelled
/// on. These four are the built-in preset.
pub const LOCK_FILE_PATTERNS: &str =
    "*package-lock.json,*gemfile.lock,*podfile.lock,*cartfile.resolved";

/// More lock files (not part of the default preset).
pub const MORE_LOCK_FILE_PATTERNS: &str = "yarn.lock,pnpm-lock.yaml,Cargo.lock,composer.lock,poetry.lock,Pipfile.lock,go.sum,packages.lock.json";

/// A parsed ignore list.
#[derive(Debug, Clone)]
pub struct IgnoreList {
    rules: RuleSet,
    source: String,
}

/// Splits a list at commas, semicolons and line breaks, except inside `{...}`.
fn split_list(list: &str) -> Vec<String> {
    let mut out = Vec::new();
    let (mut cur, mut depth) = (String::new(), 0u32);
    for c in list.chars() {
        match c {
            '{' => {
                depth += 1;
                cur.push(c);
            }
            '}' => {
                depth = depth.saturating_sub(1);
                cur.push(c);
            }
            ',' | ';' | '\n' | '\r' if depth == 0 => {
                if !cur.trim().is_empty() {
                    out.push(cur.trim().to_string());
                }
                cur.clear();
            }
            _ => cur.push(c),
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

impl IgnoreList {
    /// Parses a list. A pattern the glob syntax rejects is an error naming it (an unbalanced `{`
    /// is not one: like in the Glob library it is an ordinary character).
    pub fn parse(list: &str) -> Result<IgnoreList, String> {
        let opts = MatchOptions {
            case_sensitive: !cfg!(windows),
            // a path given to `is_ignored` may use either separator: it is normalised to `/` first
            windows_paths: false,
            allow_negation: true,
        };
        let rules = RuleSet::parse(&split_list(list), &opts)
            .map_err(|e| format!("bad ignore pattern: {e:?}"))?;
        Ok(IgnoreList {
            rules,
            source: list.to_string(),
        })
    }

    /// The built-in lock-file preset.
    pub fn lock_files() -> IgnoreList {
        // the constant is a fixed, valid list (the unit tests parse it), so this cannot fail
        IgnoreList::parse(LOCK_FILE_PATTERNS).expect("the built-in lock-file list is valid")
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// The entries of the list, one pattern each (for folder filters that take a pattern list).
    pub fn patterns(&self) -> Vec<String> {
        split_list(&self.source)
    }

    /// The text the list was parsed from.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Whether a file is skipped. `path` may be absolute or relative and use `/` or `\`: only its
    /// components matter, so `C:\work\app\package-lock.json` and `a/b/package-lock.json` both match
    /// `*package-lock.json`.
    pub fn is_ignored(&self, path: &str) -> bool {
        let norm = path.replace('\\', "/");
        let trimmed = norm.trim_start_matches('/');
        // drop a drive prefix such as `C:`
        let rel = match trimmed.split_once(':') {
            Some((d, rest)) if d.len() == 1 && d.chars().all(|c| c.is_ascii_alphabetic()) => rest,
            _ => trimmed,
        };
        matches!(self.rules.decide(rel, false), Decision::Excluded)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lock_file_preset_matches_the_listed_names_at_any_depth() {
        let l = IgnoreList::lock_files();
        for p in [
            "package-lock.json",
            "web/package-lock.json",
            "C:\\work\\app\\package-lock.json",
            "my-package-lock.json",
            "ios/Podfile.lock",
            "Gemfile.lock",
            "Carthage/Cartfile.resolved",
            "sub\\dir\\PODFILE.LOCK",
        ] {
            assert_eq!(
                l.is_ignored(p),
                cfg!(windows) || p.chars().all(|c| !c.is_ascii_uppercase()),
                "{p}"
            );
        }
        for p in [
            "package.json",
            "lock.json",
            "src/gemfile.rb",
            "notes/podfile.lock.txt",
            "cartfile",
        ] {
            assert!(!l.is_ignored(p), "{p}");
        }
    }

    #[test]
    fn lists_split_on_commas_semicolons_and_lines_but_not_inside_braces() {
        let l = IgnoreList::parse("*.o; *.obj\n*.{c,h}.bak ,  build/").unwrap();
        assert!(
            l.is_ignored("x.o")
                && l.is_ignored("a/x.obj")
                && l.is_ignored("a/b.c.bak")
                && l.is_ignored("b.h.bak")
        );
        assert!(!l.is_ignored("b.cpp.bak"));
        assert!(!IgnoreList::parse("").unwrap().is_ignored("anything"));
        assert!(IgnoreList::parse("").unwrap().is_empty());
    }

    #[test]
    fn a_negated_entry_re_includes() {
        let l = IgnoreList::parse("*.lock,!keep.lock").unwrap();
        assert!(l.is_ignored("a.lock"));
        assert!(!l.is_ignored("keep.lock"));
    }

    #[test]
    fn an_unbalanced_brace_is_taken_literally_like_in_the_glob_library() {
        let l = IgnoreList::parse("*.{c,h").unwrap();
        assert!(!l.is_ignored("x.c"));
    }
}
