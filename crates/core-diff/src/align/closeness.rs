//! The closeness score of two different lines (spec ALIGN-1.6, plan.md §5.2, decision D2).
//!
//! Integer arithmetic only, truncating division, the exact constants of Appendix A. The
//! pure functions here are what the unit tests check against the table of plan.md §11.3;
//! `Aligner::closeness` adds the metadata and the text cache around them.

use super::consts::{
    CLOSENESS_EMPTY_CEILING, CLOSENESS_EQUAL_EMPTY, CLOSENESS_FLOOR, CLOSENESS_POSITIVE_SCALE,
    CLOSENESS_SAME_ONLY_EMPTY,
};

/// A 256-bucket byte histogram, reused between calls. `u32` buckets cannot overflow for any
/// line a `u32` length can hold.
#[derive(Debug)]
pub struct Histogram(pub(super) Box<[u32; 256]>);

impl Default for Histogram {
    fn default() -> Self {
        Histogram(Box::new([0; 256]))
    }
}

/// Size of the multiset intersection of the bytes of `a` and `b` (spec ALIGN-1.6,
/// `ByteOverlap`). The histogram is cleaned up through the touched bytes, not with a full
/// clear, so short lines stay cheap.
pub fn byte_overlap(a: &[u8], b: &[u8], hist: &mut Histogram) -> u32 {
    let h = &mut *hist.0;
    for &x in a {
        h[x as usize] += 1;
    }
    let mut common = 0;
    for &x in b {
        let slot = &mut h[x as usize];
        if *slot > 0 {
            *slot -= 1;
            common += 1;
        }
    }
    // Reset: only the buckets `a` touched can be non-zero.
    if a.len() >= 256 {
        h.fill(0);
    } else {
        for &x in a {
            h[x as usize] = 0;
        }
    }
    common
}

/// Closeness of two lines whose hashes differ, given their closeness texts as UTF-8 bytes
/// (spec ALIGN-1.6).
///
/// * either text empty → `same_only ? -18 : clamp(-4(la + lb) - 1, -40, -11)`;
/// * `common` = multiset byte overlap; `same_only` → `common`;
/// * else `s = 3·common - la - lb`; `s > 0` → `s·25 / min(la, lb)` (truncating);
///   `s <= 0` → `max(-40, 4s - 1)`.
pub fn closeness_bytes(a: &[u8], b: &[u8], same_only: bool, hist: &mut Histogram) -> i32 {
    // i64 so that a (pathological) multi-gigabyte line cannot overflow `s * 25`.
    let (la, lb) = (a.len() as i64, b.len() as i64);
    if la == 0 || lb == 0 {
        if same_only {
            return CLOSENESS_SAME_ONLY_EMPTY;
        }
        let s = -4 * (la + lb) - 1;
        return s.clamp(
            i64::from(CLOSENESS_FLOOR),
            i64::from(CLOSENESS_EMPTY_CEILING),
        ) as i32;
    }
    closeness_with_common(la, lb, i64::from(byte_overlap(a, b, hist)), same_only)
}

/// The non-empty branch of the closeness formula, given the byte overlap `common`
/// (spec ALIGN-1.6). Both lengths must be non-zero.
pub fn closeness_with_common(la: i64, lb: i64, common: i64, same_only: bool) -> i32 {
    if same_only {
        return common as i32;
    }
    let s = 3 * common - la - lb;
    if s > 0 {
        (s * CLOSENESS_POSITIVE_SCALE / la.min(lb)) as i32
    } else {
        (4 * s - 1).max(i64::from(CLOSENESS_FLOOR)) as i32
    }
}

/// Fills `counts` (256 buckets) with the byte counts of `text`. Returns false when a bucket
/// would exceed 255, in which case the table must not be used (use [`byte_overlap`]).
pub fn fill_counts(text: &[u8], counts: &mut Vec<u8>) -> bool {
    counts.clear();
    counts.resize(256, 0);
    let mut exact = true;
    for &b in text {
        let c = &mut counts[b as usize];
        if *c == 255 {
            exact = false;
        } else {
            *c += 1;
        }
    }
    exact
}

/// Size of the multiset intersection from two count tables (`sum of min`). The inner sums stay
/// in `u16` lanes (32 x 255 fits) so the loop vectorizes.
pub fn overlap_from_counts(a: &[u8], b: &[u8]) -> u32 {
    let mut total = 0u32;
    for (ca, cb) in a.chunks_exact(32).zip(b.chunks_exact(32)) {
        let mut s = 0u16;
        for (&x, &y) in ca.iter().zip(cb) {
            s += u16::from(x.min(y));
        }
        total += u32::from(s);
    }
    total
}

/// Closeness of two lines with **equal hashes** (decision D2): `-10` when either comparable
/// text is empty, otherwise `2(wS + wT)`, minus 1 when the full lines are not byte-identical.
pub fn closeness_equal_hash(
    src_sig: bool,
    tgt_sig: bool,
    src_weight: i32,
    tgt_weight: i32,
    full_lines_identical: impl FnOnce() -> bool,
) -> i32 {
    if !src_sig || !tgt_sig {
        return CLOSENESS_EQUAL_EMPTY;
    }
    2 * (src_weight + tgt_weight) - i32::from(!full_lines_identical())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn score(a: &str, b: &str, same_only: bool) -> i32 {
        closeness_bytes(
            a.as_bytes(),
            b.as_bytes(),
            same_only,
            &mut Histogram::default(),
        )
    }

    /// The closeness table of plan.md §11.3; the values were checked with
    /// `refmodel.py closeness`.
    #[test]
    fn plan_table_different_hashes() {
        let rows: &[(&str, &str, i32, i32)] = &[
            ("bravo", "bravx", 10, 4),
            ("int x = 1;", "int x = 2;", 17, 9),
            ("return result;", "return value;", 11, 11),
            // the order-blind trap, kept in v1 (fixed in v2)
            ("a = b + c", "c = a + b", 25, 9),
            ("aaaa", "zzzz", -33, 0),
            ("abcdefghij", "klmnopqrst", -40, 0),
            ("", "abc", -13, -18),
            ("x", "", -11, -18),
        ];
        for &(a, b, plain, same_only) in rows {
            assert_eq!(score(a, b, false), plain, "{a:?} vs {b:?}");
            assert_eq!(score(a, b, true), same_only, "same-only {a:?} vs {b:?}");
        }
    }

    #[test]
    fn empty_side_formula_floor_and_ceiling() {
        // -4(la+lb)-1: 1 byte → -5 → ceiling -11; 3 bytes → -13; 10+ bytes → floor -40
        assert_eq!(score("", "x", false), -11);
        assert_eq!(score("", "abc", false), -13);
        assert_eq!(score("", "abcdefghijkl", false), -40);
        assert_eq!(score("", "", false), -11);
        assert_eq!(score("", "", true), -18);
    }

    #[test]
    fn negative_branch_is_4s_minus_1_with_floor() {
        // 'aaaa' vs 'zzzz': common 0, s = -8 → 4·(-8)-1 = -33
        assert_eq!(score("aaaa", "zzzz", false), -33);
        // 'ab' vs 'ac': common 1, s = 3·1 - 4 = -1 → 4·(-1) - 1 = -5
        assert_eq!(score("ab", "ac", false), -5);
        // very different long lines hit the floor
        assert_eq!(score("abcdefghijklmnop", "qrstuvwxyz012345", false), -40);
    }

    #[test]
    fn positive_branch_truncates() {
        // identical-ish: 'abc' vs 'abd': common 2, s = 6-6 = 0 → -1
        assert_eq!(score("abc", "abd", false), -1);
        // 'abcd' vs 'abce': common 3, s = 9-8 = 1 → 1·25/4 = 6 (truncated from 6.25)
        assert_eq!(score("abcd", "abce", false), 6);
        // a letter-permutation scores high: the v1 order-blind behaviour
        assert_eq!(score("abc", "cba", false), 25 * 3 / 3);
    }

    #[test]
    fn multiset_overlap_counts_repeats() {
        let mut h = Histogram::default();
        assert_eq!(byte_overlap(b"aab", b"aaa", &mut h), 2);
        assert_eq!(byte_overlap(b"aaa", b"aab", &mut h), 2);
        assert_eq!(byte_overlap(b"", b"abc", &mut h), 0);
        assert_eq!(byte_overlap("é".as_bytes(), "é".as_bytes(), &mut h), 2);
    }

    #[test]
    fn histogram_is_clean_after_every_call() {
        let mut h = Histogram::default();
        byte_overlap(b"abcabc", b"zzz", &mut h);
        assert!(
            h.0.iter().all(|&c| c == 0),
            "short lines: reset through the touched bytes"
        );
        let long = vec![7u8; 1000];
        byte_overlap(&long, b"\x07\x07", &mut h);
        assert!(h.0.iter().all(|&c| c == 0), "long lines: full clear");
        // identical results on a second use of the same histogram
        assert_eq!(byte_overlap(b"abc", b"cab", &mut h), 3);
    }

    #[test]
    fn long_lines_do_not_overflow() {
        let a = vec![b'a'; 70_000];
        let b = vec![b'a'; 70_000];
        // overlap 70 000 > u16::MAX would wrap with u16 buckets
        let mut h = Histogram::default();
        assert_eq!(byte_overlap(&a, &b, &mut h), 70_000);
        // s = 3·70000 - 140000 = 70000 → 70000·25/70000 = 25
        assert_eq!(closeness_bytes(&a, &b, false, &mut h), 25);
    }

    #[test]
    fn count_tables_give_the_same_overlap_as_the_histogram() {
        let mut h = Histogram::default();
        let (mut ca, mut cb) = (Vec::new(), Vec::new());
        for (a, b) in [
            ("bravo", "bravx"),
            ("a = b + c", "c = a + b"),
            ("aaaa", "zzzz"),
            ("שלום", "שלם"),
            ("x", "xxxxx"),
        ] {
            assert!(fill_counts(a.as_bytes(), &mut ca) && fill_counts(b.as_bytes(), &mut cb));
            assert_eq!(
                overlap_from_counts(&ca, &cb),
                byte_overlap(a.as_bytes(), b.as_bytes(), &mut h),
                "{a} {b}"
            );
        }
        let long = vec![b'a'; 300];
        assert!(
            !fill_counts(&long, &mut ca),
            "a bucket above 255 disables the table"
        );
    }

    #[test]
    fn equal_hash_table() {
        // identical full lines, both significant: 2·(70+70) = 280
        assert_eq!(closeness_equal_hash(true, true, 70, 70, || true), 280);
        // equal hash but the full lines differ (ignored whitespace): 279 (D2)
        assert_eq!(closeness_equal_hash(true, true, 70, 70, || false), 279);
        // either comparable text empty: -10, and the full-line check is not even made
        assert_eq!(
            closeness_equal_hash(false, true, 31, 70, || panic!("not needed")),
            -10
        );
        assert_eq!(
            closeness_equal_hash(true, false, 70, 31, || panic!("not needed")),
            -10
        );
        assert_eq!(closeness_equal_hash(false, false, 2, 2, || true), -10);
    }
}
