//! Folder diff reports: `diff -r`, `diff -ru a b`, `git diff --no-index a b` and the `--stat`
//! family (backlog E11.5-S2). A folder compare decides which files differ (by content, never by
//! time stamp); each differing pair is then diffed with [`core_diff::udiff`].
//!
//! Two output styles:
//!
//! * **GNU** (`diff -ru dir1 dir2`): a `diff -ru dir1/x dir2/x` line, `--- dir1/x<TAB>time`,
//!   `Only in dir1: name`, `Binary files dir1/x and dir2/x differ`;
//! * **Git** (`git diff --no-index dir1 dir2`): `diff --git a/dir1/x b/dir2/x`,
//!   `new file mode 100644`, `--- /dev/null`, every file that exists on one side only is a new or a
//!   deleted file.
//!
//! The order of the report is the order GNU diff walks: names sorted, a folder in its place.

use std::cmp::Ordering;
use std::fmt::Write as _;
use std::io::Read;
use std::sync::Arc;

use core_base::{CancelToken, Error, ErrorKind, Result};
use core_diff::DocSpec;
use core_diff::options::{AlignmentName, TextOptions, WhitespaceName};
use core_diff::udiff::{self, Hunk, HunkOptions, Labels};
use core_vfs::{Kind, VPath, Vfs};
use serde::Serialize;

use crate::tree::{Tree, flag};
use crate::{ContentKind, FolderCompare, FolderRules, NodeId, Status};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum DiffStyle {
    Gnu,
    Git,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PatchFormat {
    Unified,
    Context,
    Normal,
}

/// What the report prints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Report {
    Patch,
    Stat,
    NumStat,
    ShortStat,
    NameOnly,
    NameStatus,
    /// `diff -q`: one line per differing file, no hunks.
    Brief,
}

#[derive(Debug, Clone)]
pub struct FolderDiffOptions {
    pub text: TextOptions,
    pub hunk: HunkOptions,
    pub style: DiffStyle,
    pub format: PatchFormat,
    pub report: Report,
    /// `-r`: descend into folders. Without it only the files of the top folder are compared and a
    /// folder on both sides is reported as `Common subdirectories`.
    pub recursive: bool,
    /// `-N` (and always for git): a file on one side only is diffed against an empty file.
    pub new_file: bool,
    /// Exclude patterns (the folder filter syntax; `*package-lock.json` matches at any depth).
    pub exclude_files: Vec<String>,
    pub exclude_folders: Vec<String>,
    /// Files above this size are reported as too large instead of being read.
    pub max_file_bytes: u64,
    /// The command-line options GNU prints in its `diff -ru a b` lines.
    pub cmdline_flags: String,
    /// The names the user gave for the two roots, used in the labels.
    pub left_name: String,
    pub right_name: String,
    /// git's `a/` and `b/` (`--no-prefix` empties them; `--src-prefix`, `--dst-prefix` set them).
    pub src_prefix: String,
    pub dst_prefix: String,
    /// GNU prints a `diff -ru a b` line before each file of a folder diff, but not for two plain
    /// files. Ignored for git style.
    pub header_line: bool,
}

impl Default for FolderDiffOptions {
    fn default() -> Self {
        FolderDiffOptions {
            text: udiff::diff_text_options(AlignmentName::Myers, WhitespaceName::All, false),
            hunk: HunkOptions::default(),
            style: DiffStyle::Gnu,
            format: PatchFormat::Unified,
            report: Report::Patch,
            recursive: true,
            new_file: false,
            exclude_files: Vec::new(),
            exclude_folders: Vec::new(),
            max_file_bytes: 512 << 20,
            cmdline_flags: String::new(),
            left_name: "a".into(),
            right_name: "b".into(),
            src_prefix: "a/".into(),
            dst_prefix: "b/".into(),
            header_line: true,
        }
    }
}

impl FolderDiffOptions {
    /// The folder-compare rules these options need: files are equal when their content is equal
    /// under the text options, whatever their time stamps say.
    pub fn folder_rules(&self) -> FolderRules {
        let exact = self.text.whitespace == WhitespaceName::All
            && !self.text.ignore_case
            && self.text.unimportant.is_empty()
            && self.text.eol == core_diff::options::EolName::Strict;
        FolderRules {
            compare_size: exact,
            compare_timestamp: false,
            compare_attributes: false,
            compare_content: true,
            content_kind: if exact {
                ContentKind::Binary
            } else {
                ContentKind::RulesBased
            },
            content_overrides: true,
            exclude_files: self.exclude_files.clone(),
            exclude_folders: self.exclude_folders.clone(),
            include_hidden: true,
            include_system: true,
            text: self.text.clone(),
            ..FolderRules::default()
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum FileChange {
    Modified,
    Added,
    Deleted,
}

/// A file that differs (or exists on one side only, with `new_file`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileDiff {
    /// `/`-separated path below the roots.
    pub path: String,
    pub change: FileChange,
    pub binary: bool,
    /// The file was not read because it is above `max_file_bytes`.
    pub too_large: bool,
    pub hunks: Vec<Hunk>,
    pub additions: u32,
    pub deletions: u32,
    pub old_size: u64,
    pub new_size: u64,
    pub old_mtime: i64,
    pub new_mtime: i64,
}

/// One line of the report, in walk order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Entry {
    File(FileDiff),
    /// `Only in dir: name` (no `-N`).
    OnlyIn {
        left: bool,
        dir: String,
        name: String,
        is_dir: bool,
    },
    /// `Common subdirectories` (no `-r`).
    CommonDir {
        path: String,
    },
    /// A file on one side and a folder on the other.
    TypeConflict {
        path: String,
        left_is_dir: bool,
    },
    /// A file that could not be compared.
    Error {
        path: String,
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderDiff {
    pub entries: Vec<Entry>,
    /// Files that are equal under the text options.
    pub same_files: u32,
}

impl FolderDiff {
    /// GNU's exit status: 1 when anything differs, 2 when something could not be compared, else 0.
    pub fn exit_code(&self) -> i32 {
        if self
            .entries
            .iter()
            .any(|e| matches!(e, Entry::Error { .. }))
        {
            return 2;
        }
        i32::from(
            self.entries
                .iter()
                .any(|e| !matches!(e, Entry::CommonDir { .. })),
        )
    }

    pub fn files(&self) -> impl Iterator<Item = &FileDiff> {
        self.entries.iter().filter_map(|e| match e {
            Entry::File(f) => Some(f),
            _ => None,
        })
    }
}

/// 1970-01-01 in 100 ns ticks since 1601: the time GNU diff prints for a file that is not there.
const UNIX_EPOCH_TICKS: i64 = 11_644_473_600 * 10_000_000;

fn read_all(vfs: &dyn Vfs, p: &VPath, max: u64) -> Result<Option<Vec<u8>>> {
    let mut r = vfs.open_read(p)?;
    let mut out = Vec::new();
    r.by_ref()
        .take(max + 1)
        .read_to_end(&mut out)
        .map_err(|e| Error::from_io(&e, p.display()))?;
    Ok((out.len() as u64 <= max).then_some(out))
}

/// Compares two folders and builds the report. `left`/`right` are the folder roots.
pub fn diff_folders(
    left: Arc<dyn Vfs>,
    left_root: &VPath,
    right: Arc<dyn Vfs>,
    right_root: &VPath,
    opts: &FolderDiffOptions,
    cancel: &CancelToken,
) -> Result<FolderDiff> {
    let fc = FolderCompare::compare(
        left,
        left_root,
        right,
        right_root,
        opts.folder_rules(),
        cancel,
    )?;
    diff_compare(&fc, opts, cancel)
}

/// Builds the report from a finished compare (its rules must be [`FolderDiffOptions::folder_rules`]).
pub fn diff_compare(
    fc: &FolderCompare,
    opts: &FolderDiffOptions,
    cancel: &CancelToken,
) -> Result<FolderDiff> {
    let mut out: Vec<(String, Entry)> = Vec::new();
    let mut same = 0u32;
    let tree = &fc.tree;
    let mut stack: Vec<NodeId> = tree.node(Tree::ROOT).children().rev().collect();
    while let Some(id) = stack.pop() {
        cancel.check()?;
        let n = tree.node(id);
        if n.has(flag::GONE) || n.has(flag::LINK) {
            continue;
        }
        let rel = tree.path(id).display();
        let (l, r) = (&n.left, &n.right);
        match (l.present, r.present) {
            (true, true) if l.kind == Kind::Dir && r.kind == Kind::Dir => {
                if opts.recursive {
                    stack.extend(n.children().rev());
                } else {
                    out.push((rel.clone(), Entry::CommonDir { path: rel }));
                }
            }
            (true, true) if l.kind == Kind::File && r.kind == Kind::File => match n.status {
                Status::Same | Status::UnimportantDiff => same += 1,
                Status::Error => {
                    let m = fc
                        .errors
                        .get(&id)
                        .cloned()
                        .unwrap_or_else(|| "compare failed".into());
                    out.push((
                        rel.clone(),
                        Entry::Error {
                            path: rel,
                            message: m,
                        },
                    ));
                }
                _ => {
                    let e = file_entry(fc, id, &rel, FileChange::Modified, opts)?;
                    out.push((rel, e));
                }
            },
            (true, true) => {
                out.push((
                    rel.clone(),
                    Entry::TypeConflict {
                        path: rel,
                        left_is_dir: l.kind == Kind::Dir,
                    },
                ));
            }
            (true, false) | (false, true) => {
                let left_side = l.present;
                let info = if left_side { l } else { r };
                if info.kind == Kind::Dir {
                    if opts.new_file && opts.recursive {
                        stack.extend(n.children().rev());
                    } else {
                        let (dir, name) = split_dir(&rel);
                        out.push((
                            rel,
                            Entry::OnlyIn {
                                left: left_side,
                                dir,
                                name,
                                is_dir: true,
                            },
                        ));
                    }
                } else if opts.new_file {
                    let change = if left_side {
                        FileChange::Deleted
                    } else {
                        FileChange::Added
                    };
                    let e = file_entry(fc, id, &rel, change, opts)?;
                    out.push((rel, e));
                } else {
                    let (dir, name) = split_dir(&rel);
                    out.push((
                        rel,
                        Entry::OnlyIn {
                            left: left_side,
                            dir,
                            name,
                            is_dir: false,
                        },
                    ));
                }
            }
            (false, false) => {}
        }
    }
    // GNU walks sorted names, a folder in its place: compare paths component by component
    out.sort_by(|a, b| cmp_paths(&a.0, &b.0));
    Ok(FolderDiff {
        entries: out.into_iter().map(|(_, e)| e).collect(),
        same_files: same,
    })
}

fn split_dir(rel: &str) -> (String, String) {
    match rel.rsplit_once('/') {
        Some((d, n)) => (d.to_string(), n.to_string()),
        None => (String::new(), rel.to_string()),
    }
}

fn cmp_paths(a: &str, b: &str) -> Ordering {
    a.split('/')
        .map(str::as_bytes)
        .cmp(b.split('/').map(str::as_bytes))
}

fn file_entry(
    fc: &FolderCompare,
    id: NodeId,
    rel: &str,
    change: FileChange,
    opts: &FolderDiffOptions,
) -> Result<Entry> {
    let n = fc.tree.node(id);
    let (old_size, new_size) = (
        if n.left.present { n.left.size } else { 0 },
        if n.right.present { n.right.size } else { 0 },
    );
    let mut fd = FileDiff {
        path: rel.to_string(),
        change,
        binary: false,
        too_large: false,
        hunks: Vec::new(),
        additions: 0,
        deletions: 0,
        old_size,
        new_size,
        old_mtime: if n.left.present {
            n.left.mtime
        } else {
            UNIX_EPOCH_TICKS
        },
        new_mtime: if n.right.present {
            n.right.mtime
        } else {
            UNIX_EPOCH_TICKS
        },
    };
    if old_size.max(new_size) > opts.max_file_bytes {
        fd.too_large = true;
        return Ok(Entry::File(fd));
    }
    let fetch = |left_side: bool, present: bool| -> Result<Vec<u8>> {
        if !present {
            return Ok(Vec::new());
        }
        let p = fc
            .tree
            .side_path(id, left_side)
            .ok_or_else(|| Error::new(ErrorKind::NotFound, "no path"))?;
        read_all(&**fc.vfs(left_side), &p, opts.max_file_bytes)?
            .ok_or_else(|| Error::new(ErrorKind::InvalidArgument, "file grew past the size limit"))
    };
    let (a, b) = (fetch(true, n.left.present)?, fetch(false, n.right.present)?);
    fill(&mut fd, &a, &b, opts)?;
    Ok(Entry::File(fd))
}

/// Diffs the two byte buffers into `fd`: hunks and counts, or the binary flag.
fn fill(fd: &mut FileDiff, a: &[u8], b: &[u8], opts: &FolderDiffOptions) -> Result<()> {
    let body = udiff::diff_specs(
        DocSpec::Bytes(a.to_vec()),
        DocSpec::Bytes(b.to_vec()),
        &opts.text,
        &opts.hunk,
    )?;
    if body.binary {
        // equal binary files are not a difference (the compare said "different" under other rules)
        fd.binary = a != b;
    } else {
        let (add, del) = udiff::counts(&body.hunks);
        fd.additions = add;
        fd.deletions = del;
        fd.hunks = body.hunks;
    }
    Ok(())
}

/// One pair of files, not a folder: the report of `diff a b` (or `git diff --no-index a b`).
/// `old` and `new` are the contents (`None` = the file is absent, with `-N` an empty file) and
/// the times are `FILETIME` ticks (`None` = absent). `left_name`/`right_name` of `opts` are the
/// two file names as the user gave them.
pub fn diff_file_pair(
    old: Option<(Vec<u8>, i64)>,
    new: Option<(Vec<u8>, i64)>,
    opts: &FolderDiffOptions,
) -> Result<FolderDiff> {
    let change = match (&old, &new) {
        (None, Some(_)) => FileChange::Added,
        (Some(_), None) => FileChange::Deleted,
        _ => FileChange::Modified,
    };
    let (a, am) = old.unwrap_or((Vec::new(), UNIX_EPOCH_TICKS));
    let (b, bm) = new.unwrap_or((Vec::new(), UNIX_EPOCH_TICKS));
    let mut fd = FileDiff {
        path: String::new(),
        change,
        binary: false,
        too_large: (a.len() as u64).max(b.len() as u64) > opts.max_file_bytes,
        hunks: Vec::new(),
        additions: 0,
        deletions: 0,
        old_size: a.len() as u64,
        new_size: b.len() as u64,
        old_mtime: am,
        new_mtime: bm,
    };
    if !fd.too_large {
        fill(&mut fd, &a, &b, opts)?;
    }
    let differs =
        fd.too_large || fd.binary || !fd.hunks.is_empty() || (change != FileChange::Modified);
    let (entries, same) = if differs {
        (vec![Entry::File(fd)], 0)
    } else {
        (Vec::new(), 1)
    };
    Ok(FolderDiff {
        entries,
        same_files: same,
    })
}

// ------------------------------------------------------------------ rendering

fn join(prefix: &str, rel: &str) -> String {
    if rel.is_empty() {
        // a single file pair: the name the user gave is the whole path
        return prefix.to_string();
    }
    let p = prefix.trim_end_matches(['/', '\\']);
    if p.is_empty() {
        rel.to_string()
    } else {
        format!("{p}/{rel}")
    }
}

fn format_labels(style: DiffStyle, f: &FileDiff, opts: &FolderDiffOptions) -> (String, String) {
    let (l, r) = (
        join(&opts.left_name, &f.path),
        join(&opts.right_name, &f.path),
    );
    match style {
        DiffStyle::Gnu => (l, r),
        DiffStyle::Git => (
            format!("{}{l}", opts.src_prefix),
            format!("{}{r}", opts.dst_prefix),
        ),
    }
}

fn render_patch(fd: &FolderDiff, opts: &FolderDiffOptions) -> String {
    let mut out = String::new();
    for e in &fd.entries {
        match e {
            Entry::OnlyIn {
                left, dir, name, ..
            } => {
                let root = if *left {
                    &opts.left_name
                } else {
                    &opts.right_name
                };
                let _ = writeln!(
                    out,
                    "Only in {}: {name}",
                    join(root, dir).trim_end_matches('/')
                );
            }
            Entry::CommonDir { path } => {
                let _ = writeln!(
                    out,
                    "Common subdirectories: {} and {}",
                    join(&opts.left_name, path),
                    join(&opts.right_name, path)
                );
            }
            Entry::TypeConflict { path, left_is_dir } => {
                let (l, r) = (join(&opts.left_name, path), join(&opts.right_name, path));
                let (lk, rk) = if *left_is_dir {
                    ("directory", "regular file")
                } else {
                    ("regular file", "directory")
                };
                let _ = writeln!(out, "File {l} is a {lk} while file {r} is a {rk}");
            }
            Entry::Error { path, message } => {
                let _ = writeln!(out, "diff: {}: {message}", join(&opts.left_name, path));
            }
            Entry::File(f) => render_file(&mut out, f, opts),
        }
    }
    out
}

fn render_file(out: &mut String, f: &FileDiff, opts: &FolderDiffOptions) {
    let (l, r) = format_labels(opts.style, f, opts);
    match opts.style {
        DiffStyle::Gnu => {
            if f.binary || f.too_large {
                let what = if f.too_large {
                    "differ (too large to compare)"
                } else {
                    "differ"
                };
                let _ = writeln!(out, "Binary files {l} and {r} {what}");
                return;
            }
            if f.hunks.is_empty() {
                return;
            }
            if opts.header_line {
                let flags = if opts.cmdline_flags.is_empty() {
                    String::new()
                } else {
                    format!(" {}", opts.cmdline_flags)
                };
                let _ = writeln!(out, "diff{flags} {l} {r}");
            }
        }
        DiffStyle::Git => {
            let _ = writeln!(out, "diff --git {l} {r}");
            match f.change {
                FileChange::Added => out.push_str("new file mode 100644\n"),
                FileChange::Deleted => out.push_str("deleted file mode 100644\n"),
                FileChange::Modified => {}
            }
            if f.binary || f.too_large {
                let _ = writeln!(
                    out,
                    "Binary files {} and {} differ",
                    old_label(f, &l, opts.style),
                    new_label(f, &r, opts.style)
                );
                return;
            }
        }
    }
    // the context format uses the old `ctime` style for its time stamps (GNU diff does)
    let time = |t: i64| {
        if opts.format == PatchFormat::Context {
            udiff::format_context_time(t)
        } else {
            udiff::format_gnu_time(t)
        }
    };
    let labels = Labels {
        old: old_label(f, &l, opts.style),
        new: new_label(f, &r, opts.style),
        old_time: (opts.style == DiffStyle::Gnu).then(|| time(f.old_mtime)),
        new_time: (opts.style == DiffStyle::Gnu).then(|| time(f.new_mtime)),
    };
    match opts.format {
        PatchFormat::Unified => out.push_str(&udiff::render_unified(&f.hunks, &labels)),
        PatchFormat::Context => out.push_str(&udiff::render_context(&f.hunks, &labels)),
        PatchFormat::Normal => out.push_str(&udiff::render_normal(&f.hunks)),
    }
}

/// Git writes `/dev/null` for the side that does not exist; GNU `-N` keeps the real name there
/// and prints the Unix epoch as its time.
fn old_label(f: &FileDiff, l: &str, style: DiffStyle) -> String {
    if f.change == FileChange::Added && style == DiffStyle::Git {
        "/dev/null".into()
    } else {
        l.to_string()
    }
}

fn new_label(f: &FileDiff, r: &str, style: DiffStyle) -> String {
    if f.change == FileChange::Deleted && style == DiffStyle::Git {
        "/dev/null".into()
    } else {
        r.to_string()
    }
}

fn digits(mut n: u32) -> usize {
    let mut d = 1;
    while n >= 10 {
        n /= 10;
        d += 1;
    }
    d
}

fn scale_linear(it: u32, width: u32, max: u32) -> u32 {
    if it == 0 {
        0
    } else {
        1 + it * (width - 1) / max.max(1)
    }
}

fn summary_line(files: usize, add: u32, del: u32) -> String {
    let mut s = format!(" {files} file{} changed", if files == 1 { "" } else { "s" });
    if add > 0 || del == 0 {
        let _ = write!(s, ", {add} insertion{}(+)", if add == 1 { "" } else { "s" });
    }
    if del > 0 || add == 0 {
        let _ = write!(s, ", {del} deletion{}(-)", if del == 1 { "" } else { "s" });
    }
    s.push('\n');
    s
}

/// `--stat` the way git draws it (80 columns).
fn render_stat(fd: &FolderDiff, opts: &FolderDiffOptions) -> String {
    let files: Vec<&FileDiff> = fd.files().collect();
    if files.is_empty() {
        return String::new();
    }
    let width = 80u32;
    let max_change = files
        .iter()
        .map(|f| f.additions + f.deletions)
        .max()
        .unwrap_or(0);
    let name_of = |f: &FileDiff| match opts.style {
        DiffStyle::Git => f.path.clone(),
        DiffStyle::Gnu => join(&opts.right_name, &f.path),
    };
    let max_len = files
        .iter()
        .map(|f| name_of(f).chars().count())
        .max()
        .unwrap_or(0) as u32;
    // a binary file's `Bin` is three wide: git only widens the column when there is one
    let any_binary = files.iter().any(|f| f.binary || f.too_large);
    let number_width = if any_binary {
        digits(max_change).max(3)
    } else {
        digits(max_change)
    } as u32;
    let mut graph_width = max_change;
    let mut name_width = max_len;
    let width = width.max(16 + 6 + number_width);
    if name_width + number_width + 6 + graph_width > width {
        if graph_width > width * 3 / 8 - number_width - 6 {
            graph_width = (width * 3 / 8 - number_width - 6).max(6);
        }
        if name_width > width - number_width - 6 - graph_width {
            name_width = width - number_width - 6 - graph_width;
        } else {
            graph_width = width - number_width - 6 - name_width;
        }
    }
    let (mut tadd, mut tdel) = (0, 0);
    let mut out = String::new();
    for f in &files {
        let mut name = name_of(f);
        let n = name.chars().count() as u32;
        if n > name_width {
            let keep = name_width.saturating_sub(3) as usize;
            let tail: String = name.chars().skip(n as usize - keep).collect();
            name = format!("...{tail}");
        }
        let pad = " ".repeat((name_width as usize).saturating_sub(name.chars().count()));
        if f.binary || f.too_large {
            let _ = writeln!(
                out,
                " {name}{pad} | Bin {} -> {} bytes",
                f.old_size, f.new_size
            );
            continue;
        }
        let (mut a, mut d) = (f.additions, f.deletions);
        tadd += a;
        tdel += d;
        let num = a + d;
        if graph_width <= max_change {
            let mut total = scale_linear(a + d, graph_width, max_change);
            if total < 2 && a > 0 && d > 0 {
                total = 2;
            }
            if a < d {
                a = scale_linear(a, graph_width, max_change);
                d = total - a;
            } else {
                d = scale_linear(d, graph_width, max_change);
                a = total - d;
            }
        }
        let _ = writeln!(
            out,
            " {name}{pad} | {num:>w$} {}{}",
            "+".repeat(a as usize),
            "-".repeat(d as usize),
            w = number_width as usize
        );
    }
    out.push_str(&summary_line(files.len(), tadd, tdel));
    out
}

/// Renders the report in the format `opts` asks for.
pub fn render(fd: &FolderDiff, opts: &FolderDiffOptions) -> String {
    let mut out = String::new();
    match opts.report {
        Report::Patch => return render_patch(fd, opts),
        Report::Stat => return render_stat(fd, opts),
        Report::ShortStat => {
            let (a, d) = fd
                .files()
                .fold((0, 0), |(a, d), f| (a + f.additions, d + f.deletions));
            let n = fd.files().count();
            if n > 0 {
                out.push_str(&summary_line(n, a, d));
            }
        }
        Report::NumStat => {
            for f in fd.files() {
                if f.binary || f.too_large {
                    let _ = writeln!(out, "-\t-\t{}", f.path);
                } else {
                    let _ = writeln!(out, "{}\t{}\t{}", f.additions, f.deletions, f.path);
                }
            }
        }
        Report::NameOnly => {
            for f in fd.files() {
                let _ = writeln!(out, "{}", f.path);
            }
        }
        Report::NameStatus => {
            for f in fd.files() {
                let c = match f.change {
                    FileChange::Modified => 'M',
                    FileChange::Added => 'A',
                    FileChange::Deleted => 'D',
                };
                let _ = writeln!(out, "{c}\t{}", f.path);
            }
        }
        Report::Brief => {
            for e in &fd.entries {
                match e {
                    Entry::File(f) => {
                        let (l, r) = format_labels(DiffStyle::Gnu, f, opts);
                        let _ = writeln!(out, "Files {l} and {r} differ");
                    }
                    Entry::OnlyIn {
                        left, dir, name, ..
                    } => {
                        let root = if *left {
                            &opts.left_name
                        } else {
                            &opts.right_name
                        };
                        let _ = writeln!(
                            out,
                            "Only in {}: {name}",
                            join(root, dir).trim_end_matches('/')
                        );
                    }
                    Entry::CommonDir { path } => {
                        let _ = writeln!(
                            out,
                            "Common subdirectories: {} and {}",
                            join(&opts.left_name, path),
                            join(&opts.right_name, path)
                        );
                    }
                    _ => {}
                }
            }
        }
    }
    out
}
