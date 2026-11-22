//! The integration exports of ABI 1.1 (backlog E11.5-S5): diff reports, tool argument parsing,
//! ignore lists, the three-way merge stub, the Explorer selection, free space, and the navigation
//! helpers. They run in a process of their own: the selection store is moved to a temp folder with
//! an environment variable, which is global.
#![allow(unsafe_code, clippy::wildcard_imports, clippy::too_many_lines)]

mod common;

use common::*;
use diffsync_core::*;
use serde_json::{Value, json};

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

#[test]
fn diff_run_reports_text_json_and_the_gnu_exit_code() {
    let d = tempfile::tempdir().unwrap();
    write(d.path(), "a.txt", b"1\n2\n3\n");
    write(d.path(), "b.txt", b"1\nTWO\n3\n");
    let (a, b) = (d.path().join("a.txt"), d.path().join("b.txt"));
    let (st, r) = call(
        ds_diff_run,
        &json!({"args": ["-u", a.to_str().unwrap(), b.to_str().unwrap()]}),
    );
    assert_eq!(st, DS_OK);
    assert_eq!(r["exitCode"], 1);
    let out = r["stdout"].as_str().unwrap();
    assert!(out.contains("@@ -1,3 +1,3 @@\n 1\n-2\n+TWO\n 3\n"), "{out}");
    let machine: Value = serde_json::from_str(r["json"].as_str().unwrap()).unwrap();
    assert_eq!(machine["summary"]["insertions"], 1);
    // equal files: exit 0, nothing printed
    let (_, r) = call(
        ds_diff_run,
        &json!({"args": ["-u", a.to_str().unwrap(), a.to_str().unwrap()]}),
    );
    assert_eq!(
        (r["exitCode"].as_i64(), r["stdout"].as_str()),
        (Some(0), Some(""))
    );
    // trouble (a missing file, an unknown flag) is a successful call with exit code 2
    let (st, r) = call(
        ds_diff_run,
        &json!({"args": ["-u", a.to_str().unwrap(), d.path().join("nope").to_str().unwrap()]}),
    );
    assert_eq!((st, r["exitCode"].as_i64()), (DS_OK, Some(2)));
    let (st, r) = call(ds_diff_run, &json!({"args": ["--bogus", "a", "b"]}));
    assert_eq!((st, r["exitCode"].as_i64()), (DS_OK, Some(2)));
    assert!(
        r["stderr"]
            .as_str()
            .unwrap()
            .contains("unrecognized option '--bogus'")
    );
    // a malformed request is an argument error
    assert_eq!(
        call(ds_diff_run, &json!("not an object")).0,
        DS_E_INVALID_ARG
    );
}

#[test]
fn diff_run_folders_git_style_and_ignore_lists() {
    let d = tempfile::tempdir().unwrap();
    let (l, r) = (d.path().join("f1"), d.path().join("f2"));
    write(&l, "x.txt", b"old\n");
    write(&r, "x.txt", b"new\n");
    write(&l, "package-lock.json", b"{}\n");
    write(&r, "package-lock.json", b"{\"v\":2}\n");
    write(&r, "added.txt", b"hello\n");
    let (ln, rn) = (l.to_str().unwrap(), r.to_str().unwrap());
    let (_, res) = call(
        ds_diff_run,
        &json!({"dialect": "git", "args": ["--no-index", "--name-status", ln, rn], "ignoreLockFiles": true}),
    );
    assert_eq!(res["stdout"], "A\tadded.txt\nM\tx.txt\n");
    let (_, res) = call(
        ds_diff_run,
        &json!({"dialect": "git", "args": ["--no-index", "--name-status", ln, rn]}),
    );
    assert_eq!(
        res["stdout"],
        "A\tadded.txt\nM\tpackage-lock.json\nM\tx.txt\n"
    );
    let (_, res) = call(
        ds_diff_run,
        &json!({"args": ["-rq", ln, rn], "ignore": "*.lock,*package-lock.json"}),
    );
    assert!(
        !res["stdout"].as_str().unwrap().contains("package-lock"),
        "{res}"
    );
    // git diff exits 0 unless asked
    assert_eq!(res["exitCode"], 1, "GNU: different");
    let (_, g) = call(
        ds_diff_run,
        &json!({"dialect": "git", "args": ["--no-index", ln, rn]}),
    );
    assert_eq!(g["exitCode"], 0);
    let (_, g) = call(
        ds_diff_run,
        &json!({"dialect": "git", "args": ["--no-index", "--exit-code", ln, rn]}),
    );
    assert_eq!(g["exitCode"], 1);
}

#[test]
fn diff_texts_needs_no_files() {
    let (st, r) = call(
        ds_diff_texts,
        &json!({"args": ["-u"], "left": "a\nb\n", "right": "a\nc\n", "leftName": "old.txt", "rightName": "new.txt"}),
    );
    assert_eq!(st, DS_OK);
    let out = r["stdout"].as_str().unwrap();
    assert!(
        out.starts_with("--- old.txt\t")
            && out.contains("\n+++ new.txt\t")
            && out.ends_with("@@ -1,2 +1,2 @@\n a\n-b\n+c\n"),
        "{out}"
    );
    let (_, r) = call(
        ds_diff_texts,
        &json!({"dialect": "git", "args": [], "left": "x\n", "right": "y\n", "leftName": "f", "rightName": "f"}),
    );
    assert!(
        r["stdout"]
            .as_str()
            .unwrap()
            .starts_with("diff --git a/f b/f\n--- a/f\n+++ b/f\n"),
        "{r}"
    );
}

#[test]
fn tool_arguments_ignore_lists_and_the_merge_stub() {
    let (st, r) = call(
        ds_tool_parse_args,
        &json!(["L.txt", "R.txt", "B.txt", "-o", "M.txt"]),
    );
    assert_eq!(st, DS_OK);
    assert_eq!(r["invocation"]["kind"], "merge");
    assert_eq!(r["invocation"]["value"]["base"], "B.txt");
    assert_eq!(r["invocation"]["value"]["output"], "M.txt");
    let (_, r) = call(ds_tool_parse_args, &json!(["--select-left", "C:\\a"]));
    assert_eq!(
        r["invocation"],
        json!({"kind": "selectLeft", "value": "C:\\a"})
    );
    assert_eq!(
        call(ds_tool_parse_args, &json!(["only-one"])).0,
        DS_E_INVALID_ARG
    );
    assert!(
        last_error_text().contains("expected two paths"),
        "{}",
        last_error_text()
    );

    let mut b = DsBuf {
        ptr: std::ptr::null_mut(),
        len: 0,
    };
    // SAFETY: valid out pointer.
    ok(unsafe { ds_tool_default_ignore(&raw mut b) });
    let preset = take_buf(b);
    assert_eq!(
        preset,
        "*package-lock.json,*gemfile.lock,*podfile.lock,*cartfile.resolved"
    );
    let path = u16s("D:\\repo\\web\\package-lock.json");
    let mut yes = 9u8;
    // SAFETY: valid views and out pointer.
    ok(unsafe { ds_tool_is_ignored(s8(&preset), s16(&path), &raw mut yes) });
    assert_eq!(yes, 1);
    let other = u16s("D:\\repo\\web\\package.json");
    // SAFETY: valid views and out pointer.
    ok(unsafe { ds_tool_is_ignored(s8(&preset), s16(&other), &raw mut yes) });
    assert_eq!(yes, 0);

    let (_, m) = call(
        ds_merge3,
        &json!({"base": "a\n", "local": "a\n", "remote": "b\n"}),
    );
    assert_eq!(
        (m["outcome"].as_str(), m["text"].as_str()),
        (Some("clean"), Some("b\n"))
    );
    let (_, m) = call(
        ds_merge3,
        &json!({"base": "a\n", "local": "x\n", "remote": "y\n"}),
    );
    assert_eq!(m["outcome"], "conflict");
    let d = tempfile::tempdir().unwrap();
    write(d.path(), "l", b"1\n");
    write(d.path(), "r", b"2\n");
    write(d.path(), "b", b"1\n");
    let p = |n: &str| d.path().join(n).to_str().unwrap().to_string();
    let (_, m) = call(
        ds_merge_files,
        &json!({"local": p("l"), "remote": p("r"), "base": p("b"), "output": p("out")}),
    );
    assert_eq!(m["exitCode"], 0);
    assert_eq!(
        std::fs::read_to_string(d.path().join("out")).unwrap(),
        "2\n"
    );
}

#[test]
fn the_explorer_selection_and_volume_space() {
    let state = tempfile::tempdir().unwrap();
    // SAFETY: set before any other thread of this test binary reads it; this binary runs one such test
    unsafe { std::env::set_var("DIFFSYNC_STATE_DIR", state.path()) };
    let target = state.path().join("left.txt");
    std::fs::write(&target, "x").unwrap();
    let get = || {
        let mut b = DsBuf {
            ptr: std::ptr::null_mut(),
            len: 0,
        };
        // SAFETY: valid out pointer.
        ok(unsafe { ds_shell_selection_get(&raw mut b) });
        take_buf(b)
    };
    assert_eq!(get(), "", "nothing chosen yet");
    let p = path16(&target);
    // SAFETY: valid view.
    ok(unsafe { ds_shell_selection_set(s16(&p)) });
    let v: Value = serde_json::from_str(&get()).unwrap();
    assert_eq!(v["left"], target.to_str().unwrap());
    let rel = u16s("relative.txt");
    // SAFETY: valid view; the call must fail.
    assert_eq!(
        unsafe { ds_shell_selection_set(s16(&rel)) },
        DS_E_INVALID_ARG
    );
    ok(ds_shell_selection_clear());
    assert_eq!(get(), "");

    let mut info = DsVolumeInfo::default();
    let root = path16(state.path());
    // SAFETY: valid view and out pointer.
    let st = unsafe { ds_volume_info(s16(&root), &raw mut info) };
    if cfg!(windows) {
        assert_eq!(st, DS_OK, "{}", last_error_text());
        assert!(info.total_bytes > 0 && info.free_bytes <= info.total_bytes);
    } else {
        assert_eq!(st, DS_E_UNSUPPORTED);
    }
}

#[test]
fn folds_section_kinds_and_find_on_a_text_compare() {
    let lines: String = (1..=30).map(|i| format!("line {i}\n")).collect();
    let (_d, h) = text_pair(
        &lines,
        &lines
            .replace("line 15\n", "CHANGED\n")
            .replace("line 25\n", ""),
    );
    let (mut w, mut total) = (0u32, 0u32);
    let mut folds = [DsFold::default(); 8];
    // SAFETY: valid buffers and out pointers throughout.
    unsafe {
        ok(ds_text_get_folds(
            h,
            3,
            0,
            0,
            8,
            folds.as_mut_ptr(),
            &raw mut w,
            &raw mut total,
        ));
        assert_eq!((w, total), (3, 3));
        assert_eq!((folds[0].first_row, folds[0].row_count), (0, 11));
        // the same call with a small buffer still reports the total
        ok(ds_text_get_folds(
            h,
            3,
            0,
            1,
            1,
            folds.as_mut_ptr(),
            &raw mut w,
            &raw mut total,
        ));
        assert_eq!((w, total), (1, 3));
        let mut kinds = [9u8; 4];
        ok(ds_text_get_section_kinds(
            h,
            0,
            4,
            kinds.as_mut_ptr(),
            &raw mut w,
        ));
        assert_eq!(
            &kinds[..w as usize],
            [2, 1],
            "a replacement, then a deletion"
        );
        let mut row = 0u32;
        ok(ds_text_find_section(h, 0, 0, 1, &raw mut row));
        assert!(row > 14);
        assert_eq!(
            ds_text_find_section(h, 0, 0, 0, &raw mut row),
            DS_E_NOT_FOUND,
            "no insert-only section"
        );
        assert_eq!(
            ds_text_find_section(h, 0, 0, 7, &raw mut row),
            DS_E_INVALID_ARG
        );
        let mut hit = DsFindHit::default();
        ok(ds_text_find(h, s8("changed"), 0, 0, 0, &raw mut hit));
        assert_eq!(
            (hit.row, hit.line, hit.side, hit.start, hit.end),
            (14, 14, 1, 0, 7)
        );
        assert_eq!(
            ds_text_find(h, s8("changed"), 1, 0, 0, &raw mut hit),
            DS_E_NOT_FOUND,
            "case sensitive"
        );
        assert_eq!(
            ds_text_find(h, s8("("), 4, 0, 0, &raw mut hit),
            DS_E_INVALID_ARG,
            "a bad regular expression"
        );
    }
    ds_close(h);
}
