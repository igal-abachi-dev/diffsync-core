//! File-system abstraction (plan.md §6): the `Vfs` trait, path types and root parsing, the
//! generic copy engine (safe copy, attribute retry, verify), destination-name validation, and
//! the backends `LocalStd`, `MemVfs` and `FaultVfs` (the Windows backend is in `local_win32`).
//!
//! `unsafe` is allowed only in `local_win32` (AGENTS.md); the crate-level lint is `deny`.
#![deny(unsafe_code)]

pub mod copy;
pub mod fault;
pub mod fold;
pub mod identity;
pub mod local_std;
#[cfg(windows)]
pub mod local_win32;
pub mod mem;
pub mod path;
pub mod root;
pub mod validate;
pub mod vfs;

pub use copy::{
    copy_between, copy_stream, copy_tree, is_temp_name, temp_sibling_name, write_atomic,
};
pub use fault::{FaultVfs, Op};
pub use local_std::LocalStd;
#[cfg(windows)]
pub use local_win32::{LocalWin32, set_no_buffering_threshold};
pub use mem::MemVfs;
pub use path::{Name, VPath};
pub use root::{RootKind, VfsRoot};
pub use validate::{NameCheck, case_collisions, validate_name};
pub use vfs::{CopyOptions, DeleteMode, Entry, Kind, ReadSeek, Vfs, VfsCaps, attr};
