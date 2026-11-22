//! Text compare exports (plan.md §10.2; backlog E9-S4). Rows are pulled by the host in small
//! batches; nothing here blocks on the worker.
#![allow(unsafe_code)]

use std::cell::RefCell;
use std::ffi::c_void;
use std::sync::{Arc, OnceLock, Weak};

use core_diff::session::{DiffRow, Dir, RowKind, Section, event};
use core_diff::{DocSpec, Side, Status, TextCompare, TextOptions};

use crate::events::Bridge;
use crate::options;
use crate::registry::{self, Object};
use crate::status::{DS_E_NOT_FOUND, DS_OK, DsStatus, Fail, Res, ffi_entry};
use crate::types::*;
use crate::util::{finish, json_arg, os_path, out, units, write_buffer};

pub struct TextObj {
    pub tc: Arc<TextCompare>,
    bridge: Arc<Bridge>,
    /// The editor, created by the first edit (backlog E11.5-S14).
    pub edit: parking_lot::Mutex<Option<crate::edit::EditState>>,
}

impl TextObj {
    /// `ds_close`: stop the worker's work and detach the events.
    pub fn shutdown(&self) {
        self.tc.cancel();
        self.bridge.detach();
    }
}

impl Drop for TextObj {
    fn drop(&mut self) {
        self.tc.cancel();
    }
}

impl std::fmt::Debug for TextObj {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TextObj")
    }
}

fn state_code(s: &Status) -> u32 {
    match s {
        Status::Loading => 0,
        Status::Aligning { .. } => 1,
        Status::Done => 2,
        Status::Failed { .. } => 3,
        Status::Cancelled => 4,
        Status::Binary => 5,
    }
}

fn side_of(side: u8) -> Res<Side> {
    match side {
        0 => Ok(Side::Left),
        1 => Ok(Side::Right),
        _ => Err(Fail::invalid("side must be 0 (left) or 1 (right)")),
    }
}

/// Opens a text compare of two files. Events: `DS_EV_STATE`, `DS_EV_ROWS`. Call
/// `ds_text_status` once after opening: events sent before the handle was known are held.
///
/// # Safety
/// All pointers must be valid for the lengths given; `out` for a write.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ds_text_open(
    left: DsStr16,
    right: DsStr16,
    options_json: DsStr8,
    cb: DsEventFn,
    user: *mut c_void,
    out_handle: *mut DsHandle,
) -> DsStatus {
    ffi_entry!({
        // SAFETY: the ABI contract of this function covers every pointer argument.
        let (l, r, json, outp) = unsafe {
            (
                units(left)?,
                units(right)?,
                json_arg(options_json)?,
                out(out_handle)?,
            )
        };
        let opts = options::text_options(json)?;
        let bridge = Bridge::new(cb, user);
        let cell: Arc<OnceLock<Weak<TextCompare>>> = Arc::new(OnceLock::new());
        let (b, c) = (bridge.clone(), cell.clone());
        let sink: core_diff::session::EventSink = Arc::new(move |bits: u32| {
            // the session already coalesces its events to 60 Hz, so none are held back here;
            // `a` of a state event is 255 while the session is still being created (call status)
            if bits & event::STATE != 0 {
                let code = c
                    .get()
                    .and_then(Weak::upgrade)
                    .map_or(255, |t| state_code(&t.status()));
                b.emit(DS_EV_STATE, u64::from(code), 0, true);
            }
            if bits & event::ROWS != 0 {
                let rows = c.get().and_then(Weak::upgrade).map_or(0, |t| t.row_count());
                b.emit(DS_EV_ROWS, u64::from(rows), 0, true);
            }
        });
        // a path the core would resolve against the process current directory is refused
        let (lp, rp) = (os_path(l), os_path(r));
        if !lp.is_absolute() || !rp.is_absolute() {
            return Err(Fail::invalid("file paths must be absolute"));
        }
        let tc = TextCompare::start(DocSpec::Path(lp), DocSpec::Path(rp), opts, Some(sink))?;
        let _ = cell.set(Arc::downgrade(&tc));
        let h = registry::insert(Object::Text(Arc::new(TextObj {
            tc,
            bridge: bridge.clone(),
            edit: parking_lot::Mutex::new(None),
        })));
        bridge.arm(h);
        *outp = h;
        Ok(())
    })
}

#[unsafe(no_mangle)]
/// # Safety
/// `out_status` must be valid for a write.
pub unsafe extern "C" fn ds_text_status(h: DsHandle, out_status: *mut DsTextStatus) -> DsStatus {
    ffi_entry!({
        let t = registry::text(h)?;
        // SAFETY: the ABI contract covers the out pointer.
        let o = unsafe { out(out_status) }?;
        let status = t.tc.status();
        let s = t.tc.stats();
        *o = DsTextStatus {
            state: state_code(&status),
            rows: t.tc.row_count(),
            generation: t.tc.generation(),
            same: s.same,
            important_diff: s.important_diff,
            unimportant_diff: s.unimportant_diff,
            left_orphan: s.left_orphan,
            right_orphan: s.right_orphan,
            unimportant_left_orphan: s.unimportant_left_orphan,
            unimportant_right_orphan: s.unimportant_right_orphan,
            sections: s.sections,
            important_sections: s.important_sections,
            reserved: 0,
        };
        if let Status::Failed { error } = status {
            crate::status::set_last_error(&error);
        }
        Ok(())
    })
}

thread_local! {
    static ROWS: RefCell<Vec<DiffRow>> = const { RefCell::new(Vec::new()) };
    static LINE: RefCell<Vec<u16>> = const { RefCell::new(Vec::new()) };
}

/// The index of the section that contains `row` (`u32::MAX` if none).
fn section_of(tc: &TextCompare, row: u32, count: u32) -> u32 {
    let (mut lo, mut hi) = (0u32, count);
    let mut one = [Section {
        first_row: 0,
        row_count: 0,
        important: false,
    }];
    // last section whose first row is <= row
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if tc.sections(mid, &mut one) == 0 || one[0].first_row > row {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    let Some(idx) = lo.checked_sub(1) else {
        return u32::MAX;
    };
    if tc.sections(idx, &mut one) == 1 && row < one[0].first_row + one[0].row_count {
        idx
    } else {
        u32::MAX
    }
}

/// Copies rows `first..first + count` (fewer at the end) into `out_rows`.
///
/// # Safety
/// `out_rows` must be valid for `count` rows, `written` for a write.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ds_text_get_rows(
    h: DsHandle,
    first: u32,
    count: u32,
    out_rows: *mut DsTextRow,
    written: *mut u32,
) -> DsStatus {
    ffi_entry!({
        let t = registry::text(h)?;
        // SAFETY: the ABI contract covers the buffers.
        let (w, dst) = unsafe { (out(written)?, slice_mut(out_rows, count as usize)?) };
        *w = 0;
        // never more than the rows that exist: the buffer is sized from the host's count
        let count = count.min(t.tc.row_count().saturating_sub(first));
        ROWS.with(|cell| {
            let mut buf = cell.borrow_mut();
            let blank = DiffRow {
                left: 0,
                right: 0,
                kind: RowKind::Same,
            };
            // one row of context in front: whether the first row starts a section
            let ctx = usize::from(first > 0);
            buf.clear();
            buf.resize(count as usize + ctx, blank);
            let start = first - u32::from(first > 0);
            let n = t.tc.rows(start, &mut buf[..]);
            // the section count is read after the rows: a section published in between is seen
            let sections = t.tc.stats().sections;
            let mut section = u32::MAX;
            let mut prev_diff = ctx == 1 && n > 0 && buf[0].kind != RowKind::Same;
            for (i, r) in buf[ctx..n.max(ctx)].iter().enumerate() {
                let diff = r.kind != RowKind::Same;
                let starts = diff && !prev_diff;
                if diff && (starts || section == u32::MAX) {
                    section = section_of(&t.tc, first + i as u32, sections);
                }
                if !diff {
                    section = u32::MAX;
                }
                dst[i] = DsTextRow {
                    left_line: r.left,
                    right_line: r.right,
                    kind: r.kind as u8,
                    flags: u8::from(starts) | (u8::from(r.kind.is_important()) << 1),
                    reserved: 0,
                    section: if diff { section } else { u32::MAX },
                };
                prev_diff = diff;
                *w = i as u32 + 1;
            }
        });
        Ok(())
    })
}

/// A mutable counterpart of [`slice`].
///
/// # Safety
/// `ptr` must be valid for `len` writes.
unsafe fn slice_mut<'a, T>(ptr: *mut T, len: usize) -> Res<&'a mut [T]> {
    if len == 0 {
        return Ok(&mut []);
    }
    if ptr.is_null() {
        return Err(Fail::invalid("a null pointer with a non-zero length"));
    }
    // SAFETY: non-null; validity for `len` writes is the caller's contract.
    Ok(unsafe { std::slice::from_raw_parts_mut(ptr, len) })
}

/// Copies sections `first..first + count` into `out_sections`.
///
/// # Safety
/// `out_sections` must be valid for `count` entries, `written` for a write.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ds_text_get_sections(
    h: DsHandle,
    first: u32,
    count: u32,
    out_sections: *mut DsSection,
    written: *mut u32,
) -> DsStatus {
    ffi_entry!({
        let t = registry::text(h)?;
        // SAFETY: the ABI contract covers the buffers.
        let (w, dst) = unsafe { (out(written)?, slice_mut(out_sections, count as usize)?) };
        // never more than the sections that exist: the buffer is sized from the host's count
        let count = count.min(t.tc.stats().sections.saturating_sub(first));
        let mut buf = vec![
            Section {
                first_row: 0,
                row_count: 0,
                important: false
            };
            count as usize
        ];
        let n = t.tc.sections(first, &mut buf);
        for (d, s) in dst.iter_mut().zip(&buf[..n]) {
            *d = DsSection {
                first_row: s.first_row,
                row_count: s.row_count,
                important: u8::from(s.important),
                pad: [0; 3],
            };
        }
        *w = n as u32;
        Ok(())
    })
}

/// The text of one line as UTF-16 (two-call buffer pattern) and its info.
///
/// # Safety
/// `buf` must be valid for `cap` units (or null with `cap` 0); `len` and `info` for writes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ds_text_get_line(
    h: DsHandle,
    side: u8,
    line: u32,
    buf: *mut u16,
    cap: u32,
    len: *mut u32,
    info: *mut DsLineInfo,
) -> DsStatus {
    ffi_entry!({
        let t = registry::text(h)?;
        let side = side_of(side)?;
        LINE.with(|cell| {
            let mut v = cell.borrow_mut();
            if !t.tc.line_utf16(side, line, &mut v) {
                return Err(Fail::new(DS_E_NOT_FOUND, "no such line"));
            }
            if !info.is_null() {
                let ending = t.tc.line_ending(side, line).map_or(255, |e| e as u8);
                // SAFETY: non-null, valid for a write by the ABI contract.
                unsafe {
                    *info = DsLineInfo {
                        ending,
                        pad: [0; 3],
                        utf16_len: v.len() as u32,
                    };
                }
            }
            // SAFETY: the ABI contract covers `buf` and `len`.
            finish(unsafe { write_buffer(&v, buf, cap as usize, len) })
        })
    })
}

fn ranges(src: &[core_diff::session::CharRange], dst: &mut [DsRange]) {
    for (d, r) in dst.iter_mut().zip(src) {
        *d = DsRange {
            start: r.start,
            len: r.len,
            kind: u8::from(r.important),
            pad: [0; 3],
        };
    }
}

/// The word-level highlights of a paired row (two-call pattern for both sides).
///
/// # Safety
/// Buffers must be valid for their capacities (or null with capacity 0); counts for writes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ds_text_get_intraline(
    h: DsHandle,
    row: u32,
    left: *mut DsRange,
    lcap: u32,
    ln: *mut u32,
    right: *mut DsRange,
    rcap: u32,
    rn: *mut u32,
) -> DsStatus {
    ffi_entry!({
        let t = registry::text(h)?;
        // only a null check here: `write_buffer` below makes the one live reference to each count
        if ln.is_null() || rn.is_null() {
            return Err(Fail::invalid("a required out-parameter is null"));
        }
        let il = t.tc.intraline(row).unwrap_or_default();
        let mut l: Vec<DsRange> = vec![DsRange::default(); il.left.len()];
        let mut r: Vec<DsRange> = vec![DsRange::default(); il.right.len()];
        ranges(&il.left, &mut l);
        ranges(&il.right, &mut r);
        // SAFETY: as above.
        let a = unsafe { write_buffer(&l, left, lcap as usize, ln) }?;
        // SAFETY: as above.
        let b = unsafe { write_buffer(&r, right, rcap as usize, rn) }?;
        finish(Ok(if a == DS_OK { b } else { a }))
    })
}

/// Finds the next (`dir >= 0`) or previous (`dir < 0`) difference row.
///
/// # Safety
/// `out_row` must be valid for a write.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ds_text_find_diff(
    h: DsHandle,
    from_row: u32,
    dir: i32,
    important_only: u8,
    out_row: *mut u32,
) -> DsStatus {
    ffi_entry!({
        let t = registry::text(h)?;
        // SAFETY: the ABI contract covers the out pointer.
        let o = unsafe { out(out_row) }?;
        let d = if dir >= 0 { Dir::Next } else { Dir::Prev };
        match t.tc.find_diff(from_row, d, important_only != 0) {
            Some(r) => {
                *o = r;
                Ok(())
            }
            None => Err(Fail::new(DS_E_NOT_FOUND, "no further difference")),
        }
    })
}

/// Fills `n` overview buckets (0 equal ... higher = more severe).
///
/// # Safety
/// `buckets` must be valid for `n` writes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ds_text_get_overview(h: DsHandle, buckets: *mut u8, n: u32) -> DsStatus {
    ffi_entry!({
        let t = registry::text(h)?;
        // SAFETY: the ABI contract covers the buffer.
        let b = unsafe { slice_mut(buckets, n as usize) }?;
        t.tc.overview(b);
        Ok(())
    })
}

/// Facts about a loaded document (encoding, endings, size).
///
/// # Safety
/// `out_info` must be valid for a write.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ds_text_doc_info(
    h: DsHandle,
    side: u8,
    out_info: *mut DsDocInfo,
) -> DsStatus {
    ffi_entry!({
        let t = registry::text(h)?;
        let side = side_of(side)?;
        // SAFETY: the ABI contract covers the out pointer.
        let o = unsafe { out(out_info) }?;
        let d =
            t.tc.doc_info(side)
                .ok_or_else(|| Fail::busy("the document is not loaded yet"))?;
        let mut enc = [0u8; 24];
        let name = d.encoding.as_bytes();
        let n = name.len().min(23);
        enc[..n].copy_from_slice(&name[..n]);
        *o = DsDocInfo {
            line_count: d.line_count,
            is_binary: u8::from(d.is_binary),
            has_bom: u8::from(d.has_bom),
            pad: [0; 2],
            byte_len: d.byte_len,
            ending_counts: d.ending_counts,
            encoding: enc,
        };
        Ok(())
    })
}

/// Starts a new alignment with other options; old rows stay readable until it replaces them.
///
/// # Safety
/// `options_json` must be valid for its length.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ds_text_recompare(h: DsHandle, options_json: DsStr8) -> DsStatus {
    ffi_entry!({
        let t = registry::text(h)?;
        // SAFETY: the ABI contract covers the JSON view.
        let json = unsafe { json_arg(options_json) }?;
        let opts: TextOptions = options::text_options(json)?;
        t.tc.recompare(opts)?;
        let _ = &t.bridge;
        Ok(())
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn ds_text_cancel(h: DsHandle) -> DsStatus {
    ffi_entry!({
        registry::text(h)?.tc.cancel();
        Ok(())
    })
}
