//! Big runs (spec ALIGN-1.4, fixes B1 and B2): many of them in one file, and the depth of the
//! work stack (backlog E2-S7.1).
//!
//! The backlog asked for a "depth-10k test without stack overflow". The aligner uses an explicit
//! work stack, so the call stack is never at risk; what can be shown instead is that the stack
//! stays tiny, because the block before a big run is a sub-rectangle of the window that found
//! the run first: a big run inside it would have been found first (its start cell is visited
//! earlier by the L-shaped scan). The test below checks that on thousands of big runs.

use core_base::CancelToken;
use core_diff::{AlignConfig, AlignOp, AlignRange, AlignScratch, VecSource, align_with_scratch};
use core_text::rules::LineRules;

/// `blocks` repetitions of: one line that differs on each side, then 80 shared lines (weight
/// 5600 >= 5000, so each block ends in a big run).
fn build(blocks: usize) -> (Vec<String>, Vec<String>) {
    let (mut l, mut r) = (Vec::new(), Vec::new());
    for b in 0..blocks {
        l.push(format!("left head {b} abcdefghij"));
        r.push(format!("RIGHT HEAD {b} KLMNOPQRST"));
        for k in 0..80 {
            let shared = format!("shared {b} {k}");
            l.push(shared.clone());
            r.push(shared);
        }
    }
    (l, r)
}

#[test]
fn thousands_of_big_runs_keep_the_work_stack_tiny() {
    let blocks = 2000;
    let (l, r) = build(blocks);
    let src = VecSource::new(&l, &r, &LineRules::new());
    let mut scratch = AlignScratch::new();
    let mut ops: Vec<AlignOp> = Vec::new();
    align_with_scratch(
        &src,
        &AlignConfig::default(),
        AlignRange::full(),
        &CancelToken::new(),
        &mut |o| ops.push(o),
        &mut scratch,
    )
    .unwrap();

    // every block: the head pair, then the run (80 lines; may be split in pieces of 501 only
    // above 500 lines, so it is one op here)
    let paired: u32 = ops.iter().filter(|o| o.is_paired()).map(|o| o.amount).sum();
    assert_eq!(paired as usize, blocks * 81, "every line is paired");
    assert!(ops.iter().all(|o| o.is_paired() && o.src == o.tgt));
    let runs = ops.iter().filter(|o| o.amount >= 80).count();
    assert!(runs >= blocks - 1, "{runs} run ops for {blocks} blocks");

    assert!(
        scratch.max_stack_depth() >= 2,
        "the observer saw the Extend + Block pair"
    );
    assert!(
        scratch.max_stack_depth() <= 3,
        "the work stack grew to {} frames",
        scratch.max_stack_depth()
    );
}

#[test]
fn big_run_extension_ignores_the_window_edge_and_respects_the_block_stop() {
    // B1: 3000 shared lines after a 5-line differing head, skew 100. The run is found in the
    // first window (100 lines) but must extend through all 3000 lines (to the block stop),
    // emitted as pieces of 501 lines with the remainder last, with no break at the window edge.
    let head_l: Vec<String> = (0..5).map(|k| format!("abcdefghijkl {k}")).collect();
    let head_r: Vec<String> = (0..5).map(|k| format!("nopqrstuvwxy {k}")).collect();
    let body: Vec<String> = (0..3000).map(|k| format!("shared line {k}")).collect();
    let l: Vec<String> = head_l.iter().chain(&body).cloned().collect();
    let r: Vec<String> = head_r.iter().chain(&body).cloned().collect();
    let src = VecSource::new(&l, &r, &LineRules::new());
    let cfg = AlignConfig {
        skew_tolerance: 100,
        ..AlignConfig::default()
    };
    let mut ops: Vec<AlignOp> = Vec::new();
    align_with_scratch(
        &src,
        &cfg,
        AlignRange::full(),
        &CancelToken::new(),
        &mut |o| ops.push(o),
        &mut AlignScratch::new(),
    )
    .unwrap();
    let amounts: Vec<u32> = ops.iter().map(|o| o.amount).collect();
    // 5 header pairs, then 5 pieces of 501 (2505 lines) and the remaining 495
    assert_eq!(amounts, vec![5, 501, 501, 501, 501, 501, 495], "{ops:?}");
    assert_eq!(ops[1].src, 5);
    assert_eq!(ops[2].src, 5 + 501);
}
