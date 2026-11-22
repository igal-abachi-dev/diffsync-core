//! Editing exports (backlog E11.5-S14, ABI 1.2): copy a line, a section or the whole file from one
//! side of a text compare to the other, edit lines, undo and redo, convert the encoding and line
//! endings, and save. Plus the folder-side items: ignore marks and rename.
//!
//! One JSON request per call, one JSON report back; the rules are in `docs/abi.md`. Every edit
//! restarts the compare on the new text under the same handle: the status shows `Aligning` and the
//! generation changes, and the host pulls rows again as it does after a recompare.
#![allow(unsafe_code)]

use std::path::PathBuf;

use core_base::Ticks;
use core_diff::Side;
use core_diff::edit::{DocState, EditDoc, EditOutcome, Editor, EolKind, RangeEdit};
use core_sync::{SaveOptions, save_file, save_in_place};
use core_text::{DecodeOptions, TextEncoding};
use serde::{Deserialize, Serialize};

use crate::registry;
use crate::status::{DsStatus, Fail, Res, ffi_entry};
use crate::text::TextObj;
use crate::types::*;
use crate::util::{json_arg, out, to_buf};

/// The editor of a session and what each side was loaded from.
#[derive(Debug)]
pub struct EditState {
    editor: Editor,
    /// Path, size and modification time of each side's file when it was read (or last saved).
    sources: [Option<(PathBuf, u64, Ticks)>; 2],
}

fn source_of(t: &TextObj, side: Side) -> Option<(PathBuf, u64, Ticks)> {
    let info = t.tc.doc_info(side)?;
    let path = info.source.path?;
    let mtime = info
        .source
        .mtime
        .map_or(0, core_base::ticks::ticks_from_system_time);
    Some((path, info.source.size, mtime))
}

impl EditState {
    fn begin(t: &TextObj) -> Res<EditState> {
        let sources = [source_of(t, Side::Left), source_of(t, Side::Right)];
        Ok(EditState {
            editor: Editor::begin(t.tc.clone())?,
            sources,
        })
    }
}

fn yes() -> bool {
    true
}

#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
enum SideName {
    Left,
    Right,
}

impl SideName {
    fn side(self) -> Side {
        match self {
            SideName::Left => Side::Left,
            SideName::Right => Side::Right,
        }
    }

    fn index(self) -> usize {
        usize::from(matches!(self, SideName::Right))
    }
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "camelCase", rename_all_fields = "camelCase")]
enum EditRequest {
    CopyRows {
        from: SideName,
        first_row: u32,
        last_row: u32,
    },
    CopySection {
        from: SideName,
        section: u32,
    },
    CopyAll {
        from: SideName,
        #[serde(default)]
        with_format: bool,
    },
    ReplaceLines {
        side: SideName,
        first_line: usize,
        remove_lines: usize,
        text: String,
        #[serde(default = "yes")]
        refresh: bool,
    },
    /// One edit of the host's text buffer: `startLine`, `startColumn`, `endLine`, `endColumn`
    /// (0-based lines, UTF-16 columns) and `text`.
    ReplaceRange {
        side: SideName,
        start_line: usize,
        start_column: usize,
        end_line: usize,
        end_column: usize,
        text: String,
        #[serde(default = "yes")]
        refresh: bool,
    },
    /// A batch of range edits that all refer to the current text; one undo step.
    ApplyEdits {
        side: SideName,
        edits: Vec<RangeEdit>,
        #[serde(default = "yes")]
        refresh: bool,
    },
    /// The host's whole buffer, for a side that fell behind.
    SetText {
        side: SideName,
        text: String,
        #[serde(default = "yes")]
        refresh: bool,
    },
    /// Restarts the compare on the current text (after edits made with `refresh: false`).
    Refresh,
    Undo {
        side: SideName,
    },
    Redo {
        side: SideName,
    },
    Convert {
        side: SideName,
        eol: Option<EolKind>,
        encoding: Option<String>,
    },
    Reinterpret {
        side: SideName,
        encoding: String,
        #[serde(default)]
        discard_changes: bool,
    },
    State,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct EditReport {
    changed: bool,
    removed: u32,
    inserted: u32,
    /// The compare shows older text than the documents hold: edits were made with
    /// `refresh: false`; send `{"op":"refresh"}` (copy operations do it themselves).
    stale: bool,
    generation: u64,
    left: DocState,
    right: DocState,
}

fn encoding_of(label: &str) -> Res<TextEncoding> {
    TextEncoding::parse(label)
        .ok_or_else(|| Fail::invalid(format!("'{label}' is not a known text encoding")))
}

fn report(t: &TextObj, e: &EditState, o: Option<EditOutcome>, changed: bool) -> EditReport {
    let [left, right] = e.editor.states();
    let o = o.unwrap_or(EditOutcome {
        changed,
        removed: 0,
        inserted: 0,
    });
    EditReport {
        changed: o.changed || changed,
        removed: o.removed,
        inserted: o.inserted,
        stale: e.editor.is_stale(),
        generation: t.tc.generation(),
        left,
        right,
    }
}

fn with_edit<R>(t: &TextObj, f: impl FnOnce(&mut EditState) -> Res<R>) -> Res<R> {
    let mut g = t.edit.lock();
    if g.is_none() {
        *g = Some(EditState::begin(t)?);
    }
    match g.as_mut() {
        Some(e) => f(e),
        None => Err(Fail::invalid("no editor")),
    }
}

/// One edit: copy rows, a section or the whole text between the sides, replace lines, undo, redo,
/// convert encoding or line endings, read a side again in another encoding, or ask for the state.
/// The report holds `changed`, the lines `removed` and `inserted`, the compare's `generation` and
/// the state of both sides (`dirty`, `canUndo`, `canRedo`, `encoding`, `bom`, `endingCounts`,
/// `mixedEndings`, `lossy`).
///
/// # Safety
/// `request` must be valid for its length; `out_report` for a write (free with `ds_buf_free`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ds_text_edit(
    session: DsHandle,
    request: DsStr8,
    out_report: *mut DsBuf,
) -> DsStatus {
    ffi_entry!({
        let t = registry::text(session)?;
        // SAFETY: the ABI contract covers both pointers.
        let (json, o) = unsafe { (json_arg(request)?, out(out_report)?) };
        let req: EditRequest = serde_json::from_str(json)
            .map_err(|e| Fail::invalid(format!("bad edit request: {e}")))?;
        let rep = with_edit(&t, |e| {
            let ed = &mut e.editor;
            let (outcome, changed) = match req {
                EditRequest::CopyRows {
                    from,
                    first_row,
                    last_row,
                } => (Some(ed.copy_rows(from.side(), first_row, last_row)?), false),
                EditRequest::CopySection { from, section } => {
                    (Some(ed.copy_section(from.side(), section)?), false)
                }
                EditRequest::CopyAll { from, with_format } => {
                    (Some(ed.copy_all(from.side(), with_format)?), false)
                }
                EditRequest::ReplaceLines {
                    side,
                    first_line,
                    remove_lines,
                    text,
                    refresh,
                } => (
                    Some(ed.replace_lines(
                        side.side(),
                        first_line,
                        remove_lines,
                        &text,
                        refresh,
                    )?),
                    false,
                ),
                EditRequest::ReplaceRange {
                    side,
                    start_line,
                    start_column,
                    end_line,
                    end_column,
                    text,
                    refresh,
                } => {
                    let edit = RangeEdit {
                        start_line,
                        start_column,
                        end_line,
                        end_column,
                        text,
                    };
                    (Some(ed.replace_range(side.side(), &edit, refresh)?), false)
                }
                EditRequest::ApplyEdits {
                    side,
                    edits,
                    refresh,
                } => (Some(ed.apply_edits(side.side(), &edits, refresh)?), false),
                EditRequest::SetText {
                    side,
                    text,
                    refresh,
                } => (Some(ed.set_text(side.side(), &text, refresh)?), false),
                EditRequest::Refresh => {
                    let was = ed.is_stale();
                    ed.refresh()?;
                    (None, was)
                }
                EditRequest::Undo { side } => (None, ed.undo(side.side())?),
                EditRequest::Redo { side } => (None, ed.redo(side.side())?),
                EditRequest::Convert {
                    side,
                    eol,
                    encoding,
                } => {
                    let enc = encoding.as_deref().map(encoding_of).transpose()?;
                    (None, ed.convert(side.side(), eol, enc)?)
                }
                EditRequest::Reinterpret {
                    side,
                    encoding,
                    discard_changes,
                } => {
                    let enc = encoding_of(&encoding)?;
                    if ed.doc(side.side()).is_dirty() && !discard_changes {
                        return Err(Fail::invalid(
                            "the side has unsaved changes; reading it again in another encoding drops them (pass discardChanges)",
                        ));
                    }
                    let Some((path, _, _)) = e.sources[side.index()].clone() else {
                        return Err(Fail::invalid("this side did not come from a file"));
                    };
                    let max = 512u64 << 20;
                    let md = std::fs::metadata(&path)
                        .map_err(|x| Fail::from(core_base::Error::from_io(&x, &path)))?;
                    if md.len() > max {
                        return Err(Fail::invalid("the file is too large to read again"));
                    }
                    let bytes = std::fs::read(&path)
                        .map_err(|x| Fail::from(core_base::Error::from_io(&x, &path)))?;
                    let doc = EditDoc::from_bytes(bytes, &DecodeOptions::default(), Some(enc))?;
                    ed.reinterpret(side.side(), doc)?;
                    e.sources[side.index()] = Some((
                        path,
                        md.len(),
                        md.modified()
                            .map_or(0, core_base::ticks::ticks_from_system_time),
                    ));
                    (None, true)
                }
                EditRequest::State => (None, false),
            };
            Ok(report(&t, e, outcome, changed))
        })?;
        *o = to_buf(serde_json::to_vec(&rep).map_err(|e| Fail::invalid(e.to_string()))?);
        Ok(())
    })
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct SaveRequest {
    side: Option<SideName>,
    /// Save As: an absolute path. Default: the file the side was read from.
    path: Option<String>,
    /// `atomic` (default: temporary file and rename) or `inPlace` (write into the existing file,
    /// which keeps alternate data streams, hard links and permissions).
    mode: Option<String>,
    backup_suffix: Option<String>,
    /// Write although the file changed on disk after it was read.
    overwrite_changed: bool,
    write_readonly: bool,
    /// Save As over a different file that exists.
    overwrite_existing: bool,
    /// Convert before saving.
    eol: Option<EolKind>,
    encoding: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SaveReport {
    path: String,
    bytes: u64,
    backup: Option<String>,
    size: u64,
    mtime: Ticks,
    replaced: bool,
    state: DocState,
}

/// Saves one side: the text in its encoding with its byte order mark and line endings, through a
/// temporary file and a rename, after checking that the file is still what was read (decision D63).
/// A refused save changes nothing. Request fields: `side`, `path` (Save As), `mode`,
/// `backupSuffix`, `overwriteChanged`, `writeReadonly`, `overwriteExisting`, `eol`, `encoding`.
///
/// # Safety
/// `request` must be valid for its length; `out_report` for a write (free with `ds_buf_free`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ds_text_save(
    session: DsHandle,
    request: DsStr8,
    out_report: *mut DsBuf,
) -> DsStatus {
    ffi_entry!({
        let t = registry::text(session)?;
        // SAFETY: the ABI contract covers both pointers.
        let (json, o) = unsafe { (json_arg(request)?, out(out_report)?) };
        let req: SaveRequest = serde_json::from_str(json)
            .map_err(|e| Fail::invalid(format!("bad save request: {e}")))?;
        let side = req
            .side
            .ok_or_else(|| Fail::invalid("the request has no `side`"))?;
        let rep = with_edit(&t, |e| {
            if req.eol.is_some() || req.encoding.is_some() {
                let enc = req.encoding.as_deref().map(encoding_of).transpose()?;
                e.editor.convert(side.side(), req.eol, enc)?;
            }
            let bytes = e.editor.doc(side.side()).to_bytes()?;
            let source = e.sources[side.index()].clone();
            let target = match (&req.path, &source) {
                (Some(p), _) => PathBuf::from(p),
                (None, Some((p, _, _))) => p.clone(),
                (None, None) => {
                    return Err(Fail::invalid(
                        "this side has no file name; pass `path` (Save As)",
                    ));
                }
            };
            if !target.is_absolute() {
                return Err(Fail::invalid("the path must be absolute"));
            }
            let same_file = source.as_ref().is_some_and(|(p, _, _)| *p == target);
            if !same_file && target.exists() && !req.overwrite_existing {
                return Err(Fail::new(
                    crate::status::DS_E_ALREADY_EXISTS,
                    format!(
                        "'{}' already exists (pass overwriteExisting)",
                        target.display()
                    ),
                ));
            }
            let opts = SaveOptions {
                expect: source
                    .as_ref()
                    .filter(|_| same_file)
                    .map(|(_, s, m)| (*s, *m)),
                overwrite_changed: req.overwrite_changed,
                write_readonly: req.write_readonly,
                backup_suffix: req.backup_suffix.clone(),
            };
            let r = match req.mode.as_deref().unwrap_or("atomic") {
                "atomic" => {
                    let wide: Vec<u16> = target.to_string_lossy().encode_utf16().collect();
                    let loc = crate::sync::loc_for(&wide)?;
                    save_file(&*loc.vfs, &loc.path, &bytes, &opts)?
                }
                "inPlace" => save_in_place(&target, &bytes, &opts)?,
                other => return Err(Fail::invalid(format!("unknown save mode '{other}'"))),
            };
            e.editor.doc_mut(side.side()).mark_saved();
            e.sources[side.index()] = Some((target.clone(), r.size, r.mtime));
            Ok(SaveReport {
                path: target.display().to_string(),
                bytes: r.bytes,
                backup: r.backup,
                size: r.size,
                mtime: r.mtime,
                replaced: r.replaced,
                state: e.editor.doc(side.side()).state(),
            })
        })?;
        *o = to_buf(serde_json::to_vec(&rep).map_err(|e| Fail::invalid(e.to_string()))?);
        Ok(())
    })
}
