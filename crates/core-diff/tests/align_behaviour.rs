//! Behavioural tests of the Standard/None aligner that are not golden outputs: cancellation,
//! laziness and caching, ranges, configuration errors (backlog E2-S1, E2-S2.3, E2-S4, E2-S9).

use std::cell::Cell;
use std::time::{Duration, Instant};

use core_base::{CancelToken, ErrorKind};
use core_diff::align::consts::{GAP_SPLIT, closeness_cache_size};
use core_diff::{
    AlignConfig, AlignOp, AlignRange, AlignScratch, LineSource, Side, VecSource, align,
    align_with_scratch,
};
use core_text::meta::LineMeta;
use core_text::rules::LineRules;

fn ops_of(
    left: &[&str],
    right: &[&str],
    cfg: &AlignConfig,
) -> Vec<(Option<u32>, Option<u32>, u32)> {
    let src = VecSource::new(left, right, &LineRules::new());
    let mut ops = Vec::new();
    align(
        &src,
        cfg,
        AlignRange::full(),
        &CancelToken::new(),
        &mut |o| ops.push(o),
    )
    .unwrap();
    ops.iter()
        .map(|o| (o.src_opt(), o.tgt_opt(), o.amount))
        .collect()
}

/// A source that counts what the aligner asks for.
struct Counting<'a> {
    inner: &'a VecSource,
    metas: Cell<u32>,
    texts: Cell<u32>,
    exists: Cell<u32>,
}

impl<'a> Counting<'a> {
    fn new(inner: &'a VecSource) -> Self {
        Counting {
            inner,
            metas: Cell::new(0),
            texts: Cell::new(0),
            exists: Cell::new(0),
        }
    }
}

impl LineSource for Counting<'_> {
    fn exists(&self, side: Side, idx: u32) -> bool {
        self.exists.set(self.exists.get() + 1);
        self.inner.exists(side, idx)
    }
    fn meta(&self, side: Side, idx: u32) -> LineMeta {
        self.metas.set(self.metas.get() + 1);
        self.inner.meta(side, idx)
    }
    fn closeness_text(&self, side: Side, idx: u32, out: &mut Vec<u8>) {
        self.texts.set(self.texts.get() + 1);
        self.inner.closeness_text(side, idx, out);
    }
    fn full_text_eq(&self, a: u32, b: u32) -> bool {
        self.inner.full_text_eq(a, b)
    }
}

/// `n` lines over disjoint alphabets: left from `[a-m]`, right from `[n-z]`, 12 characters
/// each, so every left/right pair scores exactly -40.
fn disjoint_lines(n: usize) -> (Vec<String>, Vec<String>) {
    let make = |alphabet: &str| -> Vec<String> {
        let a: Vec<char> = alphabet.chars().collect();
        (0..n)
            .map(|k| {
                let (mut x, mut digits) = (k, [0usize; 4]);
                for d in &mut digits {
                    *d = x % 13;
                    x /= 13;
                }
                (0..12).map(|i| a[digits[i % 4]]).collect()
            })
            .collect()
    };
    (make("abcdefghijklm"), make("nopqrstuvwxyz"))
}

#[test]
fn each_line_is_loaded_once_and_its_closeness_text_is_cached() {
    // 100 x 100 lines that all differ: one window, one gap DP. Every pair needs both
    // closeness texts, but the ring cache (128 entries >= skew 100) builds each text once.
    let (l, r) = disjoint_lines(100);
    let src = VecSource::new(&l, &r, &LineRules::new());
    let counting = Counting::new(&src);
    let cfg = AlignConfig {
        skew_tolerance: 100,
        ..AlignConfig::default()
    };
    assert_eq!(closeness_cache_size(100), 128);
    let mut ops = Vec::new();
    align(
        &counting,
        &cfg,
        AlignRange::full(),
        &CancelToken::new(),
        &mut |o| ops.push(o),
    )
    .unwrap();
    assert_eq!(ops, vec![AlignOp::paired(0, 0, 100)]);
    assert_eq!(
        counting.texts.get(),
        200,
        "closeness text built once per line and side"
    );
    assert!(
        counting.metas.get() <= 256,
        "metadata of a line is read at most once: {}",
        counting.metas.get()
    );
    assert!(
        counting.exists.get() < 40,
        "the end of each side is found by bisection: {}",
        counting.exists.get()
    );
}

#[test]
fn metadata_is_loaded_lazily_in_chunks() {
    // Identical files: the top-level walk pairs line by line and never needs more than the
    // current chunk of 128 lines ahead.
    let lines: Vec<String> = (0..1000).map(|k| format!("line {k}")).collect();
    let src = VecSource::new(&lines, &lines, &LineRules::new());
    let counting = Counting::new(&src);
    let cancel = CancelToken::new();
    let mut seen = 0;
    let mut stop_after = |_: AlignOp| {
        seen += 1;
        if seen == 10 {
            cancel.cancel();
        }
    };
    let err = align(
        &counting,
        &AlignConfig::default(),
        AlignRange::full(),
        &cancel,
        &mut stop_after,
    )
    .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Cancelled);
    assert!(
        counting.metas.get() <= 2 * 128,
        "{} metadata reads for 10 ops",
        counting.metas.get()
    );
}

#[test]
fn cancelled_before_start_returns_cancelled_without_ops() {
    let (l, r) = disjoint_lines(10);
    let src = VecSource::new(&l, &r, &LineRules::new());
    let cancel = CancelToken::new();
    cancel.cancel();
    let mut n = 0;
    let err = align(
        &src,
        &AlignConfig::default(),
        AlignRange::full(),
        &cancel,
        &mut |_| n += 1,
    )
    .unwrap_err();
    assert!(err.is_cancelled());
    assert_eq!(n, 0);
}

/// Backlog E2-S9: cancelling during a 1M-line alignment returns within 50 ms.
#[test]
fn cancel_during_a_million_line_alignment_returns_within_50_ms() {
    // Two million-line files that share nothing: windows of gap DP, the slowest case.
    let n = 1_000_000;
    let left: Vec<String> = (0..n)
        .map(|k| format!("left only line number {k}"))
        .collect();
    let right: Vec<String> = (0..n)
        .map(|k| format!("RIGHT ONLY LINE NUMBER {k}"))
        .collect();
    let src = VecSource::new(&left, &right, &LineRules::new());
    let cancel = CancelToken::new();
    let canceller = {
        let cancel = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            let at = Instant::now();
            cancel.cancel();
            at
        })
    };
    let started = Instant::now();
    let result = align(
        &src,
        &AlignConfig::default(),
        AlignRange::full(),
        &cancel,
        &mut |_| {},
    );
    let finished = Instant::now();
    let cancelled_at = canceller.join().unwrap();
    assert!(
        result.unwrap_err().is_cancelled(),
        "the alignment finished before the cancel (too fast?)"
    );
    let latency = finished.saturating_duration_since(cancelled_at);
    assert!(
        latency < Duration::from_millis(50),
        "cancel took {latency:?} (run {:?})",
        started.elapsed()
    );
}

#[test]
fn ranges_limit_both_sides() {
    let l = ["a0", "a1", "a2", "a3", "a4", "a5", "a6"];
    let r = ["b0", "b1", "a2", "a3", "a4", "b5"];
    let src = VecSource::new(&l, &r, &LineRules::new());
    let mut ops = Vec::new();
    let range = AlignRange {
        src: 2..5,
        tgt: 2..5,
    };
    align(
        &src,
        &AlignConfig::default(),
        range,
        &CancelToken::new(),
        &mut |o| ops.push(o),
    )
    .unwrap();
    // the three equal lines, one op each from the top-level walk; nothing outside the range
    assert_eq!(
        ops,
        vec![
            AlignOp::paired(2, 2, 1),
            AlignOp::paired(3, 3, 1),
            AlignOp::paired(4, 4, 1)
        ]
    );

    // a range end beyond the data is clamped to the last line
    let mut ops = Vec::new();
    let range = AlignRange {
        src: 5..1000,
        tgt: 4..u32::MAX,
    };
    align(
        &src,
        &AlignConfig::default(),
        range,
        &CancelToken::new(),
        &mut |o| ops.push(o),
    )
    .unwrap();
    let total_src: u32 = ops
        .iter()
        .filter_map(|o| o.src_opt().map(|_| o.amount))
        .sum();
    let total_tgt: u32 = ops
        .iter()
        .filter_map(|o| o.tgt_opt().map(|_| o.amount))
        .sum();
    assert_eq!((total_src, total_tgt), (2, 2));
    assert!(
        ops.iter()
            .all(|o| o.src_opt().is_none_or(|s| s >= 5) && o.tgt_opt().is_none_or(|t| t >= 4))
    );

    // an empty range emits nothing
    let mut n = 0;
    align(
        &src,
        &AlignConfig::default(),
        AlignRange {
            src: 3..3,
            tgt: 0..0,
        },
        &CancelToken::new(),
        &mut |_| n += 1,
    )
    .unwrap();
    assert_eq!(n, 0);
}

#[test]
fn invalid_configuration_is_rejected_before_any_work() {
    let src = VecSource::new(&["a"], &["a"], &LineRules::new());
    let mut n = 0;
    let bad = AlignConfig {
        never_align_diffs: true,
        ..AlignConfig::default()
    };
    let err = align(
        &src,
        &bad,
        AlignRange::full(),
        &CancelToken::new(),
        &mut |_| n += 1,
    )
    .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::InvalidArgument);
    let bad = AlignConfig {
        skew_tolerance: 0,
        ..AlignConfig::default()
    };
    assert_eq!(
        align(
            &src,
            &bad,
            AlignRange::full(),
            &CancelToken::new(),
            &mut |_| n += 1
        )
        .unwrap_err()
        .kind(),
        ErrorKind::InvalidArgument
    );
    assert_eq!(n, 0);
}

#[test]
fn orphans_always_sig_stops_the_top_level_walk_at_a_blank_orphan() {
    // spec: ALIGN-1.3: with orphans_always_sig a one-sided insignificant line ends the walk,
    // so the rest goes to the block aligner and the blank becomes an orphan row there.
    let cfg = AlignConfig {
        orphans_always_sig: true,
        ..AlignConfig::default()
    };
    let ops = ops_of(&["x", "", "y"], &["x", "y"], &cfg);
    // block aligner from line 1 (the blank) : pair y with y via the DP, blank is an orphan
    assert_eq!(
        ops,
        vec![
            (Some(0), Some(0), 1),
            (Some(1), None, 1),
            (Some(2), Some(1), 1)
        ]
    );
}

#[test]
fn both_insignificant_lines_join_the_pending_gap_when_hashes_differ() {
    // spec: ALIGN-1.3: two insignificant lines with different hashes open a pending gap; the
    // gap is aligned when the next equal significant pair closes it.
    let ops = ops_of(
        &["x", "  ", "y"],
        &["x", "\t\t\t", "y"],
        &AlignConfig::default(),
    );
    // 'x', then the pending gap [1,2) x [1,2) aligned by the block aligner, then 'y'
    assert_eq!(
        ops,
        vec![
            (Some(0), Some(0), 1),
            (Some(1), Some(1), 1),
            (Some(2), Some(2), 1)
        ]
    );
}

/// Backlog E2-S4.3: a gap above 16M cells is split along the diagonal into blocks of 2000
/// lines, never allocated whole, and the leftovers become orphans.
#[test]
fn a_5000_by_5000_gap_is_split_into_diagonal_blocks() {
    let n = 5000;
    let (l, r) = disjoint_lines(n);
    let src = VecSource::new(&l, &r, &LineRules::new());
    // the window covers everything (skew 5000), so the whole gap is aligned in one go
    let cfg = AlignConfig {
        skew_tolerance: 5000,
        ..AlignConfig::default()
    };
    let mut scratch = AlignScratch::new();
    let mut ops = Vec::new();
    align_with_scratch(
        &src,
        &cfg,
        AlignRange::full(),
        &CancelToken::new(),
        &mut |o| ops.push(o),
        &mut scratch,
    )
    .unwrap();
    // (5001 x 5001 = 25M cells > 16M): blocks of GAP_SPLIT = 2000, all pairs on the diagonal
    assert_eq!(GAP_SPLIT, 2000);
    assert_eq!(
        ops,
        vec![
            AlignOp::paired(0, 0, 2000),
            AlignOp::paired(2000, 2000, 2000),
            AlignOp::paired(4000, 4000, 1000)
        ]
    );
    // bounded memory: the DP buffers cover one 2000 x 2000 block, not the whole gap
    let bytes = scratch.capacity_bytes();
    assert!(bytes < 40 * 1024 * 1024, "scratch holds {bytes} bytes");
}

#[test]
fn an_exhausted_side_leaves_the_other_sides_rest_as_orphans_in_chunks_of_200() {
    // skew 10: the first window pairs the ten lines of the short side (more_after, partial
    // trace); then that side is exhausted and the 450 remaining lines go out in chunks of 200
    // (spec ALIGN-1.4), not as one op and not through the DP.
    let (l, r) = disjoint_lines(10);
    let right: Vec<String> = r
        .iter()
        .cloned()
        .chain((0..450).map(|k| format!("zzzz extra {k}")))
        .collect();
    let src = VecSource::new(&l, &right, &LineRules::new());
    let cfg = AlignConfig {
        skew_tolerance: 10,
        ..AlignConfig::default()
    };
    let mut ops = Vec::new();
    align(
        &src,
        &cfg,
        AlignRange::full(),
        &CancelToken::new(),
        &mut |o| ops.push(o),
    )
    .unwrap();
    assert_eq!(
        ops,
        vec![
            AlignOp::paired(0, 0, 10),
            AlignOp::tgt_orphans(10, 200),
            AlignOp::tgt_orphans(210, 200),
            AlignOp::tgt_orphans(410, 50),
        ]
    );
}
