//! The text compare session (plan.md §5.4): rows, sections, statistics, progressive delivery,
//! word-level diff, navigation, binary documents and verdict mode.

mod compare;
pub mod intraline;
pub mod nav;
pub mod rows;
mod verdict;

pub use compare::{
    BATCH_INTERVAL, BATCH_ROWS, DocInfo, DocSpec, EVENT_INTERVAL, EventSink, Status, TextCompare,
    binary_equal, event,
};
pub use intraline::{CharRange, IntraLine};
pub use nav::{FindHit, FindOptions, Fold, SectionKind};
pub use rows::{DiffRow, Dir, RowKind, RowStore, Section, Stats};
pub use verdict::{Verdict, compare_verdict, compare_verdict_files};
