//! Folder, view, plan, job and file-operation exports (backlog E9-S5, S6).
#![allow(unsafe_code, clippy::wildcard_imports, clippy::too_many_lines)]

mod common;

use std::path::Path;
use std::time::Duration;

use common::*;
use diffsync_core::*;

fn tree(l: &Path, r: &Path) {
    write(l, "same.txt", b"same");
    write(l, "lonly.txt", b"only on the left");
    write(l, "dir/a.txt", b"aaa");
    write(l, "dir/sub/b.txt", b"bb");
    write(r, "same.txt", b"same");
    write(r, "ronly.txt", b"only on the right");
    write(r, "old/x.txt", b"x");
}

fn name_of(f: DsHandle, node: u32, side: u8) -> String {
    let mut len = 0u32;
    // SAFETY: a null buffer with capacity 0 asks for the size.
    let s = unsafe { ds_folder_node_name(f, node, side, std::ptr::null_mut(), 0, &raw mut len) };
    assert!(
        s == DS_E_BUFFER_TOO_SMALL || (s == DS_OK && len == 0),
        "{s}"
    );
    let mut buf = vec![0u16; len as usize];
    // SAFETY: `buf` is valid for `len` units.
    ok(unsafe { ds_folder_node_name(f, node, side, buf.as_mut_ptr(), len, &raw mut len) });
    String::from_utf16(&buf).unwrap()
}

fn view_rows(v: DsHandle) -> Vec<DsDirRow> {
    let (mut count, mut generation) = (0u32, 0u64);
    // SAFETY: both out pointers are valid.
    ok(unsafe { ds_view_row_count(v, &raw mut count, &raw mut generation) });
    let mut rows = vec![DsDirRow::default(); count as usize];
    let mut w = 0u32;
    // SAFETY: `rows` is valid for `count` entries.
    ok(unsafe { ds_view_get_rows(v, 0, count, rows.as_mut_ptr(), &raw mut w) });
    rows.truncate(w as usize);
    rows
}

#[test]
fn folder_status_nodes_names_and_report() {
    let (l, r) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    tree(l.path(), r.path());
    let f = folder_open(l.path(), r.path(), "", NO_CB, std::ptr::null_mut());
    let st = folder_status(f);
    assert_eq!(st.state, 1);
    assert_eq!(st.node_count, 10, "root and nine items");
    assert_eq!(
        (st.left_orphan, st.right_orphan, st.diff),
        (5, 3, 0),
        "{st:?}"
    );
    assert_eq!(st.sync_blocked, 0);
    // node 1.. are the children of the root, folders first
    let mut info = DsDirRow::default();
    // SAFETY: `info` is valid for a write.
    ok(unsafe { ds_folder_node_info(f, 1, &raw mut info) });
    assert_eq!((info.node, info.parent, info.depth), (1, 0, 1));
    assert_eq!(info.left.kind, 1, "a folder");
    assert_eq!(name_of(f, 1, 2), "dir");
    assert_eq!(name_of(f, 1, 0), "dir");
    // SAFETY: valid out pointer.
    assert_eq!(
        unsafe { ds_folder_node_info(f, 9999, &raw mut info) },
        DS_E_NOT_FOUND
    );
    let mut b = DsBuf {
        ptr: std::ptr::null_mut(),
        len: 0,
    };
    // SAFETY: `b` is valid for a write.
    ok(unsafe { ds_folder_report(f, &raw mut b) });
    let rep: serde_json::Value = serde_json::from_str(&take_buf(b)).unwrap();
    assert_eq!(rep["syncBlocked"], false);
    ds_close(f);
}

#[test]
fn a_folder_that_is_still_scanning_is_busy_and_a_bad_root_fails() {
    let (l, r) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    // the same folder on both sides is refused when the compare starts
    let f = folder_open(l.path(), l.path(), "", NO_CB, std::ptr::null_mut());
    assert_eq!(folder_status(f).state, 2, "failed");
    assert!(
        last_error_text().contains("same folder"),
        "{}",
        last_error_text()
    );
    let mut info = DsDirRow::default();
    // SAFETY: valid out pointer.
    let s = unsafe { ds_folder_node_info(f, 0, &raw mut info) };
    assert!(s < 0, "no nodes of a failed compare");
    ds_close(f);
    let _ = r;
}

#[test]
fn views_filter_sort_search_expand_and_follow_changes() {
    let (l, r) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    tree(l.path(), r.path());
    let f = folder_open(l.path(), r.path(), "", NO_CB, std::ptr::null_mut());
    let mut v = 0u64;
    // SAFETY: valid JSON view and out pointer.
    ok(unsafe { ds_view_create(f, s8(""), &raw mut v) });
    let rows = view_rows(v);
    let names: Vec<String> = rows.iter().map(|x| name_of(f, x.node, 2)).collect();
    assert_eq!(
        names,
        ["dir", "old", "lonly.txt", "ronly.txt", "same.txt"],
        "folders first, collapsed"
    );
    // expand `dir`
    let dir_node = rows[0].node;
    assert_eq!(ds_view_expand(v, dir_node, 1, 0), DS_OK);
    assert_eq!(view_rows(v).len(), 5 + 2);
    assert_eq!(ds_view_expand(v, dir_node, 1, 1), DS_OK, "recursive");
    assert_eq!(view_rows(v).len(), 5 + 2 + 1);
    assert_eq!(ds_view_expand(v, dir_node, 0, 0), DS_OK);
    assert_eq!(view_rows(v).len(), 5);
    // filter + search + sort through JSON
    // SAFETY: valid JSON view.
    ok(unsafe { ds_view_set(v, s8(r#"{"filter":"orphans","search":"only"}"#)) });
    let names: Vec<String> = view_rows(v).iter().map(|x| name_of(f, x.node, 2)).collect();
    assert_eq!(names, ["lonly.txt", "ronly.txt"]);
    // SAFETY: valid JSON view.
    ok(unsafe { ds_view_set(v, s8(r#"{"sort":{"key":"name","descending":true}}"#)) });
    let names: Vec<String> = view_rows(v).iter().map(|x| name_of(f, x.node, 2)).collect();
    assert_eq!(names, ["ronly.txt", "lonly.txt"]);
    // SAFETY: valid JSON view; the call must fail.
    assert_eq!(
        unsafe { ds_view_set(v, s8(r#"{"filter":"nonsense"}"#)) },
        DS_E_INVALID_ARG
    );
    // an external change plus refresh: the view's generation moves
    ok(unsafe { ds_view_set(v, s8(r#"{"filter":"all","search":""}"#)) });
    let (mut c0, mut g0) = (0u32, 0u64);
    // SAFETY: valid out pointers.
    ok(unsafe { ds_view_row_count(v, &raw mut c0, &raw mut g0) });
    write(
        r.path(),
        "lonly.txt",
        b"now it exists on the right too, longer",
    );
    let lonly = rows
        .iter()
        .find(|x| name_of(f, x.node, 2) == "lonly.txt")
        .unwrap()
        .node;
    let before = folder_status(f).revision;
    // SAFETY: `nodes` is valid for 1 entry.
    ok(unsafe { ds_folder_refresh(f, &raw const lonly, 1) });
    wait_until("the refresh", || folder_status(f).revision > before);
    let (mut c1, mut g1) = (0u32, 0u64);
    // SAFETY: valid out pointers.
    ok(unsafe { ds_view_row_count(v, &raw mut c1, &raw mut g1) });
    assert!(g1 > g0, "the view rebuilt after the tree changed");
    let mut info = DsDirRow::default();
    // SAFETY: valid out pointer.
    ok(unsafe { ds_folder_node_info(f, lonly, &raw mut info) });
    assert!(info.left.present == 1 && info.right.present == 1);
    assert_ne!(info.status, 5, "no longer a left orphan");
    ds_close(v);
    ds_close(f);
}

#[test]
fn content_compare_runs_in_the_background() {
    let (l, r) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    write(l.path(), "f.txt", b"a\r\nb\r\n");
    write(r.path(), "f.txt", b"a\nb\n");
    let f = folder_open(l.path(), r.path(), "", NO_CB, std::ptr::null_mut());
    let mut info = DsDirRow::default();
    // SAFETY: valid out pointer.
    ok(unsafe { ds_folder_node_info(f, 1, &raw mut info) });
    assert_eq!(info.status, 2, "sizes differ: different");
    let before = folder_status(f).revision;
    let node = 1u32;
    // SAFETY: `nodes` is valid for 1 entry.
    ok(unsafe { ds_folder_compare_content(f, &raw const node, 1) });
    wait_until("the content compare", || folder_status(f).revision > before);
    // SAFETY: valid out pointer.
    ok(unsafe { ds_folder_node_info(f, 1, &raw mut info) });
    assert_eq!(info.status, 1, "unimportant difference (line endings)");
    assert_eq!(info.flags & 2, 2, "content compared");
    ds_close(f);
}

// ---------------------------------------------------------------- sync

fn plan_for(f: DsHandle, rules: &str) -> DsHandle {
    let mut p = 0u64;
    // SAFETY: valid JSON view and out pointer; no selection.
    ok(unsafe { ds_sync_plan(f, s8(rules), std::ptr::null(), 0, &raw mut p) });
    p
}

fn summary(p: DsHandle) -> DsPlanSummary {
    let mut s = DsPlanSummary::default();
    // SAFETY: `s` is valid for a write.
    ok(unsafe { ds_plan_summary(p, &raw mut s) });
    s
}

fn job_status(j: DsHandle) -> DsJobStatus {
    let mut s = DsJobStatus::default();
    // SAFETY: `s` is valid for a write.
    ok(unsafe { ds_job_status(j, &raw mut s) });
    s
}

fn job_result(j: DsHandle) -> serde_json::Value {
    let mut b = DsBuf {
        ptr: std::ptr::null_mut(),
        len: 0,
    };
    // SAFETY: `b` is valid for a write.
    ok(unsafe { ds_job_result(j, &raw mut b) });
    serde_json::from_str(&take_buf(b)).unwrap()
}

#[test]
fn plan_summary_actions_prompts_and_dry_run_report() {
    let (l, r) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    tree(l.path(), r.path());
    write(l.path(), "conflict.txt", b"left version");
    write(r.path(), "conflict.txt", b"right!!!!");
    let f = folder_open(l.path(), r.path(), "", NO_CB, std::ptr::null_mut());
    let p = plan_for(f, r#"{"preset":"updateBoth"}"#);
    let s = summary(p);
    assert_eq!(
        (s.copies_to_right, s.copies_to_left),
        (3 + 1 - 1, 1 + 1),
        "{s:?}"
    );
    assert_eq!(s.conflicts, 1, "conflict.txt is a prompt (D10)");
    assert!(s.action_count >= 1);
    let mut acts = vec![DsPlanAction::default(); s.action_count as usize];
    let mut w = 0u32;
    // SAFETY: `acts` is valid for `action_count` entries.
    ok(unsafe { ds_plan_get_actions(p, 0, s.action_count, acts.as_mut_ptr(), &raw mut w) });
    assert_eq!(w, s.action_count);
    assert!(
        acts.iter().any(|a| a.kind == 1 && a.is_folder == 1),
        "a folder is created on the right"
    );
    // the prompt is data
    let mut nodes = [0u32; 4];
    // SAFETY: `nodes` is valid for 4 entries.
    ok(unsafe { ds_plan_get_prompts(p, 0, 4, nodes.as_mut_ptr(), &raw mut w) });
    assert_eq!(w, 1);
    assert_eq!(name_of(f, nodes[0], 2), "conflict.txt");
    // resolve it: copy to the left
    assert_eq!(ds_plan_set_op(p, nodes[0], 2), DS_OK);
    let s2 = summary(p);
    assert_eq!((s2.conflicts, s2.copies_to_left), (0, s.copies_to_left + 1));
    assert_eq!(ds_plan_set_op(p, nodes[0], 9), DS_E_INVALID_ARG);
    let mut b = DsBuf {
        ptr: std::ptr::null_mut(),
        len: 0,
    };
    // SAFETY: `b` is valid for a write.
    ok(unsafe { ds_plan_report(p, &raw mut b) });
    let rep: serde_json::Value = serde_json::from_str(&take_buf(b)).unwrap();
    assert!(
        rep["lines"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x.as_str().unwrap().starts_with("copy-to-right"))
    );
    ds_close(p);
    ds_close(f);
}

#[test]
fn nested_roots_refuse_to_plan() {
    let d = tempfile::tempdir().unwrap();
    write(d.path(), "inner/f.txt", b"x");
    let f = folder_open(
        d.path(),
        &d.path().join("inner"),
        "",
        NO_CB,
        std::ptr::null_mut(),
    );
    assert_eq!(folder_status(f).sync_blocked, 1);
    let mut p = 0u64;
    // SAFETY: valid JSON view and out pointer; the call must fail.
    let s = unsafe { ds_sync_plan(f, s8(""), std::ptr::null(), 0, &raw mut p) };
    assert_eq!(s, DS_E_INVALID_ARG);
    assert!(
        last_error_text().contains("inside"),
        "{}",
        last_error_text()
    );
    ds_close(f);
}

#[test]
fn a_sync_job_runs_and_reports_its_result() {
    let (l, r) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    tree(l.path(), r.path());
    let events = new_events();
    let f = folder_open(l.path(), r.path(), "", NO_CB, std::ptr::null_mut());
    let p = plan_for(f, r#"{"preset":"mirrorToRight"}"#);
    let mut j = 0u64;
    let log_dir = tempfile::tempdir().unwrap();
    let exec = format!(
        r#"{{"logDir":{}}}"#,
        serde_json::to_string(log_dir.path().to_str().unwrap()).unwrap()
    );
    // SAFETY: valid views, callback and out pointer; `events` outlives the job.
    ok(unsafe { ds_plan_execute(p, s8(&exec), Some(record), user_of(&events), &raw mut j) });
    wait_until("the job", || job_status(j).state == 2);
    let st = job_status(j);
    assert_eq!(
        (st.failed, st.skipped, st.aborted, st.cancelled),
        (0, 0, 0, 0),
        "{st:?}"
    );
    assert_eq!(
        std::fs::read(r.path().join("dir/sub/b.txt")).unwrap(),
        b"bb"
    );
    assert!(!r.path().join("old").exists());
    assert!(!r.path().join("ronly.txt").exists());
    let res = job_result(j);
    assert_eq!(res["failed"], 0);
    assert!(res["done"].as_u64().unwrap() >= 5);
    // the tree followed the job: a fresh plan has nothing to do
    wait_until("the tree to settle", || {
        summary(plan_for(f, r#"{"preset":"mirrorToRight"}"#)).action_count == 0
    });
    // events: progress and a finished state, on a core thread
    std::thread::sleep(Duration::from_millis(50));
    let ev = events.lock().clone();
    assert!(ev.iter().any(|e| e.kind == DS_EV_PROGRESS));
    assert!(ev.iter().any(|e| e.kind == DS_EV_STATE && e.a == 2));
    assert!(ev.iter().all(|e| e.thread != std::thread::current().id()));
    assert_eq!(
        std::fs::read_dir(log_dir.path()).unwrap().count(),
        1,
        "the log was written"
    );
    ds_close(j);
    ds_close(p);
    ds_close(f);
}

/// Backlog E9-S6.2: the question flow works from a different thread than the event callback.
#[test]
fn a_question_is_answered_from_another_thread() {
    let (l, r) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    write(l.path(), "f.txt", b"left");
    write(r.path(), "f.txt", b"old right");
    let f = folder_open(l.path(), r.path(), "", NO_CB, std::ptr::null_mut());
    let p = plan_for(f, r#"{"preset":"mirrorToRight"}"#);
    // the destination changes between plan and execute (plan.md §9.1)
    write(r.path(), "f.txt", b"edited meanwhile");
    let events = new_events();
    let mut j = 0u64;
    // SAFETY: valid views, callback and out pointer.
    ok(unsafe { ds_plan_execute(p, s8(""), Some(record), user_of(&events), &raw mut j) });
    wait_until("the question event", || {
        events.lock().iter().any(|e| e.kind == DS_EV_QUESTION)
    });
    let (qid, callback_thread) = {
        let ev = events.lock();
        let e = ev.iter().find(|e| e.kind == DS_EV_QUESTION).unwrap();
        (e.a, e.thread)
    };
    assert_ne!(callback_thread, std::thread::current().id());
    assert_eq!(job_status(j).has_question, 1);
    let mut q = DsQuestion::default();
    let mut len = 0u32;
    // SAFETY: a null message buffer with capacity 0 asks for the size.
    let s = unsafe { ds_job_get_question(j, &raw mut q, std::ptr::null_mut(), 0, &raw mut len) };
    assert_eq!(s, DS_E_BUFFER_TOO_SMALL);
    assert_eq!((q.id, q.kind), (qid, 0), "changed since compare");
    let mut msg = vec![0u16; len as usize];
    // SAFETY: `msg` is valid for `len` units.
    ok(unsafe { ds_job_get_question(j, &raw mut q, msg.as_mut_ptr(), len, &raw mut len) });
    assert!(String::from_utf16(&msg).unwrap().contains("changed"));
    // a wrong id is refused; the right one is answered from this (the test's own) thread
    assert_eq!(ds_job_answer(j, qid + 100, 1), DS_E_NOT_FOUND);
    assert_eq!(ds_job_answer(j, qid, 7), DS_E_INVALID_ARG);
    assert_eq!(ds_job_answer(j, qid, 1), DS_OK, "skip");
    wait_until("the job", || job_status(j).state == 2);
    let res = job_result(j);
    assert_eq!(
        (res["skipped"].as_u64(), res["done"].as_u64()),
        (Some(1), Some(0))
    );
    assert_eq!(
        std::fs::read(r.path().join("f.txt")).unwrap(),
        b"edited meanwhile"
    );
    // pause / resume / cancel on a finished job are harmless
    assert_eq!(ds_job_pause(j), DS_OK);
    assert_eq!(ds_job_resume(j), DS_OK);
    assert_eq!(ds_job_cancel(j), DS_OK);
    ds_close(j);
    ds_close(p);
    ds_close(f);
}

#[test]
fn closing_a_running_job_cancels_it_and_a_result_before_the_end_is_busy() {
    let (l, r) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let big = vec![7u8; 24 << 20];
    for i in 0..8 {
        write(l.path(), &format!("f{i}.bin"), &big);
    }
    let f = folder_open(l.path(), r.path(), "", NO_CB, std::ptr::null_mut());
    let p = plan_for(f, r#"{"preset":"updateRight"}"#);
    let mut j = 0u64;
    // SAFETY: valid views and out pointer.
    ok(unsafe { ds_plan_execute(p, s8(""), NO_CB, std::ptr::null_mut(), &raw mut j) });
    let mut b = DsBuf {
        ptr: std::ptr::null_mut(),
        len: 0,
    };
    // SAFETY: `b` is valid for a write.
    let s = unsafe { ds_job_result(j, &raw mut b) };
    if s == DS_E_BUSY {
        assert_eq!(ds_job_pause(j), DS_OK);
        assert_eq!(ds_job_resume(j), DS_OK);
    } else {
        take_buf(b); // a very fast disk finished it already
    }
    assert_eq!(ds_close(j), DS_OK);
    assert_eq!(ds_close(j), DS_E_INVALID_HANDLE);
    // no temp file is left behind a cancelled copy
    std::thread::sleep(Duration::from_millis(300));
    let temps: Vec<_> = std::fs::read_dir(r.path())
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".dctmp~"))
        .collect();
    assert!(temps.is_empty(), "{temps:?}");
    ds_close(p);
    ds_close(f);
}

// ---------------------------------------------------------------- file operations

fn list_of(paths: &[Vec<u16>]) -> (Vec<DsStr16>, DsStr16List) {
    let v: Vec<DsStr16> = paths.iter().map(|p| s16(p)).collect();
    let l = DsStr16List {
        items: v.as_ptr(),
        count: v.len(),
    };
    (v, l)
}

fn run_job(j: DsHandle) -> serde_json::Value {
    wait_until("the file operation", || job_status(j).state == 2);
    let r = job_result(j);
    ds_close(j);
    r
}

#[test]
fn file_operations_copy_move_and_delete() {
    let d = tempfile::tempdir().unwrap();
    write(d.path(), "src/a.txt", b"aaa");
    write(d.path(), "src/dir/b.txt", b"bb");
    std::fs::create_dir_all(d.path().join("dst")).unwrap();
    std::fs::create_dir_all(d.path().join("dst2")).unwrap();
    let items = [
        path16(&d.path().join("src").join("a.txt")),
        path16(&d.path().join("src").join("dir")),
    ];
    let (_keep, list) = list_of(&items);
    let dst = path16(&d.path().join("dst"));
    let mut j = 0u64;
    // SAFETY: valid list, views and out pointer.
    ok(unsafe {
        ds_fileops_copy(
            list,
            s16(&dst),
            s8(""),
            NO_CB,
            std::ptr::null_mut(),
            &raw mut j,
        )
    });
    assert_eq!(run_job(j)["failed"], 0);
    assert_eq!(std::fs::read(d.path().join("dst/a.txt")).unwrap(), b"aaa");
    assert_eq!(
        std::fs::read(d.path().join("dst/dir/b.txt")).unwrap(),
        b"bb"
    );
    assert!(d.path().join("src/a.txt").exists());
    // move inside one volume: a rename
    let dst2 = path16(&d.path().join("dst2"));
    // SAFETY: valid list, views and out pointer.
    ok(unsafe {
        ds_fileops_move(
            list,
            s16(&dst2),
            s8(""),
            NO_CB,
            std::ptr::null_mut(),
            &raw mut j,
        )
    });
    assert_eq!(run_job(j)["failed"], 0);
    assert!(!d.path().join("src/a.txt").exists() && d.path().join("dst2/a.txt").exists());
    // delete
    let gone = [
        path16(&d.path().join("dst2").join("dir")),
        path16(&d.path().join("dst2").join("a.txt")),
    ];
    let (_k2, l2) = list_of(&gone);
    // SAFETY: valid list, views and out pointer.
    ok(unsafe { ds_fileops_delete(l2, s8(""), NO_CB, std::ptr::null_mut(), &raw mut j) });
    assert_eq!(run_job(j)["failed"], 0);
    assert!(
        std::fs::read_dir(d.path().join("dst2"))
            .unwrap()
            .next()
            .is_none()
    );
    // a folder into itself is refused before anything runs
    let inner = path16(&d.path().join("dst").join("dir"));
    let outer = [path16(&d.path().join("dst"))];
    let (_k3, l3) = list_of(&outer);
    // SAFETY: valid list, views and out pointer; the call must fail.
    let s = unsafe {
        ds_fileops_copy(
            l3,
            s16(&inner),
            s8(""),
            NO_CB,
            std::ptr::null_mut(),
            &raw mut j,
        )
    };
    assert_eq!(s, DS_E_INVALID_ARG);
}

#[test]
fn bad_options_and_names_for_file_operations() {
    let d = tempfile::tempdir().unwrap();
    write(d.path(), "a.txt", b"1");
    let items = [path16(&d.path().join("a.txt"))];
    let (_k, list) = list_of(&items);
    let dst = path16(d.path());
    let mut j = 0u64;
    // SAFETY: valid list, views and out pointer; the call must fail.
    let s = unsafe {
        ds_fileops_copy(
            list,
            s16(&dst),
            s8("{broken"),
            NO_CB,
            std::ptr::null_mut(),
            &raw mut j,
        )
    };
    assert_eq!(s, DS_E_INVALID_ARG);
    // copying onto an existing name without overwrite
    write(d.path(), "sub/a.txt", b"already here");
    let sub = path16(&d.path().join("sub"));
    // SAFETY: as above.
    let s = unsafe {
        ds_fileops_copy(
            list,
            s16(&sub),
            s8(""),
            NO_CB,
            std::ptr::null_mut(),
            &raw mut j,
        )
    };
    assert_eq!(s, DS_E_ALREADY_EXISTS);
    // a file onto itself is refused even with overwrite
    // SAFETY: as above.
    let s = unsafe {
        ds_fileops_copy(
            list,
            s16(&dst),
            s8(r#"{"overwrite":true}"#),
            NO_CB,
            std::ptr::null_mut(),
            &raw mut j,
        )
    };
    assert_eq!(s, DS_E_INVALID_ARG);
}
