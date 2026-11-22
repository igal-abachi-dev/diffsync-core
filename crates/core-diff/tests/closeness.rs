//! The closeness table of plan.md §11.3 through the public API (backlog E2-S2): every value
//! was checked with `refmodel.py closeness`. The aligner-level behaviour (D2 twin preference)
//! is pinned by the golden case `d2_whitespace_twin_preferred`.

use core_diff::align::closeness::{Histogram, closeness_bytes, closeness_equal_hash};

fn score(a: &str, b: &str, same_only: bool) -> i32 {
    closeness_bytes(
        a.as_bytes(),
        b.as_bytes(),
        same_only,
        &mut Histogram::default(),
    )
}

#[test]
fn different_hash_rows() {
    // (A, B, score, score with closeness_same_only)
    let table: &[(&str, &str, i32, i32)] = &[
        ("bravo", "bravx", 10, 4),
        ("int x = 1;", "int x = 2;", 17, 9),
        ("return result;", "return value;", 11, 11),
        ("a = b + c", "c = a + b", 25, 9), // the order-blind trap, kept in v1
        ("aaaa", "zzzz", -33, 0),
        ("abcdefghij", "klmnopqrst", -40, 0), // floor
        ("", "abc", -13, -18),
        ("x", "", -11, -18), // ceiling
    ];
    for &(a, b, plain, same_only) in table {
        assert_eq!(score(a, b, false), plain, "{a:?} vs {b:?}");
        assert_eq!(score(a, b, true), same_only, "same-only {a:?} vs {b:?}");
        // closeness of two texts is symmetric
        assert_eq!(score(b, a, false), plain, "{b:?} vs {a:?}");
    }
}

#[test]
fn equal_hash_rows_decision_d2() {
    // identical full lines, significant: 2 * (70 + 70)
    assert_eq!(closeness_equal_hash(true, true, 70, 70, || true), 280);
    // equal hash, full lines differ only in ignored whitespace: one less
    assert_eq!(closeness_equal_hash(true, true, 70, 70, || false), 279);
    // comparable text empty on either side: -10
    assert_eq!(closeness_equal_hash(false, false, 2, 2, || true), -10);
    assert_eq!(closeness_equal_hash(true, false, 70, 31, || true), -10);
}

#[test]
fn closeness_off_scores_are_constants() {
    // These come from the aligner (`Aligner::closeness`); the pure functions only cover the
    // texts. With closeness off: -18, and -5000 with never_align_diffs (aligner tests).
    assert_eq!(core_diff::align::consts::NO_CLOSENESS_SCORE, -18);
    assert_eq!(core_diff::align::consts::NEVER_ALIGN_SCORE, -5000);
}
