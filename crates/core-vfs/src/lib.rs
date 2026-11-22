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
    check_destination_unchanged, copy_between, copy_stream, copy_tree, is_temp_name,
    temp_sibling_name, write_atomic,
};
pub use fault::{FaultVfs, Op};
pub use local_std::LocalStd;
#[cfg(windows)]
pub use local_win32::{LocalWin32, set_no_buffering_threshold, volume_space};
pub use mem::MemVfs;
pub use path::{Name, VPath};
pub use root::{RootKind, VfsRoot};
pub use validate::{NameCheck, case_collisions, is_reserved_windows_name, validate_name};
pub use vfs::{CopyOptions, DeleteMode, Entry, Kind, ReadSeek, Vfs, VfsCaps, attr};

/// Free and total bytes of the volume that holds `path`, for a status bar. Windows only for now:
/// elsewhere it is `Unsupported` (the std library has no portable call, and the core avoids a libc
/// dependency).
#[cfg(not(windows))]
pub fn volume_space(path: &std::path::Path) -> core_base::Result<(u64, u64)> {
    Err(core_base::Error::new(
        core_base::ErrorKind::Unsupported,
        format!(
            "volume space of {} is not available on this platform",
            path.display()
        ),
    ))
}
