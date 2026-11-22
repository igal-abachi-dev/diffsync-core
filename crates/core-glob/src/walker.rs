//! The pattern walker (port of `GlobWalker`, plan.md §7.5, backlog E6-S4): finds the entries of a
//! `Vfs` that match a glob pattern.
//!
//! It starts at the pattern's static prefix (`src/foo/*.cs` starts in `src/foo`), supports
//! match-base patterns, directory results and folder-only (`name/`) patterns, de-duplicates
//! results over brace expansions, and returns paths relative to the **base**, even though the walk
//! starts deeper. Links are never traversed (the `Vfs` trait reports them as links and does not
//! follow them), so link cycles cannot occur. An unreadable sub-folder is recorded and the walk
//! goes on; only an unreadable start folder with no result is reported as an error entry too.

use std::collections::HashSet;

use core_base::{CancelToken, Error, Result};
use core_vfs::{Entry, Kind, Name, VPath, Vfs};

use crate::braces::{MAX_EXPANSIONS, expand_braces};
use crate::rules::{MatchOptions, Pattern, RuleSet};

/// Options of a walk.
#[derive(Debug, Clone, Default)]
pub struct WalkOptions {
    pub match_options: MatchOptions,
    /// Also return folders that match.
    pub include_directories: bool,
    /// Folders matching these rules are not entered and not returned.
    pub ignore: Option<RuleSet>,
}

/// One match, relative to the base folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalkHit {
    pub path: VPath,
    pub kind: Kind,
}

/// Results plus the folders that could not be read.
#[derive(Debug, Default)]
pub struct WalkReport {
    pub hits: Vec<WalkHit>,
    pub errors: Vec<(VPath, Error)>,
}

/// Walks `base` for entries matching `pattern`.
pub fn walk(
    vfs: &dyn Vfs,
    base: &VPath,
    pattern: &str,
    o: &WalkOptions,
    cancel: &CancelToken,
) -> Result<WalkReport> {
    let mut report = WalkReport::default();
    let mut seen: HashSet<VPath> = HashSet::new();
    for pat in expand_braces(pattern, MAX_EXPANSIONS)? {
        let Some(p) = Pattern::compile(&pat, &o.match_options) else {
            continue;
        };
        // start at the static prefix
        let prefix = p.static_prefix();
        let mut start_rel = VPath::root();
        let mut start_abs = base.clone();
        for u in &prefix {
            start_rel = start_rel.join(Name::from_units(u));
            start_abs = start_abs.join(Name::from_units(u));
        }
        match vfs.stat(&start_abs)? {
            Some(e) if e.kind == Kind::Dir => {}
            _ => continue,
        }
        let mut stack = vec![(start_abs, start_rel)];
        while let Some((abs, rel)) = stack.pop() {
            cancel.check()?;
            let mut entries: Vec<Entry> = Vec::new();
            if let Err(e) = vfs.list(&abs, &mut entries) {
                report.errors.push((rel.clone(), e));
                continue;
            }
            entries.sort_by(|a, b| a.name.cmp(&b.name));
            let mut subdirs = Vec::new();
            for e in entries {
                let child_rel = rel.join(e.name.clone());
                let is_dir = e.kind == Kind::Dir;
                let names: Vec<&[u16]> = child_rel.components().iter().map(Name::units).collect();
                if let Some(ig) = &o.ignore {
                    if ig.decide_names(&names, is_dir).is_excluded() {
                        continue;
                    }
                }
                let wants = if is_dir {
                    o.include_directories || p.dir_only
                } else {
                    !p.dir_only
                };
                if wants && p.matches(&names, is_dir) && seen.insert(child_rel.clone()) {
                    report.hits.push(WalkHit {
                        path: child_rel.clone(),
                        kind: e.kind,
                    });
                }
                if is_dir {
                    subdirs.push((abs.join(e.name.clone()), child_rel));
                }
            }
            // depth-first in name order
            stack.extend(subdirs.into_iter().rev());
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_vfs::MemVfs;

    fn tree() -> MemVfs {
        let m = MemVfs::new();
        for f in [
            "src/a.cs",
            "src/foo/b.cs",
            "src/foo/c.txt",
            "src/foo/deep/d.cs",
            "docs/x.md",
            "docs/sub/y.md",
            "node_modules/m/z.js",
            "src/bin/out.dll",
            "top.cs",
            ".hidden",
            "a b/c.cs",
        ] {
            m.add_file(f, b"x", 1);
        }
        m
    }

    fn opts() -> WalkOptions {
        WalkOptions {
            match_options: MatchOptions {
                case_sensitive: true,
                windows_paths: true,
                allow_negation: true,
            },
            ..WalkOptions::default()
        }
    }

    fn run(m: &MemVfs, base: &str, pat: &str, o: &WalkOptions) -> Vec<String> {
        let base = VPath::from_parts(base.split('/').filter(|s| !s.is_empty()));
        let mut v: Vec<String> = walk(m, &base, pat, o, &CancelToken::new())
            .unwrap()
            .hits
            .iter()
            .map(|h| h.path.display())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn match_base_finds_names_at_any_depth() {
        assert_eq!(
            run(&tree(), "", "*.cs", &opts()),
            [
                "a b/c.cs",
                "src/a.cs",
                "src/foo/b.cs",
                "src/foo/deep/d.cs",
                "top.cs"
            ]
        );
        assert_eq!(
            run(&tree(), "", "*", &opts()).len(),
            11,
            "`*` matches every file including dot files (D15)"
        );
    }

    #[test]
    fn anchored_patterns_start_at_the_static_prefix() {
        assert_eq!(run(&tree(), "", "src/foo/*.cs", &opts()), ["src/foo/b.cs"]);
        assert_eq!(
            run(&tree(), "", "src/**/*.cs", &opts()),
            ["src/a.cs", "src/foo/b.cs", "src/foo/deep/d.cs"]
        );
        assert_eq!(
            run(&tree(), "", "docs/**/*.md", &opts()),
            ["docs/sub/y.md", "docs/x.md"]
        );
        assert!(run(&tree(), "", "nothere/*.cs", &opts()).is_empty());
    }

    #[test]
    fn results_are_relative_to_the_base_not_the_start_folder() {
        assert_eq!(run(&tree(), "src", "foo/*.cs", &opts()), ["foo/b.cs"]);
        assert_eq!(run(&tree(), "src/foo", "*.txt", &opts()), ["c.txt"]);
    }

    #[test]
    fn braces_expand_and_results_are_not_duplicated() {
        assert_eq!(
            run(&tree(), "", "docs/{x,sub/y}.md", &opts()),
            ["docs/sub/y.md", "docs/x.md"]
        );
        // two expansions that match the same file give it once
        assert_eq!(
            run(&tree(), "", "{*.cs,top.*}", &opts())
                .iter()
                .filter(|p| *p == "top.cs")
                .count(),
            1
        );
    }

    #[test]
    fn directories_only_when_asked_and_folder_only_patterns() {
        let mut o = opts();
        assert!(
            !run(&tree(), "", "foo", &o)
                .iter()
                .any(|p| p.ends_with("foo")),
            "directories are not returned by default"
        );
        o.include_directories = true;
        assert_eq!(run(&tree(), "", "foo", &o), ["src/foo"]);
        let plain = opts();
        assert_eq!(
            run(&tree(), "", "bin/", &plain),
            ["src/bin"],
            "name/ matches folders only, even without include_directories"
        );
        assert!(run(&tree(), "", "top.cs/", &plain).is_empty());
    }

    #[test]
    fn ignore_rules_prune_folders() {
        let mut o = opts();
        o.ignore = Some(
            RuleSet::parse(&["node_modules/".into(), "bin/".into()], &o.match_options).unwrap(),
        );
        let all = run(&tree(), "", "*", &o);
        assert!(
            !all.iter()
                .any(|p| p.contains("node_modules") || p.contains("bin")),
            "{all:?}"
        );
        assert!(all.contains(&"src/a.cs".to_string()));
    }

    #[test]
    fn case_insensitive_walks() {
        let mut o = opts();
        o.match_options.case_sensitive = false;
        assert_eq!(run(&tree(), "", "TOP.CS", &o), ["top.cs"]);
        assert!(run(&tree(), "", "TOP.CS", &opts()).is_empty());
    }

    #[test]
    fn links_are_listed_but_never_entered() {
        let m = tree();
        m.add_link("src/lnk", Kind::Dir);
        assert!(
            run(&m, "", "lnk", &opts()).contains(&"src/lnk".to_string()),
            "a link is returned as an entry"
        );
        assert!(
            !run(&m, "", "*", &opts())
                .iter()
                .any(|p| p.starts_with("src/lnk/"))
        );
    }

    #[test]
    fn an_unreadable_folder_is_recorded_and_the_walk_continues() {
        use core_vfs::{FaultVfs, Op};
        let v = FaultVfs::new(tree());
        v.fail_nth(Op::List, 2, core_base::ErrorKind::AccessDenied);
        let r = walk(&v, &VPath::root(), "*.cs", &opts(), &CancelToken::new()).unwrap();
        assert_eq!(r.errors.len(), 1);
        assert!(!r.hits.is_empty(), "other folders were still searched");
    }

    #[test]
    fn cancellation_and_bad_patterns() {
        let c = CancelToken::new();
        c.cancel();
        assert!(
            walk(&tree(), &VPath::root(), "*", &opts(), &c)
                .unwrap_err()
                .is_cancelled()
        );
        assert!(
            walk(
                &tree(),
                &VPath::root(),
                &"{a,b}".repeat(13),
                &opts(),
                &CancelToken::new()
            )
            .is_err()
        );
    }
}
