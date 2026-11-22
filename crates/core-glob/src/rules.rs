//! Patterns and rule sets (port of `IgnoreFilter`, plan.md §7.5, backlog E6-S3).
//!
//! A rule has segments (`literal`/wild pattern or `**`), and three flags: **negated** (`!`),
//! **dir-only** (`name/`), and **match-base** (no `/` in the pattern: matches that name at any
//! depth) versus **anchored** (`/x`, or a `/` inside the pattern: matched from the root).
//!
//! `decide` follows gitignore semantics: the last matching rule wins, and a path inside an
//! excluded folder is excluded whatever later rules say (a `!` rule cannot re-include inside an
//! excluded folder). `**` matching is memoised over (pattern index, path index).

use crate::braces::{MAX_EXPANSIONS, PatternError, expand_braces};
use crate::matcher::Segment;

/// How names are matched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MatchOptions {
    pub case_sensitive: bool,
    /// Windows sources: `\` is a path separator. Unix-like sources: `\` escapes the next
    /// character (decision D15, plan.md §7.5).
    pub windows_paths: bool,
    /// Whether a leading `!` negates (exclude lists) or is a literal (include lists).
    pub allow_negation: bool,
}

impl Default for MatchOptions {
    fn default() -> Self {
        MatchOptions {
            case_sensitive: !cfg!(windows),
            windows_paths: cfg!(windows),
            allow_negation: true,
        }
    }
}

/// The verdict of a rule set for one path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Explicitly re-included by a `!` rule (the last matching rule was negated).
    Included,
    /// Excluded: the last matching rule excludes the path, or an ancestor folder is excluded.
    Excluded,
    /// No rule matches.
    NoMatch,
}

impl Decision {
    pub fn is_excluded(self) -> bool {
        self == Decision::Excluded
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Seg {
    Globstar,
    Name(Segment),
}

/// One parsed rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    segs: Vec<Seg>,
    pub negated: bool,
    pub dir_only: bool,
    /// True when the rule has no `/`: it matches a name at any depth.
    pub match_base: bool,
    pub source: String,
}

/// A warning about a rule that cannot work as written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleWarning {
    pub rule: String,
    pub message: String,
}

/// An ordered list of rules.
#[derive(Debug, Clone, Default)]
pub struct RuleSet {
    rules: Vec<Rule>,
    opts: Option<MatchOptions>,
}

impl RuleSet {
    /// Parses ignore lines: blank lines are skipped, braces expand (before separator
    /// normalisation, as in Glob), then each result becomes one rule.
    pub fn parse(
        lines: &[String],
        opts: &MatchOptions,
    ) -> std::result::Result<RuleSet, PatternError> {
        let mut rules = Vec::new();
        for raw in lines {
            let raw = raw.trim();
            if raw.is_empty() {
                continue;
            }
            // negation first, then braces on the body (a `!` is never inside a brace block)
            let (negated, body) = match raw.strip_prefix('!') {
                Some(rest) if opts.allow_negation => (true, rest),
                _ => (false, raw),
            };
            for pat in expand_braces(body, MAX_EXPANSIONS)? {
                if let Some(rule) = parse_rule(&pat, negated, opts) {
                    rules.push(rule);
                }
            }
        }
        Ok(RuleSet {
            rules,
            opts: Some(*opts),
        })
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }

    /// Decides for a path given as `/`-separated UTF-8 (convenience over [`decide_names`]).
    pub fn decide(&self, rel: &str, is_dir: bool) -> Decision {
        let names: Vec<Vec<u16>> = rel
            .split('/')
            .filter(|s| !s.is_empty() && *s != ".")
            .map(|s| s.encode_utf16().collect())
            .collect();
        let refs: Vec<&[u16]> = names.iter().map(Vec::as_slice).collect();
        self.decide_names(&refs, is_dir)
    }

    /// Decides for a path given as name components (UTF-16 units). The empty path is never
    /// excluded.
    pub fn decide_names(&self, names: &[&[u16]], is_dir: bool) -> Decision {
        if names.is_empty() {
            return Decision::NoMatch;
        }
        // shallowest excluded ancestor folder wins: nothing inside it can be re-included
        for k in 1..names.len() {
            if self.last_match(&names[..k], true) == Some(true) {
                return Decision::Excluded;
            }
        }
        match self.last_match(names, is_dir) {
            Some(true) => Decision::Excluded,
            Some(false) => Decision::Included,
            None => Decision::NoMatch,
        }
    }

    /// Like [`RuleSet::decide_names`] but looks at the last name only: ancestors are not
    /// tested. For callers that already walked the ancestors (the folder scan opens only folders
    /// that passed their own rules) and use a rule set that must not act on folders.
    pub fn decide_leaf(&self, names: &[&[u16]], is_dir: bool) -> Decision {
        if names.is_empty() {
            return Decision::NoMatch;
        }
        match self.last_match(names, is_dir) {
            Some(true) => Decision::Excluded,
            Some(false) => Decision::Included,
            None => Decision::NoMatch,
        }
    }

    /// `Some(true)` = excluded by the last matching rule, `Some(false)` = re-included.
    fn last_match(&self, names: &[&[u16]], is_dir: bool) -> Option<bool> {
        let mut result = None;
        for r in &self.rules {
            if r.matches_exact(names, is_dir) {
                result = Some(!r.negated);
            }
        }
        result
    }

    /// Warnings for `!` rules that can never apply: a negated rule whose literal leading
    /// folders are excluded by an earlier rule (git cannot re-include inside an excluded folder).
    pub fn validate(&self) -> Vec<RuleWarning> {
        let mut out = Vec::new();
        for (i, r) in self.rules.iter().enumerate() {
            if !r.negated || r.match_base {
                continue;
            }
            // literal leading segments of the negated rule
            let lits: Vec<Vec<u16>> = r
                .segs
                .iter()
                .map_while(|s| {
                    if let Seg::Name(n) = s {
                        n.literal_units()
                    } else {
                        None
                    }
                })
                .collect();
            for k in 1..lits.len() {
                let prefix: Vec<&[u16]> = lits[..k].iter().map(Vec::as_slice).collect();
                let earlier = RuleSet {
                    rules: self.rules[..i].to_vec(),
                    opts: self.opts,
                };
                if earlier.last_match(&prefix, true) == Some(true) {
                    out.push(RuleWarning {
                        rule: format!("!{}", r.source),
                        message: "can never apply: its folder is excluded by an earlier rule, and a `!` rule cannot re-include inside an excluded folder".to_string(),
                    });
                    break;
                }
            }
        }
        out
    }
}

fn parse_rule(pat: &str, negated: bool, opts: &MatchOptions) -> Option<Rule> {
    let mut p = pat.trim().to_string();
    if opts.windows_paths {
        p = p.replace('\\', "/");
    }
    let source = p.clone();
    let dir_only = p.len() > 1 && p.ends_with('/');
    if dir_only {
        p.pop();
    }
    let anchored = p.contains('/');
    let p = p.trim_start_matches('/');
    if p.is_empty() {
        return None;
    }
    let segs: Vec<Seg> = p
        .split('/')
        .filter(|s| !s.is_empty())
        .map(|s| {
            if s == "**" {
                Seg::Globstar
            } else {
                Seg::Name(Segment::compile(s, opts.case_sensitive))
            }
        })
        .collect();
    Some(Rule {
        segs,
        negated,
        dir_only,
        match_base: !anchored,
        source,
    })
}

impl Rule {
    /// Does this rule match exactly the path `names`? (Ancestors are handled by the caller.)
    pub fn matches_exact(&self, names: &[&[u16]], is_dir: bool) -> bool {
        if self.dir_only && !is_dir {
            return false;
        }
        if self.match_base {
            // a single-segment pattern against the last name; a lone `**` matches everything
            return match (self.segs.as_slice(), names.last()) {
                ([Seg::Name(s)], Some(n)) => s.matches(n),
                ([Seg::Globstar], Some(_)) => true,
                _ => match_segments(&self.segs, names),
            };
        }
        match_segments(&self.segs, names)
    }
}

/// Matches pattern segments against path names with `**` = zero or more names. Memoised over
/// (pattern index, path index): O(P x S), so several `**` on deep paths are not exponential.
fn match_segments(pat: &[Seg], names: &[&[u16]]) -> bool {
    let (p, s) = (pat.len(), names.len());
    // reach[i][j]: pattern[i..] matches names[j..]; computed from the end
    let mut reach = vec![false; (p + 1) * (s + 1)];
    let at = |i: usize, j: usize| i * (s + 1) + j;
    reach[at(p, s)] = true;
    for i in (0..p).rev() {
        for j in (0..=s).rev() {
            reach[at(i, j)] = match &pat[i] {
                Seg::Globstar => reach[at(i + 1, j)] || (j < s && reach[at(i, j + 1)]),
                Seg::Name(seg) => j < s && seg.matches(names[j]) && reach[at(i + 1, j + 1)],
            };
        }
    }
    reach[at(0, 0)]
}

/// A single anchored pattern (no negation, no dir-only handling): used by the walker.
#[derive(Debug, Clone)]
pub struct Pattern {
    segs: Vec<Seg>,
    pub dir_only: bool,
    pub match_base: bool,
}

impl Pattern {
    pub fn compile(pat: &str, opts: &MatchOptions) -> Option<Pattern> {
        let r = parse_rule(pat, false, opts)?;
        Some(Pattern {
            segs: r.segs,
            dir_only: r.dir_only,
            match_base: r.match_base,
        })
    }

    pub fn matches(&self, names: &[&[u16]], is_dir: bool) -> bool {
        Rule {
            segs: self.segs.clone(),
            negated: false,
            dir_only: self.dir_only,
            match_base: self.match_base,
            source: String::new(),
        }
        .matches_exact(names, is_dir)
    }

    /// The leading plain-name segments (the walker starts there).
    pub fn static_prefix(&self) -> Vec<Vec<u16>> {
        if self.match_base {
            return Vec::new();
        }
        let mut out: Vec<Vec<u16>> = self
            .segs
            .iter()
            .map_while(|s| {
                if let Seg::Name(n) = s {
                    n.literal_units()
                } else {
                    None
                }
            })
            .collect();
        // the last segment names the match itself, not a folder to start in
        if out.len() == self.segs.len() {
            out.pop();
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rs(lines: &[&str], case_sensitive: bool) -> RuleSet {
        let o = MatchOptions {
            case_sensitive,
            windows_paths: true,
            allow_negation: true,
        };
        RuleSet::parse(
            &lines.iter().map(ToString::to_string).collect::<Vec<_>>(),
            &o,
        )
        .unwrap()
    }

    fn excluded(lines: &[&str], path: &str, is_dir: bool) -> bool {
        rs(lines, false).decide(path, is_dir).is_excluded()
    }

    /// The filter golden table of plan.md §11.3 (case-insensitive source unless noted).
    #[test]
    fn golden_table() {
        // `*.obj`: match-base at any depth, not a longer name
        assert!(excluded(&["*.obj"], "a.obj", false) && excluded(&["*.obj"], "src/x/a.obj", false));
        assert!(!excluded(&["*.obj"], "a.obj.bak", false));
        // `/build`: anchored
        assert!(
            excluded(&["/build"], "build/x.o", false),
            "inside the excluded folder"
        );
        assert!(!excluded(&["/build"], "src/build/x.o", false));
        // `bin/`: folders only
        assert!(excluded(&["bin/"], "bin", true) && !excluded(&["bin/"], "bin", false));
        assert!(
            excluded(&["bin/"], "src/bin", true),
            "dir-only rules still match at any depth"
        );
        // `docs/**/*.md`
        assert!(
            excluded(&["docs/**/*.md"], "docs/a.md", false)
                && excluded(&["docs/**/*.md"], "docs/x/y/a.md", false)
        );
        assert!(!excluded(&["docs/**/*.md"], "other/a.md", false));
        // `**/node_modules`
        assert!(
            excluded(&["**/node_modules"], "node_modules", true)
                && excluded(&["**/node_modules"], "a/b/node_modules", true)
        );
        // `*` matches dot files (D15)
        assert!(excluded(&["*"], ".gitignore", false));
        // `*.log`, `!keep.log`: the last rule wins
        assert!(!excluded(&["*.log", "!keep.log"], "keep.log", false));
        assert!(excluded(&["*.log", "!keep.log"], "x.log", false));
        // `logs/`, `!logs/keep.txt`: still excluded, and validation warns
        let set = rs(&["logs/", "!logs/keep.txt"], false);
        assert!(set.decide("logs/keep.txt", false).is_excluded());
        assert_eq!(set.validate().len(), 1, "{:?}", set.validate());
        // braces
        assert!(
            excluded(&["*.{jpg,png}"], "a.png", false)
                && !excluded(&["*.{jpg,png}"], "a.gif", false)
        );
        assert!(excluded(&["img_{a\\,b}.png"], "img_a,b.png", false));
        assert!(
            excluded(&["{a}.txt"], "{a}.txt", false) && !excluded(&["{a}.txt"], "a.txt", false)
        );
        // classes
        assert!(
            excluded(&["file[0-9].txt"], "file7.txt", false)
                && !excluded(&["file[0-9].txt"], "fileA.txt", false)
        );
        assert!(!excluded(&["[!a]*"], "abc", false) && excluded(&["[!a]*"], "xbc", false));
        assert!(excluded(&["[]]x"], "]x", false));
        assert!(excluded(&["a[b"], "a[b", false));
        // case sensitivity
        assert!(!rs(&["README"], true).decide("readme", false).is_excluded());
        assert!(rs(&["README"], false).decide("readme", false).is_excluded());
    }

    /// 13 brace pairs: 8192 > 4096 expansions is a pattern error.
    #[test]
    fn too_many_expansions_is_an_error() {
        let o = MatchOptions::default();
        assert!(RuleSet::parse(&["{a,b}".repeat(13)], &o).is_err());
        assert!(RuleSet::parse(&["{a,b}".repeat(12)], &o).is_ok());
    }

    /// Backlog E6-S3: `**/a/**/b/**/c` on a 200-level path answers in under a millisecond-ish
    /// (memoised; generous bound for debug builds).
    #[test]
    fn globstar_matching_is_memoised() {
        let set = rs(&["**/a/**/b/**/c"], true);
        let deep: String = (0..200)
            .map(|i| if i % 2 == 0 { "a/" } else { "b/" })
            .collect::<String>()
            + "x/c";
        let t = std::time::Instant::now();
        let r = set.decide(&deep, false);
        assert!(r.is_excluded());
        let no: String = (0..200).map(|_| "a/").collect::<String>() + "z";
        assert!(!set.decide(&no, false).is_excluded());
        assert!(
            t.elapsed() < std::time::Duration::from_millis(100),
            "{:?}",
            t.elapsed()
        );
    }

    #[test]
    fn negation_only_when_allowed_and_blank_lines_are_skipped() {
        let o = MatchOptions {
            allow_negation: false,
            windows_paths: true,
            case_sensitive: true,
        };
        let set = RuleSet::parse(&["!x".to_string(), "  ".to_string()], &o).unwrap();
        assert_eq!(set.rules().len(), 1);
        assert!(
            !set.rules()[0].negated,
            "`!x` is a literal pattern in include lists"
        );
    }

    #[test]
    fn an_excluded_folder_excludes_everything_inside_whatever_follows() {
        let set = rs(&["build/", "!build/keep/**"], true);
        assert!(set.decide("build/keep/a.txt", false).is_excluded());
        assert!(set.decide("build", true).is_excluded());
    }

    #[test]
    fn backslashes_are_separators_on_windows_and_escapes_elsewhere() {
        let win = MatchOptions {
            windows_paths: true,
            case_sensitive: true,
            allow_negation: true,
        };
        let unix = MatchOptions {
            windows_paths: false,
            ..win
        };
        let lines = ["a\\b".to_string()];
        assert!(
            RuleSet::parse(&lines, &win)
                .unwrap()
                .decide("a/b", false)
                .is_excluded()
        );
        let u = RuleSet::parse(&lines, &unix).unwrap();
        assert!(
            u.decide("ab", false).is_excluded(),
            "`\\b` escapes b on Unix sources"
        );
        assert!(!u.decide("a/b", false).is_excluded());
    }

    #[test]
    fn dir_only_pattern_with_inner_slash_is_anchored() {
        assert!(excluded(&["src/gen/"], "src/gen", true));
        assert!(!excluded(&["src/gen/"], "x/src/gen", true));
        assert!(!excluded(&["src/gen/"], "src/gen", false));
    }

    #[test]
    fn decide_leaf_ignores_ancestors() {
        let u = |s: &str| -> Vec<u16> { s.encode_utf16().collect() };
        let set = rs(&["*.obj"], true);
        let (a, f) = (u("x.obj"), u("inner.txt"));
        // the full decision excludes anything inside a folder that matches
        assert!(set.decide_names(&[&a, &f], false).is_excluded());
        // the leaf decision looks at the last name only
        assert!(!set.decide_leaf(&[&a, &f], false).is_excluded());
        assert!(set.decide_leaf(&[&f, &a], false).is_excluded());
        assert_eq!(set.decide_leaf(&[], false), Decision::NoMatch);
    }

    #[test]
    fn no_rules_means_no_match() {
        assert_eq!(rs(&[], true).decide("a/b", false), Decision::NoMatch);
        assert_eq!(rs(&["*.x"], true).decide("a/b", false), Decision::NoMatch);
        assert_eq!(rs(&["*.x"], true).decide("", true), Decision::NoMatch);
    }

    #[test]
    fn static_prefix_and_pattern_matching() {
        let o = MatchOptions {
            windows_paths: true,
            case_sensitive: true,
            allow_negation: false,
        };
        let p = Pattern::compile("src/foo/*.cs", &o).unwrap();
        let prefix: Vec<String> = p
            .static_prefix()
            .iter()
            .map(|u| String::from_utf16_lossy(u))
            .collect();
        assert_eq!(prefix, ["src", "foo"]);
        let names: Vec<Vec<u16>> = ["src", "foo", "a.cs"]
            .iter()
            .map(|s| s.encode_utf16().collect())
            .collect();
        let refs: Vec<&[u16]> = names.iter().map(Vec::as_slice).collect();
        assert!(p.matches(&refs, false));
        assert!(
            Pattern::compile("*.cs", &o)
                .unwrap()
                .static_prefix()
                .is_empty()
        );
    }
}
