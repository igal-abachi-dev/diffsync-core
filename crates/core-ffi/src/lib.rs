//! C ABI of the diffsync core, built as `diffsync_core.dll` (plan.md §10; backlog E9).
//!
//! Conventions (see `docs/abi.md`): every export is `extern "C"`, returns a `DsStatus`, and runs
//! inside a panic barrier; handles are generation-checked `u64`s (`ds_close` closes any of
//! them); user-data strings are UTF-16 views, options and reports are UTF-8 JSON; output buffers
//! use the two-call pattern (`DS_E_BUFFER_TOO_SMALL` plus the needed size).
//!
//! `unsafe` is allowed in this crate (raw pointers at the ABI boundary); every block carries a
//! `// SAFETY:` comment or is an `unsafe fn` whose doc states the caller's contract.
#![allow(unsafe_code)]
// the ABI types are imported as a set, and C-style entry points take many arguments
#![allow(
    clippy::wildcard_imports,
    clippy::too_many_arguments,
    clippy::missing_safety_doc
)]

pub mod edit;
pub mod events;
pub mod folder;
pub mod options;
pub mod registry;
pub mod status;
pub mod sync;
#[cfg(feature = "test-panic")]
pub mod testexports;
pub mod text;
pub mod tools;
pub mod types;
pub mod util;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::Mutex;

pub use edit::{ds_text_edit, ds_text_save};
pub use folder::{
    ds_folder_cancel, ds_folder_compare_content, ds_folder_node_info, ds_folder_node_name,
    ds_folder_open, ds_folder_refresh, ds_folder_report, ds_folder_set_ignored, ds_folder_status,
    ds_view_create, ds_view_expand, ds_view_get_rows, ds_view_row_count, ds_view_set,
};
pub use status::*;
pub use sync::{
    ds_fileops_copy, ds_fileops_delete, ds_fileops_move, ds_fileops_rename, ds_job_answer,
    ds_job_cancel, ds_job_get_question, ds_job_pause, ds_job_result, ds_job_resume, ds_job_status,
    ds_maintenance_sweep, ds_plan_execute, ds_plan_get_actions, ds_plan_get_prompts,
    ds_plan_report, ds_plan_set_op, ds_plan_summary, ds_sync_plan,
};
pub use text::{
    ds_text_cancel, ds_text_doc_info, ds_text_find_diff, ds_text_get_intraline, ds_text_get_line,
    ds_text_get_overview, ds_text_get_rows, ds_text_get_sections, ds_text_open, ds_text_recompare,
    ds_text_status,
};
pub use tools::{
    ds_bytes_diff, ds_diff_run, ds_diff_texts, ds_json_patch, ds_merge_files, ds_merge3,
    ds_patch_apply, ds_shell_selection_clear, ds_shell_selection_get, ds_shell_selection_set,
    ds_text_find, ds_text_find_section, ds_text_get_folds, ds_text_get_section_kinds,
    ds_tool_default_ignore, ds_tool_is_ignored, ds_tool_parse_args, ds_volume_info,
};
pub use types::*;

/// The ABI version: `major << 16 | minor`. 1.0 was frozen at backlog E10-S3 (docs/abi.md); 1.1 added
/// the integration exports of E11.5. Additive changes bump the minor, a breaking change would bump
/// the major and is avoided.
pub const ABI_MAJOR: u32 = 1;
pub const ABI_MINOR: u32 = 2;

/// The default folder for execution logs, set by `ds_init`.
pub static DEFAULT_LOG_DIR: Mutex<Option<PathBuf>> = Mutex::new(None);

static INITIALISED: AtomicBool = AtomicBool::new(false);

/// `major << 16 | minor` of this library.
#[unsafe(no_mangle)]
pub extern "C" fn ds_abi_version() -> u32 {
    (ABI_MAJOR << 16) | ABI_MINOR
}

/// Initialises the library once: worker threads of the shared pool, the log sink, the default
/// log folder. Calling it again returns `DS_E_BUSY`. Calling no other function first is fine:
/// defaults are used.
///
/// # Safety
/// `opts` must be null or point to a valid `DsInitOptions` (with its string views valid).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ds_init(opts: *const DsInitOptions) -> DsStatus {
    status::ffi_entry!({
        // SAFETY: the ABI contract: null or a valid struct.
        let Some(o) = (unsafe { opts.as_ref() }) else {
            return Ok(());
        };
        if (o.struct_size as usize) < size_of::<DsInitOptions>() {
            return Err(Fail::invalid("struct_size is too small"));
        }
        // everything is validated before anything is applied, so a failed init changes nothing
        // SAFETY: the ABI contract covers the string views.
        let dir = unsafe { util::units(o.log_dir) }?;
        let log_dir = (!dir.is_empty()).then(|| util::os_path(dir));
        if log_dir.as_ref().is_some_and(|d| !d.is_absolute()) {
            return Err(Fail::invalid("log_dir must be an absolute path"));
        }
        if o.worker_threads > MAX_WORKER_THREADS {
            return Err(Fail::invalid("worker_threads is above 1024"));
        }
        if INITIALISED.swap(true, Ordering::SeqCst) {
            return Err(Fail::busy("the library is already initialised"));
        }
        events::set_log_sink(o.log_fn, o.log_user);
        if o.worker_threads > 0 {
            // the global pool can be configured once; a later attempt is harmless
            let _ = rayon::ThreadPoolBuilder::new()
                .num_threads(o.worker_threads as usize)
                .build_global();
        }
        if log_dir.is_some() {
            *DEFAULT_LOG_DIR.lock() = log_dir;
        }
        Ok(())
    })
}

/// Upper bound for `DsInitOptions::worker_threads` (a garbage value must not exhaust the machine).
const MAX_WORKER_THREADS: u32 = 1024;

/// Copies the thread's last error message (UTF-8, no NUL) into `buf`; `*needed` always gets the
/// full length.
///
/// # Safety
/// `buf` must be valid for `cap` bytes (or null with `cap` 0); `needed` null or valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ds_last_error(buf: *mut u8, cap: usize, needed: *mut usize) -> DsStatus {
    // not wrapped in `ffi_entry!`: it must not overwrite the message it reports
    let msg = status::last_error();
    let bytes = msg.as_bytes();
    if !needed.is_null() {
        // SAFETY: non-null, valid for a write by the ABI contract.
        unsafe { *needed = bytes.len() };
    }
    let n = bytes.len().min(cap);
    if n > 0 && !buf.is_null() {
        // SAFETY: `buf` is valid for `cap >= n` bytes and does not overlap the message.
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), buf, n) };
    }
    if bytes.len() > cap {
        DS_E_BUFFER_TOO_SMALL
    } else {
        DS_OK
    }
}

/// Frees a buffer the core returned (`DsBuf`).
///
/// # Safety
/// `b` must come from a core function and not have been freed before.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ds_buf_free(b: DsBuf) {
    // SAFETY: forwarded contract.
    unsafe { util::free_buf(b) };
}

/// Closes any handle (a text compare, folder compare, view, plan or job). Work in progress is
/// cancelled. A stale or already closed handle is `DS_E_INVALID_HANDLE`.
#[unsafe(no_mangle)]
pub extern "C" fn ds_close(h: DsHandle) -> DsStatus {
    status::ffi_entry!(registry::close(h))
}

/// The JSON Schema of every option set, as UTF-8 JSON in a `DsBuf` (free with `ds_buf_free`).
///
/// # Safety
/// `out` must be valid for a write.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ds_options_schema(out: *mut DsBuf) -> DsStatus {
    status::ffi_entry!({
        // SAFETY: the ABI contract covers the out pointer.
        let o = unsafe { util::out(out) }?;
        *o = util::to_buf(options::schema_json().into_bytes());
        Ok(())
    })
}
