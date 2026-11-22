//! Window search: find the diagonal runs of equal hashes inside one skew window (spec
//! ALIGN-1.4). The L-shaped scan visits step `k`'s column from row `k` upwards, then row `k`
//! leftwards, over a bitset of visited cells (about 500 KB for a 2000 x 2000 window).

use core_base::Result;

use super::aligner::Aligner;
use super::consts::{BIG_RUN_WEIGHT, MIN_RUN_WEIGHT};
use super::scratch::{AlignScratch, BigRun, Run};
use super::source::Side;

/// What a window search found. No exceptions as control flow (plan.md §5.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WindowOutcome {
    /// The runs heavier than the minimum are in `AlignScratch::runs`.
    Runs,
    /// A run reached the big-run weight; the scan stopped at once.
    BigRun(BigRun),
}

/// Geometry of the window being searched: `[s, ws) x [t, wt)`.
#[derive(Debug, Clone, Copy)]
pub(super) struct Win {
    pub s: u32,
    pub ws: u32,
    pub t: u32,
    pub wt: u32,
    /// Words per row of the visited bitset.
    pub row_words: usize,
}

#[inline]
fn bit(win: &Win, i: u32, j: u32) -> (usize, u64) {
    let (r, c) = ((i - win.s) as usize, (j - win.t) as usize);
    (r * win.row_words + (c >> 6), 1u64 << (c & 63))
}

/// Visits cell `(i, j)`: when it starts an unvisited diagonal run, measures the run, marks it
/// (plus the cell that stops it) as visited and records it when heavier than the minimum.
/// Returns `Some` when the run reaches the big-run weight.
fn find_run(s: &mut AlignScratch, win: &Win, i: u32, j: u32) -> Option<BigRun> {
    let AlignScratch {
        sides,
        visited,
        runs,
        ..
    } = s;
    let (word, mask) = bit(win, i, j);
    if visited[word] & mask != 0 {
        return None;
    }
    let (l, r) = (&sides[0], &sides[1]);
    let (rs, rt) = (i, j);
    let (mut a, mut b) = (i, j);
    let mut count = 0u32;
    let mut weight = 0i64;
    // spec: ALIGN-1.4: a run of equal hashes along the diagonal, weight = sum of the source
    // line weights; it ends at the window edge (the window is the scan area).
    while a < win.ws && b < win.wt {
        let (ma, mb) = (
            l.metas[(a - l.start) as usize],
            r.metas[(b - r.start) as usize],
        );
        if ma.hash != mb.hash {
            break;
        }
        weight += i64::from(ma.weight);
        count += 1;
        a += 1;
        b += 1;
        if weight >= BIG_RUN_WEIGHT {
            return Some(BigRun {
                src: rs,
                tgt: rt,
                count,
            });
        }
    }
    // mark the run and the first cell after it (when it is inside the window)
    let (mut a, mut b) = (rs, rt);
    for _ in 0..=count {
        let (w, m) = bit(win, a, b);
        visited[w] |= m;
        a += 1;
        b += 1;
        if a >= win.ws || b >= win.wt {
            break;
        }
    }
    if weight > MIN_RUN_WEIGHT {
        runs.push(Run {
            count: i64::from(count),
            weight,
            src: i64::from(rs),
            tgt: i64::from(rt),
        });
    }
    None
}

/// The L-shaped scan order of a window `[s, ws) x [t, wt)` (spec ALIGN-1.4): the top-left
/// cell, then for every step the new column from its row upwards and the new row leftwards.
/// `visit` returns `Some` to stop the scan; `step` runs once per step (cancellation).
fn l_scan<B>(
    (s, ws, t, wt): (u32, u32, u32, u32),
    visit: &mut impl FnMut(u32, u32) -> Option<B>,
    step: &mut impl FnMut() -> Result<()>,
) -> Result<Option<B>> {
    if let Some(b) = visit(s, t) {
        return Ok(Some(b));
    }
    let (mut i, mut j) = (s, t);
    loop {
        // advance the corner of the L along both sides while there is room
        if j + 1 < wt {
            j += 1;
        }
        if i + 1 < ws {
            i += 1;
        }
        // column `j`, from row `i` upwards
        for k in (s..=i).rev() {
            if let Some(b) = visit(k, j) {
                return Ok(Some(b));
            }
        }
        // row `i`, leftwards from column `j - 1`
        for k in (t..j).rev() {
            if let Some(b) = visit(i, k) {
                return Ok(Some(b));
            }
        }
        step()?;
        if i + 1 >= ws && j + 1 >= wt {
            return Ok(None);
        }
    }
}

impl Aligner<'_> {
    /// Scans the window `[s, ws) x [t, wt)` for runs (spec ALIGN-1.4). Clears the previous
    /// run list; a big run abandons the search at once.
    pub fn search_window(&mut self, s: u32, ws: u32, t: u32, wt: u32) -> Result<WindowOutcome> {
        self.ensure(Side::Left, ws - 1);
        self.ensure(Side::Right, wt - 1);
        let (rows, cols) = ((ws - s) as usize, (wt - t) as usize);
        let win = Win {
            s,
            ws,
            t,
            wt,
            row_words: cols.div_ceil(64),
        };
        let sc = &mut *self.s;
        sc.runs.clear();
        sc.visited.clear();
        sc.visited.resize(rows * win.row_words, 0);
        let cancel = self.cancel;
        let big = l_scan(
            (s, ws, t, wt),
            &mut |i, j| find_run(sc, &win, i, j),
            &mut || cancel.check(),
        )?;
        Ok(big.map_or(WindowOutcome::Runs, WindowOutcome::BigRun))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn order(s: u32, ws: u32, t: u32, wt: u32) -> Vec<(u32, u32)> {
        let mut seen = Vec::new();
        let none: Option<()> = l_scan(
            (s, ws, t, wt),
            &mut |i, j| {
                seen.push((i, j));
                None
            },
            &mut || Ok(()),
        )
        .unwrap();
        assert!(none.is_none());
        seen
    }

    /// Backlog E2-S6.3: the visit order on a 4x4 window. Step k scans column k from row k
    /// upwards, then row k leftwards from column k-1; cell (r, c) is visited at step
    /// max(r, c).
    #[test]
    fn l_scan_visit_order_on_a_4x4_window() {
        #[rustfmt::skip]
        let expected: Vec<(u32, u32)> = vec![
            (0, 0),                                                  // top-left corner
            (1, 1), (0, 1), (1, 0),                                  // step 1
            (2, 2), (1, 2), (0, 2), (2, 1), (2, 0),                  // step 2
            (3, 3), (2, 3), (1, 3), (0, 3), (3, 2), (3, 1), (3, 0),  // step 3
        ];
        assert_eq!(order(0, 4, 0, 4), expected);
    }

    #[test]
    fn scan_offsets_follow_the_window_origin() {
        let shifted = order(10, 12, 20, 22);
        assert_eq!(shifted, vec![(10, 20), (11, 21), (10, 21), (11, 20)]);
    }

    #[test]
    fn rectangular_windows_cover_every_cell() {
        for (rows, cols) in [(1, 1), (1, 5), (5, 1), (3, 7), (7, 3), (2, 2)] {
            let visited = order(0, rows, 0, cols);
            let mut cells: Vec<_> = visited.clone();
            cells.sort_unstable();
            cells.dedup();
            assert_eq!(
                cells.len() as u32,
                rows * cols,
                "{rows}x{cols}: missing cells"
            );
            // the first cell of every diagonal is visited before the rest of it
            let pos = |c: (u32, u32)| visited.iter().position(|&v| v == c).unwrap();
            for r in 1..rows {
                for c in 1..cols {
                    assert!(
                        pos((r - 1, c - 1)) < pos((r, c)),
                        "{rows}x{cols}: ({r},{c}) before its diagonal predecessor"
                    );
                }
            }
        }
    }

    #[test]
    fn visiting_can_stop_the_scan() {
        let mut n = 0;
        let r = l_scan(
            (0, 4, 0, 4),
            &mut |i, j| {
                n += 1;
                (i == 1 && j == 1).then_some((i, j))
            },
            &mut || Ok(()),
        )
        .unwrap();
        assert_eq!(r, Some((1, 1)));
        assert_eq!(n, 2);
    }

    #[test]
    fn bit_addressing_is_unique_per_cell() {
        let win = Win {
            s: 10,
            ws: 10 + 70,
            t: 5,
            wt: 5 + 130,
            row_words: 130usize.div_ceil(64),
        };
        let mut seen = std::collections::HashSet::new();
        for i in 10..80 {
            for j in 5..135 {
                assert!(seen.insert(bit(&win, i, j)), "collision at ({i},{j})");
            }
        }
    }

    // ---- find_run through `search_window` (backlog E2-S6.2, run-weight tests) ----------------

    use crate::align::aligner::Aligner;
    use crate::align::config::AlignConfig;
    use crate::align::source::VecSource;
    use core_base::CancelToken;
    use core_text::rules::LineRules;

    /// Lines named `l<i>:<W>` get the line weight `W` (a significant line weighs `2w + 50` for a
    /// hook value `w`, so the hook returns `(W - 50) / 2`; `W` must be even).
    fn named(weights: &[i32]) -> Vec<String> {
        weights
            .iter()
            .enumerate()
            .map(|(i, w)| format!("l{i}:{w}"))
            .collect()
    }

    fn hook(line: &str) -> i32 {
        let w: i32 = line
            .rsplit(':')
            .next()
            .and_then(|x| x.parse().ok())
            .unwrap_or(100);
        (w - 50) / 2
    }

    /// Searches the whole window of two identical-or-not line lists; returns the outcome and the
    /// recorded `(src, tgt, count, weight)` runs.
    fn search(left: &[String], right: &[String]) -> (WindowOutcome, Vec<(i64, i64, i64, i64)>) {
        let src = VecSource::with_weights(left, right, &LineRules::new(), Some(&hook));
        let cancel = CancelToken::new();
        let mut scratch = AlignScratch::new();
        let range = crate::align::AlignRange {
            src: 0..left.len() as u32,
            tgt: 0..right.len() as u32,
        };
        let mut sink = |_: crate::align::source::AlignOp| {};
        let cfg = AlignConfig::default().resolve().unwrap();
        let outcome = {
            let mut a = Aligner::new(&src, cfg, &cancel, &mut sink, &mut scratch, &range);
            a.search_window(0, left.len() as u32, 0, right.len() as u32)
                .unwrap()
        };
        let runs = scratch
            .runs
            .iter()
            .map(|r| (r.src, r.tgt, r.count, r.weight))
            .collect();
        (outcome, runs)
    }

    /// spec: ALIGN-1.4: a run is kept when its weight is above 500, so exactly 500 is not kept
    /// and 502 is.
    #[test]
    fn a_run_of_exactly_500_is_not_recorded_and_above_is() {
        let five = named(&[100; 5]);
        let (out, runs) = search(&five, &five);
        assert_eq!(out, WindowOutcome::Runs);
        assert!(runs.is_empty(), "{runs:?}");
        let heavier = named(&[100, 100, 100, 100, 102]);
        let (_, runs) = search(&heavier, &heavier);
        assert_eq!(runs, [(0, 0, 5, 502)]);
        let single = named(&[502]);
        assert_eq!(search(&single, &single).1, [(0, 0, 1, 502)]);
    }

    /// spec: ALIGN-1.4: a weight of 5000 returns `BigRun` at once, counting the lines so far.
    #[test]
    fn reaching_5000_returns_a_big_run() {
        let long = named(&[100; 60]);
        let (out, runs) = search(&long, &long);
        assert_eq!(
            out,
            WindowOutcome::BigRun(BigRun {
                src: 0,
                tgt: 0,
                count: 50
            })
        );
        assert!(runs.is_empty(), "the scan stopped at once");
        // 4998 is not big, 5000 is
        let mut w = vec![100; 49];
        w.push(98);
        let below = named(&w);
        assert_eq!(search(&below, &below).0, WindowOutcome::Runs);
        let mut w = vec![100; 49];
        w.push(100);
        let at = named(&w);
        assert!(matches!(search(&at, &at).0, WindowOutcome::BigRun(b) if b.count == 50));
    }

    /// Hook weights saturate at `i32::MAX` and runs are summed in i64: nothing overflows.
    #[test]
    fn huge_hook_weights_do_not_overflow() {
        let huge: Vec<String> = (0..4).map(|i| format!("l{i}:{}", i32::MAX)).collect();
        // `hook` maps W to (W - 50) / 2, so use a direct hook for the extreme value
        let src =
            VecSource::with_weights(&huge, &huge, &LineRules::new(), Some(&|_: &str| i32::MAX));
        let cancel = CancelToken::new();
        let mut scratch = AlignScratch::new();
        let range = crate::align::AlignRange {
            src: 0..4,
            tgt: 0..4,
        };
        let mut sink = |_: crate::align::source::AlignOp| {};
        let cfg = AlignConfig::default().resolve().unwrap();
        let mut a = Aligner::new(&src, cfg, &cancel, &mut sink, &mut scratch, &range);
        assert_eq!(
            a.search_window(0, 4, 0, 4).unwrap(),
            WindowOutcome::BigRun(BigRun {
                src: 0,
                tgt: 0,
                count: 1
            })
        );
    }

    /// A run offset from the origin is found from its own start cell, once.
    #[test]
    fn runs_off_the_main_diagonal_are_found_once() {
        let common = named(&[100; 6]);
        let mut left = vec!["x0".to_string(), "x1".to_string()];
        left.extend(common.iter().cloned());
        let mut right = vec!["y0".to_string()];
        right.extend(common.iter().cloned());
        let (_, runs) = search(&left, &right);
        assert_eq!(runs, [(2, 1, 6, 600)]);
    }
}
