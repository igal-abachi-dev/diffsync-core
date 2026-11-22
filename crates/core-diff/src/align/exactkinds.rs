//! The Myers, Histogram and Patience align kinds (spec ALIGN-1.7, ALIGN-1.8, decision D12).
//!
//! * Myers / Histogram: exact matches over the whole range, then every changed hunk between
//!   matches is paired or gap-filled (`align_exact`, `fill_changed_hunk`).
//! * Patience: the same top level as Standard; the block step is Patience anchors with
//!   orphan chunks of 2000 (`patience_block`, `patience_core`).
//!
//! Every exact match is confirmed with comparable-text equality (`confirm`), so a CRC-32
//! collision can never pair two different lines (plan.md §5.3).

use core_base::Result;

use super::aligner::Aligner;
use super::config::AlignKind;
use super::consts::ORPHAN_CHUNK_PATIENCE;
use super::scratch::PatienceWork;
use super::source::{ORPHAN, Side};
use crate::exact::{histogram_matches_into, myers_matches_into, patience_anchors};

impl Aligner<'_> {
    /// True when source line `i` and target line `j` have equal hashes, the same
    /// significance, and equal comparable text (the closeness text; the full line for
    /// insignificant lines). Both lines must be loaded.
    pub fn confirm(&mut self, i: u32, j: u32) -> bool {
        let (a, b) = (self.ml(Side::Left, i), self.ml(Side::Right, j));
        if a.hash != b.hash || a.is_sig() != b.is_sig() {
            return false;
        }
        if self.src.full_text_eq(i, j) {
            return true; // identical full lines have identical comparable texts
        }
        let (sa, sb) = (
            self.cache_slot(Side::Left, i),
            self.cache_slot(Side::Right, j),
        );
        self.s.cache[0][sa].text == self.s.cache[1][sb].text
    }

    /// spec: ALIGN-1.7 `FillChangedHunk`: a hunk between exact matches. One side empty →
    /// orphans; with closeness → the gap aligner; with `never_align_diffs` → orphans of both
    /// sides; otherwise (estimated) pair in order and leave the rest as orphans.
    pub fn fill_changed_hunk(&mut self, s0: u32, s1: u32, t0: u32, t1: u32) -> Result<()> {
        let (n, m) = (s1.saturating_sub(s0), t1.saturating_sub(t0));
        if n == 0 || m == 0 {
            self.emit(s0, ORPHAN, n)?;
            return self.emit(ORPHAN, t0, m);
        }
        if self.cfg.use_closeness {
            self.align_gap(s0, s1, t0, t1, false)?;
        } else if self.cfg.never_align_diffs {
            self.emit(s0, ORPHAN, n)?;
            self.emit(ORPHAN, t0, m)?;
        } else {
            let p = n.min(m);
            self.emit(s0, t0, p)?;
            if n > p {
                self.emit(s0 + p, ORPHAN, n - p)?;
            }
            if m > p {
                self.emit(ORPHAN, t0 + p, m - p)?;
            }
        }
        Ok(())
    }

    /// spec: ALIGN-1.7 `AlignWholeExact`: exact matches over the whole range, then the hunks.
    pub fn align_exact(&mut self) -> Result<()> {
        let (s0, t0) = (self.s.sides[0].start, self.s.sides[1].start);
        let (s1, t1) = (self.stop[0], self.stop[1]);
        if s1 > s0 {
            self.ensure(Side::Left, s1 - 1);
        }
        if t1 > t0 {
            self.ensure(Side::Right, t1 - 1);
        }
        let mut hashes = std::mem::take(&mut self.s.hash_a);
        let mut hashes_b = std::mem::take(&mut self.s.hash_b);
        let mut matches = std::mem::take(&mut self.s.matches);
        hashes.clear();
        hashes.extend(
            self.s.sides[0].metas[..(s1 - s0) as usize]
                .iter()
                .map(|m| m.hash),
        );
        hashes_b.clear();
        hashes_b.extend(
            self.s.sides[1].metas[..(t1 - t0) as usize]
                .iter()
                .map(|m| m.hash),
        );
        matches.clear();
        let found = if self.cfg.kind == AlignKind::Histogram {
            histogram_matches_into(
                &hashes,
                &hashes_b,
                self.cancel,
                &mut self.s.histogram,
                &mut matches,
            )
        } else {
            myers_matches_into(
                &hashes,
                &hashes_b,
                self.cancel,
                &mut self.s.myers,
                &mut matches,
            )
        };
        let result = found.and_then(|()| self.emit_exact(&mut matches, s0, s1, t0, t1));
        self.s.hash_a = hashes;
        self.s.hash_b = hashes_b;
        self.s.matches = matches;
        result
    }

    fn emit_exact(
        &mut self,
        matches: &mut Vec<(u32, u32)>,
        s0: u32,
        s1: u32,
        t0: u32,
        t1: u32,
    ) -> Result<()> {
        // a hash collision must never pair different lines: drop unconfirmed matches
        let mut kept = Vec::with_capacity(matches.len());
        for &(a, b) in matches.iter() {
            if self.confirm(s0 + a, t0 + b) {
                kept.push((a, b));
            }
        }
        *matches = kept;
        let (mut ps, mut pt) = (s0, t0);
        let mut k = 0;
        while k < matches.len() {
            let (ra, rb) = matches[k];
            let mut len = 1;
            while k + len < matches.len() && matches[k + len] == (ra + len as u32, rb + len as u32)
            {
                len += 1;
            }
            self.fill_changed_hunk(ps, s0 + ra, pt, t0 + rb)?;
            self.emit(s0 + ra, t0 + rb, len as u32)?;
            ps = s0 + ra + len as u32;
            pt = t0 + rb + len as u32;
            k += len;
        }
        self.fill_changed_hunk(ps, s1, pt, t1)
    }

    /// spec: ALIGN-1.8 `AlignBlockPatience` for the block `[s0, s1) x [t0, t1)`. A block that
    /// reaches the end of either side is "open ended": an exhausted side leaves the other as
    /// orphans in chunks of 2000.
    pub fn patience_block(&mut self, s0: u32, s1: u32, t0: u32, t1: u32) -> Result<()> {
        let open_ended = s1 == self.stop[0] || t1 == self.stop[1];
        if open_ended && s0 >= s1 {
            return self.orphan_chunks(ORPHAN, t0, t1);
        }
        if open_ended && t0 >= t1 {
            return self.orphan_chunks(s0, ORPHAN, s1);
        }
        self.patience_core(s0, s1, t0, t1)
    }

    fn orphan_chunks(&mut self, s: u32, t: u32, end: u32) -> Result<()> {
        let (mut at, tgt) = if s == ORPHAN { (t, true) } else { (s, false) };
        while at < end {
            self.cancel.check()?;
            let n = (end - at).min(ORPHAN_CHUNK_PATIENCE);
            if tgt {
                self.emit(ORPHAN, at, n)?;
            } else {
                self.emit(at, ORPHAN, n)?;
            }
            at += n;
        }
        Ok(())
    }

    /// `PatienceCore` on an explicit stack: common prefix and suffix, then the longest chain
    /// of lines unique on both sides, then the parts between the anchors; hunks without anchors
    /// go to `fill_changed_hunk`.
    fn patience_core(&mut self, s0: u32, s1: u32, t0: u32, t1: u32) -> Result<()> {
        if s1 > s0 {
            self.ensure(Side::Left, s1 - 1);
        }
        if t1 > t0 {
            self.ensure(Side::Right, t1 - 1);
        }
        let (base_s, base_t) = (s0, t0);
        let mut ha = std::mem::take(&mut self.s.hash_a);
        let mut hb = std::mem::take(&mut self.s.hash_b);
        let mut stack = std::mem::take(&mut self.s.pwork);
        let mut anchors = std::mem::take(&mut self.s.matches);
        ha.clear();
        ha.extend(
            self.s.sides[0].metas
                [(s0 - self.s.sides[0].start) as usize..(s1 - self.s.sides[0].start) as usize]
                .iter()
                .map(|m| m.hash),
        );
        hb.clear();
        hb.extend(
            self.s.sides[1].metas
                [(t0 - self.s.sides[1].start) as usize..(t1 - self.s.sides[1].start) as usize]
                .iter()
                .map(|m| m.hash),
        );
        stack.clear();
        stack.push(PatienceWork::Core { s0, s1, t0, t1 });
        let r = self.patience_run(&mut stack, &ha, &hb, (base_s, base_t), &mut anchors);
        self.s.hash_a = ha;
        self.s.hash_b = hb;
        self.s.pwork = stack;
        self.s.matches = anchors;
        r
    }

    fn patience_run(
        &mut self,
        stack: &mut Vec<PatienceWork>,
        ha: &[u32],
        hb: &[u32],
        (base_s, base_t): (u32, u32),
        anchors: &mut Vec<(u32, u32)>,
    ) -> Result<()> {
        while let Some(work) = stack.pop() {
            match work {
                PatienceWork::Emit { src, tgt, amount } => self.emit(src, tgt, amount)?,
                PatienceWork::Hunk { s0, s1, t0, t1 } => self.fill_changed_hunk(s0, s1, t0, t1)?,
                PatienceWork::Core {
                    mut s0,
                    mut s1,
                    mut t0,
                    mut t1,
                } => {
                    self.cancel.check()?;
                    if s0 >= s1 || t0 >= t1 {
                        self.emit(s0, ORPHAN, s1.saturating_sub(s0))?;
                        self.emit(ORPHAN, t0, t1.saturating_sub(t0))?;
                        continue;
                    }
                    let h = |hs: &[u32], base: u32, i: u32| hs[(i - base) as usize];
                    // common prefix
                    let mut pre = 0;
                    while s0 + pre < s1
                        && t0 + pre < t1
                        && h(ha, base_s, s0 + pre) == h(hb, base_t, t0 + pre)
                        && self.confirm(s0 + pre, t0 + pre)
                    {
                        pre += 1;
                    }
                    self.emit(s0, t0, pre)?;
                    s0 += pre;
                    t0 += pre;
                    // common suffix
                    let mut suf = 0;
                    while s1 - suf > s0
                        && t1 - suf > t0
                        && h(ha, base_s, s1 - 1 - suf) == h(hb, base_t, t1 - 1 - suf)
                        && self.confirm(s1 - 1 - suf, t1 - 1 - suf)
                    {
                        suf += 1;
                    }
                    s1 -= suf;
                    t1 -= suf;
                    if suf > 0 {
                        stack.push(PatienceWork::Emit {
                            src: s1,
                            tgt: t1,
                            amount: suf,
                        });
                    }
                    if s0 >= s1 || t0 >= t1 {
                        self.emit(s0, ORPHAN, s1 - s0)?;
                        self.emit(ORPHAN, t0, t1 - t0)?;
                        continue;
                    }
                    let (sd0, sd1) = (&self.s.sides[0], &self.s.sides[1]);
                    patience_anchors(
                        ha,
                        hb,
                        ((s0 - base_s) as usize, (s1 - base_s) as usize),
                        ((t0 - base_t) as usize, (t1 - base_t) as usize),
                        |i| sd0.metas[i + (base_s - sd0.start) as usize].is_sig(),
                        |j| sd1.metas[j + (base_t - sd1.start) as usize].is_sig(),
                        &mut self.s.patience,
                        anchors,
                    );
                    // anchors are relative to the block base; drop unconfirmed ones
                    let mut confirmed = Vec::with_capacity(anchors.len());
                    for &(a, b) in anchors.iter() {
                        if self.confirm(base_s + a, base_t + b) {
                            confirmed.push((base_s + a, base_t + b));
                        }
                    }
                    if confirmed.is_empty() {
                        stack.push(PatienceWork::Hunk { s0, s1, t0, t1 });
                        continue;
                    }
                    // tasks in order: core, anchor, core, ..., core; pushed in reverse
                    let (mut hi_s, mut hi_t) = (s1, t1);
                    for &(a, b) in confirmed.iter().rev() {
                        stack.push(PatienceWork::Core {
                            s0: a + 1,
                            s1: hi_s,
                            t0: b + 1,
                            t1: hi_t,
                        });
                        stack.push(PatienceWork::Emit {
                            src: a,
                            tgt: b,
                            amount: 1,
                        });
                        hi_s = a;
                        hi_t = b;
                    }
                    stack.push(PatienceWork::Core {
                        s0,
                        s1: hi_s,
                        t0,
                        t1: hi_t,
                    });
                }
            }
        }
        Ok(())
    }
}
