//! Exports for the integration layer (backlog E11.5-S5, ABI 1.1): patch-style diff output, the
//! argument parsers of other programs, ignore lists, the Explorer selection, the three-way merge
//! stub, free disk space, and the navigation helpers of the compare window.
//!
//! The JSON requests and reports are described in `docs/abi.md`. A diff that finds a difference or
//! cannot read a file is still a successful *call*: the report carries the GNU exit code (0, 1, 2)
//! and the text for standard error. A malformed request is `DS_E_INVALID_ARG`.
#![allow(unsafe_code)]

use core_diff::session::{Dir, SectionKind};
use core_diff::session::{FindOptions, Section};
use core_glob::IgnoreList;
use core_tools::{
    Dialect, RunOptions, SelectionStore, merge_files, merge3, parse_diff_args, parse_invocation,
    run, run_texts,
};
use serde::Deserialize;
use serde_json::json;

use crate::registry;
use crate::status::{DS_E_NOT_FOUND, DsStatus, Fail, Res, ffi_entry};
use crate::types::*;
use crate::util::{json_arg, os_path, out, to_buf, units};

fn parse<T: for<'de> Deserialize<'de>>(s: DsStr8) -> Res<T> {
    // SAFETY: the ABI contract covers the view.
    let text = unsafe { json_arg(s) }?;
    serde_json::from_str(text).map_err(|e| Fail::invalid(format!("bad request: {e}")))
}

fn dialect_of(s: Option<&str>) -> Dialect {
    if s.is_some_and(|d| d.eq_ignore_ascii_case("git")) {
        Dialect::Git
    } else {
        Dialect::Gnu
    }
}

fn run_options(
    ignore: Option<&str>,
    lock_files: bool,
    max_file_bytes: Option<u64>,
) -> Res<RunOptions> {
    let mut ro = RunOptions::default();
    if let Some(m) = max_file_bytes {
        ro.max_file_bytes = m;
    }
    let list = match (ignore, lock_files) {
        (Some(l), _) if !l.trim().is_empty() => Some(IgnoreList::parse(l).map_err(Fail::invalid)?),
        (_, true) => Some(IgnoreList::lock_files()),
        _ => None,
    };
    ro.ignore = list;
    Ok(ro)
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct DiffRunRequest {
    /// The arguments after the program name, as the command line has them.
    args: Vec<String>,
    /// `"gnu"` (default) or `"git"`.
    dialect: Option<String>,
    /// A pattern list of files to skip.
    ignore: Option<String>,
    /// Skip the lock files of the built-in preset.
    ignore_lock_files: bool,
    max_file_bytes: Option<u64>,
    /// Texts instead of files (`ds_diff_texts`).
    left: Option<String>,
    right: Option<String>,
    left_name: Option<String>,
    right_name: Option<String>,
}

fn arg_error(msg: &str) -> core_tools::DiffOutput {
    core_tools::DiffOutput {
        stderr: format!("diff: {msg}\n"),
        exit_code: 2,
        json: json!({"exitCode": 2, "error": msg}).to_string(),
        ..core_tools::DiffOutput::default()
    }
}

/// Runs a `diff` or `git diff` command line on the local file system and reports what it would
/// print: JSON `{stdout, stderr, exitCode, json, ignored}` (`json` is the machine-readable form of
/// the comparison, itself a JSON document in a string). Request:
/// `{"args": ["-ru", "a", "b"], "dialect": "gnu", "ignore": "*.lock", "ignoreLockFiles": false}`.
/// Synchronous: a folder diff of a large tree takes as long as it takes, so call it off the UI thread.
///
/// # Safety
/// `request` must be valid for its length; `out_report` for a write (free with `ds_buf_free`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ds_diff_run(request: DsStr8, out_report: *mut DsBuf) -> DsStatus {
    ffi_entry!({
        let req: DiffRunRequest = parse(request)?;
        // SAFETY: the ABI contract covers the out pointer.
        let o = unsafe { out(out_report) }?;
        let ro = run_options(
            req.ignore.as_deref(),
            req.ignore_lock_files,
            req.max_file_bytes,
        )?;
        let result = match parse_diff_args(&req.args, dialect_of(req.dialect.as_deref())) {
            Ok(r) => run(&r, &ro),
            Err(e) => arg_error(&e.0),
        };
        *o = to_buf(serde_json::to_vec(&result).map_err(|e| Fail::invalid(e.to_string()))?);
        Ok(())
    })
}

/// Like [`ds_diff_run`] for two texts that are not files (a UI buffer, an MCP tool argument): the
/// request has `left` and `right` as text, `leftName` and `rightName` for the headers, and `args`
/// for the flags only (no operands).
///
/// # Safety
/// As `ds_diff_run`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ds_diff_texts(request: DsStr8, out_report: *mut DsBuf) -> DsStatus {
    ffi_entry!({
        let req: DiffRunRequest = parse(request)?;
        // SAFETY: the ABI contract covers the out pointer.
        let o = unsafe { out(out_report) }?;
        let (l, r) = (
            req.left_name.clone().unwrap_or_else(|| "a".into()),
            req.right_name.clone().unwrap_or_else(|| "b".into()),
        );
        let mut args = req.args.clone();
        args.extend([l, r]);
        let ro = run_options(
            req.ignore.as_deref(),
            req.ignore_lock_files,
            req.max_file_bytes,
        )?;
        let result = match parse_diff_args(&args, dialect_of(req.dialect.as_deref())) {
            Ok(r) => run_texts(
                &r,
                req.left.as_deref().unwrap_or(""),
                req.right.as_deref().unwrap_or(""),
                &ro,
            ),
            Err(e) => arg_error(&e.0),
        };
        *o = to_buf(serde_json::to_vec(&result).map_err(|e| Fail::invalid(e.to_string()))?);
        Ok(())
    })
}

/// Parses how another program started the tool: a JSON array of the arguments (after the program
/// name). The report is `{invocation: {kind, value}, unknown: [...]}` with `kind` one of `compare`,
/// `merge`, `gitExternalDiff`, `selectLeft`, `compareSelected`. A command line the protocol does not
/// understand is `DS_E_INVALID_ARG` with the reason in `ds_last_error`.
///
/// # Safety
/// `args_json` must be valid for its length; `out_report` for a write.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ds_tool_parse_args(args_json: DsStr8, out_report: *mut DsBuf) -> DsStatus {
    ffi_entry!({
        let args: Vec<String> = parse(args_json)?;
        // SAFETY: the ABI contract covers the out pointer.
        let o = unsafe { out(out_report) }?;
        let parsed = parse_invocation(&args).map_err(|e| Fail::invalid(e.0))?;
        *o = to_buf(serde_json::to_vec(&parsed).map_err(|e| Fail::invalid(e.to_string()))?);
        Ok(())
    })
}

/// The built-in ignore list (`*package-lock.json,*gemfile.lock,*podfile.lock,*cartfile.resolved`)
/// as UTF-8 text.
///
/// # Safety
/// `out_text` must be valid for a write.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ds_tool_default_ignore(out_text: *mut DsBuf) -> DsStatus {
    ffi_entry!({
        // SAFETY: the ABI contract covers the out pointer.
        let o = unsafe { out(out_text) }?;
        *o = to_buf(core_glob::LOCK_FILE_PATTERNS.as_bytes().to_vec());
        Ok(())
    })
}

/// Whether `path` is matched by the pattern list (comma, semicolon or line separated; case is
/// ignored on Windows). `*out_ignored` is 1 or 0.
///
/// # Safety
/// The views must be valid for their lengths; `out_ignored` for a write.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ds_tool_is_ignored(
    patterns: DsStr8,
    path: DsStr16,
    out_ignored: *mut u8,
) -> DsStatus {
    ffi_entry!({
        // SAFETY: the ABI contract covers every pointer argument.
        let (pat, p, o) = unsafe { (json_arg(patterns)?, units(path)?, out(out_ignored)?) };
        let list = IgnoreList::parse(pat).map_err(Fail::invalid)?;
        *o = u8::from(list.is_ignored(&String::from_utf16_lossy(p)));
        Ok(())
    })
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Merge3Request {
    base: String,
    local: String,
    remote: String,
}

/// Three-way merge of three texts. **A stub** (docs/abi.md): when only one side changed, or both
/// made the same change, the report is `{"outcome":"clean","text":...,"reason":...}`; otherwise
/// `{"outcome":"conflict","reason":...}` and the interactive merge has to decide.
///
/// # Safety
/// `request` must be valid for its length; `out_report` for a write.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ds_merge3(request: DsStr8, out_report: *mut DsBuf) -> DsStatus {
    ffi_entry!({
        let r: Merge3Request = parse(request)?;
        // SAFETY: the ABI contract covers the out pointer.
        let o = unsafe { out(out_report) }?;
        let outcome = merge3(&r.base, &r.local, &r.remote);
        *o = to_buf(serde_json::to_vec(&outcome).map_err(|e| Fail::invalid(e.to_string()))?);
        Ok(())
    })
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct MergeFilesRequest {
    local: String,
    remote: String,
    base: Option<String>,
    output: String,
}

/// The merge-tool contract of `git mergetool` and SourceTree on files: writes `output` and reports
/// `{"exitCode":0}` when merged, `{"exitCode":1,"message":...}` when not (the stub of `ds_merge3`
/// applies), `2` for trouble. Request: `{"local","remote","base","output"}` (absolute paths).
///
/// # Safety
/// `request` must be valid for its length; `out_report` for a write.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ds_merge_files(request: DsStr8, out_report: *mut DsBuf) -> DsStatus {
    ffi_entry!({
        let r: MergeFilesRequest = parse(request)?;
        // SAFETY: the ABI contract covers the out pointer.
        let o = unsafe { out(out_report) }?;
        let res = merge_files(
            std::path::Path::new(&r.local),
            std::path::Path::new(&r.remote),
            r.base.as_deref().map(std::path::Path::new),
            std::path::Path::new(&r.output),
        );
        *o = to_buf(
            json!({"exitCode": res.exit_code, "message": res.message})
                .to_string()
                .into_bytes(),
        );
        Ok(())
    })
}

fn store() -> Res<SelectionStore> {
    SelectionStore::for_current_user().ok_or_else(|| {
        Fail::new(
            crate::status::DS_E_UNSUPPORTED,
            "no local data folder for the selection",
        )
    })
}

/// Remembers `path` as the left side of the next compare (the Explorer "Select left side").
///
/// # Safety
/// `path` must be valid for its length.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ds_shell_selection_set(path: DsStr16) -> DsStatus {
    ffi_entry!({
        // SAFETY: the ABI contract covers the view.
        let p = unsafe { units(path) }?;
        let path = os_path(p);
        if !path.is_absolute() {
            return Err(Fail::invalid("the selection must be an absolute path"));
        }
        let s = path
            .to_str()
            .ok_or_else(|| Fail::invalid("the path is not valid Unicode"))?;
        store()?
            .set_left(s)
            .map_err(|e| Fail::new(crate::status::DS_E_IO, e.to_string()))?;
        Ok(())
    })
}

/// The remembered left side as JSON `{"left": path, "chosenAt": seconds}`, or an empty buffer when
/// there is none (never chosen, too old, or the path no longer exists).
///
/// # Safety
/// `out_selection` must be valid for a write.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ds_shell_selection_get(out_selection: *mut DsBuf) -> DsStatus {
    ffi_entry!({
        // SAFETY: the ABI contract covers the out pointer.
        let o = unsafe { out(out_selection) }?;
        *o = match store()?.left() {
            Some(s) => to_buf(serde_json::to_vec(&s).map_err(|e| Fail::invalid(e.to_string()))?),
            None => to_buf(Vec::new()),
        };
        Ok(())
    })
}

/// Forgets the remembered left side.
#[unsafe(no_mangle)]
pub extern "C" fn ds_shell_selection_clear() -> DsStatus {
    ffi_entry!({
        store()?.clear();
        Ok(())
    })
}

/// Free and total bytes of the volume that holds `path` (the status bar of a folder compare).
/// Windows only for now (`DS_E_UNSUPPORTED` elsewhere).
///
/// # Safety
/// `path` must be valid for its length; `out_info` for a write.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ds_volume_info(path: DsStr16, out_info: *mut DsVolumeInfo) -> DsStatus {
    ffi_entry!({
        // SAFETY: the ABI contract covers both pointers.
        let (p, o) = unsafe { (units(path)?, out(out_info)?) };
        let (free, total) = core_vfs::volume_space(&os_path(p))?;
        *o = DsVolumeInfo {
            free_bytes: free,
            total_bytes: total,
        };
        Ok(())
    })
}

/// The runs of rows the *Context* view hides: rows farther than `context` rows from a difference
/// (`important_only` != 0: from an important one). Copies folds `first..first + count` into
/// `out_folds`; `*total` is the number of folds. A file without differences folds completely.
///
/// # Safety
/// `out_folds` must be valid for `count` entries (or null with `count` 0); `written` and `total`
/// for writes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ds_text_get_folds(
    h: DsHandle,
    context: u32,
    important_only: u8,
    first: u32,
    count: u32,
    out_folds: *mut DsFold,
    written: *mut u32,
    total: *mut u32,
) -> DsStatus {
    ffi_entry!({
        let t = registry::text(h)?;
        // SAFETY: the ABI contract covers the buffers.
        let (w, tot) = unsafe { (out(written)?, out(total)?) };
        let folds = t.tc.folds(context, important_only != 0);
        *tot = folds.len() as u32;
        let slice = folds.get(first as usize..).unwrap_or(&[]);
        let n = slice.len().min(count as usize);
        if n > 0 {
            if out_folds.is_null() {
                return Err(Fail::invalid("a null buffer with a non-zero count"));
            }
            // SAFETY: non-null; validity for `count >= n` writes is the caller's contract.
            let dst = unsafe { std::slice::from_raw_parts_mut(out_folds, n) };
            for (d, f) in dst.iter_mut().zip(slice) {
                *d = DsFold {
                    first_row: f.first_row,
                    row_count: f.row_count,
                };
            }
        }
        *w = n as u32;
        Ok(())
    })
}

/// The kind of sections `first..first + count`: 0 insert (lines only on the right), 1 delete (lines
/// only on the left), 2 replace (changed lines). One byte each.
///
/// # Safety
/// `out_kinds` must be valid for `count` bytes (or null with `count` 0); `written` for a write.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ds_text_get_section_kinds(
    h: DsHandle,
    first: u32,
    count: u32,
    out_kinds: *mut u8,
    written: *mut u32,
) -> DsStatus {
    ffi_entry!({
        let t = registry::text(h)?;
        // SAFETY: the ABI contract covers the buffers.
        let w = unsafe { out(written)? };
        let count = count.min(t.tc.stats().sections.saturating_sub(first));
        let mut secs = vec![
            Section {
                first_row: 0,
                row_count: 0,
                important: false
            };
            count as usize
        ];
        let n = t.tc.sections(first, &mut secs);
        if n > 0 {
            if out_kinds.is_null() {
                return Err(Fail::invalid("a null buffer with a non-zero count"));
            }
            // SAFETY: non-null; validity for `count >= n` writes is the caller's contract.
            let dst = unsafe { std::slice::from_raw_parts_mut(out_kinds, n) };
            for (d, s) in dst.iter_mut().zip(&secs[..n]) {
                *d = match t.tc.section_kind(s) {
                    SectionKind::Insert => 0,
                    SectionKind::Delete => 1,
                    SectionKind::Replace => 2,
                };
            }
        }
        *w = n as u32;
        Ok(())
    })
}

/// The first row of the next (`dir` 0) or previous (`dir` 1) section of `kind` (see
/// `ds_text_get_section_kinds`), counting from `from_row` inclusive. `DS_E_NOT_FOUND` at the ends.
///
/// # Safety
/// `out_row` must be valid for a write.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ds_text_find_section(
    h: DsHandle,
    from_row: u32,
    dir: u8,
    kind: u8,
    out_row: *mut u32,
) -> DsStatus {
    ffi_entry!({
        let t = registry::text(h)?;
        // SAFETY: the ABI contract covers the out pointer.
        let o = unsafe { out(out_row) }?;
        let kind = match kind {
            0 => SectionKind::Insert,
            1 => SectionKind::Delete,
            2 => SectionKind::Replace,
            _ => {
                return Err(Fail::invalid(
                    "kind must be 0 (insert), 1 (delete) or 2 (replace)",
                ));
            }
        };
        let dir = if dir == 0 { Dir::Next } else { Dir::Prev };
        match t.tc.find_section_of_kind(from_row, dir, kind) {
            Some(r) => {
                *o = r;
                Ok(())
            }
            None => Err(Fail::new(DS_E_NOT_FOUND, "no such section")),
        }
    })
}

/// Finds `query` (UTF-8) in the lines of the rows from `from_row` inclusive, forwards (`dir` 0) or
/// backwards (`dir` 1). `flags`: 1 case sensitive, 2 whole word, 4 regular expression, 8 skip the
/// left side, 16 skip the right side. `DS_E_NOT_FOUND` when there is no match; a bad regular
/// expression is `DS_E_INVALID_ARG`.
///
/// # Safety
/// `query` must be valid for its length; `out_hit` for a write.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ds_text_find(
    h: DsHandle,
    query: DsStr8,
    flags: u32,
    from_row: u32,
    dir: u8,
    out_hit: *mut DsFindHit,
) -> DsStatus {
    ffi_entry!({
        let t = registry::text(h)?;
        // SAFETY: the ABI contract covers both pointers.
        let (q, o) = unsafe { (json_arg(query)?, out(out_hit)?) };
        let opts = FindOptions {
            case_sensitive: flags & 1 != 0,
            whole_word: flags & 2 != 0,
            regex: flags & 4 != 0,
            left: flags & 8 == 0,
            right: flags & 16 == 0,
        };
        let dir = if dir == 0 { Dir::Next } else { Dir::Prev };
        match t.tc.find(q, &opts, from_row, dir)? {
            Some(hit) => {
                *o = DsFindHit {
                    row: hit.row,
                    line: hit.line,
                    start: hit.start,
                    end: hit.end,
                    side: u8::from(!hit.left_side),
                    pad: [0; 3],
                };
                Ok(())
            }
            None => Err(Fail::new(DS_E_NOT_FOUND, "no match")),
        }
    })
}

// ---------------------------------------------------------------- patch formats (ABI 1.2)

/// Applies a patch (backlog E11.5-S16): a unified diff or git mailbox patch, SEARCH/REPLACE blocks,
/// JSON Patch (RFC 6902) or a byte script, to the files under `root`, or to the text given as
/// `text`. All or nothing unless `partial`; `dryRun` writes nothing. The request is
/// [`core_tools::apply::ApplyRequest`] (`patch`, `format`, `root`, `target`, `text`, `strip`,
/// `reverse`, `dryRun`, `fuzz`, `ignoreWhitespace`, `partial`, `maxFileBytes`); the report has `ok`,
/// `written`, `format`, `files[]` (per file the hunks, blocks or operations with their verdicts)
/// and `error`. A patch that does not apply is a successful call with `ok: false`.
///
/// # Safety
/// `request` must be valid for its length; `out_report` for a write (free with `ds_buf_free`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ds_patch_apply(request: DsStr8, out_report: *mut DsBuf) -> DsStatus {
    ffi_entry!({
        let req: core_tools::apply::ApplyRequest = parse(request)?;
        // SAFETY: the ABI contract covers the out pointer.
        let o = unsafe { out(out_report) }?;
        let rep = core_tools::apply::apply(&req);
        *o = to_buf(serde_json::to_vec(&rep).map_err(|e| Fail::invalid(e.to_string()))?);
        Ok(())
    })
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "camelCase")]
enum JsonPatchRequest {
    /// The operations that turn `left` into `right`.
    Diff {
        left: serde_json::Value,
        right: serde_json::Value,
    },
    /// `doc` with `patch` applied.
    Apply {
        doc: serde_json::Value,
        patch: Vec<serde_json::Value>,
    },
}

/// JSON Patch (RFC 6902) on documents given in the request: `{"op":"diff","left":..,"right":..}`
/// answers `{"ok":true,"patch":[..]}`, `{"op":"apply","doc":..,"patch":[..]}` answers
/// `{"ok":true,"doc":..}` or `{"ok":false,"error":"operation 2: ...","index":2}`.
///
/// # Safety
/// `request` must be valid for its length; `out_report` for a write (free with `ds_buf_free`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ds_json_patch(request: DsStr8, out_report: *mut DsBuf) -> DsStatus {
    ffi_entry!({
        let req: JsonPatchRequest = parse(request)?;
        // SAFETY: the ABI contract covers the out pointer.
        let o = unsafe { out(out_report) }?;
        let v = match req {
            JsonPatchRequest::Diff { left, right } => {
                json!({"ok": true, "patch": core_tools::jsonpatch::diff(&left, &right)})
            }
            JsonPatchRequest::Apply { doc, patch } => {
                match core_tools::jsonpatch::apply(&doc, &patch) {
                    Ok(d) => json!({"ok": true, "doc": d}),
                    Err(e) => json!({"ok": false, "error": e.to_string(), "index": e.index}),
                }
            }
        };
        *o = to_buf(serde_json::to_vec(&v).map_err(|e| Fail::invalid(e.to_string()))?);
        Ok(())
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct BytesDiffRequest {
    left: String,
    right: String,
    /// `fc` (default), `cmp` or `script`.
    format: String,
    max_runs: usize,
    max_file_bytes: u64,
}

impl Default for BytesDiffRequest {
    fn default() -> Self {
        BytesDiffRequest {
            left: String::new(),
            right: String::new(),
            format: "fc".into(),
            max_runs: 10_000,
            max_file_bytes: 512 << 20,
        }
    }
}

/// Which bytes differ between two files (the report for a binary file that a line diff calls
/// "Binary files differ"): `{identical, leftLen, rightLen, differingBytes, runs[{offset, left,
/// right}], truncated, text}`, where `text` is `fc /b`, `cmp -l` or offset-script text (the script
/// can be applied with `ds_patch_apply`, format `byteScript`). An unreadable file is a failed call.
///
/// # Safety
/// `request` must be valid for its length; `out_report` for a write (free with `ds_buf_free`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ds_bytes_diff(request: DsStr8, out_report: *mut DsBuf) -> DsStatus {
    ffi_entry!({
        let req: BytesDiffRequest = parse(request)?;
        // SAFETY: the ABI contract covers the out pointer.
        let o = unsafe { out(out_report) }?;
        let read = |p: &str| -> Res<Vec<u8>> {
            let path = std::path::Path::new(p);
            if !path.is_absolute() {
                return Err(Fail::invalid("file paths must be absolute"));
            }
            let md = std::fs::metadata(path)
                .map_err(|e| Fail::from(core_base::Error::from_io(&e, path)))?;
            if md.len() > req.max_file_bytes {
                return Err(Fail::invalid(format!("{p} is larger than the size limit")));
            }
            std::fs::read(path).map_err(|e| Fail::from(core_base::Error::from_io(&e, path)))
        };
        let (a, b) = (read(&req.left)?, read(&req.right)?);
        let d = core_tools::bindiff::compare(&a, &b, req.max_runs);
        let fmt = match req.format.as_str() {
            "fc" | "" => core_tools::bindiff::ByteFormat::Fc,
            "cmp" => core_tools::bindiff::ByteFormat::CmpList,
            "script" => core_tools::bindiff::ByteFormat::Script,
            other => return Err(Fail::invalid(format!("unknown byte diff format '{other}'"))),
        };
        let text = core_tools::bindiff::render(&d, fmt, &req.left, &req.right);
        let mut v = serde_json::to_value(&d).map_err(|e| Fail::invalid(e.to_string()))?;
        v["identical"] = json!(d.identical());
        v["text"] = json!(text);
        *o = to_buf(serde_json::to_vec(&v).map_err(|e| Fail::invalid(e.to_string()))?);
        Ok(())
    })
}
