//! Golden alignment cases (backlog E2-S9, plan.md §11.3): every directory under
//! `tests/fixtures/text` is aligned with its `options.json` and the resulting ops and rows
//! are compared with `expected.ops` / `expected.rows`.
//!
//! The expected files come from the reference model plus the hand-derived values of plan.md
//! §11.3 (see `tests/fixtures/text/gen_golden.py`), never from this engine.

#[path = "../../../tests/common/mod.rs"]
mod common;

use common::{Case, ExpOp, ExpRow, diff_report, fixtures_root, list_cases, load_case};
use core_base::CancelToken;
use core_diff::{AlignOp, AlignRange, DocSource, Side, TextOptions, align};
use core_text::TextDocument;
use core_text::encoding::DecodeOptions;

struct Outcome {
    ops: Vec<ExpOp>,
    rows: Vec<ExpRow>,
}

fn opt(v: u32) -> Option<u32> {
    (v != core_diff::ORPHAN).then_some(v)
}

/// Runs one case through the engine.
fn run_case(case: &Case) -> Outcome {
    let options = case
        .options_json
        .as_deref()
        .map_or_else(|| Ok(TextOptions::default()), TextOptions::from_json)
        .unwrap_or_else(|e| panic!("{}: {e}", case.name));
    let rules = options.line_rules().unwrap();
    let dopt = DecodeOptions::default();
    let left = TextDocument::from_bytes(case.left.clone(), &dopt).unwrap();
    let right = TextDocument::from_bytes(case.right.clone(), &dopt).unwrap();
    let source = DocSource::new(&left, &right, &rules, None);

    let mut ops: Vec<AlignOp> = Vec::new();
    align(
        &source,
        &options.align_config(),
        AlignRange::full(),
        &CancelToken::new(),
        &mut |op| ops.push(op),
    )
    .unwrap_or_else(|e| panic!("{}: {e}", case.name));

    let orphans_sig = options.orphans_always_sig;
    let mut rows = Vec::new();
    for op in &ops {
        for k in 0..op.amount {
            let (l, r) = (op.src_opt().map(|v| v + k), op.tgt_opt().map(|v| v + k));
            let kind = match (l, r) {
                (Some(a), Some(b)) => {
                    if source.full_text_eq(a, b) {
                        "Same"
                    } else if source.comparable_eq(a, b) {
                        "Unimportant"
                    } else {
                        "Different"
                    }
                }
                (Some(a), None) => {
                    if orphans_sig || !source.is_insignificant(Side::Left, a) {
                        "LeftOnly"
                    } else {
                        "UnimportantLeftOnly"
                    }
                }
                (None, Some(b)) => {
                    if orphans_sig || !source.is_insignificant(Side::Right, b) {
                        "RightOnly"
                    } else {
                        "UnimportantRightOnly"
                    }
                }
                (None, None) => unreachable!(),
            };
            rows.push(ExpRow {
                left: l,
                right: r,
                kind: kind.to_string(),
            });
        }
    }
    let ops = ops
        .iter()
        .map(|o| ExpOp {
            src: opt(o.src),
            tgt: opt(o.tgt),
            amount: o.amount,
        })
        .collect();
    Outcome { ops, rows }
}

use core_diff::LineSource as _;

#[test]
fn all_golden_cases() {
    let root = fixtures_root(env!("CARGO_MANIFEST_DIR"));
    let dirs = list_cases(&root).unwrap();
    let mut failures = Vec::new();
    let mut checked = 0;
    for dir in dirs {
        let case = load_case(&dir).unwrap_or_else(|e| panic!("{e}"));
        let got = run_case(&case);
        let mut report = String::new();
        if got.ops != case.ops {
            report += &diff_report("ops", &case.ops, &got.ops);
        }
        if got.rows != case.rows {
            report += &diff_report("rows", &case.rows, &got.rows);
        }
        if !report.is_empty() {
            failures.push(format!("--- {}\n{report}", case.name));
        }
        checked += 1;
    }
    assert!(
        checked >= 14,
        "expected at least G1-G14, found {checked} cases"
    );
    assert!(
        failures.is_empty(),
        "golden failures:\n{}",
        failures.join("\n")
    );
}

/// Backlog E2-S9.1: the runner works on the G4 example from the loader test.
#[test]
fn g4_runs_through_the_runner() {
    let root = fixtures_root(env!("CARGO_MANIFEST_DIR"));
    let case = load_case(&root.join("g4_right_only_insert")).unwrap();
    let got = run_case(&case);
    assert_eq!(got.ops.len(), 4);
    assert_eq!(got.ops, case.ops);
}

/// G13/G14 are hand-derived (the model cannot run big runs). Derivation, see the fixtures:
///
/// **G13** (10 header lines, 100 shared). Top level: the first lines differ and are both
/// significant, so the walk stops at once and the whole file goes to the block aligner. The
/// window covers everything (110 < 2000, no `more_after`). The L-scan reaches cell (10,10) at
/// step 10 (column 10 from row 10 upwards) and the run from there has weight 70 per line;
/// 72 lines give 5040 >= 5000, so a big run (10,10,72) is found. Fix B2: the 10x10 block
/// before it is aligned first: no run, gap DP with every pair at -40, all paths tie, ties
/// prefer the pair, the trace is the diagonal: one op `(0,0,10)`. Fix B1: the run extends to
/// the block stop at line 110: 100 lines, never above 500, one op `(10,10,100)`.
///
/// **G14** (500 headers, 8000 shared). Window 2000: the L-scan reaches (500,500) at step 500;
/// big run (500,500,72). Pre-run block 500x500: `(0,0,500)` by the same tie argument. The run
/// extends to the outer stop 8500 (fix B1). A piece is emitted when its count exceeds 500, so
/// pieces have 501 lines: starts 500 + 501k for k = 0..14 (15 pieces, 7515 lines), and the
/// remaining 8000 - 7515 = 485 lines come last as `(8015,8015,485)`. There is no op boundary
/// at line 2000, where the window ended.
#[test]
fn golden_g13_g14() {
    let root = fixtures_root(env!("CARGO_MANIFEST_DIR"));
    for name in [
        "g13_big_run_after_header",
        "g14_big_run_crosses_window_edge",
    ] {
        let case = load_case(&root.join(name)).unwrap();
        let got = run_case(&case);
        assert_eq!(
            got.ops,
            case.ops,
            "{name}\n{}",
            diff_report("ops", &case.ops, &got.ops)
        );
    }
    // The G14 expectation is exactly the derivation above.
    let case = load_case(&root.join("g14_big_run_crosses_window_edge")).unwrap();
    assert_eq!(case.ops.len(), 17);
    assert_eq!(case.ops[0].amount, 500);
    for (k, op) in case.ops[1..16].iter().enumerate() {
        assert_eq!(
            (op.src, op.tgt, op.amount),
            (Some(500 + 501 * k as u32), Some(500 + 501 * k as u32), 501)
        );
    }
    assert_eq!(case.ops[16].amount, 485);
}
