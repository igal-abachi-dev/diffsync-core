//! The Myers, Histogram and Patience align kinds (backlog E3-S5): hand-derived cases, the
//! CRC-collision guard, and the coverage invariants of E2-S9.4 for every kind.

use core_base::CancelToken;
use core_diff::{
    AlignConfig, AlignKind, AlignOp, AlignRange, AlignScratch, VecSource, align_with_scratch,
};
use core_diff::{LineSource, Side};
use core_text::rules::LineRules;
use proptest::prelude::*;

fn run(left: &[String], right: &[String], cfg: &AlignConfig) -> Vec<AlignOp> {
    let src = VecSource::new(left, right, &LineRules::new());
    let mut ops = Vec::new();
    align_with_scratch(
        &src,
        cfg,
        AlignRange::full(),
        &CancelToken::new(),
        &mut |o| ops.push(o),
        &mut AlignScratch::new(),
    )
    .unwrap();
    ops
}

fn strs(v: &[&str]) -> Vec<String> {
    v.iter().map(ToString::to_string).collect()
}

fn show(ops: &[AlignOp]) -> Vec<String> {
    ops.iter().map(ToString::to_string).collect()
}

fn cfg(kind: AlignKind) -> AlignConfig {
    AlignConfig {
        kind,
        ..AlignConfig::default()
    }
}

/// Myers has closeness off by default: a changed hunk pairs its lines in order (spec ALIGN-1.7,
/// "estimated"), the equal runs are exact matches.
/// `a b c d` vs `a x c d`: matches (0,0), (2,2), (3,3) → ops `0 0 1`, hunk `1 1 1`, run `2 2 2`.
#[test]
fn myers_pairs_changed_hunks_in_order_and_emits_match_runs() {
    let ops = run(
        &strs(&["a", "b", "c", "d"]),
        &strs(&["a", "x", "c", "d"]),
        &cfg(AlignKind::Myers),
    );
    assert_eq!(show(&ops), ["0 0 1", "1 1 1", "2 2 2"]);
}

/// An unequal hunk: 2 source lines against 1 target line pair the first, orphan the rest.
#[test]
fn myers_hunk_with_unequal_sides_leaves_orphans() {
    let ops = run(
        &strs(&["a", "p", "q", "z"]),
        &strs(&["a", "r", "z"]),
        &cfg(AlignKind::Myers),
    );
    assert_eq!(show(&ops), ["0 0 1", "1 1 1", "2 - 1", "3 2 1"]);
}

/// Histogram has closeness on, so a changed hunk goes through the gap DP.
#[test]
fn histogram_fills_hunks_with_the_closeness_dp() {
    let ops = run(
        &strs(&["a", "int x = 1;", "z"]),
        &strs(&["a", "int x = 2;", "z"]),
        &cfg(AlignKind::Histogram),
    );
    assert_eq!(show(&ops), ["0 0 1", "1 1 1", "2 2 1"]);
}

/// Patience: `a` pairs at the top level; the block `x1 x2 b` vs `y1 b` has the common suffix
/// `b`, no unique anchors, so the hunk `x1 x2` vs `y1` goes to the DP (pair first, then the
/// orphan) and the suffix is emitted last.
#[test]
fn patience_trims_the_suffix_and_gap_fills_the_hunk() {
    let ops = run(
        &strs(&["a", "x1", "x2", "b"]),
        &strs(&["a", "y1", "b"]),
        &cfg(AlignKind::Patience),
    );
    assert_eq!(show(&ops), ["0 0 1", "1 1 1", "2 - 1", "3 2 1"]);
}

/// Patience anchors: unique lines `m1` and `m2` on both sides split the block.
#[test]
fn patience_uses_unique_anchors() {
    let l = strs(&["h", "p1", "m1", "p2", "m2", "p3"]);
    let r = strs(&["h2", "q1", "m1", "q2", "m2", "q3"]);
    let ops = run(&l, &r, &cfg(AlignKind::Patience));
    // m1 and m2 are paired as anchors, everything else is paired around them
    assert!(
        ops.iter()
            .any(|o| o.src == 2 && o.tgt == 2 && o.amount == 1),
        "{:?}",
        show(&ops)
    );
    assert!(
        ops.iter()
            .any(|o| o.src == 4 && o.tgt == 4 && o.amount == 1),
        "{:?}",
        show(&ops)
    );
}

/// Patience orphan chunks are 2000 lines (spec ALIGN-1.8), not 200.
#[test]
fn patience_orphan_chunks_are_2000() {
    let right: Vec<String> = (0..4500).map(|k| format!("line {k}")).collect();
    let ops = run(&[], &right, &cfg(AlignKind::Patience));
    assert_eq!(show(&ops), ["- 0 2000", "- 2000 2000", "- 4000 500"]);
}

/// Two different strings with the same CRC line hash (crafted with CRC-32 forcing: both give
/// `0x1234ABCD`). Backlog E3-S5.3: a collision never pairs different lines.
const COLLIDE_A: &str = "ab\u{406e}\u{2485}";
const COLLIDE_B: &str = "xyz\u{5654}\u{44c4}";

#[test]
fn the_crafted_strings_really_collide() {
    let src = VecSource::new(&[COLLIDE_A], &[COLLIDE_B], &LineRules::new());
    assert_eq!(src.meta(Side::Left, 0).hash, 0x1234_ABCD);
    assert_eq!(src.meta(Side::Left, 0).hash, src.meta(Side::Right, 0).hash);
    assert_ne!(COLLIDE_A, COLLIDE_B);
}

#[test]
fn a_crc_collision_never_pairs_different_lines_in_the_exact_kinds() {
    // never_align_diffs: unmatched lines can only be orphans, so a (wrongly) accepted match
    // would show up as a paired op.
    for kind in [AlignKind::Myers, AlignKind::Histogram, AlignKind::Patience] {
        let c = AlignConfig {
            kind,
            use_closeness: Some(false),
            never_align_diffs: true,
            ..AlignConfig::default()
        };
        let ops = run(&strs(&[COLLIDE_A]), &strs(&[COLLIDE_B]), &c);
        assert_eq!(show(&ops), ["0 - 1", "- 0 1"], "{kind:?}");
        // with a real match around it, the colliding pair is still not matched
        let ops = run(
            &strs(&["s", COLLIDE_A, "e"]),
            &strs(&["s", COLLIDE_B, "e"]),
            &c,
        );
        assert!(
            ops.iter().all(|o| !(o.src == 1 && o.tgt == 1)),
            "{kind:?}: {:?}",
            show(&ops)
        );
    }
}

// ----------------------------------------------------------------- properties

const POOL: &[&str] = &[
    "alpha",
    "beta",
    "gamma",
    "delta",
    "",
    "  alpha",
    "// c",
    "int x = 1;",
    "int x = 2;",
    "}",
    "end;",
];

fn text() -> impl Strategy<Value = Vec<String>> {
    proptest::collection::vec(
        proptest::sample::select(POOL).prop_map(str::to_owned),
        0..60,
    )
}

fn coverage(ops: &[AlignOp], n: usize, m: usize) -> Result<(), TestCaseError> {
    let (mut s, mut t) = (0u32, 0u32);
    for op in ops {
        prop_assert!(op.amount >= 1);
        if let Some(x) = op.src_opt() {
            prop_assert_eq!(x, s, "{:?}", ops);
            s += op.amount;
        }
        if let Some(x) = op.tgt_opt() {
            prop_assert_eq!(x, t, "{:?}", ops);
            t += op.amount;
        }
    }
    prop_assert_eq!((s as usize, t as usize), (n, m));
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 5_000, ..ProptestConfig::default() })]

    /// The coverage invariant holds for every kind and option mix.
    #[test]
    fn every_kind_covers_each_line_once_in_order(
        l in text(), r in text(),
        kind in prop_oneof![Just(AlignKind::Patience), Just(AlignKind::Myers), Just(AlignKind::Histogram)],
        closeness in proptest::option::of(any::<bool>()),
    ) {
        let c = AlignConfig { kind, use_closeness: closeness, ..AlignConfig::default() };
        coverage(&run(&l, &r, &c), l.len(), r.len())?;
    }

    /// Identical inputs give only diagonal pairs, for every kind.
    #[test]
    fn every_kind_pairs_identical_inputs(
        l in text(),
        kind in prop_oneof![Just(AlignKind::Patience), Just(AlignKind::Myers), Just(AlignKind::Histogram)],
    ) {
        let ops = run(&l, &l, &cfg(kind));
        let mut next = 0;
        for op in &ops {
            prop_assert_eq!((op.src, op.tgt), (next, next));
            next += op.amount;
        }
        prop_assert_eq!(next as usize, l.len());
    }
}
