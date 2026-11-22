//! Random calls into the C ABI (backlog E11-S1, "FFI entry points with random handles and
//! buffers"). A cargo-fuzz target cannot link the cdylib under libFuzzer, so the same idea runs
//! here with a seeded generator: random handles, arguments and small buffers (the pointers
//! themselves are always valid; memory safety of bad pointers is the host's contract), mixed with
//! real handles from earlier calls so views, plans and jobs are used and closed in odd orders.
//! Nothing may panic (no export returns `DS_E_INTERNAL`), hang or leak a handle (the registry's
//! open count is back at its baseline after every round). Every other round has an event callback. `DS_FFI_ROUNDS` sets the number of rounds (default
//! 150); a campaign uses a large number.
#![allow(unsafe_code, clippy::wildcard_imports, clippy::too_many_lines)]

mod common;

use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use common::*;
use diffsync_core::*;

/// A panic inside an export is turned into `DS_E_INTERNAL` by the barrier; a random call must
/// never provoke one (reviewer finding, E11-S6: the results used to be thrown away).
fn chk(s: DsStatus) -> DsStatus {
    assert_ne!(
        s,
        DS_E_INTERNAL,
        "an export panicked: {}",
        last_error_text()
    );
    s
}

static EVENTS_SEEN: AtomicU64 = AtomicU64::new(0);

unsafe extern "C" fn count_events(_u: *mut c_void, _src: u64, _kind: u32, _a: u64, _b: u64) {
    EVENTS_SEEN.fetch_add(1, Ordering::Relaxed);
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn byte(&mut self) -> u8 {
        (self.next() >> 24) as u8
    }
    fn word(&mut self) -> u32 {
        // mostly small numbers (valid nodes and rows), sometimes huge ones
        if self.next() % 8 == 0 {
            self.next() as u32
        } else {
            u32::from(self.byte() % 40)
        }
    }
}

fn v16(v: &[u16]) -> DsStr16 {
    DsStr16 {
        ptr: v.as_ptr(),
        len: v.len(),
    }
}

fn v8(s: &[u8]) -> DsStr8 {
    DsStr8 {
        ptr: s.as_ptr(),
        len: s.len(),
    }
}

const JSONS: [&str; 9] = [
    "",
    "{}",
    "{\"version\":1}",
    "{\"preset\":\"mirrorToRight\"}",
    "{\"filter\":\"differences\",\"search\":\"x\"}",
    "{\"alignment\":\"patience\",\"skewTolerance\":3}",
    "{\"whitespace\":\"all\",\"ignoreCase\":true}",
    "{not json",
    "{\"version\":9}",
];

#[test]
fn random_calls_never_panic_hang_or_leak() {
    let rounds: u64 = std::env::var("DS_FFI_ROUNDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(150);
    let dir = tempfile::tempdir().unwrap();
    let (l, r) = (dir.path().join("L"), dir.path().join("R"));
    write(&l, "d/x.txt", b"x");
    write(&r, "d/x.txt", b"xy");
    write(&l, "only.txt", b"o");
    write(dir.path(), "a.txt", b"one\ntwo\nthree\n");
    write(dir.path(), "b.txt", b"one\n2\nthree\nfour\n");
    let (left, right) = (
        path16(&dir.path().join("a.txt")),
        path16(&dir.path().join("b.txt")),
    );
    let (lroot, rroot) = (path16(&l), path16(&r));
    let scratch = dir.path().join("scratch");
    std::fs::create_dir_all(&scratch).unwrap();
    for i in 0..4 {
        write(&scratch, &format!("c{i}.txt"), b"c");
        write(&scratch, &format!("d{i}.txt"), b"d");
    }
    let scratch16 = path16(&scratch);
    let base_handles = diffsync_core::registry::open_count();
    let started = Instant::now();
    let mut calls = 0u64;
    for round in 0..rounds {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15 ^ (round + 1).wrapping_mul(0xD1B5_4A32_D192_ED03));
        let mut handles: Vec<u64> = vec![0, 1, u64::MAX, 0xDEAD_BEEF];
        // every other round has a real callback, so the event bridge (delivery, close, held events)
        // is part of the random mix
        let cb: DsEventFn = if round % 2 == 0 {
            Some(count_events)
        } else {
            None
        };
        for _ in 0..40 {
            let op = rng.byte() % 27;
            let h = if rng.byte() % 3 == 0 {
                rng.next()
            } else {
                handles[rng.next() as usize % handles.len()]
            };
            let json = JSONS[rng.next() as usize % JSONS.len()].as_bytes();
            calls += 1;
            // SAFETY: every pointer below is valid for its length; handles are arbitrary on purpose.
            unsafe {
                match op {
                    0 => {
                        let mut o = 0u64;
                        if chk(ds_text_open(
                            v16(&left),
                            v16(&right),
                            v8(json),
                            cb,
                            std::ptr::null_mut(),
                            &raw mut o,
                        )) == DS_OK
                        {
                            handles.push(o);
                        }
                    }
                    1 => {
                        let mut o = 0u64;
                        if chk(ds_folder_open(
                            v16(&lroot),
                            v16(&rroot),
                            v8(json),
                            cb,
                            std::ptr::null_mut(),
                            &raw mut o,
                        )) == DS_OK
                        {
                            handles.push(o);
                        }
                    }
                    2 => {
                        let mut o = 0u64;
                        if chk(ds_view_create(h, v8(json), &raw mut o)) == DS_OK {
                            handles.push(o);
                        }
                    }
                    3 => {
                        let mut o = 0u64;
                        if chk(ds_sync_plan(h, v8(json), std::ptr::null(), 0, &raw mut o)) == DS_OK
                        {
                            handles.push(o);
                        }
                    }
                    4 => {
                        let mut o = 0u64;
                        if chk(ds_plan_execute(
                            h,
                            v8(b"{}"),
                            cb,
                            std::ptr::null_mut(),
                            &raw mut o,
                        )) == DS_OK
                        {
                            handles.push(o);
                        }
                    }
                    5 => {
                        let mut rows = [DsTextRow::default(); 8];
                        let mut w = 0u32;
                        let _ = chk(ds_text_get_rows(
                            h,
                            rng.word(),
                            8,
                            rows.as_mut_ptr(),
                            &raw mut w,
                        ));
                    }
                    6 => {
                        let mut buf = [0u16; 4];
                        let mut len = 0u32;
                        let mut info = DsLineInfo::default();
                        let _ = chk(ds_text_get_line(
                            h,
                            rng.byte() & 3,
                            rng.word(),
                            buf.as_mut_ptr(),
                            4,
                            &raw mut len,
                            &raw mut info,
                        ));
                    }
                    7 => {
                        let (mut ln, mut rn) = (0u32, 0u32);
                        let mut lr = [DsRange::default(); 2];
                        let _ = chk(ds_text_get_intraline(
                            h,
                            rng.word(),
                            lr.as_mut_ptr(),
                            2,
                            &raw mut ln,
                            std::ptr::null_mut(),
                            0,
                            &raw mut rn,
                        ));
                    }
                    8 => {
                        let mut row = 0u32;
                        let (from, dir, imp) = (rng.word(), rng.byte() as i8, rng.byte());
                        let _ = chk(ds_text_find_diff(
                            h,
                            from,
                            i32::from(dir),
                            imp,
                            &raw mut row,
                        ));
                    }
                    9 => {
                        let _ = chk(ds_text_recompare(h, v8(json)));
                    }
                    10 => {
                        let mut rows = [DsDirRow::default(); 4];
                        let mut w = 0u32;
                        let _ = chk(ds_view_get_rows(
                            h,
                            rng.word(),
                            4,
                            rows.as_mut_ptr(),
                            &raw mut w,
                        ));
                    }
                    11 => {
                        let (n, a, b) = (rng.word(), rng.byte(), rng.byte());
                        let _ = chk(ds_view_expand(h, n, a, b));
                        let _ = chk(ds_view_set(h, v8(json)));
                    }
                    12 => {
                        let mut info = DsDirRow::default();
                        let _ = chk(ds_folder_node_info(h, rng.word(), &raw mut info));
                        let mut name = [0u16; 3];
                        let mut len = 0u32;
                        let (n, side) = (rng.word(), rng.byte() & 3);
                        let _ = chk(ds_folder_node_name(
                            h,
                            n,
                            side,
                            name.as_mut_ptr(),
                            3,
                            &raw mut len,
                        ));
                    }
                    13 => {
                        let n = [rng.word(), 0];
                        let _ = chk(ds_folder_refresh(h, n.as_ptr(), 2));
                        let _ = chk(ds_folder_compare_content(h, n.as_ptr(), 1));
                    }
                    14 => {
                        let (n, o) = (rng.word(), rng.byte() % 8);
                        let _ = chk(ds_plan_set_op(h, n, o));
                        let mut s = DsPlanSummary::default();
                        let _ = chk(ds_plan_summary(h, &raw mut s));
                    }
                    15 => {
                        let mut st = DsJobStatus::default();
                        let _ = chk(ds_job_status(h, &raw mut st));
                        let mut q = DsQuestion::default();
                        let mut len = 0u32;
                        let _ = chk(ds_job_get_question(
                            h,
                            &raw mut q,
                            std::ptr::null_mut(),
                            0,
                            &raw mut len,
                        ));
                        let _ = chk(ds_job_answer(h, q.id, rng.byte() % 5));
                    }
                    16 => {
                        let _ = chk(ds_job_pause(h));
                        let _ = chk(ds_job_resume(h));
                        let _ = chk(ds_job_cancel(h));
                    }
                    17 => {
                        let mut st = DsTextStatus::default();
                        let _ = chk(ds_text_status(h, &raw mut st));
                        let mut fs = DsFolderStatus::default();
                        let _ = chk(ds_folder_status(h, &raw mut fs));
                    }
                    18 => {
                        let mut b = DsBuf {
                            ptr: std::ptr::null_mut(),
                            len: 0,
                        };
                        if chk(ds_job_result(h, &raw mut b)) == DS_OK
                            || chk(ds_plan_report(h, &raw mut b)) == DS_OK
                            || chk(ds_folder_report(h, &raw mut b)) == DS_OK
                        {
                            ds_buf_free(b);
                        }
                    }
                    19 => {
                        let mut buf = [0u8; 8];
                        let mut needed = 0usize;
                        let _ = chk(ds_last_error(buf.as_mut_ptr(), 8, &raw mut needed));
                        let mut b = DsBuf {
                            ptr: std::ptr::null_mut(),
                            len: 0,
                        };
                        if chk(ds_options_schema(&raw mut b)) == DS_OK {
                            ds_buf_free(b);
                        }
                    }
                    22 => {
                        let mut secs = [DsSection::default(); 4];
                        let mut w = 0u32;
                        let _ = chk(ds_text_get_sections(
                            h,
                            rng.word(),
                            4,
                            secs.as_mut_ptr(),
                            &raw mut w,
                        ));
                        let mut buckets = [0u8; 16];
                        let _ = chk(ds_text_get_overview(h, buckets.as_mut_ptr(), 16));
                        let mut info = DsDocInfo::default();
                        let _ = chk(ds_text_doc_info(h, rng.byte() & 3, &raw mut info));
                    }
                    23 => {
                        let mut acts = [DsPlanAction::default(); 4];
                        let (mut w, mut nodes) = (0u32, [0u32; 4]);
                        let _ = chk(ds_plan_get_actions(
                            h,
                            rng.word(),
                            4,
                            acts.as_mut_ptr(),
                            &raw mut w,
                        ));
                        let _ = chk(ds_plan_get_prompts(
                            h,
                            rng.word(),
                            4,
                            nodes.as_mut_ptr(),
                            &raw mut w,
                        ));
                    }
                    24 => {
                        // file operations only ever touch the scratch folder of this test
                        let item = [path16(&scratch.join(format!("c{}.txt", rng.byte() % 4)))];
                        let views: Vec<DsStr16> = item.iter().map(|p| v16(p)).collect();
                        let list = DsStr16List {
                            items: views.as_ptr(),
                            count: views.len(),
                        };
                        let mut o = 0u64;
                        if chk(ds_fileops_copy(
                            list,
                            v16(&scratch16),
                            v8(json),
                            cb,
                            std::ptr::null_mut(),
                            &raw mut o,
                        )) == DS_OK
                        {
                            handles.push(o);
                        }
                    }
                    25 => {
                        let item = [path16(&scratch.join(format!("d{}.txt", rng.byte() % 4)))];
                        let views: Vec<DsStr16> = item.iter().map(|p| v16(p)).collect();
                        let list = DsStr16List {
                            items: views.as_ptr(),
                            count: views.len(),
                        };
                        let mut o = 0u64;
                        if chk(ds_fileops_delete(
                            list,
                            v8(json),
                            cb,
                            std::ptr::null_mut(),
                            &raw mut o,
                        )) == DS_OK
                        {
                            handles.push(o);
                        }
                    }
                    20 => {
                        let _ = chk(ds_text_cancel(h));
                        let _ = chk(ds_folder_cancel(h));
                    }
                    _ => {
                        let _ = chk(ds_close(h));
                    }
                }
            }
        }
        for h in handles {
            let _ = chk(ds_close(h));
        }
        assert_eq!(
            diffsync_core::registry::open_count(),
            base_handles,
            "round {round}: a handle was leaked"
        );
        assert!(
            started.elapsed() < Duration::from_secs(900),
            "the random calls hang"
        );
    }
    // every handle the rounds opened was closed again (what remains belongs to other tests)
    eprintln!("{calls} random calls in {:?}", started.elapsed());
}
