//! Myers O(ND) matches in linear space (plan.md §5.3, decision D8, backlog E3-S1).
//!
//! A port of `Dc.Myers.DcMyersMatches`: the common prefix and suffix are trimmed first, then
//! the *middle snake* is found from both ends at once and the two halves are solved
//! separately. There is **no heuristic cut-off**, so the result is minimal: the number of
//! matches is the length of a longest common subsequence.
//!
//! The recursion of the original is an explicit work stack here (AGENTS.md: no unbounded
//! recursion). Cancellation is checked once per `D` step.

use core_base::{CancelToken, Error, ErrorKind, Result};

use super::prefilter::Prefilter;

/// Work items of the explicit stack. Pushed in reverse so they pop in output order.
#[derive(Debug, Clone, Copy)]
enum Work {
    /// Diff `a[a0..a0+n]` against `b[b0..b0+m]`.
    Diff {
        a0: usize,
        n: usize,
        b0: usize,
        m: usize,
    },
    /// Emit `len` matches starting at `(a, b)`.
    Emit { a: usize, b: usize, len: usize },
}

/// Reusable buffers: the forward and backward `V` arrays and the work stack.
#[derive(Debug, Default)]
pub struct MyersScratch {
    vf: Vec<i32>,
    vb: Vec<i32>,
    stack: Vec<Work>,
    pre: Prefilter,
    tmp: Vec<(u32, u32)>,
}

impl MyersScratch {
    pub fn new() -> Self {
        Self::default()
    }
}

/// The result of one middle-snake search.
#[derive(Debug, Clone, Copy)]
struct Snake {
    /// Edit distance of the (sub)problem.
    d: usize,
    x: usize,
    y: usize,
    u: usize,
    v: usize,
}

struct Engine<'a> {
    a: &'a [u32],
    b: &'a [u32],
    off: i32,
    vf: &'a mut [i32],
    vb: &'a mut [i32],
    cancel: &'a CancelToken,
}

impl Engine<'_> {
    /// The middle snake of `a[a0..a0+n]` x `b[b0..b0+m]` (both non-empty, prefix and suffix
    /// already trimmed). spec: ALIGN-1.7 (standard linear-space Myers, "estimated").
    fn middle_snake(&mut self, a0: usize, n: usize, b0: usize, m: usize) -> Result<Snake> {
        let (ni, mi) = (n as i32, m as i32);
        let delta = ni - mi;
        let odd = delta & 1 != 0;
        let max_d = (ni + mi + 1) / 2;
        let off = self.off;
        let (asl, bsl) = (&self.a[a0..a0 + n], &self.b[b0..b0 + m]);
        let ix = |k: i32| (off + k) as usize;
        self.vf[ix(1)] = 0;
        self.vb[ix(1)] = 0;
        for dd in 0..=max_d {
            self.cancel.check()?;
            // forward pass
            let mut k = -dd;
            while k <= dd {
                let mut xx = if k == -dd || (k != dd && self.vf[ix(k - 1)] < self.vf[ix(k + 1)]) {
                    self.vf[ix(k + 1)]
                } else {
                    self.vf[ix(k - 1)] + 1
                };
                let mut yy = xx - k;
                let (xi, yi) = (xx, yy);
                // length of the equal run on this diagonal (slices keep the bounds checks out of
                // the inner loop)
                // a diagonal can run past the end of either side: no equal run then
                let run = match (asl.get(xx as usize..), bsl.get(yy as usize..)) {
                    (Some(p), Some(q)) => {
                        p.iter().zip(q).take_while(|(p, q)| p == q).count() as i32
                    }
                    _ => 0,
                };
                xx += run;
                yy += run;
                self.vf[ix(k)] = xx;
                let inv_k = delta - k;
                if odd
                    && inv_k >= -(dd - 1)
                    && inv_k < dd
                    && self.vf[ix(k)] + self.vb[ix(inv_k)] >= ni
                {
                    return Ok(Snake {
                        d: (2 * dd - 1) as usize,
                        x: xi as usize,
                        y: yi as usize,
                        u: xx as usize,
                        v: yy as usize,
                    });
                }
                k += 2;
            }
            // backward pass, on the reversed sequences
            let mut k = -dd;
            while k <= dd {
                let mut xx = if k == -dd || (k != dd && self.vb[ix(k - 1)] < self.vb[ix(k + 1)]) {
                    self.vb[ix(k + 1)]
                } else {
                    self.vb[ix(k - 1)] + 1
                };
                let mut yy = xx - k;
                let (xi, yi) = (xx, yy);
                let run = match (asl.get(..(ni - xx) as usize), bsl.get(..(mi - yy) as usize)) {
                    (Some(p), Some(q)) => p
                        .iter()
                        .rev()
                        .zip(q.iter().rev())
                        .take_while(|(p, q)| p == q)
                        .count() as i32,
                    _ => 0,
                };
                xx += run;
                yy += run;
                self.vb[ix(k)] = xx;
                let inv_k = delta - k;
                if !odd && inv_k >= -dd && inv_k <= dd && self.vb[ix(k)] + self.vf[ix(inv_k)] >= ni
                {
                    return Ok(Snake {
                        d: (2 * dd) as usize,
                        x: (ni - xx) as usize,
                        y: (mi - yy) as usize,
                        u: (ni - xi) as usize,
                        v: (mi - yi) as usize,
                    });
                }
                k += 2;
            }
        }
        // Not reachable for valid input: some D <= (n + m + 1) / 2 always meets in the middle.
        Err(Error::internal("Myers middle snake not found"))
    }
}

/// Matches between `a` and `b` as increasing index pairs `(i, j)` with `a[i] == b[j]`. The
/// number of pairs is the length of a longest common subsequence.
pub fn myers_matches(a: &[u32], b: &[u32], cancel: &CancelToken) -> Result<Vec<(u32, u32)>> {
    let mut out = Vec::new();
    myers_matches_into(a, b, cancel, &mut MyersScratch::new(), &mut out)?;
    Ok(out)
}

/// Like [`myers_matches`], but appends to `out` and reuses `scratch`. The common ends are cut
/// off and lines that occur on one side only are dropped first (see [`Prefilter`]); neither
/// changes the number of matches.
pub fn myers_matches_into(
    a: &[u32],
    b: &[u32],
    cancel: &CancelToken,
    scratch: &mut MyersScratch,
    out: &mut Vec<(u32, u32)>,
) -> Result<()> {
    // decision: D40: reduce first; the match count is unchanged
    let mut pre = std::mem::take(&mut scratch.pre);
    let mut tmp = std::mem::take(&mut scratch.tmp);
    tmp.clear();
    let trim = pre.reduce(a, b);
    let (la, lb) = (pre.a.len(), pre.b.len());
    let res = myers_range_into(&pre.a, &pre.b, (0, la), (0, lb), cancel, scratch, &mut tmp);
    if res.is_ok() {
        pre.emit(trim, (a.len(), b.len()), &tmp, out);
    }
    scratch.pre = pre;
    scratch.tmp = tmp;
    res
}

/// Matches between `a[a0..a0+n]` and `b[b0..b0+m]`; the indexes in `out` are absolute.
pub fn myers_range_into(
    a: &[u32],
    b: &[u32],
    (a0, n): (usize, usize),
    (b0, m): (usize, usize),
    cancel: &CancelToken,
    scratch: &mut MyersScratch,
    out: &mut Vec<(u32, u32)>,
) -> Result<()> {
    if n == 0 || m == 0 {
        return Ok(());
    }
    if n + m >= (i32::MAX / 2) as usize
        || a.len() >= u32::MAX as usize
        || b.len() >= u32::MAX as usize
    {
        return Err(Error::new(
            ErrorKind::Unsupported,
            "sequences too long for Myers",
        ));
    }
    // V arrays: indexes -(D+1)..=D+1 around the centre `off`, D <= (n + m + 1) / 2
    #[allow(clippy::manual_midpoint)] // the sum cannot overflow: checked against i32::MAX / 2 above
    let off = ((n + m) / 2 + 3) as i32;
    let size = 2 * off as usize + 2;
    scratch.vf.clear();
    scratch.vf.resize(size, 0);
    scratch.vb.clear();
    scratch.vb.resize(size, 0);
    scratch.stack.clear();
    scratch.stack.push(Work::Diff { a0, n, b0, m });
    let MyersScratch { vf, vb, stack, .. } = scratch;
    let mut eng = Engine {
        a,
        b,
        off,
        vf,
        vb,
        cancel,
    };

    while let Some(work) = stack.pop() {
        match work {
            Work::Emit { a, b, len } => {
                out.extend((0..len).map(|k| ((a + k) as u32, (b + k) as u32)));
            }
            Work::Diff {
                mut a0,
                mut n,
                mut b0,
                mut m,
            } => {
                // common prefix: emitted now (it comes first in output order)
                let mut prefix = 0;
                while n > 0 && m > 0 && a[a0] == b[b0] {
                    prefix += 1;
                    a0 += 1;
                    b0 += 1;
                    n -= 1;
                    m -= 1;
                }
                out.extend(
                    (0..prefix).map(|k| ((a0 - prefix + k) as u32, (b0 - prefix + k) as u32)),
                );
                // common suffix: emitted after everything in between
                let mut suffix = 0;
                while n > suffix && m > suffix && a[a0 + n - 1 - suffix] == b[b0 + m - 1 - suffix] {
                    suffix += 1;
                }
                n -= suffix;
                m -= suffix;
                if suffix > 0 {
                    stack.push(Work::Emit {
                        a: a0 + n,
                        b: b0 + m,
                        len: suffix,
                    });
                }
                if n > 0 && m > 0 {
                    let s = eng.middle_snake(a0, n, b0, m)?;
                    // With prefix and suffix trimmed and both sides non-empty, D >= 2. A
                    // smaller D cannot leave matches (spec, `Dc.Myers`).
                    if s.d > 1 {
                        stack.push(Work::Diff {
                            a0: a0 + s.u,
                            n: n - s.u,
                            b0: b0 + s.v,
                            m: m - s.v,
                        });
                        stack.push(Work::Emit {
                            a: a0 + s.x,
                            b: b0 + s.y,
                            len: s.u - s.x,
                        });
                        stack.push(Work::Diff {
                            a0,
                            n: s.x,
                            b0,
                            m: s.y,
                        });
                    }
                }
            }
        }
    }
    Ok(())
}
