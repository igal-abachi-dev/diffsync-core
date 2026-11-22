//! Patience anchors (plan.md §5.3, spec ALIGN-1.8, backlog E3-S3).
//!
//! The lines that occur exactly once in *both* ranges, taking only "usable" lines into account,
//! then the longest increasing subsequence of their target positions (patience sorting,
//! O(n log n)). A port of `Dc.Myers.DcPatienceAnchors`, with half-open ranges.

use super::{FastMap, fast_map};

#[derive(Debug, Clone, Copy, Default)]
struct Occ {
    count_a: u32,
    count_b: u32,
    pos_a: u32,
    pos_b: u32,
}

/// Reusable buffers.
#[derive(Debug, Default)]
pub struct PatienceScratch {
    occ: FastMap<u32, Occ>,
    cand: Vec<(u32, u32)>,
    tails: Vec<u32>,
    prev: Vec<u32>,
}

impl PatienceScratch {
    pub fn new() -> Self {
        Self {
            occ: fast_map(),
            ..Self::default()
        }
    }
}

const NONE: u32 = u32::MAX;

/// Writes the anchors of `a[a0..a1)` x `b[b0..b1)` to `out` (cleared first) as increasing
/// `(i, j)` pairs with `a[i] == b[j]`. Only items for which `usable_a(i)` / `usable_b(j)` hold
/// count (the aligner passes "is significant"), both for uniqueness and as anchors.
pub fn patience_anchors(
    a: &[u32],
    b: &[u32],
    (a0, a1): (usize, usize),
    (b0, b1): (usize, usize),
    usable_a: impl Fn(usize) -> bool,
    usable_b: impl Fn(usize) -> bool,
    scratch: &mut PatienceScratch,
    out: &mut Vec<(u32, u32)>,
) {
    out.clear();
    let PatienceScratch {
        occ,
        cand,
        tails,
        prev,
    } = scratch;
    occ.clear();
    for i in (a0..a1).filter(|&i| usable_a(i)) {
        let o = occ.entry(a[i]).or_default();
        o.count_a += 1;
        o.pos_a = i as u32;
    }
    for j in (b0..b1).filter(|&j| usable_b(j)) {
        if let Some(o) = occ.get_mut(&b[j]) {
            o.count_b += 1;
            o.pos_b = j as u32;
        }
    }
    // candidates in order of their position in a
    cand.clear();
    for i in (a0..a1).filter(|&i| usable_a(i)) {
        if let Some(o) = occ.get(&a[i]) {
            if o.count_a == 1 && o.count_b == 1 {
                cand.push((i as u32, o.pos_b));
            }
        }
    }
    if cand.is_empty() {
        return;
    }

    // longest increasing subsequence on the target position (patience sorting)
    let n = cand.len();
    tails.clear();
    prev.clear();
    prev.resize(n, NONE);
    for (idx, &(_, pb)) in cand.iter().enumerate() {
        // first pile whose top is >= pb
        let lo = tails.partition_point(|&t| cand[t as usize].1 < pb);
        if lo > 0 {
            prev[idx] = tails[lo - 1];
        }
        if lo == tails.len() {
            tails.push(idx as u32);
        } else {
            tails[lo] = idx as u32;
        }
    }
    let len = tails.len();
    out.resize(len, (0, 0));
    let mut k = tails[len - 1];
    for slot in out.iter_mut().rev() {
        *slot = cand[k as usize];
        k = prev[k as usize];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn anchors(a: &[u32], b: &[u32]) -> Vec<(u32, u32)> {
        let mut out = Vec::new();
        patience_anchors(
            a,
            b,
            (0, a.len()),
            (0, b.len()),
            |_| true,
            |_| true,
            &mut PatienceScratch::new(),
            &mut out,
        );
        out
    }

    #[test]
    fn unique_lines_in_both_become_anchors() {
        // 1 and 3 are unique in both; 2 occurs twice on the left
        assert_eq!(anchors(&[1, 2, 2, 3], &[1, 2, 3]), vec![(0, 0), (3, 2)]);
    }

    #[test]
    fn longest_increasing_chain_wins_over_crossing_pairs() {
        // a = 1 2 3 4 5, b = 2 1 3 5 4: LIS of b-positions (1,0,2,4,3) has length 3
        let got = anchors(&[1, 2, 3, 4, 5], &[2, 1, 3, 5, 4]);
        assert_eq!(got.len(), 3);
        assert!(got.windows(2).all(|w| w[0].0 < w[1].0 && w[0].1 < w[1].1));
        for (i, j) in got {
            assert_eq!([1, 2, 3, 4, 5][i as usize], [2, 1, 3, 5, 4][j as usize]);
        }
    }

    #[test]
    fn no_unique_lines_means_no_anchors() {
        assert!(anchors(&[1, 1, 2, 2], &[1, 1, 2, 2]).is_empty());
        assert!(anchors(&[], &[1]).is_empty());
        assert!(anchors(&[1], &[2]).is_empty());
    }

    #[test]
    fn items_unique_on_one_side_only_are_not_anchors() {
        assert!(anchors(&[7], &[7, 7]).is_empty());
        assert!(anchors(&[7, 7], &[7]).is_empty());
    }

    #[test]
    fn unusable_items_neither_count_nor_anchor() {
        let (a, b) = ([5u32, 9, 5], [5u32, 9]);
        let mut out = Vec::new();
        let mut sc = PatienceScratch::new();
        // 5 appears twice in a (not unique), 9 is unique; with item 1 of a unusable, no anchor
        patience_anchors(
            &a,
            &b,
            (0, 3),
            (0, 2),
            |i| i != 1,
            |_| true,
            &mut sc,
            &mut out,
        );
        assert!(out.is_empty());
        // with the duplicate 5 unusable, 5 becomes unique
        patience_anchors(
            &a,
            &b,
            (0, 3),
            (0, 2),
            |i| i != 2,
            |_| true,
            &mut sc,
            &mut out,
        );
        assert_eq!(out, vec![(0, 0), (1, 1)]);
    }

    #[test]
    fn ranges_restrict_the_search() {
        let a = [1u32, 2, 3, 4];
        let b = [9u32, 3, 4, 9];
        let mut out = Vec::new();
        patience_anchors(
            &a,
            &b,
            (1, 4),
            (1, 3),
            |_| true,
            |_| true,
            &mut PatienceScratch::new(),
            &mut out,
        );
        assert_eq!(out, vec![(2, 1), (3, 2)]);
    }
}
