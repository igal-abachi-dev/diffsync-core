//! Text documents (plan.md §4): decoding, the line index, line rules and per-line alignment
//! metadata. No alignment logic lives here; `core-diff` consumes [`LineMeta`] and
//! [`TextDocument`].
//!
//! `unsafe` is confined to two small places: the memory mapping in `mmap` and the one-line
//! `GetACP` declaration in `ansi`. Both carry `// SAFETY:` notes.
#![deny(unsafe_code)]

pub mod ansi;
pub mod document;
pub mod encoding;
pub mod fold;
pub mod format;
pub mod lines;
pub mod meta;
mod mmap;
pub mod rules;

pub use document::{DEFAULT_MMAP_THRESHOLD, LoadOptions, READ_BLOCK, SourceId, TextDocument};
pub use encoding::{DecodeOptions, Encoding, TextEncoding, first_unmappable};
pub use format::FileFormat;
pub use lines::{LineEnding, LineIndex, LineSpan, index_lines};
pub use meta::{LineMeta, META_CHUNK, MetaCursor, WeightFn, compute_all_parallel};
pub use rules::{EolCompare, LineRules, Scratch, WhitespaceMode, is_blank};
