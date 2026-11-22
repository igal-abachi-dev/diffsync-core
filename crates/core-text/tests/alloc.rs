//! `normalize` and `LineMeta::compute` do not allocate once their scratch buffers are warm
//! (backlog E1-S4.5, plan.md §4.2, fix B7). A counting global allocator, counted per thread so
//! the test harness threads do not matter.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use core_text::meta::LineMeta;
use core_text::rules::{LineRules, Scratch, WhitespaceMode};

thread_local! {
    static ALLOCS: Cell<u64> = const { Cell::new(0) };
}

struct Counting;

fn bump() {
    // `try_with`: the allocator can run while thread-local storage is being torn down.
    let _ = ALLOCS.try_with(|c| c.set(c.get() + 1));
}

// SAFETY: every method forwards to the system allocator with the same arguments, so all
// contracts of `GlobalAlloc` are upheld by `System`; the counter only touches a
// thread-local `Cell`.
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

#[test]
fn warm_scratch_never_allocates_even_when_every_stage_rewrites_the_line() {
    let rules = LineRules::new()
        .with_ignore_case(true)
        .with_whitespace(WhitespaceMode::IgnoreChanges)
        .with_unimportant(["//.*$"])
        .unwrap();
    let mut scratch = Scratch::new();

    // Lines the rules leave alone (lower case, single spaces, no comment).
    let untouched: Vec<String> = (0..200)
        .map(|i| format!("let value_{i} = other + {i};"))
        .collect();
    // Lines every stage rewrites: comment removal, whitespace collapse, case folding.
    let rewritten: Vec<String> = (0..200)
        .map(|i| format!("  Let   VALUE_{i}\t=  Other + {i};  // Note {i}"))
        .collect();

    // Warm up: grow both scratch buffers to the longest line.
    for l in rewritten.iter().chain(&untouched) {
        let _ = LineMeta::compute(l, &rules, &mut scratch, None);
    }

    let before = allocs();
    let mut sink = 0u32;
    for _ in 0..20 {
        for l in untouched.iter().chain(&rewritten) {
            let m = LineMeta::compute(l, &rules, &mut scratch, None);
            sink = sink.wrapping_add(m.hash);
        }
    }
    let used = allocs() - before;
    assert_eq!(
        used, 0,
        "{used} allocations in 8000 metadata computations (sink {sink})"
    );
}

#[test]
fn borrowing_path_needs_no_warm_up_at_all() {
    let rules = LineRules::new(); // default: trim only, which slices the input
    let mut scratch = Scratch::new();
    let lines: Vec<String> = (0..100).map(|i| format!("  line {i}  ")).collect();
    let before = allocs();
    for l in &lines {
        let n = rules.normalize(l, &mut scratch);
        assert!(!n.is_empty());
    }
    assert_eq!(allocs() - before, 0);
}
