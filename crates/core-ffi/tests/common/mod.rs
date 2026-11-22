//! Helpers for calling the exports the way a C host does.
#![allow(dead_code, unsafe_code, clippy::wildcard_imports)]

use std::ffi::c_void;
use std::path::Path;
use std::sync::{Arc, LazyLock};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

use diffsync_core::*;
use parking_lot::Mutex;

pub fn u16s(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

pub fn s16(v: &[u16]) -> DsStr16 {
    DsStr16 {
        ptr: v.as_ptr(),
        len: v.len(),
    }
}

pub fn s8(s: &str) -> DsStr8 {
    DsStr8 {
        ptr: s.as_ptr(),
        len: s.len(),
    }
}

pub fn path16(p: &Path) -> Vec<u16> {
    u16s(p.to_str().unwrap())
}

pub const NO_CB: DsEventFn = None;

pub fn ok(status: DsStatus) {
    assert!(status == DS_OK, "status {status}: {}", last_error_text());
}

pub fn last_error_text() -> String {
    let mut needed = 0usize;
    // SAFETY: a null buffer with capacity 0 asks only for the size.
    unsafe { ds_last_error(std::ptr::null_mut(), 0, &raw mut needed) };
    let mut buf = vec![0u8; needed];
    // SAFETY: `buf` is valid for `needed` bytes.
    unsafe { ds_last_error(buf.as_mut_ptr(), buf.len(), &raw mut needed) };
    String::from_utf8_lossy(&buf).into_owned()
}

pub fn take_buf(b: DsBuf) -> String {
    // SAFETY: `b` was returned by the core and is freed exactly once below.
    let text = unsafe { std::slice::from_raw_parts(b.ptr, b.len) };
    let s = String::from_utf8_lossy(text).into_owned();
    // SAFETY: `b` came from the core and has not been freed.
    unsafe { ds_buf_free(b) };
    s
}

pub fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
    let t = Instant::now();
    while !f() {
        assert!(
            t.elapsed() < Duration::from_secs(30),
            "timed out waiting for {what}"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

// ---------------------------------------------------------------- events

#[derive(Debug, Clone, Copy)]
pub struct Event {
    pub thread: ThreadId,
    pub src: u64,
    pub kind: u32,
    pub a: u64,
    pub b: u64,
    pub at: Instant,
}

pub type Events = Arc<Mutex<Vec<Event>>>;

pub fn new_events() -> Events {
    Arc::new(Mutex::new(Vec::new()))
}

/// The callback: `user` is a leaked `Arc<Mutex<Vec<Event>>>` pointer.
pub unsafe extern "C" fn record(user: *mut c_void, src: u64, kind: u32, a: u64, b: u64) {
    // SAFETY: `user` is the pointer from `user_of`, valid while the Arc is alive.
    let events = unsafe { &*(user as *const Mutex<Vec<Event>>) };
    events.lock().push(Event {
        thread: std::thread::current().id(),
        src,
        kind,
        a,
        b,
        at: Instant::now(),
    });
}

pub fn user_of(e: &Events) -> *mut c_void {
    Arc::as_ptr(e).cast_mut().cast()
}

static LOGS: LazyLock<Mutex<Vec<(u32, String)>>> = LazyLock::new(|| Mutex::new(Vec::new()));

pub unsafe extern "C" fn log_sink(_user: *mut c_void, level: u32, msg: *const u8, len: usize) {
    // SAFETY: the core passes a valid UTF-8 buffer for the call.
    let m = unsafe { std::slice::from_raw_parts(msg, len) };
    LOGS.lock()
        .push((level, String::from_utf8_lossy(m).into_owned()));
}

pub fn logs() -> Vec<(u32, String)> {
    LOGS.lock().clone()
}

// ---------------------------------------------------------------- scenario helpers

pub fn write(dir: &Path, name: &str, data: &[u8]) {
    let p = dir.join(name);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, data).unwrap();
}

pub fn text_pair(left: &str, right: &str) -> (tempfile::TempDir, DsHandle) {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "l.txt", left.as_bytes());
    write(dir.path(), "r.txt", right.as_bytes());
    let (l, r) = (
        path16(&dir.path().join("l.txt")),
        path16(&dir.path().join("r.txt")),
    );
    let mut h = 0u64;
    // SAFETY: all views and the out pointer are valid for the call.
    ok(unsafe {
        ds_text_open(
            s16(&l),
            s16(&r),
            s8(""),
            NO_CB,
            std::ptr::null_mut(),
            &raw mut h,
        )
    });
    wait_until("the text compare", || {
        let mut st = DsTextStatus::default();
        // SAFETY: `st` is valid for a write.
        ok(unsafe { ds_text_status(h, &raw mut st) });
        st.state == 2
    });
    (dir, h)
}

pub fn folder_open(
    left: &Path,
    right: &Path,
    json: &str,
    cb: DsEventFn,
    user: *mut c_void,
) -> DsHandle {
    let (l, r) = (path16(left), path16(right));
    let mut h = 0u64;
    // SAFETY: all views and the out pointer are valid for the call.
    ok(unsafe { ds_folder_open(s16(&l), s16(&r), s8(json), cb, user, &raw mut h) });
    wait_until("the folder compare", || {
        let mut st = DsFolderStatus::default();
        // SAFETY: `st` is valid for a write.
        ok(unsafe { ds_folder_status(h, &raw mut st) });
        st.state != 0
    });
    h
}

pub fn folder_status(h: DsHandle) -> DsFolderStatus {
    let mut st = DsFolderStatus::default();
    // SAFETY: `st` is valid for a write.
    ok(unsafe { ds_folder_status(h, &raw mut st) });
    st
}
