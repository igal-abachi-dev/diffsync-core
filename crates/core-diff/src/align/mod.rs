//! Line alignment (plan.md §5.1, §5.2; spec ALIGN-1).
//!
//! `align` turns two line sources into a stream of [`AlignOp`]s. This module implements the
//! Standard aligner, `None` (Standard with skew 1 and closeness off) and the exact-match
//! kinds Myers, Histogram and Patience.

mod aligner;
pub mod closeness;
mod config;
pub mod consts;
mod exactkinds;
mod gap;
mod runs;
mod scratch;
mod source;
mod window;

pub use aligner::{AlignRange, align, align_with_scratch};
pub use config::{AlignConfig, AlignKind, Resolved};
pub use scratch::AlignScratch;
pub use source::{AlignOp, DocSource, LineSource, ORPHAN, Side, VecSource};
