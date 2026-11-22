//! Applying a patch (backlog E11.5-S16): unified diffs and git mailbox patches, SEARCH/REPLACE
//! blocks, JSON Patch (RFC 6902) and byte scripts, to files under a folder or to a text that is
//! not a file (a UI buffer, an MCP tool argument).
//!
//! The rules, which are the §9 write rules applied to patches (decision D58):
//!
//! * everything is computed in memory first; if any hunk, block or operation fails nothing is
//!   written, unless the caller asks for `partial`;
//! * a path in a patch must stay below the root: absolute paths, drive letters, `..` and any
//!   folder that is a link or a junction are refused;
//! * a file is replaced through a temporary file and a rename, after a check that it still has the
//!   size and time it had when it was read;
//! * `dryRun` does everything but the writes and reports what would happen.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use core_diff::patch::{
    ApplyOptions, FileKind, FilePatch, HunkResult, MailHeader, apply_file_patch, parse_patches,
};
use core_diff::searchreplace::{BlockResult, SrBlock, apply_blocks, parse_blocks};
use serde::{Deserialize, Serialize};

use crate::{bindiff, jsonpatch};

/// What to apply and where.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ApplyRequest {
    /// The patch text.
    pub patch: String,
    /// `auto` (default), `unified` (also git and mailbox patches), `searchReplace`, `jsonPatch`,
    /// `byteScript`.
    pub format: String,
    /// The folder the paths in the patch are relative to.
    pub root: String,
    /// The file for formats that carry no path (JSON Patch, byte scripts), for blocks without a
    /// file name, and for a one-file unified patch whose names are not wanted. Relative to `root`.
    pub target: String,
    /// Apply to this text instead of to files; the result is in the report, nothing is read or
    /// written. A unified patch must then hold one file; paths are ignored.
    pub text: Option<String>,
    /// Leading path components to drop (`patch -p`). Default: 1 for `a/` and `b/` prefixes.
    pub strip: Option<usize>,
    pub reverse: bool,
    pub dry_run: bool,
    /// Context lines that may be dropped to find a hunk's place (default 2).
    pub fuzz: u32,
    pub ignore_whitespace: bool,
    /// Write the files that did apply although others did not.
    pub partial: bool,
    /// Files above this size are refused.
    pub max_file_bytes: u64,
}

impl Default for ApplyRequest {
    fn default() -> Self {
        ApplyRequest {
            patch: String::new(),
            format: "auto".into(),
            root: String::new(),
            target: String::new(),
            text: None,
            strip: None,
            reverse: false,
            dry_run: false,
            fuzz: 2,
            ignore_whitespace: false,
            partial: false,
            max_file_bytes: 64 << 20,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct FileReport {
    pub path: String,
    /// `modify`, `create`, `delete`, `rename`.
    pub action: String,
    pub ok: bool,
    pub message: String,
    pub hunks: Vec<HunkResult>,
    pub blocks: Vec<BlockResult>,
    /// JSON operations or bytes changed.
    pub changes: usize,
    pub additions: u32,
    pub deletions: u32,
    /// The result, for a request with `text`.
    pub text: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ApplyReport {
    /// Every hunk, block and operation applied.
    pub ok: bool,
    pub dry_run: bool,
    /// Files were written or removed.
    pub written: bool,
    pub format: String,
    pub files: Vec<FileReport>,
    pub error: Option<String>,
    /// The mail header of the first mail of a mailbox patch.
    pub mail: Option<MailHeader>,
}

fn failed(format: &str, msg: impl Into<String>) -> ApplyReport {
    ApplyReport {
        format: format.into(),
        error: Some(msg.into()),
        ..ApplyReport::default()
    }
}

/// What is to be written for one file.
struct Plan {
    /// Path of the file that gets the new content, when there is one.
    dst: Option<PathBuf>,
    bytes: Vec<u8>,
    /// A file to remove (deleted, or the old name of a rename).
    remove: Option<PathBuf>,
    /// The files read for this plan and what they looked like.
    seen: Vec<(PathBuf, Option<(u64, SystemTime)>)>,
}

fn sniff(patch: &str) -> &'static str {
    let t = patch.trim_start();
    if t.starts_with('[') || t.starts_with('{') {
        return "jsonPatch";
    }
    if patch.lines().any(|l| l.starts_with("<<<<<<< SEARCH")) {
        return "searchReplace";
    }
    let script = patch
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with(['>', '#', ';']))
        .is_some_and(|l| {
            l.split_once(':').is_some_and(|(a, b)| {
                !a.is_empty()
                    && a.len() <= 16
                    && a.bytes().all(|c| c.is_ascii_hexdigit())
                    && (b.contains("->") || b.split_whitespace().count() == 2)
            })
        });
    if script { "byteScript" } else { "unified" }
}

fn is_reparse(md: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        md.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        md.file_type().is_symlink()
    }
}

/// `root` + `rel`, refusing anything that leaves the root or passes through a link.
fn safe_join(root: &Path, rel: &str) -> Result<PathBuf, String> {
    if rel.trim().is_empty() {
        return Err("the patch has no file name".into());
    }
    let norm = rel.replace('\\', "/");
    if norm.starts_with('/') {
        return Err(format!("'{rel}' is an absolute path"));
    }
    let mut p = root.to_path_buf();
    for c in norm.split('/') {
        match c {
            "" | "." => {}
            ".." => return Err(format!("'{rel}' leaves the folder (..)")),
            c if c.contains(':') || c.contains('\0') => {
                return Err(format!("'{rel}' has a drive or stream name"));
            }
            // names Windows reads differently from what they say: a trailing dot or space is
            // dropped, `nul` and `con.txt` are devices
            c if c.ends_with(['.', ' ']) || core_vfs::is_reserved_windows_name(c) => {
                return Err(format!("'{c}' is not a name a patch may create or change"));
            }
            c => p.push(c),
        }
        if let Ok(md) = std::fs::symlink_metadata(&p) {
            if is_reparse(&md) {
                return Err(format!(
                    "'{}' is a link or junction; a patch never goes through one",
                    p.display()
                ));
            }
        }
    }
    Ok(p)
}

type Seen = Vec<(PathBuf, Option<(u64, SystemTime)>)>;

fn fingerprint(p: &Path) -> Option<(u64, SystemTime)> {
    let md = std::fs::metadata(p).ok()?;
    Some((md.len(), md.modified().ok()?))
}

fn read_text(p: &Path, max: u64, seen: &mut Seen) -> Result<(String, bool), String> {
    let md = std::fs::metadata(p).map_err(|e| format!("{}: {e}", p.display()))?;
    if !md.is_file() {
        return Err(format!("{} is not a file", p.display()));
    }
    if md.len() > max {
        return Err(format!("{} is larger than {max} bytes", p.display()));
    }
    let b = std::fs::read(p).map_err(|e| format!("{}: {e}", p.display()))?;
    seen.push((p.to_path_buf(), fingerprint(p)));
    if b.starts_with(&[0xFF, 0xFE]) || b.starts_with(&[0xFE, 0xFF]) {
        return Err(format!(
            "{} is UTF-16; only UTF-8 text is patched",
            p.display()
        ));
    }
    let (body, bom) = match b.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        Some(r) => (r.to_vec(), true),
        None => (b, false),
    };
    String::from_utf8(body)
        .map(|s| (s, bom))
        .map_err(|_| format!("{} is not UTF-8 text", p.display()))
}

fn with_bom(s: &str, bom: bool) -> Vec<u8> {
    let mut v = Vec::with_capacity(s.len() + 3);
    if bom {
        v.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
    }
    v.extend_from_slice(s.as_bytes());
    v
}

/// Applies the request. Never panics and never leaves a half-written file behind.
pub fn apply(req: &ApplyRequest) -> ApplyReport {
    let format = match req.format.as_str() {
        "" | "auto" => sniff(&req.patch),
        "unified" => "unified",
        "searchReplace" => "searchReplace",
        "jsonPatch" => "jsonPatch",
        "byteScript" => "byteScript",
        o => return failed(o, format!("unknown patch format '{o}'")),
    };
    let root = PathBuf::from(&req.root);
    if req.text.is_none() && !root.is_absolute() {
        return failed(
            format,
            "root must be an absolute path (the process's current folder is never used)",
        );
    }
    if req.text.is_none() && !root.is_dir() {
        return failed(format, format!("the folder '{}' does not exist", req.root));
    }
    let mut report = ApplyReport {
        format: format.into(),
        dry_run: req.dry_run,
        ..ApplyReport::default()
    };
    let mut plans: Vec<Plan> = Vec::new();
    let res = match format {
        "unified" => unified(req, &root, &mut report, &mut plans),
        "searchReplace" => search_replace(req, &root, &mut report, &mut plans),
        "jsonPatch" => json_patch(req, &root, &mut report, &mut plans),
        _ => byte_script(req, &root, &mut report, &mut plans),
    };
    if let Err(e) = res {
        report.error = Some(e);
        report.ok = false;
        return report;
    }
    if let Some(dup) = duplicate_path(&plans) {
        report.error = Some(format!(
            "two patches change {}; each would be computed from the file as it is now and the second would overwrite the first. Apply them one after another",
            dup.display()
        ));
        report.ok = false;
        return report;
    }
    report.ok = report.files.iter().all(|f| f.ok);
    if req.text.is_some() || req.dry_run || plans.is_empty() {
        return report;
    }
    if !report.ok && !req.partial {
        return report; // all or nothing
    }
    match commit(&plans) {
        Ok(()) => report.written = true,
        Err(e) => {
            report.ok = false;
            report.error = Some(e);
        }
    }
    report
}

fn opts(req: &ApplyRequest) -> ApplyOptions {
    ApplyOptions {
        reverse: req.reverse,
        fuzz: req.fuzz,
        ignore_whitespace: req.ignore_whitespace,
    }
}

fn auto_strip(files: &[&FilePatch]) -> usize {
    let prefixed = |p: &Option<String>, pre: &str| p.as_ref().is_none_or(|s| s.starts_with(pre));
    usize::from(
        !files.is_empty()
            && files
                .iter()
                .all(|f| prefixed(&f.old_path, "a/") && prefixed(&f.new_path, "b/")),
    )
}

fn single_target(req: &ApplyRequest, root: &Path) -> Result<PathBuf, String> {
    if req.target.is_empty() {
        return Err("this patch format has no file names; pass `target`".into());
    }
    safe_join(root, &req.target)
}

fn unified(
    req: &ApplyRequest,
    root: &Path,
    report: &mut ApplyReport,
    plans: &mut Vec<Plan>,
) -> Result<(), String> {
    let patches = parse_patches(&req.patch).map_err(|e| e.to_string())?;
    report.mail = patches.first().and_then(|p| p.mail.clone());
    let files: Vec<&FilePatch> = patches.iter().flat_map(|p| &p.files).collect();
    let strip = req.strip.unwrap_or_else(|| auto_strip(&files));
    if req.text.is_some() && files.len() != 1 {
        return Err(format!(
            "a patch applied to a text must hold one file, this one holds {}",
            files.len()
        ));
    }
    for fp in files {
        // forward: old -> new; reversed: new -> old
        let (src, dst) = if req.reverse {
            (fp.new_path_stripped(strip), fp.old_path_stripped(strip))
        } else {
            (fp.old_path_stripped(strip), fp.new_path_stripped(strip))
        };
        let kind = match (&src, &dst) {
            (None, _) => FileKind::Create,
            (_, None) => FileKind::Delete,
            (Some(a), Some(b)) if a != b => FileKind::Rename,
            _ => FileKind::Modify,
        };
        let shown = dst.clone().or_else(|| src.clone()).unwrap_or_default();
        let mut fr = FileReport {
            path: shown.clone(),
            action: format!("{kind:?}").to_lowercase(),
            additions: fp.additions(),
            deletions: fp.deletions(),
            ..FileReport::default()
        };
        if fp.binary {
            fr.message = "a binary patch; only text patches are applied".into();
            report.files.push(fr);
            continue;
        }
        let mut seen: Seen = Vec::new();
        let outcome = (|| -> Result<Option<Plan>, String> {
            let (base, bom, src_path) = if let Some(t) = &req.text {
                (t.clone(), false, None)
            } else if kind == FileKind::Create {
                (String::new(), false, None)
            } else {
                let sp = safe_join(root, src.as_deref().unwrap_or(""))?;
                let (t, b) = read_text(&sp, req.max_file_bytes, &mut seen)?;
                (t, b, Some(sp))
            };
            let dst_path = match (&dst, req.text.is_some()) {
                (Some(d), false) => Some(safe_join(root, d)?),
                _ => None,
            };
            if matches!(kind, FileKind::Create | FileKind::Rename) {
                if let Some(d) = &dst_path {
                    if d.exists() {
                        return Err(format!("{} already exists", d.display()));
                    }
                    // and it must still be absent when the files are put in place
                    seen.push((d.clone(), None));
                }
            }
            let applied = apply_file_patch(&base, fp, &opts(req));
            fr.hunks.clone_from(&applied.hunks);
            if applied.all_applied() {
                fr.ok = true;
            } else {
                let bad = applied
                    .hunks
                    .iter()
                    .filter(|h| !h.applied)
                    .map(|h| h.message.clone())
                    .collect::<Vec<_>>()
                    .join("; ");
                if !req.partial {
                    return Err(bad);
                }
                fr.message = bad;
                fr.ok = false;
            }
            if kind == FileKind::Delete && !applied.text.is_empty() && applied.all_applied() {
                return Err("the file is not empty after the deletion hunks".into());
            }
            if req.text.is_some() {
                fr.text = Some(applied.text);
                return Ok(None);
            }
            Ok(Some(Plan {
                dst: (kind != FileKind::Delete)
                    .then(|| dst_path.clone())
                    .flatten(),
                bytes: with_bom(&applied.text, bom),
                remove: match kind {
                    FileKind::Delete | FileKind::Rename => src_path,
                    _ => None,
                },
                seen: std::mem::take(&mut seen),
            }))
        })();
        match outcome {
            Ok(Some(p)) => {
                if fr.ok || req.partial {
                    plans.push(p);
                }
            }
            Ok(None) => {}
            Err(m) => {
                fr.ok = false;
                fr.message = m;
            }
        }
        report.files.push(fr);
    }
    Ok(())
}

fn search_replace(
    req: &ApplyRequest,
    root: &Path,
    report: &mut ApplyReport,
    plans: &mut Vec<Plan>,
) -> Result<(), String> {
    if req.reverse {
        return Err("SEARCH/REPLACE blocks cannot be reversed; swap SEARCH and REPLACE".into());
    }
    let blocks = parse_blocks(&req.patch).map_err(|e| e.to_string())?;
    // group by file, in order of first appearance
    let mut groups: Vec<(String, Vec<&SrBlock>)> = Vec::new();
    for b in &blocks {
        let name = match (&b.path, req.text.is_some()) {
            (_, true) => String::new(),
            (Some(p), _) if req.target.is_empty() => p.clone(),
            _ if !req.target.is_empty() => req.target.clone(),
            _ => return Err("a block has no file name; pass `target`".into()),
        };
        match groups.iter_mut().find(|g| g.0 == name) {
            Some(g) => g.1.push(b),
            None => groups.push((name, vec![b])),
        }
    }
    for (name, bs) in groups {
        let mut fr = FileReport {
            path: name.clone(),
            action: "modify".into(),
            ..FileReport::default()
        };
        let mut seen: Seen = Vec::new();
        let outcome = (|| -> Result<(String, bool, Option<PathBuf>), String> {
            if let Some(t) = &req.text {
                return Ok((t.clone(), false, None));
            }
            let p = safe_join(root, &name)?;
            if p.exists() {
                let (t, bom) = read_text(&p, req.max_file_bytes, &mut seen)?;
                Ok((t, bom, Some(p)))
            } else if bs.first().is_some_and(|b| b.search.is_empty()) {
                fr.action = "create".into();
                seen.push((p.clone(), None));
                Ok((String::new(), false, Some(p)))
            } else {
                Err(format!("{} does not exist", p.display()))
            }
        })();
        match outcome {
            Err(m) => {
                fr.message = m;
            }
            Ok((text, bom, path)) => {
                let (new_text, results) = apply_blocks(&text, &bs);
                fr.ok = results.iter().all(|r| r.applied);
                fr.message = results
                    .iter()
                    .filter(|r| !r.applied)
                    .map(|r| r.message.clone())
                    .collect::<Vec<_>>()
                    .join("; ");
                fr.blocks = results;
                if req.text.is_some() {
                    fr.text = Some(new_text);
                } else if (fr.ok || req.partial) && new_text != text {
                    plans.push(Plan {
                        dst: path,
                        bytes: with_bom(&new_text, bom),
                        remove: None,
                        seen,
                    });
                }
            }
        }
        report.files.push(fr);
    }
    Ok(())
}

fn json_patch(
    req: &ApplyRequest,
    root: &Path,
    report: &mut ApplyReport,
    plans: &mut Vec<Plan>,
) -> Result<(), String> {
    let ops: Vec<serde_json::Value> = serde_json::from_str(&req.patch)
        .map_err(|e| format!("the JSON Patch is not an array of operations: {e}"))?;
    let mut seen: Seen = Vec::new();
    let (src, path, bom) = if let Some(t) = &req.text {
        (t.clone(), None, false)
    } else {
        let p = single_target(req, root)?;
        let (t, bom) = read_text(&p, req.max_file_bytes, &mut seen)?;
        (t, Some(p), bom)
    };
    let doc: serde_json::Value =
        serde_json::from_str(&src).map_err(|e| format!("the document is not JSON: {e}"))?;
    let mut fr = FileReport {
        path: req.target.clone(),
        action: "modify".into(),
        changes: ops.len(),
        ..FileReport::default()
    };
    match jsonpatch::apply(&doc, &ops) {
        Err(e) => fr.message = e.to_string(),
        Ok(new) => {
            fr.ok = true;
            // the file is written pretty-printed with two spaces; its original layout is not kept
            let mut text = serde_json::to_string_pretty(&new).map_err(|e| e.to_string())?;
            text.push('\n');
            if req.text.is_some() {
                fr.text = Some(text);
            } else if let Some(p) = path {
                plans.push(Plan {
                    dst: Some(p),
                    bytes: with_bom(&text, bom),
                    remove: None,
                    seen,
                });
            }
        }
    }
    report.files.push(fr);
    Ok(())
}

fn byte_script(
    req: &ApplyRequest,
    root: &Path,
    report: &mut ApplyReport,
    plans: &mut Vec<Plan>,
) -> Result<(), String> {
    if req.text.is_some() {
        return Err("a byte script applies to a file, not to a text".into());
    }
    let p = single_target(req, root)?;
    let md = std::fs::metadata(&p).map_err(|e| format!("{}: {e}", p.display()))?;
    if md.len() > req.max_file_bytes {
        return Err(format!("{} is too large", p.display()));
    }
    let mut data = std::fs::read(&p).map_err(|e| format!("{}: {e}", p.display()))?;
    let seen = vec![(p.clone(), fingerprint(&p))];
    let mut fr = FileReport {
        path: req.target.clone(),
        action: "modify".into(),
        ..FileReport::default()
    };
    match bindiff::apply_script(&mut data, &req.patch, req.reverse) {
        Err(e) => fr.message = e.to_string(),
        Ok(n) => {
            fr.ok = true;
            fr.changes = n;
            plans.push(Plan {
                dst: Some(p),
                bytes: data,
                remove: None,
                seen,
            });
        }
    }
    report.files.push(fr);
    Ok(())
}

/// A path two plans both write, remove or read (a series of patches that touch one file).
fn duplicate_path(plans: &[Plan]) -> Option<PathBuf> {
    let mut seen = std::collections::HashSet::new();
    for p in plans {
        for path in p.dst.iter().chain(p.remove.iter()) {
            if !seen.insert(path.clone()) {
                return Some(path.clone());
            }
        }
    }
    None
}

fn aside_name(path: &Path, tag: &str, n: usize) -> PathBuf {
    let name = path
        .file_name()
        .map_or_else(String::new, |s| s.to_string_lossy().into_owned());
    let tmp = core_vfs::temp_sibling_name(&core_vfs::Name::new(&format!("{name}.{tag}{n}")));
    path.with_file_name(tmp.to_string_lossy())
}

/// Writes every plan, or none: all temporary files are written first, the sources are checked
/// against what was read, and only then are files put in place. A file that is replaced or removed
/// is first moved aside, and when any step fails everything done so far is undone, so the folder
/// is as it was (a crash in the middle leaves `.dctmp~` files, which the maintenance sweep finds).
fn commit(plans: &[Plan]) -> Result<(), String> {
    use std::io::Write as _;
    let mut temps: Vec<(PathBuf, PathBuf)> = Vec::new(); // (temp, destination)
    // a read-only file is not replaced (renaming over it would be refused, moving it aside would
    // not): the user's protection of it stands
    for p in plans {
        for path in p.dst.iter().chain(p.remove.iter()) {
            if std::fs::metadata(path).is_ok_and(|m| m.permissions().readonly()) {
                return Err(format!(
                    "{} is read-only; nothing was written",
                    path.display()
                ));
            }
        }
    }
    // new folders for created files; they are removed again when the commit is refused
    let mut created_dirs: Vec<PathBuf> = Vec::new();
    for p in plans {
        let Some(dst) = &p.dst else { continue };
        let mut missing: Vec<PathBuf> = Vec::new();
        let mut cur = dst.parent();
        while let Some(d) = cur {
            if d.exists() {
                break;
            }
            missing.push(d.to_path_buf());
            cur = d.parent();
        }
        for d in missing.into_iter().rev() {
            if let Err(e) = std::fs::create_dir(&d) {
                for c in created_dirs.iter().rev() {
                    let _ = std::fs::remove_dir(c);
                }
                return Err(format!("{}: {e}", d.display()));
            }
            created_dirs.push(d);
        }
    }
    let cleanup = |temps: &[(PathBuf, PathBuf)]| {
        for (t, _) in temps {
            let _ = std::fs::remove_file(t);
        }
        for d in created_dirs.iter().rev() {
            let _ = std::fs::remove_dir(d);
        }
    };
    for (n, p) in plans.iter().enumerate() {
        let Some(dst) = &p.dst else { continue };
        let tmp = aside_name(dst, "new", n);
        let written = (|| -> std::io::Result<()> {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp)?;
            f.write_all(&p.bytes)?;
            f.sync_all()?;
            Ok(())
        })();
        if let Err(e) = written {
            let _ = std::fs::remove_file(&tmp);
            cleanup(&temps);
            return Err(format!("{}: {e}", dst.display()));
        }
        if let Ok(md) = std::fs::metadata(dst) {
            let _ = std::fs::set_permissions(&tmp, md.permissions());
        }
        temps.push((tmp, dst.clone()));
    }
    for p in plans {
        for (path, fp) in &p.seen {
            if fingerprint(path) != *fp {
                cleanup(&temps);
                return Err(format!(
                    "{} changed after it was read; nothing was written",
                    path.display()
                ));
            }
        }
    }
    // (new path, the old content moved aside) for files put in place; (old path, aside) for removals
    let mut placed: Vec<(PathBuf, Option<PathBuf>)> = Vec::new();
    let mut removed: Vec<(PathBuf, PathBuf)> = Vec::new();
    let undo = |placed: &[(PathBuf, Option<PathBuf>)],
                removed: &[(PathBuf, PathBuf)],
                temps: &[(PathBuf, PathBuf)],
                dirs: &[PathBuf]| {
        for (r, aside) in removed.iter().rev() {
            let _ = std::fs::rename(aside, r);
        }
        for (dst, aside) in placed.iter().rev() {
            let _ = std::fs::remove_file(dst);
            if let Some(a) = aside {
                let _ = std::fs::rename(a, dst);
            }
        }
        for (t, _) in temps {
            let _ = std::fs::remove_file(t);
        }
        for d in dirs.iter().rev() {
            let _ = std::fs::remove_dir(d);
        }
    };
    for (n, (tmp, dst)) in temps.iter().enumerate() {
        let aside = if dst.exists() {
            let a = aside_name(dst, "old", n);
            if let Err(e) = std::fs::rename(dst, &a) {
                undo(&placed, &removed, &temps, &created_dirs);
                return Err(format!("{}: {e}; everything was put back", dst.display()));
            }
            Some(a)
        } else {
            None
        };
        if let Err(e) = std::fs::rename(tmp, dst) {
            if let Some(a) = &aside {
                let _ = std::fs::rename(a, dst);
            }
            undo(&placed, &removed, &temps, &created_dirs);
            return Err(format!("{}: {e}; everything was put back", dst.display()));
        }
        placed.push((dst.clone(), aside));
    }
    for (n, p) in plans.iter().enumerate() {
        if let Some(r) = &p.remove {
            let a = aside_name(r, "gone", n);
            if let Err(e) = std::fs::rename(r, &a) {
                undo(&placed, &removed, &[], &created_dirs);
                return Err(format!("{}: {e}; everything was put back", r.display()));
            }
            removed.push((r.clone(), a));
        }
    }
    // done: the old content is no longer needed
    for (_, aside) in &placed {
        if let Some(a) = aside {
            let _ = std::fs::remove_file(a);
        }
    }
    for (_, a) in &removed {
        let _ = std::fs::remove_file(a);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(root: &Path, patch: &str) -> ApplyRequest {
        ApplyRequest {
            patch: patch.into(),
            root: root.to_string_lossy().into_owned(),
            ..ApplyRequest::default()
        }
    }

    const P: &str =
        "diff --git a/f.txt b/f.txt\n--- a/f.txt\n+++ b/f.txt\n@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n";

    #[test]
    fn a_git_patch_is_applied_dry_run_first() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("f.txt"), "a\nb\nc\n").unwrap();
        let mut r = req(d.path(), P);
        r.dry_run = true;
        let rep = apply(&r);
        assert!(rep.ok && !rep.written, "{rep:?}");
        assert_eq!(
            std::fs::read_to_string(d.path().join("f.txt")).unwrap(),
            "a\nb\nc\n"
        );
        r.dry_run = false;
        let rep = apply(&r);
        assert!(rep.ok && rep.written, "{rep:?}");
        assert_eq!(
            std::fs::read_to_string(d.path().join("f.txt")).unwrap(),
            "a\nB\nc\n"
        );
        assert_eq!(rep.files[0].path, "f.txt");
        // reversed, the file is back; nothing is left over
        r.reverse = true;
        assert!(apply(&r).ok);
        assert_eq!(
            std::fs::read_to_string(d.path().join("f.txt")).unwrap(),
            "a\nb\nc\n"
        );
        let names: Vec<_> = std::fs::read_dir(d.path()).unwrap().collect();
        assert_eq!(names.len(), 1);
    }

    #[test]
    fn nothing_is_written_when_one_file_fails() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("f.txt"), "a\nb\nc\n").unwrap();
        std::fs::write(d.path().join("g.txt"), "other\n").unwrap();
        let two = format!(
            "{P}diff --git a/g.txt b/g.txt\n--- a/g.txt\n+++ b/g.txt\n@@ -1 +1 @@\n-nope\n+yes\n"
        );
        let rep = apply(&req(d.path(), &two));
        assert!(!rep.ok && !rep.written);
        assert_eq!(
            std::fs::read_to_string(d.path().join("f.txt")).unwrap(),
            "a\nb\nc\n"
        );
        assert!(rep.files[1].message.contains("does not match"));
        // with `partial` the good file is written
        let mut r = req(d.path(), &two);
        r.partial = true;
        let rep = apply(&r);
        assert!(!rep.ok && rep.written);
        assert_eq!(
            std::fs::read_to_string(d.path().join("f.txt")).unwrap(),
            "a\nB\nc\n"
        );
        assert_eq!(
            std::fs::read_to_string(d.path().join("g.txt")).unwrap(),
            "other\n"
        );
    }

    #[test]
    fn create_delete_and_rename() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("old.txt"), "x\n").unwrap();
        std::fs::write(d.path().join("gone.txt"), "bye\n").unwrap();
        let p = "diff --git a/new/n.txt b/new/n.txt\nnew file mode 100644\n--- /dev/null\n+++ b/new/n.txt\n@@ -0,0 +1 @@\n+hello\ndiff --git a/gone.txt b/gone.txt\ndeleted file mode 100644\n--- a/gone.txt\n+++ /dev/null\n@@ -1 +0,0 @@\n-bye\ndiff --git a/old.txt b/moved.txt\nsimilarity index 100%\nrename from old.txt\nrename to moved.txt\n";
        let rep = apply(&req(d.path(), p));
        assert!(rep.ok && rep.written, "{rep:?}");
        assert_eq!(
            std::fs::read_to_string(d.path().join("new/n.txt")).unwrap(),
            "hello\n"
        );
        assert!(!d.path().join("gone.txt").exists() && !d.path().join("old.txt").exists());
        assert_eq!(
            std::fs::read_to_string(d.path().join("moved.txt")).unwrap(),
            "x\n"
        );
        // creating a file that exists is refused
        let again = apply(&req(d.path(), p.split("diff --git a/gone").next().unwrap()));
        assert!(!again.ok && again.files[0].message.contains("already exists"));
    }

    #[test]
    fn paths_that_leave_the_folder_are_refused() {
        let d = tempfile::tempdir().unwrap();
        for bad in ["../x.txt", "/etc/x", "C:/x.txt", "a/../../x"] {
            let p = format!("--- {bad}\n+++ {bad}\n@@ -0,0 +1 @@\n+x\n");
            let mut r = req(d.path(), &p);
            r.strip = Some(0);
            let rep = apply(&r);
            assert!(!rep.ok && !rep.written, "{bad}: {rep:?}");
        }
        assert!(!d.path().parent().unwrap().join("x.txt").exists());
    }

    #[test]
    fn a_text_is_patched_in_memory() {
        let r = ApplyRequest {
            patch: P.into(),
            text: Some("a\nb\nc\n".into()),
            ..ApplyRequest::default()
        };
        let rep = apply(&r);
        assert!(rep.ok && !rep.written);
        assert_eq!(rep.files[0].text.as_deref(), Some("a\nB\nc\n"));
    }

    #[test]
    fn search_replace_json_and_byte_scripts_apply_to_files() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join("app.py"),
            "def main():\n    x = 1\n    print(x)\n",
        )
        .unwrap();
        let blk = "app.py\n<<<<<<< SEARCH\n    x = 1\n=======\n    x = 2\n>>>>>>> REPLACE\n";
        let rep = apply(&req(d.path(), blk));
        assert!(rep.ok && rep.written, "{rep:?}");
        assert_eq!(rep.format, "searchReplace");
        assert_eq!(
            std::fs::read_to_string(d.path().join("app.py")).unwrap(),
            "def main():\n    x = 2\n    print(x)\n"
        );
        // not found: nothing changes and the message says so
        let rep = apply(&req(d.path(), blk));
        assert!(
            !rep.ok && rep.files[0].message.contains("not found"),
            "{rep:?}"
        );

        std::fs::write(
            d.path().join("c.json"),
            "{\"config\": {\"x\": 1}, \"keep\": [1, 2]}",
        )
        .unwrap();
        let mut r = req(
            d.path(),
            r#"[{"op":"replace","path":"/config/x","value":2}]"#,
        );
        r.target = "c.json".into();
        let rep = apply(&r);
        assert!(rep.ok && rep.written, "{rep:?}");
        assert_eq!(rep.format, "jsonPatch");
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(d.path().join("c.json")).unwrap())
                .unwrap();
        assert_eq!(v["config"]["x"], 2);

        std::fs::write(d.path().join("a.bin"), [0u8, 1, 0x74, 3]).unwrap();
        let mut r = req(d.path(), "00000002:74->75\n");
        r.target = "a.bin".into();
        let rep = apply(&r);
        assert!(
            rep.ok && rep.written && rep.format == "byteScript",
            "{rep:?}"
        );
        assert_eq!(
            std::fs::read(d.path().join("a.bin")).unwrap(),
            [0, 1, 0x75, 3]
        );
        assert!(!apply(&r).ok);
    }

    #[cfg(windows)]
    #[test]
    fn a_junction_in_the_path_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let (real, root) = (d.path().join("real"), d.path().join("root"));
        std::fs::create_dir_all(&real).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        let st = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(root.join("link"))
            .arg(&real)
            .output()
            .unwrap();
        if !st.status.success() {
            return; // no junction rights on this machine
        }
        let p = "--- a/link/x.txt\n+++ b/link/x.txt\n@@ -0,0 +1 @@\n+x\n";
        let rep = apply(&req(&root, p));
        assert!(!rep.ok && !real.join("x.txt").exists(), "{rep:?}");
    }
}
