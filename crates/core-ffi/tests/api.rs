//! The C ABI called from Rust the way a host would call it (backlog E9-S1..S6).
#![allow(unsafe_code, clippy::wildcard_imports, clippy::too_many_lines)]

mod common;

use common::*;
use diffsync_core::*;

// ---------------------------------------------------------------- misuse and infrastructure

#[test]
fn abi_version_and_struct_sizes() {
    assert_eq!(ds_abi_version() >> 16, ABI_MAJOR);
    assert_eq!(ds_abi_version() & 0xFFFF, ABI_MINOR);
    assert_eq!(size_of::<DsTextRow>(), 16);
    assert_eq!(size_of::<DsRange>(), 12);
    assert_eq!(size_of::<DsSideInfo>(), 32);
    assert_eq!(size_of::<DsDirRow>(), 88);
    assert_eq!(size_of::<DsPlanAction>(), 16);
    assert_eq!(std::mem::offset_of!(DsDirRow, left), 16);
    assert_eq!(std::mem::offset_of!(DsDirRow, right), 48);
    assert_eq!(std::mem::offset_of!(DsTextRow, section), 12);
}

/// Backlog E9-S1.1: a stale, double-closed, zero or foreign handle is `DS_E_INVALID_HANDLE`.
#[test]
fn stale_double_closed_and_wrong_kind_handles_are_refused() {
    let (_dir, h) = text_pair("a\nb\n", "a\nc\n");
    let mut st = DsTextStatus::default();
    // SAFETY: `st` is valid for a write.
    assert_eq!(unsafe { ds_text_status(h, &raw mut st) }, DS_OK);
    // a text handle is not a folder handle
    let mut fs = DsFolderStatus::default();
    // SAFETY: `fs` is valid for a write.
    assert_eq!(
        unsafe { ds_folder_status(h, &raw mut fs) },
        DS_E_INVALID_HANDLE
    );
    assert_eq!(ds_close(h), DS_OK);
    assert_eq!(ds_close(h), DS_E_INVALID_HANDLE, "double close");
    // SAFETY: `st` is valid for a write.
    assert_eq!(
        unsafe { ds_text_status(h, &raw mut st) },
        DS_E_INVALID_HANDLE,
        "stale handle"
    );
    assert_eq!(ds_close(0), DS_E_INVALID_HANDLE);
    assert_eq!(ds_text_cancel(0xDEAD_BEEF_0000_0001), DS_E_INVALID_HANDLE);
    // a new handle never equals the old one (generation counter)
    let (_d2, h2) = text_pair("x\n", "x\n");
    assert_ne!(h, h2);
    assert_eq!(ds_close(h2), DS_OK);
}

#[test]
fn null_pointers_and_bad_arguments_are_errors_not_crashes() {
    let (_dir, h) = text_pair("a\n", "b\n");
    // SAFETY: deliberately passing null out-parameters; the core must refuse them.
    unsafe {
        assert_eq!(ds_text_status(h, std::ptr::null_mut()), DS_E_INVALID_ARG);
        assert_eq!(
            ds_text_get_rows(h, 0, 5, std::ptr::null_mut(), std::ptr::null_mut()),
            DS_E_INVALID_ARG
        );
        let mut w = 0u32;
        assert_eq!(
            ds_text_get_rows(h, 0, 5, std::ptr::null_mut(), &raw mut w),
            DS_E_INVALID_ARG,
            "null buffer, count > 0"
        );
        assert_eq!(
            ds_text_get_rows(h, 0, 0, std::ptr::null_mut(), &raw mut w),
            DS_OK,
            "count 0 needs no buffer"
        );
        let mut hh = 0u64;
        let mut out = DsDocInfo::default();
        assert_eq!(
            ds_text_doc_info(h, 7, &raw mut out),
            DS_E_INVALID_ARG,
            "side 7"
        );
        assert_eq!(
            ds_text_open(
                s16(&[]),
                s16(&[]),
                s8("{not json"),
                NO_CB,
                std::ptr::null_mut(),
                &raw mut hh
            ),
            DS_E_INVALID_ARG
        );
    }
    assert!(
        last_error_text().contains("options"),
        "{}",
        last_error_text()
    );
    ds_close(h);
}

#[cfg(feature = "test-panic")]
#[test]
fn a_panic_becomes_an_internal_error_and_a_message() {
    assert_eq!(diffsync_core::testexports::ds_test_panic(), DS_E_INTERNAL);
    assert!(last_error_text().contains("panic"), "{}", last_error_text());
}

#[test]
fn last_error_uses_the_two_call_pattern() {
    let dir = tempfile::tempdir().unwrap();
    let (l, r) = (
        path16(&dir.path().join("no-left")),
        path16(&dir.path().join("no-right")),
    );
    let mut h = 0u64;
    // SAFETY: valid views and out pointer; loading fails later, in the session.
    let s = unsafe {
        ds_text_open(
            s16(&l),
            s16(&r),
            s8(""),
            NO_CB,
            std::ptr::null_mut(),
            &raw mut h,
        )
    };
    assert_eq!(
        s, DS_OK,
        "the failure shows up as a failed session, not at open"
    );
    wait_until("the failed session", || {
        let mut st = DsTextStatus::default();
        // SAFETY: `st` is valid for a write.
        unsafe { ds_text_status(h, &raw mut st) };
        st.state == 3
    });
    assert!(
        last_error_text().to_lowercase().contains("not found"),
        "{}",
        last_error_text()
    );
    let mut needed = 0usize;
    let mut tiny = [0u8; 4];
    // SAFETY: `tiny` is valid for 4 bytes; `needed` for a write.
    let s = unsafe { ds_last_error(tiny.as_mut_ptr(), tiny.len(), &raw mut needed) };
    assert_eq!(s, DS_E_BUFFER_TOO_SMALL);
    assert!(needed > 4);
    ds_close(h);
}

#[test]
fn the_options_schema_describes_every_option_set() {
    let mut b = DsBuf {
        ptr: std::ptr::null_mut(),
        len: 0,
    };
    // SAFETY: `b` is valid for a write.
    ok(unsafe { ds_options_schema(&raw mut b) });
    let json: serde_json::Value = serde_json::from_str(&take_buf(b)).unwrap();
    for section in ["text", "folder", "sync", "exec", "view"] {
        assert!(
            json["properties"][section]["properties"]["version"].is_object(),
            "{section}"
        );
    }
    let enum_of = |sec: &str, f: &str| {
        json["properties"][sec]["properties"][f]["enum"]
            .as_array()
            .unwrap()
            .len()
    };
    assert_eq!(enum_of("text", "alignment"), 5);
    assert_eq!(enum_of("sync", "preset"), 5);
    // every field of every default options struct is in the schema
    let defaults = [
        (
            "text",
            serde_json::to_value(core_diff::TextOptions::default()).unwrap(),
        ),
        (
            "folder",
            serde_json::to_value(core_folder::FolderRules::default()).unwrap(),
        ),
        (
            "sync",
            serde_json::to_value(core_sync::SyncRules::default()).unwrap(),
        ),
    ];
    for (sec, v) in defaults {
        for k in v.as_object().unwrap().keys() {
            assert!(
                json["properties"][sec]["properties"][k.as_str()].is_object(),
                "{sec}.{k}"
            );
        }
    }
}

/// Backlog E9-S3.2: sample options validate against the schema (types and enum values).
#[test]
fn sample_options_validate_against_the_schema() {
    let mut b = DsBuf {
        ptr: std::ptr::null_mut(),
        len: 0,
    };
    // SAFETY: `b` is valid for a write.
    ok(unsafe { ds_options_schema(&raw mut b) });
    let schema: serde_json::Value = serde_json::from_str(&take_buf(b)).unwrap();
    let check = |sec: &str, sample: &str| {
        let v: serde_json::Value = serde_json::from_str(sample).unwrap();
        for (k, val) in v.as_object().unwrap() {
            let p = &schema["properties"][sec]["properties"][k.as_str()];
            assert!(p.is_object(), "{sec}.{k} is not in the schema");
            let ty = match val {
                serde_json::Value::Bool(_) => "boolean",
                serde_json::Value::Number(n) if n.is_f64() => "number",
                serde_json::Value::Number(_) => "integer",
                serde_json::Value::String(_) => "string",
                serde_json::Value::Array(_) => "array",
                _ => "object",
            };
            assert_eq!(p["type"], ty, "{sec}.{k}");
            if let Some(e) = p["enum"].as_array() {
                assert!(e.contains(val), "{sec}.{k} = {val}");
            }
        }
    };
    check(
        "text",
        r#"{"version":1,"alignment":"patience","whitespace":"changes","ignoreCase":true,"skewTolerance":50}"#,
    );
    check(
        "folder",
        r#"{"version":1,"contentKind":"binary","compareContent":true,"excludeFolders":["bin"],"timestampToleranceS":3}"#,
    );
    check(
        "sync",
        r#"{"version":1,"preset":"mirrorToRight","useRecycleBin":false,"safeCopy":true}"#,
    );
    check("view", r#"{"filter":"differences","search":"x"}"#);
}

#[test]
fn unknown_options_are_warnings_and_a_wrong_version_is_an_error() {
    let mut h = 0u64;
    // SAFETY: valid views and out pointer.
    unsafe {
        ds_init(&DsInitOptions {
            struct_size: size_of::<DsInitOptions>() as u32,
            worker_threads: 0,
            log_fn: Some(log_sink),
            log_user: std::ptr::null_mut(),
            log_dir: s16(&[]),
            temp_dir: s16(&[]),
        });
    }
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "a", b"x\n");
    let (l, r) = (path16(&dir.path().join("a")), path16(&dir.path().join("a")));
    // SAFETY: valid views and out pointer.
    ok(unsafe {
        ds_text_open(
            s16(&l),
            s16(&r),
            s8(r#"{"version":1,"someFutureOption":true}"#),
            NO_CB,
            std::ptr::null_mut(),
            &raw mut h,
        )
    });
    ds_close(h);
    // the sink may belong to another test's init: the warning is only checked when it is ours
    if !logs().is_empty() {
        assert!(
            logs()
                .iter()
                .any(|(lvl, m)| *lvl == 1 && m.contains("someFutureOption")),
            "{:?}",
            logs()
        );
    }
    // SAFETY: valid views and out pointer; the call must fail.
    let s = unsafe {
        ds_text_open(
            s16(&l),
            s16(&r),
            s8(r#"{"version":2}"#),
            NO_CB,
            std::ptr::null_mut(),
            &raw mut h,
        )
    };
    assert_eq!(s, DS_E_INVALID_ARG);
    assert!(
        last_error_text().contains("version"),
        "{}",
        last_error_text()
    );
}

// ---------------------------------------------------------------- text compare

fn rows(h: DsHandle, first: u32, count: u32) -> Vec<DsTextRow> {
    let mut buf = vec![DsTextRow::default(); count as usize];
    let mut w = 0u32;
    // SAFETY: `buf` is valid for `count` rows; `w` for a write.
    ok(unsafe { ds_text_get_rows(h, first, count, buf.as_mut_ptr(), &raw mut w) });
    buf.truncate(w as usize);
    buf
}

#[test]
fn text_rows_sections_status_and_navigation() {
    let left = "a\nb\nc\nd\ne\nf\ng\n";
    let right = "a\nB\nc\nd\ne\nF\nF2\ng\n";
    let (_dir, h) = text_pair(left, right);
    let mut st = DsTextStatus::default();
    // SAFETY: `st` is valid for a write.
    ok(unsafe { ds_text_status(h, &raw mut st) });
    assert_eq!(st.state, 2);
    assert_eq!(st.rows, 8);
    assert_eq!((st.same, st.sections, st.important_sections), (5, 2, 2));
    let r = rows(h, 0, 100);
    assert_eq!(r.len(), 8);
    assert_eq!((r[0].kind, r[0].section, r[0].flags), (0, u32::MAX, 0));
    assert_eq!(r[1].kind, 2, "b / B paired and different");
    assert_eq!((r[1].left_line, r[1].right_line), (1, 1));
    assert_eq!(r[1].flags & 1, 1, "section start");
    assert_eq!(r[1].flags & 2, 2, "important");
    assert_eq!(r[1].section, 0);
    assert_eq!(r[5].section, 1);
    assert_eq!(
        r[6].flags & 1,
        0,
        "the second row of a section does not start one"
    );
    assert_eq!(r[6].section, 1);
    assert_eq!(
        r[6].left_line,
        u32::MAX,
        "right-only row: the left side is absent"
    );
    // a batch that starts inside a section keeps the section index and the start flag
    let tail = rows(h, 6, 3);
    assert_eq!((tail[0].flags & 1, tail[0].section), (0, 1));
    // sections
    let mut secs = [DsSection::default(); 4];
    let mut w = 0u32;
    // SAFETY: `secs` is valid for 4 entries; `w` for a write.
    ok(unsafe { ds_text_get_sections(h, 0, 4, secs.as_mut_ptr(), &raw mut w) });
    assert_eq!(w, 2);
    assert_eq!(
        (
            secs[0].first_row,
            secs[0].row_count,
            secs[1].first_row,
            secs[1].row_count
        ),
        (1, 1, 5, 2)
    );
    // navigation
    let mut row = 0u32;
    // SAFETY: `row` is valid for a write.
    ok(unsafe { ds_text_find_diff(h, 2, 1, 0, &raw mut row) });
    assert_eq!(row, 5);
    // SAFETY: as above.
    ok(unsafe { ds_text_find_diff(h, 5, -1, 0, &raw mut row) });
    assert_eq!(row, 1);
    // SAFETY: as above.
    assert_eq!(
        unsafe { ds_text_find_diff(h, 7, 1, 0, &raw mut row) },
        DS_E_NOT_FOUND
    );
    // overview
    let mut buckets = [0u8; 9];
    // SAFETY: `buckets` is valid for 9 writes.
    ok(unsafe { ds_text_get_overview(h, buckets.as_mut_ptr(), 9) });
    assert!(buckets.iter().any(|&b| b > 0));
    ds_close(h);
}

#[test]
fn line_text_uses_the_two_call_pattern_and_reports_endings() {
    let (_dir, h) = text_pair("héllo wörld\r\nsecond\n", "héllo wörld\r\nsecond");
    let mut len = 0u32;
    let mut info = DsLineInfo::default();
    // SAFETY: a null buffer with capacity 0 asks for the size; `len` and `info` are valid.
    let s = unsafe {
        ds_text_get_line(
            h,
            0,
            0,
            std::ptr::null_mut(),
            0,
            &raw mut len,
            &raw mut info,
        )
    };
    assert_eq!(s, DS_E_BUFFER_TOO_SMALL);
    assert_eq!(len, 11);
    assert_eq!((info.ending, info.utf16_len), (1, 11), "CRLF");
    let mut buf = vec![0u16; len as usize];
    // SAFETY: `buf` is valid for `len` units.
    ok(unsafe { ds_text_get_line(h, 0, 0, buf.as_mut_ptr(), len, &raw mut len, &raw mut info) });
    assert_eq!(String::from_utf16(&buf).unwrap(), "héllo wörld");
    // too small: partial copy plus the needed size
    let mut small = [0u16; 3];
    // SAFETY: `small` is valid for 3 units.
    let s = unsafe {
        ds_text_get_line(
            h,
            0,
            0,
            small.as_mut_ptr(),
            3,
            &raw mut len,
            std::ptr::null_mut(),
        )
    };
    assert_eq!((s, len), (DS_E_BUFFER_TOO_SMALL, 11));
    assert_eq!(String::from_utf16(&small).unwrap(), "hél");
    // the last line of the right file has no ending
    // SAFETY: valid out pointers.
    ok(unsafe { ds_text_get_line(h, 1, 1, buf.as_mut_ptr(), 11, &raw mut len, &raw mut info) });
    assert_eq!((len, info.ending), (6, 0));
    // SAFETY: as above.
    assert_eq!(
        unsafe { ds_text_get_line(h, 0, 99, buf.as_mut_ptr(), 11, &raw mut len, &raw mut info) },
        DS_E_NOT_FOUND
    );
    ds_close(h);
}

#[test]
fn intraline_ranges_and_document_info() {
    let (_dir, h) = text_pair("let total = price * qty;\n", "let total = price * count;\n");
    let (mut ln, mut rn) = (0u32, 0u32);
    // SAFETY: null buffers with capacity 0 ask for the counts.
    let s = unsafe {
        ds_text_get_intraline(
            h,
            0,
            std::ptr::null_mut(),
            0,
            &raw mut ln,
            std::ptr::null_mut(),
            0,
            &raw mut rn,
        )
    };
    assert_eq!(s, DS_E_BUFFER_TOO_SMALL);
    assert!(ln >= 1 && rn >= 1, "{ln} {rn}");
    let (mut l, mut r) = (
        vec![DsRange::default(); ln as usize],
        vec![DsRange::default(); rn as usize],
    );
    // SAFETY: both buffers are valid for their counts.
    ok(unsafe {
        ds_text_get_intraline(
            h,
            0,
            l.as_mut_ptr(),
            ln,
            &raw mut ln,
            r.as_mut_ptr(),
            rn,
            &raw mut rn,
        )
    });
    let covered = |v: &[DsRange], text: &str| -> String {
        let u: Vec<u16> = text.encode_utf16().collect();
        v.iter()
            .map(|x| String::from_utf16_lossy(&u[x.start as usize..(x.start + x.len) as usize]))
            .collect::<Vec<_>>()
            .join("|")
    };
    assert_eq!(covered(&l, "let total = price * qty;"), "qty");
    assert_eq!(covered(&r, "let total = price * count;"), "count");
    assert_eq!(l[0].kind, 1, "important");
    let mut d = DsDocInfo::default();
    // SAFETY: `d` is valid for a write.
    ok(unsafe { ds_text_doc_info(h, 0, &raw mut d) });
    assert_eq!((d.line_count, d.is_binary, d.byte_len), (1, 0, 25));
    assert_eq!(d.ending_counts[2], 1, "one LF");
    assert!(d.encoding.starts_with(b"UTF-8"), "{:?}", &d.encoding[..8]);
    ds_close(h);
}

#[test]
fn recompare_changes_the_options_and_the_generation() {
    let (_dir, h) = text_pair("a  b\nc\n", "a b\nc\n");
    let mut st = DsTextStatus::default();
    // SAFETY: `st` is valid for a write.
    ok(unsafe { ds_text_status(h, &raw mut st) });
    let g0 = st.generation;
    assert!(st.important_diff + st.unimportant_diff > 0);
    // SAFETY: valid JSON view.
    ok(unsafe { ds_text_recompare(h, s8(r#"{"whitespace":"changes"}"#)) });
    wait_until("the recompare", || {
        // SAFETY: `st` is valid for a write.
        ok(unsafe { ds_text_status(h, &raw mut st) });
        st.state == 2 && st.generation > g0
    });
    assert_eq!(st.important_diff, 0, "whitespace changes are ignored now");
    ds_close(h);
}

// ---------------------------------------------------------------- events

/// Backlog E9-S2.1: events come on worker threads only, are rate limited, and a callback that
/// panics does not hurt the core.
#[test]
fn events_arrive_on_worker_threads_and_are_coalesced() {
    let dir = tempfile::tempdir().unwrap();
    let big: String = (0..30_000).map(|i| format!("line number {i}\n")).collect();
    let changed = big.replace("line number 15000\n", "CHANGED\n");
    write(dir.path(), "l.txt", big.as_bytes());
    write(dir.path(), "r.txt", changed.as_bytes());
    let events = new_events();
    let (l, r) = (
        path16(&dir.path().join("l.txt")),
        path16(&dir.path().join("r.txt")),
    );
    let mut h = 0u64;
    let main = std::thread::current().id();
    // SAFETY: valid views, callback and out pointer; `events` outlives the session below.
    ok(unsafe {
        ds_text_open(
            s16(&l),
            s16(&r),
            s8(""),
            Some(record),
            user_of(&events),
            &raw mut h,
        )
    });
    wait_until("done", || {
        let mut st = DsTextStatus::default();
        // SAFETY: `st` is valid for a write.
        unsafe { ds_text_status(h, &raw mut st) };
        st.state == 2
    });
    std::thread::sleep(std::time::Duration::from_millis(100));
    let ev = events.lock().clone();
    assert!(!ev.is_empty());
    assert!(
        ev.iter().all(|e| e.thread != main),
        "callbacks come on core threads only"
    );
    assert!(ev.iter().any(|e| e.kind == DS_EV_STATE), "{ev:?}");
    // at most one event per kind per 16 ms (allow slack for scheduling)
    for kind in [DS_EV_ROWS, DS_EV_STATE] {
        let times: Vec<_> = ev.iter().filter(|e| e.kind == kind).map(|e| e.at).collect();
        let (first, last) = (times.first().unwrap(), times.last().unwrap());
        let span = last.duration_since(*first).as_millis() as usize;
        assert!(
            times.len() <= span / 10 + 3,
            "kind {kind}: {} events in {span} ms",
            times.len()
        );
    }
    ds_close(h);
}
