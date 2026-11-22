//! Histogram diff matches (plan.md §5.3, decision D8, D12, backlog E3-S2).
//!
//! A port of `Dc.Myers.DcHistogramMatches`: pick the line of `a` that occurs *least often* in
//! the range and is also in `b`, grow the longest equal region around that anchor, and solve
//! the parts before and after it the same way. When every candidate occurs more than 64 times,
//! the range is handed to Myers.
//!
//! Decision D39: the anchor search resumes behind the region it just grew (jgit and imara-diff do
//! the same), so a long equal run is scanned once, not once per element.
//!
//! Decision D24: candidates with more than 64 occurrences are never used. The Delphi code
//! starts its best count at 65 and so still accepts a candidate with exactly 65 copies; the plan
//! and the backlog say "more than 64", which is what this does.

use core_base::{CancelToken, Result};

use super::myers::{MyersScratch, myers_range_into};
use super::prefilter::Prefilter;

/// A candidate anchor may occur at most this often in the range of `a` (decision D24).
pub const MAX_CHAIN: u32 = 64;

/// Chain head of one dense id; valid only while `stamp` equals the current region number, so
/// the table never needs clearing between regions.
#[derive(Debug, Clone, Copy, Default)]
struct Head {
    first: u32,
    count: u32,
    stamp: u32,
}

#[derive(Debug, Clone, Copy)]
enum Work {
    Diff {
        a0: usize,
        a1: usize,
        b0: usize,
        b1: usize,
    },
    Emit {
        a: usize,
        b: usize,
        len: usize,
    },
}

/// Reusable buffers.
#[derive(Debug, Default)]
pub struct HistogramScratch {
    heads: Vec<Head>,
    next: Vec<u32>,
    stack: Vec<Work>,
    myers: MyersScratch,
    pre: Prefilter,
    tmp: Vec<(u32, u32)>,
}

impl HistogramScratch {
    pub fn new() -> Self {
        Self::default()
    }
}

const NONE: u32 = u32::MAX;

/// Histogram matches of `a` and `b`: increasing index pairs with `a[i] == b[j]`.
pub fn histogram_matches(a: &[u32], b: &[u32], cancel: &CancelToken) -> Result<Vec<(u32, u32)>> {
    let mut out = Vec::new();
    histogram_matches_into(a, b, cancel, &mut HistogramScratch::new(), &mut out)?;
    Ok(out)
}

/// Histogram matches appended to `out`. The common ends are cut off and lines that occur on
/// one side only are dropped first (see [`Prefilter`]), so the regions below work on dense ids.
pub fn histogram_matches_into(
    a: &[u32],
    b: &[u32],
    cancel: &CancelToken,
    scratch: &mut HistogramScratch,
    out: &mut Vec<(u32, u32)>,
) -> Result<()> {
    // decision: D40: reduce first, then work on dense ids
    let mut pre = std::mem::take(&mut scratch.pre);
    let mut tmp = std::mem::take(&mut scratch.tmp);
    tmp.clear();
    let trim = pre.reduce(a, b);
    let res = histogram_dense(&pre.a, &pre.b, pre.ids, cancel, scratch, &mut tmp);
    if res.is_ok() {
        pre.emit(trim, (a.len(), b.len()), &tmp, out);
    }
    scratch.pre = pre;
    scratch.tmp = tmp;
    res
}

/// The histogram algorithm on sequences of dense ids `0..ids`.
fn histogram_dense(
    a: &[u32],
    b: &[u32],
    ids: u32,
    cancel: &CancelToken,
    scratch: &mut HistogramScratch,
    out: &mut Vec<(u32, u32)>,
) -> Result<()> {
    scratch.heads.clear();
    scratch.heads.resize(ids as usize, Head::default());
    let mut region = 0u32;
    scratch.stack.clear();
    scratch.stack.push(Work::Diff {
        a0: 0,
        a1: a.len(),
        b0: 0,
        b1: b.len(),
    });
    let HistogramScratch {
        heads,
        next,
        stack,
        myers,
        ..
    } = scratch;

    while let Some(work) = stack.pop() {
        match work {
            Work::Emit { a, b, len } => {
                out.extend((0..len).map(|k| ((a + k) as u32, (b + k) as u32)));
            }
            Work::Diff {
                mut a0,
                mut a1,
                mut b0,
                mut b1,
            } => {
                cancel.check()?;
                // common prefix
                let start = a0;
                while a0 < a1 && b0 < b1 && a[a0] == b[b0] {
                    a0 += 1;
                    b0 += 1;
                }
                out.extend((start..a0).map(|i| (i as u32, (b0 - (a0 - i)) as u32)));
                // common suffix, emitted last
                let mut suffix = 0;
                while a1 - suffix > a0
                    && b1 - suffix > b0
                    && a[a1 - 1 - suffix] == b[b1 - 1 - suffix]
                {
                    suffix += 1;
                }
                a1 -= suffix;
                b1 -= suffix;
                if suffix > 0 {
                    stack.push(Work::Emit {
                        a: a1,
                        b: b1,
                        len: suffix,
                    });
                }
                if a0 >= a1 || b0 >= b1 {
                    continue;
                }

                // chains of equal items of a[a0..a1), each in ascending order
                region += 1;
                next.clear();
                next.resize(a1 - a0, NONE);
                for i in (a0..a1).rev() {
                    let h = &mut heads[a[i] as usize];
                    if h.stamp != region {
                        *h = Head {
                            first: NONE,
                            count: 0,
                            stamp: region,
                        };
                    }
                    next[i - a0] = h.first;
                    h.first = i as u32;
                    h.count += 1;
                }

                // best anchor: fewest occurrences in a, then the longest region
                let (mut best_count, mut best_len) = (MAX_CHAIN + 1, 0usize);
                let (mut bs, mut bt, mut be, mut bf) = (0, 0, 0, 0);
                let mut j = b0;
                while j < b1 {
                    // decision: D39: after a candidate's region, resume behind its end in `b`
                    // (as jgit and imara-diff do); rescanning a long equal run once per element
                    // is quadratic
                    let mut resume = j + 1;
                    let Head {
                        first,
                        count,
                        stamp,
                    } = heads[b[j] as usize];
                    if stamp != region || count > best_count || count > MAX_CHAIN {
                        j = resume;
                        continue;
                    }
                    let mut i = first;
                    while i != NONE {
                        let (mut s, mut t) = (i as usize, j);
                        while s > a0 && t > b0 && a[s - 1] == b[t - 1] {
                            s -= 1;
                            t -= 1;
                        }
                        let (mut e, mut f) = (i as usize + 1, j + 1);
                        while e < a1 && f < b1 && a[e] == b[f] {
                            e += 1;
                            f += 1;
                        }
                        resume = resume.max(f);
                        let len = e - s;
                        if count < best_count || (count == best_count && len > best_len) {
                            best_count = count;
                            best_len = len;
                            (bs, bt, be, bf) = (s, t, e, f);
                        }
                        i = next[i as usize - a0];
                    }
                    j = resume;
                }

                if best_len == 0 {
                    // every candidate is too common (or none): exact Myers on this range
                    myers_range_into(a, b, (a0, a1 - a0), (b0, b1 - b0), cancel, myers, out)?;
                } else {
                    stack.push(Work::Diff {
                        a0: be,
                        a1,
                        b0: bf,
                        b1,
                    });
                    stack.push(Work::Emit {
                        a: bs,
                        b: bt,
                        len: best_len,
                    });
                    stack.push(Work::Diff {
                        a0,
                        a1: bs,
                        b0,
                        b1: bt,
                    });
                }
            }
        }
    }
    Ok(())
}
