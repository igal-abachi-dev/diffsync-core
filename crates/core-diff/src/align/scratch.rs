//! Reusable working memory of one aligner (plan.md §5.2, fix B7): after warm-up the window
//! loop and the gap DP allocate nothing.

use core_text::meta::LineMeta;

use super::closeness::Histogram;
use crate::exact::{HistogramScratch, MyersScratch, PatienceScratch};

/// Metadata of the lines of one side, loaded lazily in chunks (spec ALIGN-1.2).
#[derive(Debug, Default)]
pub(super) struct SideData {
    /// Index of the first line of the range being aligned.
    pub start: u32,
    /// One past the last line of the range (resolved at the start of a run).
    pub stop: u32,
    /// `metas[k]` is the metadata of line `start + k`.
    pub metas: Vec<LineMeta>,
}

impl SideData {
    pub fn loaded_end(&self) -> u32 {
        self.start + self.metas.len() as u32
    }
}

/// One slot of the closeness text ring cache.
#[derive(Debug, Default)]
pub(super) struct CacheEntry {
    /// The line this slot holds, `u32::MAX` when empty.
    pub index: u32,
    pub text: Vec<u8>,
    /// 256 byte counts of `text` (see `fill_counts`), valid when `exact`.
    pub counts: Vec<u8>,
    pub exact: bool,
}

/// A diagonal run of equal hashes in the window. Signed so clipping and trimming can never
/// underflow (fix B3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Run {
    pub count: i64,
    pub weight: i64,
    pub src: i64,
    pub tgt: i64,
}

/// A run that reached the big-run weight (spec ALIGN-1.4): found at `(src, tgt)` after
/// `count` lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct BigRun {
    pub src: u32,
    pub tgt: u32,
    pub count: u32,
}

/// Pending work of the block aligner. An explicit stack replaces recursion (fix B2), so
/// nesting depth is limited by memory, never by the call stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Frame {
    /// Continue the window loop of the block `[s, s1) x [t, t1)`.
    Block { s: u32, s1: u32, t: u32, t1: u32 },
    /// Extend a big run to the outer block stop `(s1, t1)` and emit it (fix B1), then
    /// continue the block after it.
    Extend { big: BigRun, s1: u32, t1: u32 },
}

/// Pending work of the Patience block step (explicit stack instead of recursion).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PatienceWork {
    Core { s0: u32, s1: u32, t0: u32, t1: u32 },
    Hunk { s0: u32, s1: u32, t0: u32, t1: u32 },
    Emit { src: u32, tgt: u32, amount: u32 },
}

/// All buffers an alignment needs. Create one and pass it to
/// [`align_with_scratch`](super::align_with_scratch) to reuse the memory across runs.
#[derive(Debug, Default)]
pub struct AlignScratch {
    pub(super) sides: [SideData; 2],
    pub(super) cache: [Vec<CacheEntry>; 2],
    pub(super) hist: Histogram,
    /// DP column of the gap aligner.
    pub(super) dp: Vec<i32>,
    /// Traceback of the gap aligner, 2 bits per cell.
    pub(super) trace: Vec<u8>,
    /// Visited cells of the window, one bit per cell.
    pub(super) visited: Vec<u64>,
    pub(super) runs: Vec<Run>,
    pub(super) stack: Vec<Frame>,
    /// Deepest the block stack got in the last run (diagnostics and tests).
    pub(super) max_depth: usize,
    // buffers of the exact kinds (Myers, Histogram, Patience); the Standard path never uses them
    pub(super) hash_a: Vec<u32>,
    pub(super) hash_b: Vec<u32>,
    pub(super) matches: Vec<(u32, u32)>,
    pub(super) pwork: Vec<PatienceWork>,
    pub(super) myers: MyersScratch,
    pub(super) histogram: HistogramScratch,
    pub(super) patience: PatienceScratch,
}

impl AlignScratch {
    pub fn new() -> Self {
        Self::default()
    }

    /// The deepest the block stack got during the last run. Nesting comes only from the block
    /// before a big run, which cannot itself contain a big run (it is a sub-rectangle of the
    /// window that found the run first), so this stays tiny: a handful of frames.
    pub fn max_stack_depth(&self) -> usize {
        self.max_depth
    }

    /// Bytes currently reserved by all buffers (for memory-bound tests and benchmarks).
    pub fn capacity_bytes(&self) -> usize {
        use std::mem::size_of;
        self.sides
            .iter()
            .map(|s| s.metas.capacity() * size_of::<LineMeta>())
            .sum::<usize>()
            + self
                .cache
                .iter()
                .flat_map(|c| c.iter())
                .map(|e| e.text.capacity() + e.counts.capacity() + size_of::<CacheEntry>())
                .sum::<usize>()
            + self.dp.capacity() * size_of::<i32>()
            + self.trace.capacity()
            + self.visited.capacity() * size_of::<u64>()
            + self.runs.capacity() * size_of::<Run>()
            + self.stack.capacity() * size_of::<Frame>()
    }

    /// Prepares the buffers for a run with the given ring cache size.
    pub(super) fn reset(&mut self, cache_size: usize) {
        for side in &mut self.sides {
            side.metas.clear();
        }
        for cache in &mut self.cache {
            if cache.len() != cache_size {
                cache.clear();
                cache.resize_with(cache_size, CacheEntry::default);
            }
            for e in cache.iter_mut() {
                e.index = u32::MAX;
            }
        }
        self.runs.clear();
        self.stack.clear();
        self.max_depth = 0;
    }
}
