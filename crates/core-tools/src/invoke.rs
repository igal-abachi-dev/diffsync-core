//! How other programs start a compare tool (backlog E11.5-S4.1). One parser for the UI, a CLI and
//! the shell extension, so SourceTree, `git difftool`, `git mergetool`, `GIT_EXTERNAL_DIFF` and
//! Explorer all reach the same code.
//!
//! * SourceTree and `git difftool`: `tool "$LOCAL" "$REMOTE"` is a compare.
//! * Merge tool: `tool "$LOCAL" "$REMOTE" "$BASE" -o "$MERGED"` is a three-way merge, with the base
//!   in the middle (Beyond Compare's `left right center output` with the output as the fourth path
//!   works too).
//! * `GIT_EXTERNAL_DIFF` (set by `diff.external`): git calls `tool path old-file old-hex old-mode
//!   new-file new-hex new-mode` (7 arguments; 9 for a rename, with `new-path rename-metadata` added).
//! * Explorer: `--select-left PATH` remembers the left side, `--compare-selected PATH` compares it
//!   with PATH.
//!
//! Switches (a leading `/` is accepted for the Windows habit of `/title1=x`): `-o FILE`,
//! `--output=FILE`, `--title1=T`, `--title2=T`, `--title3=T` (base), `--ignore=LIST`, `--readonly`.
//! Unknown switches are collected in `unknown` (a tool must not fail because a client passes a
//! switch it does not know); the program decides whether to warn.

use serde::Serialize;

use crate::diffargs::ArgError;

/// A compare of two paths: files or folders.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompareInvocation {
    pub left: String,
    pub right: String,
    pub title_left: Option<String>,
    pub title_right: Option<String>,
    /// Pattern list of files to skip (`*package-lock.json,...`).
    pub ignore: Option<String>,
    pub read_only: bool,
}

/// A three-way (or two-way, without a base) merge whose result goes to `output`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MergeInvocation {
    /// "Mine" ($LOCAL).
    pub local: String,
    /// "Theirs" ($REMOTE).
    pub remote: String,
    /// The common ancestor ($BASE).
    pub base: Option<String>,
    /// Where the merged result is written ($MERGED).
    pub output: Option<String>,
    pub title_local: Option<String>,
    pub title_remote: Option<String>,
    pub title_base: Option<String>,
    pub ignore: Option<String>,
}

/// The seven (or nine) arguments of `GIT_EXTERNAL_DIFF`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitExternalDiff {
    pub path: String,
    pub old_file: String,
    pub old_hex: String,
    pub old_mode: String,
    pub new_file: String,
    pub new_hex: String,
    pub new_mode: String,
    /// Only for a rename or copy.
    pub new_path: Option<String>,
    pub rename_metadata: Option<String>,
}

impl GitExternalDiff {
    /// A file that did not exist before (git passes `/dev/null` as the old file).
    pub fn is_added(&self) -> bool {
        self.old_file == "/dev/null" || self.old_file == "nul"
    }

    pub fn is_deleted(&self) -> bool {
        self.new_file == "/dev/null" || self.new_file == "nul"
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "camelCase")]
pub enum Invocation {
    Compare(CompareInvocation),
    Merge(MergeInvocation),
    GitExternalDiff(GitExternalDiff),
    /// Explorer: remember this path as the left side.
    SelectLeft(String),
    /// Explorer: compare the remembered left side with this path.
    CompareSelected(String),
}

/// A parsed command line and the switches nobody asked for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Parsed {
    pub invocation: Invocation,
    pub unknown: Vec<String>,
}

fn looks_like_hex(s: &str) -> bool {
    (7..=64).contains(&s.len()) && s.chars().all(|c| c.is_ascii_hexdigit())
}

fn looks_like_mode(s: &str) -> bool {
    s == "." || (s.len() == 6 && s.chars().all(|c| ('0'..='7').contains(&c)))
}

/// `GIT_EXTERNAL_DIFF` calls have a fixed shape that cannot be confused with two or three paths:
/// the hashes and the modes are recognisable.
fn git_external(args: &[String]) -> Option<GitExternalDiff> {
    if args.len() != 7 && args.len() != 9 {
        return None;
    }
    let ok = (looks_like_hex(&args[2]) || args[2].chars().all(|c| c == '0' || c == '.'))
        && (looks_like_hex(&args[5]) || args[5].chars().all(|c| c == '0' || c == '.'))
        && looks_like_mode(&args[3])
        && looks_like_mode(&args[6]);
    ok.then(|| GitExternalDiff {
        path: args[0].clone(),
        old_file: args[1].clone(),
        old_hex: args[2].clone(),
        old_mode: args[3].clone(),
        new_file: args[4].clone(),
        new_hex: args[5].clone(),
        new_mode: args[6].clone(),
        new_path: args.get(7).cloned(),
        rename_metadata: args.get(8).cloned(),
    })
}

/// Parses the arguments after the program name.
pub fn parse_invocation(args: &[String]) -> Result<Parsed, ArgError> {
    if let Some(g) = git_external(args) {
        return Ok(Parsed {
            invocation: Invocation::GitExternalDiff(g),
            unknown: Vec::new(),
        });
    }
    let (mut positional, mut unknown) = (Vec::<String>::new(), Vec::<String>::new());
    let (mut output, mut ignore, mut read_only) = (None::<String>, None::<String>, false);
    let (mut t1, mut t2, mut t3) = (None::<String>, None::<String>, None::<String>);
    let (mut select, mut selected) = (None::<String>, None::<String>);
    let mut it = args.iter();
    let mut only_paths = false;
    while let Some(a) = it.next() {
        if only_paths
            || !(a.starts_with("--") || (a.starts_with('-') && a.len() > 1) || is_slash_switch(a))
        {
            positional.push(a.clone());
            continue;
        }
        if a == "--" {
            only_paths = true;
            continue;
        }
        let body = a.trim_start_matches(['-', '/']);
        let (name, value) = match body.split_once(['=', ':']) {
            Some((n, v)) => (n.to_ascii_lowercase(), Some(v.to_string())),
            None => (body.to_ascii_lowercase(), None),
        };
        let mut val = |what: &str| -> Result<String, ArgError> {
            match value.clone() {
                Some(v) => Ok(v),
                None => it
                    .next()
                    .cloned()
                    .ok_or_else(|| ArgError(format!("{what} needs a value"))),
            }
        };
        match name.as_str() {
            "o" | "output" | "savetarget" => output = Some(val("-o")?),
            "title1" => t1 = Some(val("--title1")?),
            "title2" => t2 = Some(val("--title2")?),
            "title3" => t3 = Some(val("--title3")?),
            "ignore" => ignore = Some(val("--ignore")?),
            "readonly" | "ro" => read_only = true,
            "select-left" => select = Some(val("--select-left")?),
            "compare-selected" => selected = Some(val("--compare-selected")?),
            _ => unknown.push(a.clone()),
        }
    }
    if let Some(p) = select {
        return Ok(Parsed {
            invocation: Invocation::SelectLeft(p),
            unknown,
        });
    }
    if let Some(p) = selected {
        return Ok(Parsed {
            invocation: Invocation::CompareSelected(p),
            unknown,
        });
    }
    let invocation = match (positional.len(), output) {
        (2, None) => Invocation::Compare(CompareInvocation {
            left: positional[0].clone(),
            right: positional[1].clone(),
            title_left: t1,
            title_right: t2,
            ignore,
            read_only,
        }),
        // two paths and an output: a merge without a base
        (2, Some(o)) => Invocation::Merge(MergeInvocation {
            local: positional[0].clone(),
            remote: positional[1].clone(),
            base: None,
            output: Some(o),
            title_local: t1,
            title_remote: t2,
            title_base: t3,
            ignore,
        }),
        (3, o) => Invocation::Merge(MergeInvocation {
            local: positional[0].clone(),
            remote: positional[1].clone(),
            base: Some(positional[2].clone()),
            output: o,
            title_local: t1,
            title_remote: t2,
            title_base: t3,
            ignore,
        }),
        // Beyond Compare's `left right center output`
        (4, None) => Invocation::Merge(MergeInvocation {
            local: positional[0].clone(),
            remote: positional[1].clone(),
            base: Some(positional[2].clone()),
            output: Some(positional[3].clone()),
            title_local: t1,
            title_remote: t2,
            title_base: t3,
            ignore,
        }),
        (n, _) => {
            return Err(ArgError(format!(
                "expected two paths (compare), three paths and -o (merge) or the seven arguments of GIT_EXTERNAL_DIFF; got {n} path(s)"
            )));
        }
    };
    Ok(Parsed {
        invocation,
        unknown,
    })
}

/// `/title1=x` on Windows; a plain `/home/user/file` is a path, so a slash switch needs a letter
/// followed by `=` or `:` or being one of the known names.
fn is_slash_switch(a: &str) -> bool {
    let Some(rest) = a.strip_prefix('/') else {
        return false;
    };
    let name = rest
        .split(['=', ':'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    rest.contains(['=', ':'])
        && matches!(
            name.as_str(),
            "o" | "title1" | "title2" | "title3" | "ignore" | "savetarget"
        )
        || matches!(name.as_str(), "readonly" | "ro") && !rest.contains('/')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(a: &[&str]) -> Result<Parsed, ArgError> {
        parse_invocation(&a.iter().map(|s| (*s).to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn sourcetree_and_git_difftool_compare() {
        // `tool "$LOCAL" "$REMOTE"`
        let r = p(&[
            "C:\\Users\\me\\AppData\\Local\\Temp\\a_LOCAL_1.txt",
            "D:\\repo\\a.txt",
        ])
        .unwrap();
        let Invocation::Compare(c) = r.invocation else {
            panic!("{r:?}")
        };
        assert_eq!(c.left, "C:\\Users\\me\\AppData\\Local\\Temp\\a_LOCAL_1.txt");
        assert_eq!(c.right, "D:\\repo\\a.txt");
        assert!(r.unknown.is_empty());
    }

    #[test]
    fn sourcetree_merge_tool_with_base_and_output() {
        // `tool "$LOCAL" "$REMOTE" "$BASE" -o "$MERGED"`
        let Parsed {
            invocation: Invocation::Merge(m),
            ..
        } = p(&["L.txt", "R.txt", "B.txt", "-o", "M.txt"]).unwrap()
        else {
            panic!()
        };
        assert_eq!(
            (
                m.local.as_str(),
                m.remote.as_str(),
                m.base.as_deref(),
                m.output.as_deref()
            ),
            ("L.txt", "R.txt", Some("B.txt"), Some("M.txt"))
        );
        // the output may come first or in the `--output=` form
        let Parsed {
            invocation: Invocation::Merge(m),
            ..
        } = p(&["--output=M.txt", "L.txt", "R.txt", "B.txt"]).unwrap()
        else {
            panic!()
        };
        assert_eq!(m.output.as_deref(), Some("M.txt"));
        // Beyond Compare's four-path form
        let Parsed {
            invocation: Invocation::Merge(m),
            ..
        } = p(&["L", "R", "C", "OUT"]).unwrap()
        else {
            panic!()
        };
        assert_eq!(
            (m.base.as_deref(), m.output.as_deref()),
            (Some("C"), Some("OUT"))
        );
        // two paths and an output: no base
        let Parsed {
            invocation: Invocation::Merge(m),
            ..
        } = p(&["L", "R", "-o", "OUT"]).unwrap()
        else {
            panic!()
        };
        assert_eq!(m.base, None);
    }

    #[test]
    fn titles_ignore_list_and_unknown_switches() {
        let r = p(&[
            "--title1=Mine",
            "--title2",
            "Theirs",
            "--ignore=*package-lock.json,*gemfile.lock",
            "--weird",
            "a",
            "b",
        ])
        .unwrap();
        let Invocation::Compare(c) = r.invocation else {
            panic!()
        };
        assert_eq!(
            (c.title_left.as_deref(), c.title_right.as_deref()),
            (Some("Mine"), Some("Theirs"))
        );
        assert_eq!(
            c.ignore.as_deref(),
            Some("*package-lock.json,*gemfile.lock")
        );
        assert_eq!(r.unknown, ["--weird"]);
        // the Windows spelling
        let Invocation::Compare(c) = p(&["/title1=X", "a", "b"]).unwrap().invocation else {
            panic!()
        };
        assert_eq!(c.title_left.as_deref(), Some("X"));
        // a Unix path is a path, not a switch
        let Invocation::Compare(c) = p(&["/home/me/a.txt", "/home/me/b.txt"]).unwrap().invocation
        else {
            panic!()
        };
        assert_eq!(c.left, "/home/me/a.txt");
    }

    #[test]
    fn git_external_diff_has_seven_or_nine_arguments() {
        let r = p(&[
            "src/a.c",
            "/tmp/git-blob-aaa/a.c",
            "1111111111111111111111111111111111111111",
            "100644",
            "src/a.c",
            "2222222222222222222222222222222222222222",
            "100644",
        ])
        .unwrap();
        let Invocation::GitExternalDiff(g) = r.invocation else {
            panic!("{r:?}")
        };
        assert_eq!(
            (g.path.as_str(), g.new_file.as_str(), g.old_mode.as_str()),
            ("src/a.c", "src/a.c", "100644")
        );
        assert!(!g.is_added() && g.new_path.is_none());
        let r = p(&[
            "new.txt",
            "/dev/null",
            "0000000000000000000000000000000000000000",
            ".",
            "new.txt",
            "3333333333333333333333333333333333333333",
            "100644",
        ])
        .unwrap();
        let Invocation::GitExternalDiff(g) = r.invocation else {
            panic!()
        };
        assert!(g.is_added());
        let nine = p(&[
            "old.c",
            "/tmp/o",
            "1111111",
            "100644",
            "/tmp/n",
            "2222222",
            "100644",
            "new.c",
            "rename from old.c",
        ])
        .unwrap();
        let Invocation::GitExternalDiff(g) = nine.invocation else {
            panic!()
        };
        assert_eq!(g.new_path.as_deref(), Some("new.c"));
        // seven plain-looking arguments are not git's call
        assert!(p(&["a", "b", "c", "d", "e", "f", "g"]).is_err());
    }

    #[test]
    fn explorer_commands_and_bad_counts() {
        assert_eq!(
            p(&["--select-left", "C:\\a"]).unwrap().invocation,
            Invocation::SelectLeft("C:\\a".into())
        );
        assert_eq!(
            p(&["--compare-selected=C:\\b"]).unwrap().invocation,
            Invocation::CompareSelected("C:\\b".into())
        );
        assert!(p(&["only-one"]).is_err());
        assert!(p(&[]).is_err());
    }
}
