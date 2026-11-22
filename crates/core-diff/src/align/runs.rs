//! Run selection and emission (spec ALIGN-1.4, fix B3, decision D1).
//!
//! The runs found in a window are clipped (B3), re-weighed, sorted, trimmed against each
//! other, and emitted in position order with the gap aligner between them.

use core_base::Result;

use super::aligner::Aligner;
use super::consts::MIN_RUN_WEIGHT;
use super::scratch::Run;
use super::source::Side;

/// Weight descending, then target ascending, then source ascending (spec ALIGN-1.4). A total
/// order on runs with different start cells, so the sort is deterministic.
fn by_weight(a: &Run, b: &Run) -> std::cmp::Ordering {
    b.weight
        .cmp(&a.weight)
        .then(a.tgt.cmp(&b.tgt))
        .then(a.src.cmp(&b.src))
}

/// Target ascending, then source ascending.
fn by_position(a: &Run, b: &Run) -> std::cmp::Ordering {
    a.tgt.cmp(&b.tgt).then(a.src.cmp(&b.src))
}

impl Aligner<'_> {
    /// Re-weighs a run from the source line weights; below the minimum the weight becomes 0
    /// (spec ALIGN-1.4, `Reweigh`). A run with no lines left weighs 0.
    fn reweigh(&self, r: &mut Run) {
        if r.count <= 0 {
            r.weight = 0;
            return;
        }
        let from = r.src as u32;
        let to = from + r.count as u32;
        let w: i64 = (from..to)
            .map(|k| i64::from(self.ml(Side::Left, k).weight))
            .sum();
        r.weight = if w < MIN_RUN_WEIGHT { 0 } else { w };
    }

    /// Clips, orders and emits the runs found in the window `[s, ws) x [t, wt)`, filling the
    /// gaps between them. Returns the position the window loop continues from.
    ///
    /// Decision D1: `more_after` is one condition for the whole window. With it, runs are
    /// clipped so the window's last line stays out (fix B3: half-open, no underflow), the
    /// loop commits only up to the end of the last kept run and slides on from there, and a
    /// window without any kept run is gap-filled with a *partial* trace. Without it, nothing
    /// is clipped and everything up to the window end (= block stop) is gap-filled.
    pub fn select_runs(
        &mut self,
        s: u32,
        ws: u32,
        t: u32,
        wt: u32,
        more_after: bool,
    ) -> Result<(u32, u32)> {
        let mut runs = std::mem::take(&mut self.s.runs);
        let result = self.select_runs_inner(&mut runs, s, ws, t, wt, more_after);
        runs.clear();
        self.s.runs = runs;
        result
    }

    fn select_runs_inner(
        &mut self,
        runs: &mut [Run],
        s: u32,
        ws: u32,
        t: u32,
        wt: u32,
        more_after: bool,
    ) -> Result<(u32, u32)> {
        if !runs.is_empty() {
            if more_after {
                // fix: B3: clip runs that reach the window's last line (Delphi inclusive
                // stops: last line = ws - 1). The result can be <= 0, which drops the run.
                let (last_s, last_t) = (i64::from(ws) - 1, i64::from(wt) - 1);
                for r in runs.iter_mut() {
                    if r.count + r.src > last_s || r.count + r.tgt > last_t {
                        r.count = r.count.min(last_s - r.src).min(last_t - r.tgt);
                        self.reweigh(r);
                    }
                }
            }
            self.rank_runs(runs);
        }

        let (mut cur_s, mut cur_t) = (s, t);
        let mut accepted = 0usize;
        for r in runs.iter() {
            if r.weight <= 0 {
                continue;
            }
            let (rs, rt) = (r.src as u32, r.tgt as u32);
            if rs < cur_s || rt < cur_t {
                continue; // defensive: never emit out of order
            }
            accepted += 1;
            if rs > cur_s || rt > cur_t {
                self.align_gap(cur_s, rs, cur_t, rt, false)?;
            }
            self.emit(rs, rt, r.count as u32)?;
            cur_s = rs + r.count as u32;
            cur_t = rt + r.count as u32;
        }

        if accepted > 0 && more_after {
            // commit up to the end of the last run; the window slides from there
            return Ok((cur_s, cur_t));
        }
        if cur_s < ws || cur_t < wt {
            let partial = more_after && cur_s < ws && cur_t < wt;
            let (cs, ct) = self.align_gap(cur_s, ws, cur_t, wt, partial)?;
            return Ok((cur_s + cs, cur_t + ct));
        }
        Ok((cur_s, cur_t))
    }

    /// Sorts by weight, lets every heavier run trim or drop the lighter ones it overlaps, and
    /// leaves the runs sorted by position (spec ALIGN-1.4: `SortRunsByWeight`, the greedy
    /// loop, `SortRunsByPosition`). Dropped runs get weight -1.
    fn rank_runs(&self, runs: &mut [Run]) {
        runs.sort_by(by_weight);
        for x in 0..runs.len() {
            if runs[x].weight <= 0 {
                continue;
            }
            let a = runs[x];
            let (a_end_s, a_end_t) = (a.src + a.count - 1, a.tgt + a.count - 1);
            let mut changed = false;
            for yr in &mut runs[x + 1..] {
                let b = *yr;
                if b.weight <= 0 {
                    continue;
                }
                let (b_end_s, b_end_t) = (b.src + b.count - 1, b.tgt + b.count - 1);
                // entirely after or entirely before: no conflict
                if b.src > a_end_s && b.tgt > a_end_t {
                    continue;
                }
                if b_end_s < a.src && b_end_t < a.tgt {
                    continue;
                }
                if b.src < a.src && b.tgt < a.tgt && b_end_s < a_end_s && b_end_t < a_end_t {
                    // b runs into a's head: cut b's tail
                    yr.count = (a.src - b.src).min(a.tgt - b.tgt);
                    self.reweigh(yr);
                    changed = true;
                } else if b.src > a.src && b.tgt > a.tgt && b_end_s > a_end_s && b_end_t > a_end_t {
                    // b starts inside a's tail: cut b's head
                    let d = (a_end_s - b.src + 1).max(a_end_t - b.tgt + 1);
                    yr.count -= d;
                    yr.src += d;
                    yr.tgt += d;
                    self.reweigh(yr);
                    changed = true;
                } else {
                    yr.weight = -1; // crossing or contained: dropped
                }
            }
            if changed {
                runs[x + 1..].sort_by(by_weight);
            }
        }
        runs.sort_by(by_position);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::align::aligner::Aligner;
    use crate::align::config::AlignConfig;
    use crate::align::scratch::AlignScratch;
    use crate::align::source::{AlignOp, ORPHAN, VecSource};
    use core_base::CancelToken;
    use core_text::rules::LineRules;

    fn run(src: i64, tgt: i64, count: i64, weight: i64) -> Run {
        Run {
            count,
            weight,
            src,
            tgt,
        }
    }

    #[test]
    fn weight_order_is_weight_desc_then_tgt_then_src() {
        let mut v = [
            run(5, 9, 1, 600),
            run(1, 2, 1, 700),
            run(3, 2, 1, 600),
            run(0, 2, 1, 600),
        ];
        v.sort_by(by_weight);
        let keys: Vec<_> = v.iter().map(|r| (r.weight, r.tgt, r.src)).collect();
        assert_eq!(
            keys,
            vec![(700, 2, 1), (600, 2, 0), (600, 2, 3), (600, 9, 5)]
        );
    }

    #[test]
    fn position_order_is_tgt_then_src() {
        let mut v = [run(5, 9, 1, 1), run(1, 2, 1, 1), run(0, 2, 1, 1)];
        v.sort_by(by_position);
        assert_eq!(
            v.iter().map(|r| (r.tgt, r.src)).collect::<Vec<_>>(),
            vec![(2, 0), (2, 1), (9, 5)]
        );
    }

    /// Runs `f` with an aligner over `n` distinct significant lines per side, so every line
    /// weighs 70 and `reweigh` is `70 * count` (below 500 → 0).
    fn with_aligner<R>(n: usize, f: impl FnOnce(&mut Aligner<'_>) -> R) -> R {
        let lines: Vec<String> = (0..n).map(|k| format!("line {k}")).collect();
        let src = VecSource::new(&lines, &lines, &LineRules::new());
        let cancel = CancelToken::new();
        let mut sink = |_: AlignOp| {};
        let mut scratch = AlignScratch::new();
        let cfg = AlignConfig::default().resolve().unwrap();
        let range = crate::align::AlignRange {
            src: 0..n as u32,
            tgt: 0..n as u32,
        };
        let mut a = Aligner::new(&src, cfg, &cancel, &mut sink, &mut scratch, &range);
        a.ensure(Side::Left, n as u32 - 1);
        f(&mut a)
    }

    /// Ranks runs given as `(src, tgt, count)`; returns `(src, tgt, count, weight)` in
    /// position order. Weights start as `70 * count`.
    fn rank(runs: &[(i64, i64, i64)]) -> Vec<(i64, i64, i64, i64)> {
        with_aligner(200, |a| {
            let mut v: Vec<Run> = runs.iter().map(|&(s, t, c)| run(s, t, c, 70 * c)).collect();
            a.rank_runs(&mut v);
            v.iter()
                .map(|r| (r.src, r.tgt, r.count, r.weight))
                .collect()
        })
    }

    #[test]
    fn disjoint_runs_are_all_kept() {
        // one entirely after the other on both axes: no conflict
        assert_eq!(
            rank(&[(30, 30, 10), (0, 0, 10)]),
            vec![(0, 0, 10, 700), (30, 30, 10, 700)]
        );
    }

    #[test]
    fn lighter_run_running_into_the_head_of_a_heavier_one_loses_its_tail() {
        // a = (10,10,x10) 700; b = (2,2,x12) 840 is heavier. b ends at 13, inside a on both
        // axes and before a's end 19, so b is cut where a starts: count = min(10-2, 10-2) = 8
        // → 560, still >= 500. Heavier b is processed first, so it cuts nothing, and a
        // (starting inside b's tail) loses its head: d = max(13-10+1, 13-10+1) = 4 → a =
        // (14,14,x6) = 420 < 500 → 0.
        let got = rank(&[(10, 10, 10), (2, 2, 12)]);
        assert_eq!(got, vec![(2, 2, 12, 840), (14, 14, 6, 0)]);
    }

    #[test]
    fn tail_trim_keeps_a_run_that_stays_heavy_enough() {
        // heavy a = (10,10,x20) 1400; b = (2,2,x12) 840 ends at 13 < 29: tail cut to
        // min(8, 8) = 8 → 560 (kept).
        let got = rank(&[(10, 10, 20), (2, 2, 12)]);
        assert_eq!(got, vec![(2, 2, 8, 560), (10, 10, 20, 1400)]);
    }

    #[test]
    fn head_trim_keeps_a_run_that_stays_heavy_enough() {
        // heavy a = (0,0,x20) 1400 ends at 19; b = (12,12,x16) 1120 starts inside a's tail
        // and ends after it: d = max(19-12+1, 19-12+1) = 8 → b = (20,20,x8) 560 (kept).
        let got = rank(&[(0, 0, 20), (12, 12, 16)]);
        assert_eq!(got, vec![(0, 0, 20, 1400), (20, 20, 8, 560)]);
    }

    #[test]
    fn crossing_runs_drop_the_lighter_one() {
        // swapped blocks: a = (10,0,x12) 840 beats b = (0,12,x10) 700; b crosses a
        let got = rank(&[(0, 12, 10), (10, 0, 12)]);
        assert_eq!(got, vec![(10, 0, 12, 840), (0, 12, 10, -1)]);
    }

    #[test]
    fn contained_run_is_dropped() {
        let got = rank(&[(5, 5, 10), (0, 0, 20)]);
        assert_eq!(got, vec![(0, 0, 20, 1400), (5, 5, 10, -1)]);
    }

    #[test]
    fn runs_sharing_a_target_range_conflict() {
        // same target lines matched with two different source ranges: the later one in
        // sort order (lighter) is dropped even though it is entirely after on the source axis
        let got = rank(&[(0, 0, 10), (12, 0, 9)]);
        assert_eq!(got, vec![(0, 0, 10, 700), (12, 0, 9, -1)]);
    }

    #[test]
    fn equal_weight_ties_go_to_the_smaller_target_then_source() {
        // both 700 and crossing: the one with the smaller target (b at tgt 0) wins
        let got = rank(&[(0, 12, 10), (12, 0, 10)]);
        assert_eq!(got, vec![(12, 0, 10, 700), (0, 12, 10, -1)]);
    }

    /// Fix B3 / backlog E2-S8.1: clipping uses half-open arithmetic that cannot underflow,
    /// even for runs that start at or past the clip line.
    ///
    /// Window `[0,20) x [0,20)` with `more_after`: the last line is 19 and runs that reach it
    /// are clipped by one line. Both sides hold the same lines, so unmatched parts pair up
    /// on the diagonal.
    #[test]
    fn clipping_never_underflows_and_drops_runs_at_or_past_the_clip_line() {
        // (run, expected position, expected ops)
        type Case = ((i64, i64, i64), (u32, u32), Vec<(u32, u32, u32)>);
        let cases: Vec<Case> = vec![
            // ends at line 19: clipped to 7 lines = 490 < 500 → dropped; the window is gap-
            // filled with a partial trace that consumes the whole diagonal
            ((12, 12, 8), (20, 20), vec![(0, 0, 20)]),
            // starts AT the clip line: the count would be 0 (B3: no underflow) → dropped
            ((19, 19, 1), (20, 20), vec![(0, 0, 20)]),
            // starts past the clip line on one axis only
            ((19, 5, 1), (20, 20), vec![(0, 0, 20)]),
            ((5, 19, 1), (20, 20), vec![(0, 0, 20)]),
            // 9 lines clipped to 8 = 560: kept; commit up to its end, the window slides on
            ((11, 11, 9), (19, 19), vec![(0, 0, 11), (11, 11, 8)]),
        ];
        for (r, want_pos, want_ops) in cases {
            let (pos, ops) = with_aligner_ops(60, |a| {
                let mut runs = vec![run(r.0, r.1, 70 * r.2, 0)];
                runs[0].count = r.2;
                runs[0].weight = 70 * r.2;
                a.select_runs_inner(&mut runs, 0, 20, 0, 20, true).unwrap()
            });
            assert_eq!(pos, want_pos, "run {r:?}");
            let got: Vec<_> = ops.iter().map(|o| (o.src, o.tgt, o.amount)).collect();
            assert_eq!(got, want_ops, "run {r:?}");
        }
    }

    use proptest::prelude::*;

    proptest! {
        /// Fix B3 / backlog E2-S8.1: for random windows and random runs inside them, clipping,
        /// re-weighing and trimming never underflow or panic, the returned position stays in the
        /// window, and the emitted ops are contiguous from the window start to that position
        /// (every line exactly once, in order).
        #[test]
        fn select_runs_is_total_and_covers_each_line_once(
            ws in 1u32..40,
            wt in 1u32..40,
            more_after in any::<bool>(),
            raw in proptest::collection::vec((0u32..40, 0u32..40, 1u32..40), 0..6),
        ) {
            let n = 45usize;
            let mut runs: Vec<Run> = raw
                .into_iter()
                .filter_map(|(s, t, c)| {
                    // a run must lie inside the window
                    if s >= ws || t >= wt {
                        return None;
                    }
                    let count = c.min(ws - s).min(wt - t);
                    Some(Run { src: i64::from(s), tgt: i64::from(t), count: i64::from(count), weight: 70 * i64::from(count) })
                })
                .collect();
            let (pos, ops) = with_aligner_ops(n, |a| {
                a.select_runs_inner(&mut runs, 0, ws, 0, wt, more_after).unwrap()
            });
            prop_assert!(pos.0 <= ws && pos.1 <= wt, "position {pos:?} outside {ws}x{wt}");
            let (mut ps, mut pt) = (0u32, 0u32);
            for op in &ops {
                match (op.src, op.tgt) {
                    (ORPHAN, t) => { prop_assert_eq!(t, pt); pt += op.amount; }
                    (s, ORPHAN) => { prop_assert_eq!(s, ps); ps += op.amount; }
                    (s, t) => { prop_assert_eq!((s, t), (ps, pt)); ps += op.amount; pt += op.amount; }
                }
            }
            prop_assert_eq!((ps, pt), pos, "ops end at the returned position");
        }
    }

    /// Like `with_aligner`, but also returns the ops that were emitted.
    fn with_aligner_ops<R>(n: usize, f: impl FnOnce(&mut Aligner<'_>) -> R) -> (R, Vec<AlignOp>) {
        let lines: Vec<String> = (0..n).map(|k| format!("line {k}")).collect();
        let src = VecSource::new(&lines, &lines, &LineRules::new());
        let cancel = CancelToken::new();
        let mut ops = Vec::new();
        let mut scratch = AlignScratch::new();
        let cfg = AlignConfig::default().resolve().unwrap();
        let range = crate::align::AlignRange {
            src: 0..n as u32,
            tgt: 0..n as u32,
        };
        let r = {
            let mut sink = |op: AlignOp| ops.push(op);
            let mut a = Aligner::new(&src, cfg, &cancel, &mut sink, &mut scratch, &range);
            a.ensure(Side::Left, n as u32 - 1);
            a.ensure(Side::Right, n as u32 - 1);
            f(&mut a)
        };
        (r, ops)
    }
}
