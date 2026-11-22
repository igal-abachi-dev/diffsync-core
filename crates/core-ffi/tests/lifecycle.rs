//! Handle lifecycle, event delivery contract and misuse (reviewer findings on E9, decision D51).
#![allow(unsafe_code, clippy::wildcard_imports, clippy::too_many_lines)]

mod common;

use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use common::*;
use diffsync_core::*;

fn big_tree(dir: &std::path::Path, n: usize) -> (std::path::PathBuf, std::path::PathBuf) {
    let (l, r) = (dir.join("L"), dir.join("R"));
    for i in 0..n {
        write(&l, &format!("d{}/f{i}.txt", i % 20), b"x");
        if i % 3 != 0 {
            write(&r, &format!("d{}/f{i}.txt", i % 20), b"y");
        }
    }
    (l, r)
}

/// The BLOCKER: after `ds_close` returns, no callback runs or starts, so a host may free `user`.
#[test]
fn no_events_after_close_for_folder_job_and_text() {
    let d = tempfile::tempdir().unwrap();
    // folder scan closed while it runs
    let (l, r) = big_tree(d.path(), 6000);
    let events = new_events();
    let (lw, rw) = (path16(&l), path16(&r));
    let mut f = 0u64;
    // SAFETY: valid views, callback and out pointer; `events` outlives the handle.
    ok(unsafe {
        ds_folder_open(
            s16(&lw),
            s16(&rw),
            s8(""),
            Some(record),
            user_of(&events),
            &raw mut f,
        )
    });
    assert_eq!(ds_close(f), DS_OK);
    let after_close = events.lock().len();
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(
        events.lock().len(),
        after_close,
        "no event after ds_close (folder)"
    );

    // a job closed while it runs
    let (l2, r2) = (d.path().join("JL"), d.path().join("JR"));
    for i in 0..40 {
        write(&l2, &format!("f{i}.bin"), &vec![1u8; 2 << 20]);
    }
    std::fs::create_dir_all(&r2).unwrap();
    let f2 = folder_open(&l2, &r2, "", NO_CB, std::ptr::null_mut());
    let mut p = 0u64;
    // SAFETY: valid views and out pointer.
    ok(unsafe {
        ds_sync_plan(
            f2,
            s8(r#"{"preset":"updateRight"}"#),
            std::ptr::null(),
            0,
            &raw mut p,
        )
    });
    let jev = new_events();
    let mut j = 0u64;
    // SAFETY: valid views, callback and out pointer.
    ok(unsafe { ds_plan_execute(p, s8(""), Some(record), user_of(&jev), &raw mut j) });
    wait_until("the first job event", || !jev.lock().is_empty());
    assert_eq!(ds_close(j), DS_OK);
    let n = jev.lock().len();
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(jev.lock().len(), n, "no event after ds_close (job)");
    let temps: Vec<_> = std::fs::read_dir(&r2)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".dctmp~"))
        .collect();
    assert!(
        temps.is_empty(),
        "closing cancelled the copy cleanly: {temps:?}"
    );

    // a text compare closed right after open
    let tev = new_events();
    let big: String = (0..50_000).map(|i| format!("line {i}\n")).collect();
    write(d.path(), "a.txt", big.as_bytes());
    write(
        d.path(),
        "b.txt",
        big.replace("line 7\n", "other\n").as_bytes(),
    );
    let (a, b) = (
        path16(&d.path().join("a.txt")),
        path16(&d.path().join("b.txt")),
    );
    let mut t = 0u64;
    // SAFETY: valid views, callback and out pointer.
    ok(unsafe {
        ds_text_open(
            s16(&a),
            s16(&b),
            s8(""),
            Some(record),
            user_of(&tev),
            &raw mut t,
        )
    });
    assert_eq!(ds_close(t), DS_OK);
    let n = tev.lock().len();
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(tev.lock().len(), n, "no event after ds_close (text)");
    ds_close(p);
    ds_close(f2);
}

static CLOSER: AtomicU64 = AtomicU64::new(0);
static CLOSED_OK: AtomicU64 = AtomicU64::new(0);

unsafe extern "C" fn close_in_callback(_u: *mut c_void, src: u64, _kind: u32, _a: u64, _b: u64) {
    // the callback closes its own handle: detach must not wait for itself
    if CLOSER.swap(1, Ordering::SeqCst) == 0 {
        let s = ds_close(src);
        CLOSED_OK.store(u64::from(s == DS_OK), Ordering::SeqCst);
    }
}

/// A callback may call any API function, including `ds_close` of its own handle (no deadlock).
#[test]
fn a_callback_can_close_its_own_handle() {
    let d = tempfile::tempdir().unwrap();
    write(d.path(), "a.txt", b"a\nb\n");
    write(d.path(), "b.txt", b"a\nc\n");
    let (a, b) = (
        path16(&d.path().join("a.txt")),
        path16(&d.path().join("b.txt")),
    );
    let (tx, rx) = std::sync::mpsc::channel();
    let (a2, b2) = (a.clone(), b.clone());
    std::thread::spawn(move || {
        let mut t = 0u64;
        // SAFETY: valid views, callback and out pointer.
        let s = unsafe {
            ds_text_open(
                s16(&a2),
                s16(&b2),
                s8(""),
                Some(close_in_callback),
                std::ptr::null_mut(),
                &raw mut t,
            )
        };
        tx.send(s).unwrap();
    });
    assert_eq!(rx.recv_timeout(Duration::from_secs(20)).unwrap(), DS_OK);
    wait_until("the callback to close the handle", || {
        CLOSER.load(Ordering::SeqCst) == 1
    });
    wait_until("the close result", || CLOSED_OK.load(Ordering::SeqCst) == 1);
}

/// `ds_text_recompare` never calls the callback on the caller's thread, and a callback that
/// recompares its own session does not hang.
#[test]
fn recompare_events_come_from_a_core_thread() {
    let d = tempfile::tempdir().unwrap();
    write(d.path(), "a.txt", b"a  b\nc\n");
    write(d.path(), "b.txt", b"a b\nc\n");
    let (a, b) = (
        path16(&d.path().join("a.txt")),
        path16(&d.path().join("b.txt")),
    );
    let events = new_events();
    let mut t = 0u64;
    // SAFETY: valid views, callback and out pointer.
    ok(unsafe {
        ds_text_open(
            s16(&a),
            s16(&b),
            s8(""),
            Some(record),
            user_of(&events),
            &raw mut t,
        )
    });
    wait_until("done", || {
        let mut st = DsTextStatus::default();
        // SAFETY: `st` is valid for a write.
        unsafe { ds_text_status(t, &raw mut st) };
        st.state == 2
    });
    events.lock().clear();
    let me = std::thread::current().id();
    let mut st = DsTextStatus::default();
    // SAFETY: `st` is valid for a write.
    unsafe { ds_text_status(t, &raw mut st) };
    let g0 = st.generation;
    // SAFETY: valid JSON view.
    ok(unsafe { ds_text_recompare(t, s8(r#"{"whitespace":"changes"}"#)) });
    // the generation moved at once, before any old row went away
    // SAFETY: `st` is valid for a write.
    unsafe { ds_text_status(t, &raw mut st) };
    assert!(
        st.generation > g0,
        "generation bumped by the recompare call itself"
    );
    wait_until("the recompare", || {
        // SAFETY: `st` is valid for a write.
        unsafe { ds_text_status(t, &raw mut st) };
        st.state == 2 && st.important_diff == 0
    });
    std::thread::sleep(Duration::from_millis(100));
    assert!(
        events.lock().iter().all(|e| e.thread != me),
        "no callback on the caller's thread"
    );
    assert!(events.lock().iter().any(|e| e.kind == DS_EV_STATE));
    ds_close(t);
}

/// Events keep their order and are never delivered concurrently.
#[test]
fn events_are_serialised_and_ordered() {
    let d = tempfile::tempdir().unwrap();
    let (l, r) = big_tree(d.path(), 3000);
    let events = new_events();
    let f = folder_open(&l, &r, "", Some(record), user_of(&events));
    let _ = f;
    std::thread::sleep(Duration::from_millis(100));
    let ev = events.lock().clone();
    // progress `a` never goes backwards, the final state is the last forced event
    let mut last = 0;
    for e in ev.iter().filter(|e| e.kind == DS_EV_PROGRESS) {
        assert!(e.a >= last, "progress went backwards: {} after {last}", e.a);
        last = e.a;
    }
    let state_pos = ev
        .iter()
        .rposition(|e| e.kind == DS_EV_STATE)
        .expect("a final state event");
    assert_eq!(ev[state_pos].a, 1, "done");
    ds_close(f);
}

// ---------------------------------------------------------------- misuse

#[test]
fn wrong_kind_handles_and_null_out_parameters_are_errors() {
    let d = tempfile::tempdir().unwrap();
    let (l, r) = big_tree(d.path(), 30);
    let f = folder_open(&l, &r, "", NO_CB, std::ptr::null_mut());
    let mut v = 0u64;
    // SAFETY: valid views and out pointer.
    ok(unsafe { ds_view_create(f, s8(""), &raw mut v) });
    let mut p = 0u64;
    // SAFETY: valid views and out pointer.
    ok(unsafe { ds_sync_plan(f, s8(""), std::ptr::null(), 0, &raw mut p) });
    // SAFETY: deliberately wrong handles and null out-parameters; the core must refuse them.
    unsafe {
        let mut row = DsDirRow::default();
        let mut written = 0u32;
        assert_eq!(
            ds_view_get_rows(f, 0, 1, &raw mut row, std::ptr::null_mut()),
            DS_E_INVALID_HANDLE,
            "folder handle used as a view"
        );
        assert_eq!(
            ds_view_row_count(p, std::ptr::null_mut(), std::ptr::null_mut()),
            DS_E_INVALID_HANDLE,
            "plan handle used as a view"
        );
        assert_eq!(
            ds_view_row_count(v, std::ptr::null_mut(), std::ptr::null_mut()),
            DS_E_INVALID_ARG
        );
        assert_eq!(
            ds_plan_summary(v, std::ptr::null_mut()),
            DS_E_INVALID_HANDLE
        );
        assert_eq!(ds_plan_summary(p, std::ptr::null_mut()), DS_E_INVALID_ARG);
        assert_eq!(ds_job_status(p, std::ptr::null_mut()), DS_E_INVALID_HANDLE);
        assert_eq!(ds_folder_status(f, std::ptr::null_mut()), DS_E_INVALID_ARG);
        assert_eq!(
            ds_folder_node_info(f, 0, std::ptr::null_mut()),
            DS_E_INVALID_ARG
        );
        assert_eq!(
            ds_plan_get_actions(p, 0, 3, std::ptr::null_mut(), &raw mut written),
            DS_E_INVALID_ARG,
            "null buffer with a count"
        );
        assert_eq!(ds_job_answer(p, 1, 1), DS_E_INVALID_HANDLE);
        assert_eq!(ds_job_cancel(f), DS_E_INVALID_HANDLE);
        assert_eq!(ds_view_expand(f, 0, 1, 0), DS_E_INVALID_HANDLE);
    }
    for h in [v, p, f] {
        assert_eq!(ds_close(h), DS_OK);
    }
}

#[test]
fn file_operation_paths_and_log_dirs_are_validated() {
    let d = tempfile::tempdir().unwrap();
    write(d.path(), "a.txt", b"1");
    let dst = path16(d.path());
    let good = path16(&d.path().join("a.txt"));
    let mut j = 0u64;
    let copy = |items: &[Vec<u16>], opts: &str, j: &mut u64| -> DsStatus {
        let views: Vec<DsStr16> = items.iter().map(|p| s16(p)).collect();
        let list = DsStr16List {
            items: views.as_ptr(),
            count: views.len(),
        };
        // SAFETY: valid list, views and out pointer.
        unsafe { ds_fileops_copy(list, s16(&dst), s8(opts), NO_CB, std::ptr::null_mut(), j) }
    };
    // an unpaired surrogate would become a different name: refused, never mangled
    let mut bad = good.clone();
    bad.push(0xD800);
    assert_eq!(copy(&[bad], "", &mut j), DS_E_INVALID_ARG);
    assert!(
        last_error_text().contains("surrogate"),
        "{}",
        last_error_text()
    );
    // a relative item path
    assert_eq!(copy(&[u16s("a.txt")], "", &mut j), DS_E_INVALID_ARG);
    // an empty log folder means "none"; a relative one is refused (never the process current directory)
    assert_eq!(
        copy(
            std::slice::from_ref(&good),
            r#"{"logDir":"relative\\logs"}"#,
            &mut j
        ),
        DS_E_INVALID_ARG
    );
    assert!(
        last_error_text().contains("absolute"),
        "{}",
        last_error_text()
    );
    // a job name is part of the log file name: separators, ':' and '..' would leave `logDir`
    let logs = tempfile::tempdir().unwrap();
    let log_json = |name: &str| {
        serde_json::json!({ "logDir": logs.path().to_str().unwrap(), "jobName": name }).to_string()
    };
    for bad in ["..", "a/b", "a\\b", "C:x", "x..y"] {
        assert_eq!(
            copy(std::slice::from_ref(&good), &log_json(bad), &mut j),
            DS_E_INVALID_ARG,
            "{bad}"
        );
    }
    // the lone-surrogate root of a folder compare is refused as well
    let mut f = 0u64;
    let mut root = path16(d.path());
    root.push(0xDC00);
    // SAFETY: valid views and out pointer; the call must fail.
    let s = unsafe {
        ds_folder_open(
            s16(&root),
            s16(&dst),
            s8(""),
            NO_CB,
            std::ptr::null_mut(),
            &raw mut f,
        )
    };
    assert_eq!(s, DS_E_INVALID_ARG);
    // a relative text path
    // SAFETY: valid views and out pointer; the call must fail.
    let s = unsafe {
        ds_text_open(
            s16(&u16s("a.txt")),
            s16(&u16s("b.txt")),
            s8(""),
            NO_CB,
            std::ptr::null_mut(),
            &raw mut f,
        )
    };
    assert_eq!(s, DS_E_INVALID_ARG);
}

/// Closing while other threads read: handles are `Arc`s inside, so nothing is freed under a reader.
#[test]
fn closing_while_other_threads_read_is_safe() {
    let d = tempfile::tempdir().unwrap();
    let (l, r) = big_tree(d.path(), 2000);
    let f = folder_open(&l, &r, "", NO_CB, std::ptr::null_mut());
    let mut v = 0u64;
    // SAFETY: valid views and out pointer.
    ok(unsafe { ds_view_create(f, s8(""), &raw mut v) });
    let stop = std::sync::atomic::AtomicBool::new(false);
    std::thread::scope(|s| {
        for _ in 0..4 {
            s.spawn(|| {
                let mut rows = vec![DsDirRow::default(); 50];
                let (mut w, mut c, mut g) = (0u32, 0u32, 0u64);
                while !stop.load(Ordering::Relaxed) {
                    // SAFETY: valid buffers; the handle may be closed at any time (an error, never a crash).
                    unsafe {
                        let _ = ds_view_row_count(v, &raw mut c, &raw mut g);
                        let _ = ds_view_get_rows(v, 0, 50, rows.as_mut_ptr(), &raw mut w);
                    }
                }
            });
        }
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(ds_close(v), DS_OK);
        assert_eq!(ds_close(f), DS_OK);
        std::thread::sleep(Duration::from_millis(50));
        stop.store(true, Ordering::Relaxed);
    });
}

/// The numbers that cross the ABI are pinned (docs/abi.md, "Enum values").
#[test]
fn enum_numbers_are_pinned() {
    use core_diff::RowKind;
    use core_folder::Status;
    use core_sync::ActionKind;
    assert_eq!(
        [
            RowKind::Same,
            RowKind::Unimportant,
            RowKind::Different,
            RowKind::LeftOnly,
            RowKind::RightOnly,
            RowKind::UnimportantLeftOnly,
            RowKind::UnimportantRightOnly
        ]
        .map(|k| k as u8),
        [0, 1, 2, 3, 4, 5, 6]
    );
    assert_eq!(
        [
            Status::Same,
            Status::UnimportantDiff,
            Status::Different,
            Status::LeftNewer,
            Status::RightNewer,
            Status::LeftOrphan,
            Status::RightOrphan,
            Status::Error
        ]
        .map(|k| k as u8),
        [0, 1, 2, 3, 4, 5, 6, 7]
    );
    assert_eq!(
        [
            ActionKind::CreateFolderLeft,
            ActionKind::CreateFolderRight,
            ActionKind::CopyToLeft,
            ActionKind::CopyToRight,
            ActionKind::DeleteLeft,
            ActionKind::DeleteRight
        ]
        .map(|k| k as u8),
        [0, 1, 2, 3, 4, 5]
    );
    assert_eq!(
        [
            DS_E_INVALID_ARG,
            DS_E_INVALID_HANDLE,
            DS_E_CANCELLED,
            DS_E_NOT_FOUND,
            DS_E_BUFFER_TOO_SMALL,
            DS_E_BUSY,
            DS_E_INTERNAL
        ],
        [-1, -2, -3, -4, -6, -7, -8]
    );
    let _ = Instant::now();
}

static SLOW_IN: AtomicU64 = AtomicU64::new(0);
static SLOW_OVERLAP: AtomicU64 = AtomicU64::new(0);
static SLOW_STARTED: AtomicU64 = AtomicU64::new(0);
static SLOW_ENDED: AtomicU64 = AtomicU64::new(0);

unsafe extern "C" fn slow_callback(_u: *mut c_void, _src: u64, _kind: u32, _a: u64, _b: u64) {
    SLOW_STARTED.fetch_add(1, Ordering::SeqCst);
    if SLOW_IN.swap(1, Ordering::SeqCst) != 0 {
        SLOW_OVERLAP.fetch_add(1, Ordering::SeqCst); // two callbacks at once
    }
    std::thread::sleep(Duration::from_millis(150));
    SLOW_IN.store(0, Ordering::SeqCst);
    SLOW_ENDED.fetch_add(1, Ordering::SeqCst);
}

/// D51, the central promise: callbacks of one handle never overlap, and `ds_close` returns only
/// after a callback that runs on another thread has ended, so the host may free `user` right
/// away. (Reviewer finding, E11-S6: the counting tests above cannot see a callback in flight.)
#[test]
fn close_waits_for_a_running_callback_and_callbacks_never_overlap() {
    let d = tempfile::tempdir().unwrap();
    let body: String = (0..200_000).map(|i| format!("line {i}\n")).collect();
    write(d.path(), "a.txt", body.as_bytes());
    write(
        d.path(),
        "b.txt",
        body.replace("line 7\n", "other\n").as_bytes(),
    );
    let (a, b) = (
        path16(&d.path().join("a.txt")),
        path16(&d.path().join("b.txt")),
    );
    let mut t = 0u64;
    // SAFETY: valid views, callback and out pointer.
    ok(unsafe {
        ds_text_open(
            s16(&a),
            s16(&b),
            s8(""),
            Some(slow_callback),
            std::ptr::null_mut(),
            &raw mut t,
        )
    });
    wait_until("a callback to be running", || {
        SLOW_STARTED.load(Ordering::SeqCst) >= 1
    });
    // a callback is (or was just) in flight on a core thread: close from here
    assert_eq!(ds_close(t), DS_OK);
    assert_eq!(
        SLOW_STARTED.load(Ordering::SeqCst),
        SLOW_ENDED.load(Ordering::SeqCst),
        "ds_close returned while a callback was still running"
    );
    let n = SLOW_STARTED.load(Ordering::SeqCst);
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        SLOW_STARTED.load(Ordering::SeqCst),
        n,
        "no callback starts after ds_close returned"
    );
    assert_eq!(
        SLOW_OVERLAP.load(Ordering::SeqCst),
        0,
        "callbacks of one handle overlapped"
    );
}

/// Found by the random-call campaign (E11-S6): node numbers outside the tree are refused up
/// front instead of panicking later on a worker thread and leaving a batch that did nothing.
#[test]
fn node_numbers_outside_the_tree_are_invalid_arguments() {
    let d = tempfile::tempdir().unwrap();
    let (l, r) = big_tree(d.path(), 10);
    let f = folder_open(&l, &r, "", NO_CB, std::ptr::null_mut());
    let bad = [9_999u32];
    let mixed = [1u32, 9_999];
    // SAFETY: valid arrays and out pointers; the calls must fail.
    unsafe {
        assert_eq!(ds_folder_refresh(f, bad.as_ptr(), 1), DS_E_INVALID_ARG);
        assert_eq!(
            ds_folder_compare_content(f, mixed.as_ptr(), 2),
            DS_E_INVALID_ARG
        );
        let mut p = 0u64;
        assert_eq!(
            ds_sync_plan(f, s8(""), bad.as_ptr(), 1, &raw mut p),
            DS_E_INVALID_ARG
        );
        // valid numbers still work
        let ok_nodes = [0u32];
        assert_eq!(ds_folder_refresh(f, ok_nodes.as_ptr(), 1), DS_OK);
    }
    ds_close(f);
}

/// Plan §9.2, `ds_maintenance_sweep`: lists leftover `.dctmp~` files, deletes them only on
/// request, never touches other files, and refuses a relative root.
#[test]
fn the_maintenance_sweep_lists_and_deletes_only_temp_files() {
    let d = tempfile::tempdir().unwrap();
    write(d.path(), "keep.txt", b"keep");
    write(d.path(), "sub/a.txt.1a2b3c.dctmp~", b"half");
    write(d.path(), "b.bin.00ff00.dctmp~", b"x");
    write(d.path(), "looks.dctmp~.txt", b"not a temp file");
    let root = path16(d.path());
    let run = |delete: u8| -> serde_json::Value {
        let mut b = DsBuf {
            ptr: std::ptr::null_mut(),
            len: 0,
        };
        // SAFETY: valid view and out pointer.
        ok(unsafe { ds_maintenance_sweep(s16(&root), delete, &raw mut b) });
        // SAFETY: the buffer was just returned by the core; it is freed below.
        let text = unsafe { std::slice::from_raw_parts(b.ptr, b.len) }.to_vec();
        // SAFETY: `b` came from the core and is freed exactly once.
        unsafe { ds_buf_free(b) };
        serde_json::from_slice(&text).unwrap()
    };
    let listed = run(0);
    assert_eq!(listed["found"].as_array().unwrap().len(), 2, "{listed}");
    assert_eq!(listed["deleted"], 0);
    assert!(
        d.path().join("b.bin.00ff00.dctmp~").exists(),
        "listing deletes nothing"
    );
    let deleted = run(1);
    assert_eq!(deleted["deleted"], 2, "{deleted}");
    assert!(!d.path().join("b.bin.00ff00.dctmp~").exists());
    assert!(!d.path().join("sub").join("a.txt.1a2b3c.dctmp~").exists());
    assert!(d.path().join("keep.txt").exists() && d.path().join("looks.dctmp~.txt").exists());
    let rel = u16s("relative");
    let mut b = DsBuf {
        ptr: std::ptr::null_mut(),
        len: 0,
    };
    // SAFETY: valid view and out pointer; the call must fail.
    assert_eq!(
        unsafe { ds_maintenance_sweep(s16(&rel), 0, &raw mut b) },
        DS_E_INVALID_ARG
    );
}

/// Reviewer finding (E11-S6): a plan runs once. A second `ds_plan_execute` is refused (it used to
/// start a second job on the same plan), and the plan cannot be edited after it was claimed.
#[test]
fn a_plan_executes_once() {
    let d = tempfile::tempdir().unwrap();
    let (l, r) = (d.path().join("L"), d.path().join("R"));
    write(&l, "f.txt", b"x");
    std::fs::create_dir_all(&r).unwrap();
    let f = folder_open(&l, &r, "", NO_CB, std::ptr::null_mut());
    let mut p = 0u64;
    // SAFETY: valid views and out pointer.
    ok(unsafe {
        ds_sync_plan(
            f,
            s8(r#"{"preset":"updateRight"}"#),
            std::ptr::null(),
            0,
            &raw mut p,
        )
    });
    let (mut j1, mut j2) = (0u64, 0u64);
    // SAFETY: valid views and out pointers.
    unsafe {
        ok(ds_plan_execute(
            p,
            s8(""),
            NO_CB,
            std::ptr::null_mut(),
            &raw mut j1,
        ));
        assert_eq!(
            ds_plan_execute(p, s8(""), NO_CB, std::ptr::null_mut(), &raw mut j2),
            DS_E_BUSY
        );
    }
    assert_eq!(ds_plan_set_op(p, 1, 0), DS_E_BUSY);
    for h in [j1, p, f] {
        ds_close(h);
    }
}

/// Reviewer finding (E11-S6): a `ds_view_set` that fails (a bad filter name) changes nothing; the
/// search and the sort that came before the bad field in the document used to be applied anyway.
#[test]
fn a_failed_view_set_leaves_the_view_unchanged() {
    let d = tempfile::tempdir().unwrap();
    let (l, r) = big_tree(d.path(), 30);
    let f = folder_open(&l, &r, "", NO_CB, std::ptr::null_mut());
    let mut v = 0u64;
    // SAFETY: valid views and out pointers throughout.
    unsafe {
        ok(ds_view_create(f, s8(""), &raw mut v));
        let count = |v| {
            let (mut n, mut g) = (0u32, 0u64);
            ok(ds_view_row_count(v, &raw mut n, &raw mut g));
            n
        };
        ok(ds_view_set(v, s8(r#"{"search":"f1"}"#)));
        let before = count(v);
        assert!(before > 0);
        assert_eq!(
            ds_view_set(
                v,
                s8(r#"{"search":"no-such-name-zzz","filter":"nonsense"}"#)
            ),
            DS_E_INVALID_ARG
        );
        assert_eq!(
            count(v),
            before,
            "the search of the failed call was applied"
        );
        assert_eq!(
            ds_view_set(v, s8(r#"{"sort":{"key":"bogus"},"search":"zzz"}"#)),
            DS_E_INVALID_ARG
        );
        assert_eq!(count(v), before, "the search before a bad sort was applied");
    }
    for h in [v, f] {
        ds_close(h);
    }
}
