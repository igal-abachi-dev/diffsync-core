//! Shared primitives for every diffsync crate: errors, cancel/pause, progress, CRC-32 and
//! FILETIME ticks (plan.md §3). No file-system access and no `unsafe`.
#![forbid(unsafe_code)]

pub mod cancel;
pub mod crc32;
pub mod error;
pub mod progress;
pub mod ticks;

pub use cancel::{CancelToken, PauseGate};
pub use error::{Error, ErrorKind, Result};
pub use progress::{Progress, Stage};
pub use ticks::Ticks;
