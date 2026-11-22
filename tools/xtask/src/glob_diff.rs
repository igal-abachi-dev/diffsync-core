//! `cargo xtask glob-diff` (backlog E6-S5, plan.md §12.1): runs a battery of patterns through the
//! C# Glob library (`tools/glob-diff-runner`, which references `reference_source/Glob`
//! unmodified) and through `core-glob::walk`, over the same generated tree, and compares.
//!
//! Every difference must match one of the documented corrections of plan.md §7.5 (the
//! `EXPECTED` table); anything else fails.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use core_base::CancelToken;
use core_glob::{MatchOptions, WalkOptions, walk};
use core_vfs::{LocalStd, VPath};

/// Files created in the temporary tree (relative paths, `/`-separated).
const FILES: &[&str] = &[
    "a.obj",
    "a.obj.bak",
    "src/x/a.obj",
    "build/x.o",
    "src/build/x.o",
    "bin",
    "src/bin/out.dll",
    "docs/a.md",
    "docs/x/y/a.md",
    "other/a.md",
    "node_modules/p.js",
    "a/b/node_modules/q.js",
    ".gitignore",
    "keep.log",
    "x.log",
    "logs/keep.txt",
    "a.png",
    "a.gif",
    "img_a,b.png",
    "file7.txt",
    "fileA.txt",
    "abc",
    "xbc",
    "]x",
    "a[b",
    "README",
    "readme",
    "{a}.txt",
    "a.txt",
    "src/foo/b.cs",
    "src/foo/c.txt",
    "src/foo/deep/d.cs",
    "top.cs",
];

/// Patterns that are compared.
const PATTERNS: &[&str] = &[
    "*.obj",
    "/build",
    "build/**",
    "bin/",
    "docs/**/*.md",
    "**/node_modules/**",
    "*",
    "*.{jpg,png}",
    "*.{png,gif}",
    "img_{a\\,b}.png",
    "{a}.txt",
    "file[0-9].txt",
    "[!a]*",
    "[]]x",
    "a[b",
    "README",
    "src/foo/*.cs",
    "src/**/*.cs",
    "**/*.cs",
    "*.log",
    "logs/*.txt",
    "a?c",
    "*.md",
    "docs/*",
    "src/*/*.cs",
    "x*",
    "{src,docs}/**/*.md",
];

/// Documented differences: pattern → why the Rust result differs (plan.md §7.5 table).
const EXPECTED: &[(&str, &str)] = &[
    (
        "bin/",
        "a trailing `/` matches folders only; the C# version also returned a file named `bin`",
    ),
    (
        "img_{a\\,b}.png",
        "golden row of plan.md 11.3: an escaped comma makes the block one option and the braces go away; C# kept literal braces",
    ),
    ("build/**", TRAILING_STAR_STAR),
    ("docs/**/*.md", STAR_STAR),
    ("**/node_modules/**", TRAILING_STAR_STAR),
    ("src/**/*.cs", STAR_STAR),
    ("**/*.cs", STAR_STAR),
    ("{src,docs}/**/*.md", STAR_STAR),
];

/// Cause found and verified (backlog E6-S5): `GlobWalker.Walk` records each directory in
/// `seenDirs`, and the `**` branch then calls `Walk` for the same directory with the next
/// segment, which finds it already seen and returns nothing. A patched copy whose loop check is
/// keyed by (directory, segment index) agrees with Rust on all of these patterns
/// (`GLOB_DIFF_RUNNER_DLL` runs the check; plan.md 7.5 lists the correction).
const STAR_STAR: &str = "C# loop detection (`seenDirs`) swallows every `**` followed by more segments; Rust follows the documented `**` = zero or more folders";

/// A trailing `**` also differs on its own: C# yields only the folder itself (when folders are
/// included), Rust matches everything below (gitignore semantics, plan.md 7.5).
const TRAILING_STAR_STAR: &str = "C# loop detection swallows `**`, and a trailing `**` yields folders only; Rust matches every entry below";

fn rust_results(root: &Path) -> BTreeMap<String, String> {
    let vfs = LocalStd::new(root);
    let o = WalkOptions {
        match_options: MatchOptions {
            case_sensitive: true,
            windows_paths: true,
            allow_negation: true,
        },
        include_directories: false,
        ignore: None,
    };
    PATTERNS
        .iter()
        .map(|p| {
            let mut hits: Vec<String> = match walk(&vfs, &VPath::root(), p, &o, &CancelToken::new())
            {
                Ok(r) => r.hits.iter().map(|h| h.path.display()).collect(),
                Err(e) => vec![format!("ERROR:{e}")],
            };
            hits.sort();
            ((*p).to_string(), hits.join("|"))
        })
        .collect()
}

fn create_tree(root: &Path) -> Result<(), String> {
    for f in FILES {
        let p = root.join(f);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        // `bin` is a file here but `src/bin` is a folder: both exist on purpose
        std::fs::write(&p, b"x").map_err(|e| format!("{}: {e}", p.display()))?;
    }
    Ok(())
}

fn csharp_results(root: &Path, repo: &Path) -> Result<BTreeMap<String, String>, String> {
    let patterns_file = root.join("..").join(format!(
        "{}-patterns.txt",
        root.file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
    ));
    std::fs::write(&patterns_file, PATTERNS.join("\n")).map_err(|e| e.to_string())?;
    let project = repo.join("tools").join("glob-diff-runner");
    // GLOB_DIFF_RUNNER_DLL points at a prebuilt runner (used to check a patched copy of the
    // library without touching reference_source/)
    let dll = if let Ok(custom) = std::env::var("GLOB_DIFF_RUNNER_DLL") {
        PathBuf::from(custom)
    } else {
        let build = Command::new("dotnet")
            .args(["build", "-c", "Release", "-nologo", "-v", "q"])
            .arg(&project)
            .output()
            .map_err(|e| format!("cannot run dotnet: {e}"))?;
        if !build.status.success() {
            return Err(format!(
                "dotnet build failed:
{}",
                String::from_utf8_lossy(&build.stdout)
            ));
        }
        project
            .join("bin")
            .join("Release")
            .join("net10.0")
            .join("GlobDiffRunner.dll")
    };
    let out = Command::new("dotnet")
        .arg(&dll)
        .arg(root)
        .arg(&patterns_file)
        .output()
        .map_err(|e| format!("cannot run dotnet: {e}"))?;
    let _ = std::fs::remove_file(&patterns_file);
    if !out.status.success() {
        return Err(format!(
            "dotnet failed:\n{}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split_once('\t'))
        .map(|(p, r)| (p.to_string(), r.to_string()))
        .collect())
}

pub fn run() -> Result<(), String> {
    let repo: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let root = std::env::temp_dir().join(format!("glob-diff-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;
    let result = (|| {
        create_tree(&root)?;
        let cs = csharp_results(&root, &repo)?;
        let rs = rust_results(&root);
        let (mut same, mut expected, mut unexpected) = (0, 0, Vec::new());
        for p in PATTERNS {
            let (c, r) = (
                cs.get(*p).cloned().unwrap_or_default(),
                rs.get(*p).cloned().unwrap_or_default(),
            );
            if c == r {
                same += 1;
            } else if let Some((_, why)) = EXPECTED.iter().find(|(e, _)| e == p) {
                expected += 1;
                println!("expected difference `{p}`: {why}\n    C#:   {c}\n    Rust: {r}");
            } else {
                unexpected.push(format!("`{p}`\n    C#:   {c}\n    Rust: {r}"));
            }
        }
        println!(
            "glob-diff: {same} identical, {expected} documented differences, {} unexpected",
            unexpected.len()
        );
        if unexpected.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "unexpected differences:\n{}",
                unexpected.join("\n")
            ))
        }
    })();
    let _ = std::fs::remove_dir_all(&root);
    result
}
