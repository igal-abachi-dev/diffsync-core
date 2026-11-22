//! Runs a [`DiffRequest`] (backlog E11.5-S6.2): two files, two folders, or two texts, and returns
//! what `diff`, `diff -r` or `git diff --no-index` would print, the GNU exit status, and a JSON
//! form for programs that read the result instead of showing it.
//!
//! A wrapper program is `parse_diff_args` + `run` + print `stdout`, `stderr`, exit with `exit_code`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use core_base::CancelToken;
use core_diff::udiff::{self, HunkOptions};
use core_folder::udiff::{
    DiffStyle, FolderDiff, FolderDiffOptions, diff_file_pair, diff_folders, render,
};
use core_glob::IgnoreList;
use core_vfs::{VPath, Vfs};
use serde_json::json;

use crate::diffargs::{Dialect, DiffRequest};

/// What the wrapper may adjust besides the flags.
#[derive(Debug, Clone)]
pub struct RunOptions {
    /// Files to skip, e.g. [`core_glob::IgnoreList::lock_files`]. A file pair that matches is not
    /// compared (exit status 0, nothing printed); in a folder diff the pattern excludes files.
    pub ignore: Option<IgnoreList>,
    /// Files above this size are reported as too large instead of being read.
    pub max_file_bytes: u64,
    pub cancel: CancelToken,
}

impl Default for RunOptions {
    fn default() -> Self {
        RunOptions {
            ignore: None,
            max_file_bytes: 512 << 20,
            cancel: CancelToken::new(),
        }
    }
}

/// The result of a run.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DiffOutput {
    /// What goes to standard output.
    pub stdout: String,
    /// Messages for standard error (`diff: x: No such file or directory`).
    pub stderr: String,
    /// GNU: 0 same, 1 different, 2 trouble. git: 0 unless `--exit-code`/`--quiet`, 128-style
    /// errors are 2 here.
    pub exit_code: i32,
    /// The comparison as JSON: `{exitCode, different, summary, entries}`.
    pub json: String,
    /// The file pair was skipped because of the ignore list.
    pub ignored: bool,
}

fn fail(msg: &str) -> DiffOutput {
    DiffOutput {
        stderr: format!("{msg}\n"),
        exit_code: 2,
        json: json!({"exitCode": 2, "error": msg}).to_string(),
        ..DiffOutput::default()
    }
}

fn local(p: &Path) -> Result<Arc<dyn Vfs>, String> {
    #[cfg(windows)]
    {
        let s = p.to_str().ok_or("the path is not valid Unicode")?;
        let root = core_vfs::VfsRoot::parse_windows(s).map_err(|e| e.to_string())?;
        Ok(Arc::new(core_vfs::LocalWin32::new(root)))
    }
    #[cfg(not(windows))]
    {
        Ok(Arc::new(core_vfs::LocalStd::new(p)))
    }
}

fn ticks(t: std::time::SystemTime) -> i64 {
    core_base::ticks::ticks_from_system_time(t)
}

fn folder_options(req: &DiffRequest, ro: &RunOptions) -> FolderDiffOptions {
    let git = req.dialect == Dialect::Git;
    let mut text = udiff::diff_text_options(req.alignment, req.whitespace, req.ignore_case);
    text.eol = req.eol;
    let mut exclude = req.exclude.clone();
    if let Some(i) = &ro.ignore {
        exclude.extend(i.patterns());
    }
    FolderDiffOptions {
        text,
        hunk: HunkOptions {
            context: req.context,
            ignore_blank_lines: req.ignore_blank_lines,
            // git always shows the enclosing function in the hunk header; GNU only with -p
            func_header: git || req.func_header,
            // git prints the context lines of the new file, GNU diff those of the old one
            context_from_new: git,
        },
        style: if git { DiffStyle::Git } else { DiffStyle::Gnu },
        format: req.format,
        report: req.report,
        recursive: req.recursive,
        new_file: req.new_file || git,
        exclude_files: exclude,
        exclude_folders: Vec::new(),
        max_file_bytes: ro.max_file_bytes,
        cmdline_flags: req.cmdline_flags.clone(),
        left_name: req.left.clone(),
        right_name: req.right.clone(),
        src_prefix: req.src_prefix.clone(),
        dst_prefix: req.dst_prefix.clone(),
        header_line: true,
    }
}

fn finish(
    req: &DiffRequest,
    fd: &FolderDiff,
    opts: &FolderDiffOptions,
    ignored: bool,
) -> DiffOutput {
    let mut stdout = if req.quiet {
        String::new()
    } else {
        render(fd, opts)
    };
    let gnu_code = fd.exit_code();
    let exit_code = if gnu_code == 2 || req.dialect == Dialect::Gnu || req.exit_code_flag {
        gnu_code
    } else {
        0
    };
    if req.report_identical && gnu_code == 0 && req.dialect == Dialect::Gnu {
        stdout = format!("Files {} and {} are identical\n", req.left, req.right);
    }
    let (files, additions, deletions) = fd.files().fold((0u32, 0u32, 0u32), |(f, a, d), x| {
        (f + 1, a + x.additions, d + x.deletions)
    });
    let json = json!({
        "exitCode": exit_code,
        "different": gnu_code == 1,
        "ignored": ignored,
        "summary": {"filesChanged": files, "insertions": additions, "deletions": deletions, "sameFiles": fd.same_files},
        "entries": fd.entries,
    });
    DiffOutput {
        stdout,
        stderr: String::new(),
        exit_code,
        json: json.to_string(),
        ignored,
    }
}

fn read_file(p: &Path, max: u64) -> Result<(Vec<u8>, i64), String> {
    let md = std::fs::metadata(p).map_err(|e| format!("diff: {}: {e}", p.display()))?;
    if md.len() > max {
        return Err(format!(
            "diff: {}: file is larger than the {max}-byte limit",
            p.display()
        ));
    }
    let data = std::fs::read(p).map_err(|e| format!("diff: {}: {e}", p.display()))?;
    Ok((data, md.modified().map_or(0, ticks)))
}

/// Runs the request on the local file system.
pub fn run(req: &DiffRequest, ro: &RunOptions) -> DiffOutput {
    if req.left == "-" || req.right == "-" {
        return fail(
            "diff: standard input is not available to the core; read it and use run_texts",
        );
    }
    let (lp, rp) = (PathBuf::from(&req.left), PathBuf::from(&req.right));
    let (lm, rm) = (std::fs::metadata(&lp), std::fs::metadata(&rp));
    let opts = folder_options(req, ro);
    match (&lm, &rm) {
        (Ok(a), Ok(b)) if a.is_dir() && b.is_dir() => {
            let (Ok(la), Ok(ra)) = (std::path::absolute(&lp), std::path::absolute(&rp)) else {
                return fail("diff: cannot resolve the folder names");
            };
            let (lv, rv) = match (local(&la), local(&ra)) {
                (Ok(a), Ok(b)) => (a, b),
                (Err(e), _) | (_, Err(e)) => return fail(&format!("diff: {e}")),
            };
            match diff_folders(lv, &VPath::root(), rv, &VPath::root(), &opts, &ro.cancel) {
                Ok(fd) => finish(req, &fd, &opts, false),
                Err(e) => fail(&format!("diff: {e}")),
            }
        }
        _ => run_files(req, ro, opts, &lp, &rp, lm, rm),
    }
}

fn run_files(
    req: &DiffRequest,
    ro: &RunOptions,
    mut opts: FolderDiffOptions,
    lp: &Path,
    rp: &Path,
    lm: std::io::Result<std::fs::Metadata>,
    rm: std::io::Result<std::fs::Metadata>,
) -> DiffOutput {
    // `diff file dir` compares the file with the file of that name inside the folder
    let (mut lp, mut rp) = (lp.to_path_buf(), rp.to_path_buf());
    let (mut lm, mut rm) = (lm, rm);
    let name = |p: &Path| p.file_name().map(PathBuf::from);
    if lm.as_ref().is_ok_and(std::fs::Metadata::is_file)
        && rm.as_ref().is_ok_and(std::fs::Metadata::is_dir)
    {
        if let Some(n) = name(&lp) {
            rp = rp.join(n);
            rm = std::fs::metadata(&rp);
        }
    } else if rm.as_ref().is_ok_and(std::fs::Metadata::is_file)
        && lm.as_ref().is_ok_and(std::fs::Metadata::is_dir)
    {
        if let Some(n) = name(&rp) {
            lp = lp.join(n);
            lm = std::fs::metadata(&lp);
        }
    }
    if let Some(i) = &ro.ignore {
        if i.is_ignored(&req.left) || i.is_ignored(&req.right) {
            let fd = FolderDiff {
                entries: Vec::new(),
                same_files: 1,
            };
            return finish(req, &fd, &opts, true);
        }
    }
    let new_file = req.new_file || req.dialect == Dialect::Git;
    let load = |p: &Path,
                m: &std::io::Result<std::fs::Metadata>|
     -> Result<Option<(Vec<u8>, i64)>, String> {
        match m {
            Ok(md) if md.is_dir() => Err(format!("diff: {}: is a directory", p.display())),
            Ok(_) => read_file(p, ro.max_file_bytes).map(Some),
            Err(e)
                if e.kind() == std::io::ErrorKind::NotFound
                    && new_file
                    && req.dialect == Dialect::Gnu =>
            {
                Ok(None)
            }
            Err(e) => Err(format!("diff: {}: {e}", p.display())),
        }
    };
    let (old, new) = match (load(&lp, &lm), load(&rp, &rm)) {
        (Ok(a), Ok(b)) => (a, b),
        (Err(e), _) | (_, Err(e)) => return fail(&e),
    };
    if old.is_none() && new.is_none() {
        return fail(&format!("diff: {}: No such file or directory", req.left));
    }
    // -L replaces the names in the headers
    opts.left_name = req
        .labels
        .first()
        .cloned()
        .unwrap_or_else(|| req.left.clone());
    opts.right_name = req
        .labels
        .get(1)
        .cloned()
        .unwrap_or_else(|| req.right.clone());
    opts.header_line = false;
    match diff_file_pair(old, new, &opts) {
        Ok(fd) => finish(req, &fd, &opts, false),
        Err(e) => fail(&format!("diff: {e}")),
    }
}

/// Runs the request on two texts that are not files (an MCP server, a UI buffer, standard input).
/// `req.left` and `req.right` are only the names printed in the headers.
pub fn run_texts(req: &DiffRequest, old: &str, new: &str, ro: &RunOptions) -> DiffOutput {
    let mut opts = folder_options(req, ro);
    opts.header_line = false;
    opts.left_name = req
        .labels
        .first()
        .cloned()
        .unwrap_or_else(|| req.left.clone());
    opts.right_name = req
        .labels
        .get(1)
        .cloned()
        .unwrap_or_else(|| req.right.clone());
    let now = ticks(std::time::SystemTime::now());
    match diff_file_pair(
        Some((old.as_bytes().to_vec(), now)),
        Some((new.as_bytes().to_vec(), now)),
        &opts,
    ) {
        Ok(fd) => finish(req, &fd, &opts, false),
        Err(e) => fail(&format!("diff: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diffargs::parse_diff_args;

    fn args(a: &[&str], d: Dialect) -> DiffRequest {
        parse_diff_args(&a.iter().map(|s| (*s).to_string()).collect::<Vec<_>>(), d).unwrap()
    }

    #[test]
    fn two_texts_give_a_unified_diff_and_json() {
        let req = args(&["-u", "old.txt", "new.txt"], Dialect::Gnu);
        let o = run_texts(&req, "a\nb\nc\n", "a\nB\nc\n", &RunOptions::default());
        assert_eq!(o.exit_code, 1);
        assert!(
            o.stdout.starts_with("--- old.txt\t") && o.stdout.contains("\n+++ new.txt\t"),
            "{}",
            o.stdout
        );
        assert!(
            o.stdout.ends_with("@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n"),
            "{}",
            o.stdout
        );
        let j: serde_json::Value = serde_json::from_str(&o.json).unwrap();
        assert_eq!(j["different"], true);
        assert_eq!(j["summary"]["insertions"], 1);
        assert_eq!(j["entries"][0]["hunks"][0]["lines"][1]["op"], "-");
        let same = run_texts(&req, "x\n", "x\n", &RunOptions::default());
        assert_eq!((same.exit_code, same.stdout.as_str()), (0, ""));
    }

    #[test]
    fn git_style_for_two_texts_and_exit_codes() {
        let mut req = args(&["--no-index", "a.txt", "b.txt"], Dialect::Gnu);
        let o = run_texts(&req, "1\n", "2\n", &RunOptions::default());
        assert!(
            o.stdout.starts_with(
                "diff --git a/a.txt b/b.txt\n--- a/a.txt\n+++ b/b.txt\n@@ -1 +1 @@\n-1\n+2\n"
            ),
            "{}",
            o.stdout
        );
        assert_eq!(o.exit_code, 0, "git diff exits 0 without --exit-code");
        req.exit_code_flag = true;
        assert_eq!(
            run_texts(&req, "1\n", "2\n", &RunOptions::default()).exit_code,
            1
        );
        req.quiet = true;
        assert_eq!(
            run_texts(&req, "1\n", "2\n", &RunOptions::default()).stdout,
            ""
        );
    }

    #[test]
    fn files_folders_and_missing_operands() {
        let d = tempfile::tempdir().unwrap();
        let (a, b) = (d.path().join("a"), d.path().join("b"));
        std::fs::create_dir_all(a.join("s")).unwrap();
        std::fs::create_dir_all(b.join("s")).unwrap();
        std::fs::write(a.join("s/x.txt"), "1\n2\n").unwrap();
        std::fs::write(b.join("s/x.txt"), "1\nTWO\n").unwrap();
        std::fs::write(a.join("only.txt"), "o\n").unwrap();
        let (an, bn) = (a.to_str().unwrap(), b.to_str().unwrap());
        let o = run(
            &args(&["-ru", an, bn], Dialect::Gnu),
            &RunOptions::default(),
        );
        assert_eq!(o.exit_code, 1);
        assert!(
            o.stdout.contains("diff -ru ")
                && o.stdout.contains("+TWO\n")
                && o.stdout.contains("Only in "),
            "{}",
            o.stdout
        );
        let o = run(
            &args(&["--name-status", "--no-index", an, bn], Dialect::Gnu),
            &RunOptions {
                ..RunOptions::default()
            },
        );
        assert_eq!(o.stdout, "D\tonly.txt\nM\ts/x.txt\n");
        // a file and a folder: the file of the same name inside the folder
        let o = run(
            &args(
                &[
                    "-u",
                    a.join("s/x.txt").to_str().unwrap(),
                    b.join("s").to_str().unwrap(),
                ],
                Dialect::Gnu,
            ),
            &RunOptions::default(),
        );
        assert!(o.stdout.contains("+TWO"), "{}", o.stdout);
        // a missing file is trouble
        let o = run(
            &args(
                &["-u", an, d.path().join("nope").to_str().unwrap()],
                Dialect::Gnu,
            ),
            &RunOptions::default(),
        );
        assert_eq!(o.exit_code, 2);
        assert!(o.stderr.contains("diff:"), "{}", o.stderr);
    }

    #[test]
    fn the_ignore_list_skips_lock_files() {
        let d = tempfile::tempdir().unwrap();
        let (l, r) = (
            d.path().join("package-lock.json"),
            d.path().join("other-lock.json"),
        );
        std::fs::write(&l, "{}\n").unwrap();
        std::fs::write(&r, "{\"a\":1}\n").unwrap();
        let req = args(
            &["-u", l.to_str().unwrap(), r.to_str().unwrap()],
            Dialect::Gnu,
        );
        let ro = RunOptions {
            ignore: Some(IgnoreList::lock_files()),
            ..RunOptions::default()
        };
        let o = run(&req, &ro);
        assert!(o.ignored);
        assert_eq!((o.exit_code, o.stdout.as_str()), (0, ""));
        assert_eq!(run(&req, &RunOptions::default()).exit_code, 1);
    }
}
