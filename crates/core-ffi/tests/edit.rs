//! The editing exports of ABI 1.2 (backlog E11.5-S14, S16): copy between the sides, save, convert,
//! rename, ignore marks and the patch exports, through the C ABI.
#![allow(unsafe_code, clippy::wildcard_imports, clippy::too_many_lines)]

mod common;

use common::*;
use diffsync_core::*;
use serde_json::{Value, json};

fn call_h(
    f: unsafe extern "C" fn(DsHandle, DsStr8, *mut DsBuf) -> DsStatus,
    h: DsHandle,
    request: &Value,
) -> (DsStatus, Value) {
    let text = request.to_string();
    let mut b = DsBuf {
        ptr: std::ptr::null_mut(),
        len: 0,
    };
    // SAFETY: valid handle, view and out pointer.
    let st = unsafe { f(h, s8(&text), &raw mut b) };
    if st != DS_OK {
        return (st, Value::Null);
    }
    let s = take_buf(b);
    (st, serde_json::from_str(&s).unwrap_or(Value::String(s)))
}

fn call(
    f: unsafe extern "C" fn(DsStr8, *mut DsBuf) -> DsStatus,
    request: &Value,
) -> (DsStatus, Value) {
    let text = request.to_string();
    let mut b = DsBuf {
        ptr: std::ptr::null_mut(),
        len: 0,
    };
    // SAFETY: valid view and out pointer.
    let st = unsafe { f(s8(&text), &raw mut b) };
    if st != DS_OK {
        return (st, Value::Null);
    }
    let s = take_buf(b);
    (st, serde_json::from_str(&s).unwrap_or(Value::String(s)))
}

fn status(h: DsHandle) -> DsTextStatus {
    let mut st = DsTextStatus::default();
    // SAFETY: `st` is valid for a write.
    ok(unsafe { ds_text_status(h, &raw mut st) });
    st
}

fn settle(h: DsHandle) -> DsTextStatus {
    wait_until("the compare to settle", || status(h).state == 2);
    status(h)
}

#[test]
fn a_section_is_copied_and_the_file_saved_atomically() {
    let (dir, h) = text_pair("a\nb\nc\nd\n", "a\nB\nc\nD\n");
    assert_eq!(status(h).sections, 2);
    let g0 = status(h).generation;
    let (st, r) = call_h(
        ds_text_edit,
        h,
        &json!({"op": "copySection", "from": "left", "section": 0}),
    );
    assert_eq!(st, DS_OK, "{}", last_error_text());
    assert_eq!(r["changed"], true);
    assert_eq!(r["right"]["dirty"], true);
    assert_eq!(r["left"]["dirty"], false);
    assert!(r["generation"].as_u64().unwrap() > g0);
    let s = settle(h);
    assert_eq!(s.sections, 1);
    // the file is not touched until it is saved
    assert_eq!(
        std::fs::read(dir.path().join("r.txt")).unwrap(),
        b"a\nB\nc\nD\n"
    );
    let (st, rep) = call_h(
        ds_text_save,
        h,
        &json!({"side": "right", "backupSuffix": ".bak"}),
    );
    assert_eq!(st, DS_OK, "{}", last_error_text());
    assert_eq!(rep["state"]["dirty"], false);
    assert_eq!(
        std::fs::read(dir.path().join("r.txt")).unwrap(),
        b"a\nb\nc\nD\n"
    );
    assert_eq!(
        std::fs::read(dir.path().join("r.txt.bak")).unwrap(),
        b"a\nB\nc\nD\n"
    );
    // undo brings the text back, and it is dirty again against the saved file
    let (_, r) = call_h(ds_text_edit, h, &json!({"op": "undo", "side": "right"}));
    assert_eq!(r["changed"], true);
    assert_eq!(r["right"]["dirty"], true);
    // a file that changed on disk is not overwritten; with overwriteChanged it is
    std::fs::write(dir.path().join("r.txt"), "someone else\n").unwrap();
    let (st, _) = call_h(ds_text_save, h, &json!({"side": "right"}));
    assert_eq!(st, DS_E_CHANGED_SINCE_COMPARE, "{}", last_error_text());
    assert_eq!(
        std::fs::read(dir.path().join("r.txt")).unwrap(),
        b"someone else\n"
    );
    let (st, _) = call_h(
        ds_text_save,
        h,
        &json!({"side": "right", "overwriteChanged": true}),
    );
    assert_eq!(st, DS_OK);
    assert_eq!(
        std::fs::read(dir.path().join("r.txt")).unwrap(),
        b"a\nB\nc\nD\n"
    );
    // bad requests
    assert_eq!(
        call_h(ds_text_edit, h, &json!({"op": "nonsense"})).0,
        DS_E_INVALID_ARG
    );
    assert_eq!(
        call_h(
            ds_text_edit,
            h,
            &json!({"op": "copySection", "from": "left", "section": 99})
        )
        .0,
        DS_E_INVALID_ARG
    );
    assert_eq!(call_h(ds_text_save, h, &json!({})).0, DS_E_INVALID_ARG);
    ok(ds_close(h));
}

#[test]
fn encoding_and_line_endings_are_converted_on_save() {
    let (dir, h) = text_pair("caf\u{e9}\nx\n", "same\n");
    let out = dir.path().join("out.txt");
    let (st, r) = call_h(
        ds_text_save,
        h,
        &json!({"side": "left", "eol": "crlf", "encoding": "utf-16le", "path": out.to_str().unwrap()}),
    );
    assert_eq!(st, DS_OK, "{}", last_error_text());
    assert_eq!(r["state"]["encoding"], "utf-16le-bom");
    let bytes = std::fs::read(&out).unwrap();
    let want: Vec<u8> = [0xFF, 0xFE]
        .into_iter()
        .chain(
            "caf\u{e9}\r\nx\r\n"
                .encode_utf16()
                .flat_map(u16::to_le_bytes),
        )
        .collect();
    assert_eq!(bytes, want);
    // Save As over an existing file asks first
    let (st, _) = call_h(
        ds_text_save,
        h,
        &json!({"side": "left", "path": dir.path().join("r.txt").to_str().unwrap()}),
    );
    assert_eq!(st, DS_E_ALREADY_EXISTS);
    // a text a Western code page cannot hold is refused, and nothing is written
    let (st, _) = call_h(
        ds_text_edit,
        h,
        &json!({"op": "replaceLines", "side": "left", "firstLine": 1, "removeLines": 1, "text": "\u{65e5}\u{672c}"}),
    );
    assert_eq!(st, DS_OK, "{}", last_error_text());
    let o2 = dir.path().join("o2.txt");
    let (st, _) = call_h(
        ds_text_save,
        h,
        &json!({"side": "left", "encoding": "windows-1252", "path": o2.to_str().unwrap()}),
    );
    assert_eq!(st, DS_E_ENCODING, "{}", last_error_text());
    assert!(!o2.exists());
    assert!(
        last_error_text().contains("line 2"),
        "{}",
        last_error_text()
    );
    ok(ds_close(h));
}

#[test]
fn rename_ignore_and_the_new_view_modes() {
    let d = tempfile::tempdir().unwrap();
    let (l, r) = (d.path().join("l"), d.path().join("r"));
    write(&l, "a.txt", b"1");
    write(&l, "only.txt", b"2");
    write(&r, "a.txt", b"1");
    let f = folder_open(&l, &r, "", NO_CB, std::ptr::null_mut());
    let mut v = 0u64;
    // SAFETY: valid view and out pointer.
    ok(unsafe { ds_view_create(f, s8(r#"{"filter":"leftOrphans"}"#), &raw mut v) });
    let (mut count, mut generation) = (0u32, 0u64);
    // SAFETY: valid out pointers.
    ok(unsafe { ds_view_row_count(v, &raw mut count, &raw mut generation) });
    assert_eq!(count, 1);
    // SAFETY: valid handle and view.
    ok(unsafe { ds_view_set(v, s8(r#"{"filter":"rightOrphans"}"#)) });
    ok(unsafe { ds_view_row_count(v, &raw mut count, &raw mut generation) });
    assert_eq!(count, 0);
    assert_eq!(
        unsafe { ds_view_set(v, s8(r#"{"filter":"bogus"}"#)) },
        DS_E_INVALID_ARG
    );
    ok(unsafe { ds_view_set(v, s8(r#"{"filter":"leftOrphans"}"#)) });
    let mut rows = [DsDirRow::default(); 4];
    let mut n = 0u32;
    // SAFETY: valid buffer for four rows and a count.
    ok(unsafe { ds_view_get_rows(v, 0, 4, rows.as_mut_ptr(), &raw mut n) });
    assert_eq!(n, 1);
    let node = rows[0].node;
    // ignore the orphan: the view empties, and shows it again when asked
    ok(unsafe { ds_folder_set_ignored(f, &raw const node, 1, 1) });
    ok(unsafe { ds_view_row_count(v, &raw mut count, &raw mut generation) });
    assert_eq!(count, 0);
    ok(unsafe { ds_view_set(v, s8(r#"{"showIgnored":true}"#)) });
    ok(unsafe { ds_view_row_count(v, &raw mut count, &raw mut generation) });
    assert_eq!(count, 1);
    ok(unsafe { ds_folder_set_ignored(f, &raw const node, 1, 0) });
    assert_eq!(
        unsafe { ds_folder_set_ignored(f, [0u32].as_ptr(), 1, 1) },
        DS_E_INVALID_ARG
    );
    // rename a file as a job
    let item = path16(&l.join("only.txt"));
    let name = u16s("renamed.txt");
    let mut job = 0u64;
    // SAFETY: valid views and out pointer.
    ok(unsafe {
        ds_fileops_rename(
            s16(&item),
            s16(&name),
            s8(""),
            NO_CB,
            std::ptr::null_mut(),
            &raw mut job,
        )
    });
    wait_until("the rename job", || l.join("renamed.txt").exists());
    assert!(!l.join("only.txt").exists());
    // a name with a separator is refused before a job exists
    let bad = u16s("a/b");
    let item2 = path16(&l.join("renamed.txt"));
    let mut job2 = 0u64;
    assert_eq!(
        unsafe {
            ds_fileops_rename(
                s16(&item2),
                s16(&bad),
                s8(""),
                NO_CB,
                std::ptr::null_mut(),
                &raw mut job2,
            )
        },
        DS_E_INVALID_NAME
    );
}

#[test]
fn the_patch_exports_apply_diff_and_compare_bytes() {
    let d = tempfile::tempdir().unwrap();
    write(d.path(), "f.txt", b"a\nb\nc\n");
    let patch = "--- a/f.txt\n+++ b/f.txt\n@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n";
    let root = d.path().to_str().unwrap();
    let (st, r) = call(
        ds_patch_apply,
        &json!({"patch": patch, "root": root, "dryRun": true}),
    );
    assert_eq!(st, DS_OK);
    assert_eq!(
        (r["ok"].clone(), r["written"].clone()),
        (json!(true), json!(false))
    );
    let (_, r) = call(ds_patch_apply, &json!({"patch": patch, "root": root}));
    assert_eq!(r["written"], true, "{r}");
    assert_eq!(std::fs::read(d.path().join("f.txt")).unwrap(), b"a\nB\nc\n");
    // a patch that does not fit is a result, not an error, and writes nothing
    let (st, r) = call(ds_patch_apply, &json!({"patch": patch, "root": root}));
    assert_eq!((st, r["ok"].clone()), (DS_OK, json!(false)));
    assert_eq!(std::fs::read(d.path().join("f.txt")).unwrap(), b"a\nB\nc\n");
    // SEARCH/REPLACE onto a text, no files
    let (_, r) = call(
        ds_patch_apply,
        &json!({"patch": "<<<<<<< SEARCH\nB\n=======\nbee\n>>>>>>> REPLACE\n", "text": "a\nB\nc\n"}),
    );
    assert_eq!(r["files"][0]["text"], "a\nbee\nc\n");
    // JSON Patch
    let (_, r) = call(
        ds_json_patch,
        &json!({"op": "diff", "left": {"x": 1}, "right": {"x": 2}}),
    );
    assert_eq!(r["patch"][0]["op"], "replace");
    let (_, r) = call(
        ds_json_patch,
        &json!({"op": "apply", "doc": {"x": 1}, "patch": r["patch"]}),
    );
    assert_eq!(r["doc"], json!({"x": 2}));
    let (_, r) = call(
        ds_json_patch,
        &json!({"op": "apply", "doc": {}, "patch": [{"op": "remove", "path": "/q"}]}),
    );
    assert_eq!(
        (r["ok"].clone(), r["index"].clone()),
        (json!(false), json!(0))
    );
    // bytes
    write(d.path(), "a.bin", &[1, 2, 3, 0x74]);
    write(d.path(), "b.bin", &[1, 2, 3, 0x75]);
    let (st, r) = call(
        ds_bytes_diff,
        &json!({"left": d.path().join("a.bin").to_str().unwrap(), "right": d.path().join("b.bin").to_str().unwrap(), "format": "script"}),
    );
    assert_eq!(st, DS_OK, "{}", last_error_text());
    assert_eq!(
        (r["identical"].clone(), r["differingBytes"].clone()),
        (json!(false), json!(1))
    );
    assert_eq!(r["text"], "00000003:74->75\n");
    let (st, _) = call(ds_bytes_diff, &json!({"left": "relative", "right": "x"}));
    assert_eq!(st, DS_E_INVALID_ARG);
}
