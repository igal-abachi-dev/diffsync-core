//! Alignment engines and the text compare session (plan.md §5). No file-system access: lines
//! come from `core-text` documents or the [`align::LineSource`] trait.
#![forbid(unsafe_code)]

pub mod align;
pub mod edit;
pub mod exact;
pub mod options;
pub mod patch;
pub mod searchreplace;
pub mod session;
pub mod udiff;

pub use align::{
    AlignConfig, AlignKind, AlignOp, AlignRange, AlignScratch, DocSource, LineSource, ORPHAN, Side,
    VecSource, align, align_with_scratch,
};
pub use options::TextOptions;
pub use session::{DiffRow, DocSpec, RowKind, Status, TextCompare, Verdict};
