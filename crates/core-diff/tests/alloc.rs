//! After warm-up the aligner allocates nothing: not in the window loop, not in the gap DP, not in
//! the closeness cache (backlog E2-S10.3, plan.md §5.2, fix B7). A counting global allocator,
//! counted per thread so the test harness threads do not matter.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use core_base::CancelToken;
use core_diff::{
    AlignConfig, AlignKind, AlignOp, AlignRange, AlignScratch, VecSource, align_with_scratch,
};
use core_text::rules::LineRules;

thread_local! {
    static ALLOCS: Cell<u64> = const { Cell::new(0) };
}

struct Counting;

fn bump() {
    // `try_with`: the allocator can run while thread-local storage is being torn down.
    let _ = ALLOCS.try_with(|c| c.set(c.get() + 1));
}

// SAFETY: every method forwards to the system allocator with the same arguments, so all
// contracts of `GlobalAlloc` are upheld by `System`; the counter only touches a thread-local.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        bump();
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        bump();
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn allocs() -> u64 {
    ALLOCS.with(Cell::get)
}

/// A text that exercises everything: shared runs (kept and big), changed lines (closeness
/// DP), insertions, blanks, and a gap that needs several windows.
fn texts() -> (Vec<String>, Vec<String>) {
    let (mut l, mut r) = (Vec::new(), Vec::new());
    for b in 0..30 {
        for k in 0..12 {
            let s = format!("shared block {b} line {k}");
            l.push(s.clone());
            r.push(s);
        }
        for k in 0..9 {
            l.push(format!("int value_{b}_{k} = {k};"));
            r.push(format!("int value_{b}_{k} = {};", k + 1));
        }
        l.push(String::new());
        r.push(format!("only right {b}"));
        l.push(format!("only left {b}"));
        for k in 0..90 {
            let s = format!("long shared {b} {k}");
            l.push(s.clone());
            r.push(s);
        }
    }
    (l, r)
}

#[test]
fn a_warm_scratch_allocates_nothing_for_any_skew() {
    let (l, r) = texts();
    let src = VecSource::new(&l, &r, &LineRules::new());
    let mut scratch = AlignScratch::new();
    let mut count = 0u64;
    let cancel = CancelToken::new();
    for skew in [1u32, 7, 40, 200, 2000] {
        for kind in [AlignKind::Standard, AlignKind::None] {
            let cfg = AlignConfig {
                kind,
                skew_tolerance: skew,
                ..AlignConfig::default()
            };
            // warm-up run: buffers grow to what this input needs
            align_with_scratch(
                &src,
                &cfg,
                AlignRange::full(),
                &cancel,
                &mut |_: AlignOp| {},
                &mut scratch,
            )
            .unwrap();
            let before = allocs();
            align_with_scratch(
                &src,
                &cfg,
                AlignRange::full(),
                &cancel,
                &mut |_: AlignOp| count += 1,
                &mut scratch,
            )
            .unwrap();
            let used = allocs() - before;
            assert_eq!(
                used, 0,
                "{used} allocations in a warm run (skew {skew}, {kind:?})"
            );
        }
    }
    assert!(count > 0);
}
