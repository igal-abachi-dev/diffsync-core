//! Preprocessing shared by the exact engines (plan.md §5.3, backlog E3-S4.3).
//!
//! Before an engine runs, the common prefix and suffix are cut off and every line that occurs
//! on one side only is dropped: such a line can never be part of a common subsequence, so the
//! longest common subsequence of the reduced sequences is the same size, but the engines see
//! far fewer lines (a log where every changed line carries a new timestamp reduces to nothing).
//! The remaining lines get dense ids, so the engines can use arrays instead of hash maps.
//!
//! Matches found on the reduced sequences are mapped back to the original indexes; the order
//! is preserved, so the result is still a strictly increasing list of pairs.

use super::{FastMap, fast_map};

#[derive(Debug, Clone, Copy)]
struct Slot {
    a: u32,
    b: u32,
    dense: u32,
}

const UNSET: u32 = u32::MAX;

/// Reusable buffers of one preprocessing run.
#[derive(Debug, Default)]
pub struct Prefilter {
    slots: FastMap<u32, Slot>,
    /// Reduced left and right sequences (dense ids).
    pub a: Vec<u32>,
    pub b: Vec<u32>,
    /// Original index of each reduced element.
    ia: Vec<u32>,
    ib: Vec<u32>,
    /// Number of distinct dense ids.
    pub ids: u32,
}

/// How the full inputs split around the reduced middle.
#[derive(Debug, Clone, Copy)]
pub struct Trim {
    pub prefix: usize,
    pub suffix: usize,
}

impl Prefilter {
    pub fn new() -> Self {
        Self {
            slots: fast_map(),
            ..Self::default()
        }
    }

    /// decision: D40: cuts the common prefix and suffix of `a` and `b`, then reduces the middle into
    /// `self.a` / `self.b`.
    pub fn reduce(&mut self, a: &[u32], b: &[u32]) -> Trim {
        let mut prefix = 0;
        while prefix < a.len() && prefix < b.len() && a[prefix] == b[prefix] {
            prefix += 1;
        }
        let mut suffix = 0;
        while a.len() - suffix > prefix
            && b.len() - suffix > prefix
            && a[a.len() - 1 - suffix] == b[b.len() - 1 - suffix]
        {
            suffix += 1;
        }
        let (am, bm) = (&a[prefix..a.len() - suffix], &b[prefix..b.len() - suffix]);
        self.slots.clear();
        self.a.clear();
        self.b.clear();
        self.ia.clear();
        self.ib.clear();
        self.ids = 0;
        if am.is_empty() || bm.is_empty() {
            return Trim { prefix, suffix };
        }
        for &x in am {
            self.slots
                .entry(x)
                .or_insert(Slot {
                    a: 0,
                    b: 0,
                    dense: UNSET,
                })
                .a += 1;
        }
        for &y in bm {
            if let Some(s) = self.slots.get_mut(&y) {
                s.b += 1;
            }
        }
        for (i, &x) in am.iter().enumerate() {
            let Some(s) = self.slots.get_mut(&x) else {
                continue;
            };
            if s.b == 0 {
                continue;
            }
            if s.dense == UNSET {
                s.dense = self.ids;
                self.ids += 1;
            }
            self.a.push(s.dense);
            self.ia.push((prefix + i) as u32);
        }
        for (j, &y) in bm.iter().enumerate() {
            let Some(s) = self.slots.get(&y) else {
                continue;
            };
            if s.a == 0 || s.dense == UNSET {
                continue;
            }
            self.b.push(s.dense);
            self.ib.push((prefix + j) as u32);
        }
        Trim { prefix, suffix }
    }

    /// Appends the prefix matches, the reduced matches `reduced` mapped back to original
    /// indexes, and the suffix matches to `out`.
    pub fn emit(
        &self,
        trim: Trim,
        (na, nb): (usize, usize),
        reduced: &[(u32, u32)],
        out: &mut Vec<(u32, u32)>,
    ) {
        out.extend((0..trim.prefix).map(|k| (k as u32, k as u32)));
        out.extend(
            reduced
                .iter()
                .map(|&(i, j)| (self.ia[i as usize], self.ib[j as usize])),
        );
        out.extend(
            (0..trim.suffix)
                .map(|k| ((na - trim.suffix + k) as u32, (nb - trim.suffix + k) as u32)),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unique_lines_vanish_and_indexes_map_back() {
        let a = [1, 2, 9, 3, 4, 5];
        let b = [1, 2, 3, 8, 4, 5];
        let mut p = Prefilter::new();
        let t = p.reduce(&a, &b);
        assert_eq!((t.prefix, t.suffix), (2, 2));
        // middle: a = [9, 3], b = [3, 8]; 9 and 8 vanish
        assert_eq!((p.a.as_slice(), p.b.as_slice()), (&[0u32][..], &[0u32][..]));
        let mut out = Vec::new();
        p.emit(t, (a.len(), b.len()), &[(0, 0)], &mut out);
        assert_eq!(out, [(0, 0), (1, 1), (3, 2), (4, 4), (5, 5)]);
    }

    #[test]
    fn identical_and_disjoint_inputs_reduce_to_nothing() {
        let mut p = Prefilter::new();
        let t = p.reduce(&[1, 2, 3], &[1, 2, 3]);
        assert_eq!((t.prefix, t.suffix, p.a.len()), (3, 0, 0));
        let t = p.reduce(&[1, 2], &[3, 4]);
        assert_eq!((t.prefix, t.suffix, p.a.len(), p.b.len()), (0, 0, 0, 0));
    }
}
