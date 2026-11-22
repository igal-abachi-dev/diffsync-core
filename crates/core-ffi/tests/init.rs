//! `ds_init` in a process of its own (the library state is global).
#![allow(unsafe_code, clippy::wildcard_imports)]

mod common;

use common::*;
use diffsync_core::*;

fn opts(dir: &[u16], threads: u32) -> DsInitOptions {
    DsInitOptions {
        struct_size: size_of::<DsInitOptions>() as u32,
        worker_threads: threads,
        log_fn: None,
        log_user: std::ptr::null_mut(),
        log_dir: s16(dir),
        temp_dir: s16(&[]),
    }
}

#[test]
fn init_validates_first_applies_once_and_refuses_a_second_call() {
    // a null pointer means "defaults" and does not initialise anything
    // SAFETY: null is allowed.
    assert_eq!(unsafe { ds_init(std::ptr::null()) }, DS_OK);
    // an absurd thread count is refused, not passed to the thread pool
    let huge = opts(&[], 4_000_000_000);
    // SAFETY: valid pointer.
    assert_eq!(unsafe { ds_init(&raw const huge) }, DS_E_INVALID_ARG);
    // too small a struct
    let mut small = opts(&[], 0);
    small.struct_size = 4;
    // SAFETY: valid pointer.
    assert_eq!(unsafe { ds_init(&raw const small) }, DS_E_INVALID_ARG);
    // a relative log folder is refused and leaves the library uninitialised
    let rel = u16s("relative/logs");
    let o = opts(&rel, 0);
    // SAFETY: valid pointer and views.
    assert_eq!(unsafe { ds_init(&raw const o) }, DS_E_INVALID_ARG);
    assert!(
        last_error_text().contains("absolute"),
        "{}",
        last_error_text()
    );
    // the failed calls changed nothing: a valid one works, and only once
    let dir = tempfile::tempdir().unwrap();
    let abs = path16(dir.path());
    let good = opts(&abs, 2);
    // SAFETY: valid pointer and views.
    assert_eq!(unsafe { ds_init(&raw const good) }, DS_OK);
    // SAFETY: as above.
    assert_eq!(unsafe { ds_init(&raw const good) }, DS_E_BUSY);
    // the default log folder reaches a job that names none
    let (l, r) = (dir.path().join("L"), dir.path().join("R"));
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
    let mut j = 0u64;
    // SAFETY: valid views and out pointer.
    ok(unsafe {
        ds_plan_execute(
            p,
            s8(r#"{"logDir":""}"#),
            NO_CB,
            std::ptr::null_mut(),
            &raw mut j,
        )
    });
    wait_until("the job", || {
        let mut st = DsJobStatus::default();
        // SAFETY: `st` is valid for a write.
        unsafe { ds_job_status(j, &raw mut st) };
        st.state == 2
    });
    let logs: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".jsonl"))
        .collect();
    assert_eq!(logs.len(), 1, "the log went to the folder given to ds_init");
    for h in [j, p, f] {
        ds_close(h);
    }
}
