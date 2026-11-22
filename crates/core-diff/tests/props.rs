//! Property tests of the Standard/None aligner (backlog E2-S9.4, plan.md §12.1).
//!
//! Inputs come from a small pool of lines (so equal lines, runs and repeats are common) plus
//! blank and whitespace-variant lines, with small skew values that stress window edges.

use core_base::CancelToken;
use core_diff::{
    AlignConfig, AlignKind, AlignOp, AlignRange, AlignScratch, VecSource, align_with_scratch,
};
use core_text::rules::LineRules;
use proptest::prelude::*;

const POOL: &[&str] = &[
    "alpha one",
    "beta two",
    "gamma three",
    "delta four",
    "epsilon five",
    "zeta six",
    "eta seven",
    "theta eight",
    "iota nine",
    "kappa ten",
    "",
    "   ",
    "  alpha one",
    "alpha one  ",
    "// note",
    "int x = 1;",
    "int x = 2;",
    "int y = 2;",
    "return value;",
    "return result;",
];

fn line() -> impl Strategy<Value = String> {
    prop_oneof![
        8 => proptest::sample::select(POOL).prop_map(str::to_owned),
        2 => "[a-d]{1,6}",
    ]
}

/// A text as blocks: repeated blocks make long equal runs likely.
fn text() -> impl Strategy<Value = Vec<String>> {
    proptest::collection::vec(
        (proptest::collection::vec(line(), 1..6), 1usize..12).prop_map(|(block, reps)| {
            block
                .iter()
                .cycle()
                .take(block.len() * reps)
                .cloned()
                .collect::<Vec<_>>()
        }),
        0..8,
    )
    .prop_map(|blocks| blocks.into_iter().flatten().take(150).collect())
}

fn config() -> impl Strategy<Value = AlignConfig> {
    (
        prop_oneof![
            Just(AlignKind::Standard),
            Just(AlignKind::Standard),
            Just(AlignKind::None)
        ],
        prop_oneof![
            Just(1u32),
            Just(2),
            Just(3),
            Just(7),
            Just(12),
            Just(16),
            Just(40),
            Just(2000)
        ],
        proptest::option::of(any::<bool>()),
        any::<bool>(),
        any::<bool>(),
    )
        .prop_map(
            |(kind, skew, closeness, same_only, orphans_sig)| AlignConfig {
                kind,
                skew_tolerance: skew,
                use_closeness: closeness,
                closeness_same_only: same_only,
                never_align_diffs: false,
                orphans_always_sig: orphans_sig,
            },
        )
}

fn run(left: &[String], right: &[String], cfg: &AlignConfig, rules: &LineRules) -> Vec<AlignOp> {
    let src = VecSource::new(left, right, rules);
    let mut ops = Vec::new();
    align_with_scratch(
        &src,
        cfg,
        AlignRange::full(),
        &CancelToken::new(),
        &mut |o| ops.push(o),
        &mut AlignScratch::new(),
    )
    .expect("a valid configuration aligns");
    ops
}

/// Every line of each side appears in exactly one op, in increasing order, with no gaps.
fn check_coverage(ops: &[AlignOp], n: usize, m: usize) -> Result<(), TestCaseError> {
    let (mut next_s, mut next_t) = (0u32, 0u32);
    for (i, op) in ops.iter().enumerate() {
        prop_assert!(op.amount >= 1, "op {i} has no lines: {op:?}");
        prop_assert!(
            op.src != core_diff::ORPHAN || op.tgt != core_diff::ORPHAN,
            "op {i} has no side"
        );
        if let Some(s) = op.src_opt() {
            prop_assert_eq!(s, next_s, "op {} source start (ops {:?})", i, ops);
            next_s += op.amount;
        }
        if let Some(t) = op.tgt_opt() {
            prop_assert_eq!(t, next_t, "op {} target start (ops {:?})", i, ops);
            next_t += op.amount;
        }
    }
    prop_assert_eq!(
        (next_s as usize, next_t as usize),
        (n, m),
        "ops do not cover the input"
    );
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 10_000, ..ProptestConfig::default() })]

    /// Ops cover every line of both sides exactly once, in increasing order.
    #[test]
    fn ops_cover_every_line_once_in_order(left in text(), right in text(), cfg in config()) {
        let rules = LineRules::new();
        let ops = run(&left, &right, &cfg, &rules);
        check_coverage(&ops, left.len(), right.len())?;
    }

    /// The same holds under other rules (ignore case, ignore all whitespace, comments).
    #[test]
    fn coverage_holds_under_other_rules(left in text(), right in text(), cfg in config(),
                                         case in any::<bool>()) {
        let rules = LineRules::new()
            .with_ignore_case(case)
            .with_whitespace(core_text::WhitespaceMode::IgnoreAll)
            .with_unimportant(["//.*$"])
            .unwrap();
        let ops = run(&left, &right, &cfg, &rules);
        check_coverage(&ops, left.len(), right.len())?;
    }

    /// Identical inputs give only paired ops on the diagonal.
    #[test]
    fn identical_inputs_pair_everything(lines in text(), cfg in config()) {
        let ops = run(&lines, &lines, &cfg, &LineRules::new());
        let mut next = 0;
        for op in &ops {
            prop_assert_eq!((op.src, op.tgt), (next, next), "{:?}", ops);
            next += op.amount;
        }
        prop_assert_eq!(next as usize, lines.len());
    }

    /// `None` is a common prefix plus in-order pairing: line k pairs with line k up to the
    /// shorter side, the rest are orphans (inputs here have no insignificant lines, so no
    /// pending gap interferes).
    #[test]
    fn none_pairs_in_order(
        left in proptest::collection::vec("[a-c]{1,3}", 0..40),
        right in proptest::collection::vec("[a-c]{1,3}", 0..40),
    ) {
        let cfg = AlignConfig { kind: AlignKind::None, ..AlignConfig::default() };
        let ops = run(&left, &right, &cfg, &LineRules::new());
        check_coverage(&ops, left.len(), right.len())?;
        let common = left.len().min(right.len()) as u32;
        let mut paired = 0u32;
        for op in &ops {
            if op.is_paired() {
                prop_assert_eq!(op.src, op.tgt, "paired lines keep their index: {:?}", ops);
                paired += op.amount;
            } else if op.src_opt().is_some() {
                prop_assert!(op.src >= common, "a source orphan before the shorter side ended: {:?}", ops);
            } else {
                prop_assert!(op.tgt >= common, "a target orphan before the shorter side ended: {:?}", ops);
            }
        }
        prop_assert_eq!(paired, common);
    }

    /// Cancelling after a few ops never panics and returns `Cancelled`; ops delivered up to
    /// then were a valid prefix.
    #[test]
    fn cancelling_midway_is_clean(left in text(), right in text(), cfg in config(), stop_after in 1usize..6) {
        let src = VecSource::new(&left, &right, &LineRules::new());
        let cancel = CancelToken::new();
        let mut ops: Vec<AlignOp> = Vec::new();
        let r = align_with_scratch(&src, &cfg, AlignRange::full(), &cancel, &mut |o| {
            ops.push(o);
            if ops.len() == stop_after { cancel.cancel(); }
        }, &mut AlignScratch::new());
        match r {
            Ok(()) => check_coverage(&ops, left.len(), right.len())?,
            Err(e) => {
                prop_assert!(e.is_cancelled());
                // a valid prefix: contiguous from 0 on each side
                let (mut s, mut t) = (0u32, 0u32);
                for op in &ops {
                    if let Some(x) = op.src_opt() { prop_assert_eq!(x, s); s += op.amount; }
                    if let Some(x) = op.tgt_opt() { prop_assert_eq!(x, t); t += op.amount; }
                }
            }
        }
    }

    /// A reused scratch gives the same ops as a fresh one (no state leaks between runs).
    #[test]
    fn scratch_reuse_is_transparent(a in (text(), text()), b in (text(), text()), cfg in config()) {
        let rules = LineRules::new();
        let mut scratch = AlignScratch::new();
        let go = |l: &[String], r: &[String], scratch: &mut AlignScratch| {
            let src = VecSource::new(l, r, &rules);
            let mut ops = Vec::new();
            align_with_scratch(&src, &cfg, AlignRange::full(), &CancelToken::new(), &mut |o| ops.push(o), scratch).unwrap();
            ops
        };
        let first = go(&a.0, &a.1, &mut scratch);
        let second = go(&b.0, &b.1, &mut scratch);
        let fresh_first = go(&a.0, &a.1, &mut AlignScratch::new());
        let fresh_second = go(&b.0, &b.1, &mut AlignScratch::new());
        prop_assert_eq!(first, fresh_first);
        prop_assert_eq!(second, fresh_second);
    }
}

/// Decision D23: swapping the sides is **not** an exact mirror. The gap DP breaks ties as
/// pair > source orphan > target orphan (spec ALIGN-1.5), which favours the left side, so two
/// equally good alignments are chosen differently after a swap. Coverage and order hold either
/// way (the properties above); only the placement of orphans among equal-score choices
/// differs. The model (`refmodel.py`) agrees on both results.
///
/// Left `[a1, b2]` vs right `[b2, a1]`: the best score pairs one equal line and orphans the
/// other two (280 - 40 = 240). "Pair a1" and "pair b2" score the same; the tie goes to the
/// choice that orphans the *source* line first.
#[test]
fn swapping_sides_is_not_an_exact_mirror_decision_d23() {
    let rules = LineRules::new();
    let cfg = AlignConfig::default();
    let l: Vec<String> = ["a1", "b2"].map(String::from).to_vec();
    let r: Vec<String> = ["b2", "a1"].map(String::from).to_vec();
    let forward = run(&l, &r, &cfg, &rules);
    let swapped = run(&r, &l, &cfg, &rules);
    let show = |ops: &[AlignOp]| ops.iter().map(ToString::to_string).collect::<Vec<_>>();
    assert_eq!(show(&forward), ["0 - 1", "1 0 1", "- 1 1"]);
    assert_eq!(show(&swapped), ["0 - 1", "1 0 1", "- 1 1"]);
    // the mirror image of `forward` would be `- 0 1`, `0 1 1`, `1 - 1`: a different alignment
    let mirrored: Vec<String> = forward
        .iter()
        .map(|o| {
            AlignOp {
                src: o.tgt,
                tgt: o.src,
                amount: o.amount,
            }
            .to_string()
        })
        .collect();
    assert_ne!(mirrored, show(&swapped));
}
