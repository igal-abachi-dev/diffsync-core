//! The gap aligner: Needleman-Wunsch over the closeness scores (spec ALIGN-1.5).
//!
//! Computed from the end of the gap backwards, so the trace walks forwards and emits ops in
//! increasing order. Gap penalty -20 per unpaired line; ties prefer pair, then source orphan,
//! then target orphan; consecutive steps of the same kind become one op. A *partial* trace
//! stops as soon as one side is used up and gives the unconsumed tail back to the window loop.

use core_base::Result;

use super::aligner::Aligner;
use super::consts::{GAP_PENALTY, GAP_SPLIT, MAX_GAP_CELLS};
use super::source::{ORPHAN, Side};

/// Traceback directions, 2 bits per cell.
const DIR_SRC_ORPHAN: u8 = 1;
const DIR_TGT_ORPHAN: u8 = 2;
const DIR_PAIR: u8 = 3;

#[inline]
fn set_dir(trace: &mut [u8], cell: usize, dir: u8) {
    trace[cell >> 2] |= dir << ((cell & 3) * 2);
}

#[inline]
fn get_dir(trace: &[u8], cell: usize) -> u8 {
    (trace[cell >> 2] >> ((cell & 3) * 2)) & 3
}

impl Aligner<'_> {
    /// Aligns the gap `[s0, s1) x [t0, t1)` and emits the ops. Returns how many lines of each
    /// side were consumed: everything, unless `partial` stopped the trace early.
    ///
    /// One side empty → a single orphan op (no chunking here, spec ALIGN-1.5). Gaps above
    /// [`MAX_GAP_CELLS`] are split along the diagonal into blocks of [`GAP_SPLIT`] lines with
    /// the leftovers as orphans (our own safety limit, plan.md §5.2); a split gap is always
    /// consumed whole, partial or not.
    pub fn align_gap(
        &mut self,
        s0: u32,
        s1: u32,
        t0: u32,
        t1: u32,
        partial: bool,
    ) -> Result<(u32, u32)> {
        let (n, m) = (s1.saturating_sub(s0), t1.saturating_sub(t0));
        if m == 0 {
            self.emit(s0, ORPHAN, n)?;
            return Ok((n, 0));
        }
        if n == 0 {
            self.emit(ORPHAN, t0, m)?;
            return Ok((0, m));
        }
        if (u64::from(n) + 1) * (u64::from(m) + 1) <= MAX_GAP_CELLS {
            return self.gap_dp(s0, s1, t0, t1, partial);
        }
        let (mut cs, mut ct) = (s0, t0);
        while cs < s1 && ct < t1 {
            let (es, et) = (
                cs.saturating_add(GAP_SPLIT).min(s1),
                ct.saturating_add(GAP_SPLIT).min(t1),
            );
            self.gap_dp(cs, es, ct, et, false)?;
            cs = es;
            ct = et;
        }
        if cs < s1 {
            self.emit(cs, ORPHAN, s1 - cs)?;
        }
        if ct < t1 {
            self.emit(ORPHAN, ct, t1 - ct)?;
        }
        Ok((n, m))
    }

    /// The DP itself; both sides non-empty.
    fn gap_dp(&mut self, s0: u32, s1: u32, t0: u32, t1: u32, partial: bool) -> Result<(u32, u32)> {
        let (n, m) = ((s1 - s0) as usize, (t1 - t0) as usize);
        self.ensure(Side::Left, s1 - 1);
        self.ensure(Side::Right, t1 - 1);
        // Take the buffers out of the scratch so `closeness` can borrow `self` meanwhile.
        let mut dp = std::mem::take(&mut self.s.dp);
        let mut trace = std::mem::take(&mut self.s.trace);
        let result = self.gap_dp_with(&mut dp, &mut trace, s1, t1, (n, m), partial);
        self.s.dp = dp;
        self.s.trace = trace;
        result
    }

    fn gap_dp_with(
        &mut self,
        dp: &mut Vec<i32>,
        trace: &mut Vec<u8>,
        s1: u32,
        t1: u32,
        (n, m): (usize, usize),
        partial: bool,
    ) -> Result<(u32, u32)> {
        let stride = m + 1;
        dp.clear();
        dp.resize(n + 1, 0);
        trace.clear();
        trace.resize(((n + 1) * stride).div_ceil(4), 0);

        // Cell (i, j): best score of aligning the last i source and last j target lines of
        // the gap. Column j = 0: only source orphans.
        for i in 1..=n {
            dp[i] = dp[i - 1] + GAP_PENALTY;
            set_dir(trace, i * stride, DIR_SRC_ORPHAN);
        }
        for j in 1..=m {
            self.cancel.check()?; // once per DP row
            let mut diag = dp[0];
            dp[0] += GAP_PENALTY;
            set_dir(trace, j, DIR_TGT_ORPHAN);
            let tj = t1 - j as u32;
            for i in 1..=n {
                // pair first; a strictly better source or target orphan replaces it
                let mut best = diag + self.closeness(s1 - i as u32, tj);
                let mut dir = DIR_PAIR;
                let up = dp[i - 1] + GAP_PENALTY;
                if up > best {
                    best = up;
                    dir = DIR_SRC_ORPHAN;
                }
                let old = dp[i];
                let left = old + GAP_PENALTY;
                if left > best {
                    best = left;
                    dir = DIR_TGT_ORPHAN;
                }
                diag = old;
                dp[i] = best;
                set_dir(trace, i * stride + j, dir);
            }
        }

        // Trace forwards from (n, m): the next line of each side is `s1 - i` / `t1 - j`.
        let (mut i, mut j) = (n, m);
        let mut kind = 0u8;
        let mut count = 0u32;
        let (mut gs, mut gt) = (ORPHAN, ORPHAN);
        loop {
            let (cur_s, cur_t) = (s1 - i as u32, t1 - j as u32);
            let dir = get_dir(trace, i * stride + j);
            if dir != kind {
                if kind != 0 {
                    self.emit_step(kind, gs, gt, count)?;
                }
                kind = dir;
                count = 0;
                gs = if dir == DIR_TGT_ORPHAN { ORPHAN } else { cur_s };
                gt = if dir == DIR_SRC_ORPHAN { ORPHAN } else { cur_t };
            }
            count += 1;
            match dir {
                DIR_PAIR => {
                    i -= 1;
                    j -= 1;
                }
                DIR_TGT_ORPHAN => j -= 1,
                _ => i -= 1,
            }
            if i == 0 && j == 0 {
                break;
            }
            if partial && (i == 0 || j == 0) {
                break;
            }
        }
        if kind != 0 {
            self.emit_step(kind, gs, gt, count)?;
        }
        Ok(((n - i) as u32, (m - j) as u32))
    }

    fn emit_step(&mut self, kind: u8, gs: u32, gt: u32, count: u32) -> Result<()> {
        debug_assert!(matches!(kind, DIR_SRC_ORPHAN | DIR_TGT_ORPHAN | DIR_PAIR));
        self.emit(gs, gt, count)
    }
}

#[cfg(test)]
mod tests {
    use crate::align::aligner::Aligner;
    use crate::align::config::AlignConfig;
    use crate::align::scratch::AlignScratch;
    use crate::align::source::{AlignOp, Side, VecSource};
    use core_base::CancelToken;
    use core_text::rules::LineRules;

    /// Runs `align_gap(s0,s1,t0,t1,partial)` over the given lines; returns consumed counts and ops.
    fn gap(left: &[&str], right: &[&str], partial: bool) -> ((u32, u32), Vec<String>) {
        let src = VecSource::new(left, right, &LineRules::new());
        let cancel = CancelToken::new();
        let mut ops: Vec<AlignOp> = Vec::new();
        let mut scratch = AlignScratch::new();
        let cfg = AlignConfig::default().resolve().unwrap();
        let range = crate::align::AlignRange {
            src: 0..left.len() as u32,
            tgt: 0..right.len() as u32,
        };
        let consumed = {
            let mut sink = |o: AlignOp| ops.push(o);
            let mut a = Aligner::new(&src, cfg, &cancel, &mut sink, &mut scratch, &range);
            if !left.is_empty() {
                a.ensure(Side::Left, left.len() as u32 - 1);
            }
            if !right.is_empty() {
                a.ensure(Side::Right, right.len() as u32 - 1);
            }
            a.align_gap(0, left.len() as u32, 0, right.len() as u32, partial)
                .unwrap()
        };
        (consumed, ops.iter().map(ToString::to_string).collect())
    }

    #[test]
    fn ties_prefer_pair_then_source_orphan_then_target_orphan() {
        // disjoint lines score -40 as a pair and -40 as two orphans: the pair wins
        assert_eq!(gap(&["abcdefghij"], &["klmnopqrst"], false).1, ["0 0 1"]);
        // 2 x 1, all pairs -40: the first source line pairs, the second is a source orphan
        assert_eq!(
            gap(&["abcdefghij", "bcdefghijk"], &["klmnopqrst"], false).1,
            ["0 0 1", "1 - 1"]
        );
        // 1 x 2: the target orphan comes after the pair
        assert_eq!(
            gap(&["abcdefghij"], &["klmnopqrst", "lmnopqrstu"], false).1,
            ["0 0 1", "- 1 1"]
        );
    }

    #[test]
    fn consecutive_steps_of_one_kind_coalesce() {
        let same = ["one", "two", "three", "four"];
        assert_eq!(gap(&same, &same, false).1, ["0 0 4"]);
        assert_eq!(gap(&same, &[], false), ((4, 0), vec!["0 - 4".to_string()]));
        assert_eq!(gap(&[], &same, false), ((0, 4), vec!["- 0 4".to_string()]));
    }

    #[test]
    fn a_partial_trace_stops_when_one_side_is_used_up() {
        // 2 source lines against 5 target lines with equal lines first: the trace pairs the
        // two and stops (source used up), giving the other 3 target lines back
        let (consumed, ops) = gap(&["a", "b"], &["a", "b", "x1", "x2", "x3"], true);
        assert_eq!(consumed, (2, 2));
        assert_eq!(ops, ["0 0 2"]);
        // the same gap, not partial, consumes everything
        let (consumed, ops) = gap(&["a", "b"], &["a", "b", "x1", "x2", "x3"], false);
        assert_eq!(consumed, (2, 5));
        assert_eq!(ops, ["0 0 2", "- 2 3"]);
    }

    #[test]
    fn a_partial_trace_over_equal_sides_consumes_both() {
        let same = ["p", "q", "r"];
        assert_eq!(gap(&same, &same, true).0, (3, 3));
    }
}
