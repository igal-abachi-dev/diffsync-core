//! The Standard / None aligner: top-level walk (spec ALIGN-1.3) and the block window loop with
//! its work stack (ALIGN-1.4, fixes B1/B2, decision D1). The window search, run selection and
//! gap DP live in `window`, `runs` and `gap`.

use core_base::{CancelToken, Error, Result};
use core_text::meta::LineMeta;

use super::closeness::{
    closeness_bytes, closeness_equal_hash, closeness_with_common, fill_counts, overlap_from_counts,
};
use super::config::{AlignConfig, AlignKind, Resolved};
use super::consts::{
    LINE_GROWTH_STEP, NEVER_ALIGN_SCORE, NO_CLOSENESS_SCORE, ORPHAN_CHUNK_STANDARD,
    RUN_PIECE_EMIT_ABOVE, closeness_cache_size,
};
use super::scratch::{AlignScratch, BigRun, Frame};
use super::source::{AlignOp, LineSource, ORPHAN, Side};
use super::window::WindowOutcome;

/// The part of the two inputs to align: half-open line ranges. An end of `u32::MAX` means
/// "to the end of that side".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlignRange {
    pub src: std::ops::Range<u32>,
    pub tgt: std::ops::Range<u32>,
}

impl AlignRange {
    /// Both sides from the first line to the end.
    pub fn full() -> Self {
        AlignRange {
            src: 0..u32::MAX,
            tgt: 0..u32::MAX,
        }
    }
}

/// Aligns two line sources and writes [`AlignOp`]s to `sink`, in increasing order.
///
/// Validates and resolves `cfg` first (spec ALIGN-1.1). Returns `Err(Cancelled)` as soon as
/// `cancel` fires (checked per window, per DP row and per emitted op).
#[allow(clippy::needless_pass_by_value)] // `range` by value mirrors the plan.md §5.1 signature
pub fn align(
    src: &dyn LineSource,
    cfg: &AlignConfig,
    range: AlignRange,
    cancel: &CancelToken,
    sink: &mut dyn FnMut(AlignOp),
) -> Result<()> {
    align_with_scratch(src, cfg, range, cancel, sink, &mut AlignScratch::new())
}

/// Like [`align`], but reuses the buffers in `scratch`. After the first call that is large
/// enough, the aligner allocates nothing.
#[allow(clippy::needless_pass_by_value)] // `range` by value mirrors the plan.md §5.1 signature
pub fn align_with_scratch(
    src: &dyn LineSource,
    cfg: &AlignConfig,
    range: AlignRange,
    cancel: &CancelToken,
    sink: &mut dyn FnMut(AlignOp),
    scratch: &mut AlignScratch,
) -> Result<()> {
    let resolved = cfg.resolve()?;
    cancel.check()?;
    let mut a = Aligner::new(src, resolved, cancel, sink, scratch, &range);
    match resolved.kind {
        AlignKind::Standard | AlignKind::Patience => a.align_range(),
        AlignKind::Myers | AlignKind::Histogram => a.align_exact(),
        AlignKind::None => Err(Error::internal("AlignKind::None is resolved to Standard")),
    }
}

pub(super) struct Aligner<'a> {
    pub src: &'a dyn LineSource,
    pub cfg: Resolved,
    pub cancel: &'a CancelToken,
    pub sink: &'a mut dyn FnMut(AlignOp),
    pub s: &'a mut AlignScratch,
    /// One past the last line of each side's range.
    pub stop: [u32; 2],
    pub cache_mask: u32,
}

/// First index in `[start, limit)` that does not exist, or `limit` when all do. Gallops, then
/// bisects, so a source with a cheap `exists` needs O(log n) calls.
fn find_end(src: &dyn LineSource, side: Side, start: u32, limit: u32) -> u32 {
    if start >= limit || !src.exists(side, start) {
        return start.min(limit);
    }
    let (mut lo, mut hi) = (u64::from(start), u64::from(limit));
    let mut step = 1u64;
    while lo + step < hi {
        let probe = lo + step;
        if src.exists(side, probe as u32) {
            lo = probe;
            step *= 2;
        } else {
            hi = probe;
            break;
        }
    }
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        if src.exists(side, mid as u32) {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    hi as u32
}

impl<'a> Aligner<'a> {
    pub(super) fn new(
        src: &'a dyn LineSource,
        cfg: Resolved,
        cancel: &'a CancelToken,
        sink: &'a mut dyn FnMut(AlignOp),
        s: &'a mut AlignScratch,
        range: &AlignRange,
    ) -> Self {
        let cache_size = closeness_cache_size(cfg.skew);
        s.reset(cache_size);
        let mut stop = [0; 2];
        for (side, r) in [(Side::Left, &range.src), (Side::Right, &range.tgt)] {
            let end = find_end(src, side, r.start, r.end);
            stop[side.index()] = end;
            let sd = &mut s.sides[side.index()];
            sd.start = r.start.min(end);
            sd.stop = end;
        }
        Aligner {
            src,
            cfg,
            cancel,
            sink,
            s,
            stop,
            cache_mask: (cache_size - 1) as u32,
        }
    }

    // ------------------------------------------------------------------ output

    pub fn emit(&mut self, src: u32, tgt: u32, amount: u32) -> Result<()> {
        self.cancel.check()?;
        if amount > 0 {
            (self.sink)(AlignOp { src, tgt, amount });
        }
        Ok(())
    }

    // ---------------------------------------------------------------- metadata

    /// Loads the metadata of `side` through line `idx` (in chunks, spec ALIGN-1.2). `idx`
    /// must be inside the range.
    pub fn ensure(&mut self, side: Side, idx: u32) {
        let src = self.src;
        let sd = &mut self.s.sides[side.index()];
        let loaded = sd.loaded_end();
        if idx < loaded {
            return;
        }
        debug_assert!(idx < sd.stop, "line {idx} is past the stop {}", sd.stop);
        let chunk = LINE_GROWTH_STEP as u32;
        let rel = idx - sd.start;
        let want = (u64::from(sd.start) + u64::from((rel / chunk + 1) * chunk))
            .min(u64::from(sd.stop)) as u32;
        sd.metas.reserve((want - loaded) as usize);
        for i in loaded..want {
            sd.metas.push(src.meta(side, i));
        }
    }

    /// Metadata of a line that is known to be loaded.
    #[inline]
    pub fn ml(&self, side: Side, idx: u32) -> LineMeta {
        let sd = &self.s.sides[side.index()];
        sd.metas[(idx - sd.start) as usize]
    }

    /// Metadata of a line, loading it when needed.
    #[inline]
    pub fn meta(&mut self, side: Side, idx: u32) -> LineMeta {
        self.ensure(side, idx);
        self.ml(side, idx)
    }

    // --------------------------------------------------------------- closeness

    pub(super) fn cache_slot(&mut self, side: Side, idx: u32) -> usize {
        let slot = (idx & self.cache_mask) as usize;
        let e = &mut self.s.cache[side.index()][slot];
        if e.index != idx {
            e.text.clear();
            self.src.closeness_text(side, idx, &mut e.text);
            e.exact = fill_counts(&e.text, &mut e.counts);
            e.index = idx;
        }
        slot
    }

    /// The pair score of source line `i` and target line `j` (spec ALIGN-1.6, decision D2).
    /// Both lines must be loaded.
    pub fn closeness(&mut self, i: u32, j: u32) -> i32 {
        let (a, b) = (self.ml(Side::Left, i), self.ml(Side::Right, j));
        if a.hash == b.hash {
            let src = self.src;
            return closeness_equal_hash(a.is_sig(), b.is_sig(), a.weight, b.weight, || {
                src.full_text_eq(i, j)
            });
        }
        if self.cfg.use_closeness {
            let (sa, sb) = (
                self.cache_slot(Side::Left, i),
                self.cache_slot(Side::Right, j),
            );
            let AlignScratch { cache, hist, .. } = &mut *self.s;
            let (ea, eb) = (&cache[0][sa], &cache[1][sb]);
            if ea.exact && eb.exact && !ea.text.is_empty() && !eb.text.is_empty() {
                let common = i64::from(overlap_from_counts(&ea.counts, &eb.counts));
                return closeness_with_common(
                    ea.text.len() as i64,
                    eb.text.len() as i64,
                    common,
                    self.cfg.closeness_same_only,
                );
            }
            closeness_bytes(
                &cache[0][sa].text,
                &cache[1][sb].text,
                self.cfg.closeness_same_only,
                hist,
            )
        } else if self.cfg.never_align_diffs {
            NEVER_ALIGN_SCORE
        } else {
            NO_CLOSENESS_SCORE
        }
    }

    // --------------------------------------------------------------- top level

    /// spec: ALIGN-1.3: walk both sides together, pairing equal significant lines one by one
    /// and collecting a pending gap of insignificant lines, then hand the rest to the block
    /// aligner.
    fn align_range(&mut self) -> Result<()> {
        let (mut i, mut j) = (self.s.sides[0].start, self.s.sides[1].start);
        let (stop_s, stop_t) = (self.stop[0], self.stop[1]);
        let (mut pend_s, mut pend_t): (Option<u32>, Option<u32>) = (None, None);
        while i < stop_s && j < stop_t {
            let (a, b) = (self.meta(Side::Left, i), self.meta(Side::Right, j));
            let (sig_s, sig_t) = (a.is_sig(), b.is_sig());
            if !sig_s && !sig_t {
                if pend_s.is_none() && pend_t.is_none() && a.hash == b.hash && self.top_equal(i, j)
                {
                    self.emit(i, j, 1)?;
                } else {
                    pend_s.get_or_insert(i);
                    pend_t.get_or_insert(j);
                }
                i += 1;
                j += 1;
            } else if !sig_s {
                if self.cfg.orphans_always_sig {
                    break;
                }
                pend_s.get_or_insert(i);
                i += 1;
            } else if !sig_t {
                if self.cfg.orphans_always_sig {
                    break;
                }
                pend_t.get_or_insert(j);
                j += 1;
            } else {
                if a.hash != b.hash || !self.top_equal(i, j) {
                    break;
                }
                if pend_s.is_some() || pend_t.is_some() {
                    let ps = *pend_s.get_or_insert(i);
                    let pt = *pend_t.get_or_insert(j);
                    self.align_block(ps, i, pt, j)?;
                    pend_s = None;
                    pend_t = None;
                }
                self.emit(i, j, 1)?;
                i += 1;
                j += 1;
            }
            self.cancel.check()?;
        }
        if let Some(p) = pend_s {
            i = p;
        }
        if let Some(p) = pend_t {
            j = p;
        }
        self.align_block(i, stop_s, j, stop_t)
    }

    /// Equal hashes at the top level. Standard follows the spec (hash equality); Patience also
    /// confirms the text so a CRC collision never pairs different lines (plan.md §5.3).
    fn top_equal(&mut self, i: u32, j: u32) -> bool {
        self.cfg.kind != AlignKind::Patience || self.confirm(i, j)
    }

    // ------------------------------------------------------------ block aligner

    /// Aligns the block `[s0, s1) x [t0, t1)` with the Standard window loop. Nested blocks
    /// (the part before a big run) run on an explicit stack.
    pub fn align_block(&mut self, s0: u32, s1: u32, t0: u32, t1: u32) -> Result<()> {
        if self.cfg.kind == AlignKind::Patience {
            return self.patience_block(s0, s1, t0, t1);
        }
        let base = self.s.stack.len();
        self.s.stack.push(Frame::Block {
            s: s0,
            s1,
            t: t0,
            t1,
        });
        while self.s.stack.len() > base {
            let Some(frame) = self.s.stack.pop() else {
                break;
            };
            match frame {
                Frame::Block { s, s1, t, t1 } => self.block_loop(s, s1, t, t1)?,
                Frame::Extend { big, s1, t1 } => self.extend_big_run(big, s1, t1)?,
            }
        }
        Ok(())
    }

    /// spec: ALIGN-1.4: loop until both sides are consumed. One side exhausted → the other is
    /// emitted as orphans in chunks of 200; otherwise open a window and align it.
    fn block_loop(&mut self, mut s: u32, s1: u32, mut t: u32, t1: u32) -> Result<()> {
        loop {
            self.cancel.check()?;
            if s >= s1 && t >= t1 {
                return Ok(());
            }
            if s >= s1 {
                let n = (t1 - t).min(ORPHAN_CHUNK_STANDARD);
                self.emit(ORPHAN, t, n)?;
                t += n;
                continue;
            }
            if t >= t1 {
                let n = (s1 - s).min(ORPHAN_CHUNK_STANDARD);
                self.emit(s, ORPHAN, n)?;
                s += n;
                continue;
            }
            let ws = s.saturating_add(self.cfg.skew).min(s1);
            let wt = t.saturating_add(self.cfg.skew).min(t1);
            // decision: D1: one per-window condition drives clipping and commit-and-slide
            let more_after = ws < s1 || wt < t1;
            match self.search_window(s, ws, t, wt)? {
                WindowOutcome::BigRun(big) => {
                    // fix: B2: drop the partial run list (done by the next search) and align
                    // the block before the run first; then extend the run (fix B1) and go on
                    // with the rest of this block.
                    self.s.stack.push(Frame::Extend { big, s1, t1 });
                    self.s.stack.push(Frame::Block {
                        s,
                        s1: big.src,
                        t,
                        t1: big.tgt,
                    });
                    self.s.max_depth = self.s.max_depth.max(self.s.stack.len());
                    return Ok(());
                }
                WindowOutcome::Runs => {
                    let (ns, nt) = self.select_runs(s, ws, t, wt, more_after)?;
                    if ns == s && nt == t {
                        return Err(Error::internal(format!(
                            "alignment made no progress at source line {s}, target line {t}"
                        )));
                    }
                    s = ns;
                    t = nt;
                }
            }
        }
    }

    /// fix: B1: extend a big run to the **outer block stop** (not the window edge) and emit it
    /// in pieces: a piece goes out when its count exceeds 500, so pieces have 501 lines and the
    /// remainder comes last (spec ALIGN-1.4).
    fn extend_big_run(&mut self, big: BigRun, s1: u32, t1: u32) -> Result<()> {
        let (mut rs, mut rt, mut cnt) = (big.src, big.tgt, big.count);
        let (mut i, mut j) = (rs + cnt, rt + cnt);
        while i < s1 && j < t1 {
            self.cancel.check()?;
            if cnt > RUN_PIECE_EMIT_ABOVE {
                self.emit(rs, rt, cnt)?;
                cnt = 0;
                rs = i;
                rt = j;
            }
            if self.meta(Side::Left, i).hash != self.meta(Side::Right, j).hash {
                break;
            }
            cnt += 1;
            i += 1;
            j += 1;
        }
        if cnt > 0 {
            self.emit(rs, rt, cnt)?;
        }
        // the rest of the block continues after the run
        self.s.stack.push(Frame::Block {
            s: rs + cnt,
            s1,
            t: rt + cnt,
            t1,
        });
        Ok(())
    }
}
