//! Status codes, the thread-local last error and the panic barrier (plan.md §10.1; backlog
//! E9-S1.2). Every export runs through [`guard`], so nothing unwinds across the ABI.

use std::cell::RefCell;
use std::panic::{AssertUnwindSafe, catch_unwind};

use core_base::{Error, ErrorKind};

pub type DsStatus = i32;

pub const DS_OK: DsStatus = 0;
pub const DS_E_INVALID_ARG: DsStatus = -1;
pub const DS_E_INVALID_HANDLE: DsStatus = -2;
pub const DS_E_CANCELLED: DsStatus = -3;
pub const DS_E_NOT_FOUND: DsStatus = -4;
pub const DS_E_ACCESS_DENIED: DsStatus = -5;
pub const DS_E_BUFFER_TOO_SMALL: DsStatus = -6;
pub const DS_E_BUSY: DsStatus = -7;
pub const DS_E_INTERNAL: DsStatus = -8;
pub const DS_E_ALREADY_EXISTS: DsStatus = -9;
pub const DS_E_DISK_FULL: DsStatus = -10;
pub const DS_E_CHANGED_SINCE_COMPARE: DsStatus = -11;
pub const DS_E_UNSUPPORTED: DsStatus = -12;
pub const DS_E_IO: DsStatus = -13;
pub const DS_E_ENCODING: DsStatus = -14;
pub const DS_E_INVALID_NAME: DsStatus = -15;
pub const DS_E_PATH_TOO_LONG: DsStatus = -16;
pub const DS_E_SHARING_VIOLATION: DsStatus = -17;

/// A failure on its way to becoming a status code and a last-error message.
#[derive(Debug, Clone)]
pub struct Fail {
    pub status: DsStatus,
    pub message: String,
}

impl Fail {
    pub fn new(status: DsStatus, message: impl Into<String>) -> Fail {
        Fail {
            status,
            message: message.into(),
        }
    }

    pub fn invalid(message: impl Into<String>) -> Fail {
        Fail::new(DS_E_INVALID_ARG, message)
    }

    pub fn handle() -> Fail {
        Fail::new(
            DS_E_INVALID_HANDLE,
            "the handle is not valid (closed, stale or of another kind)",
        )
    }

    pub fn busy(message: impl Into<String>) -> Fail {
        Fail::new(DS_E_BUSY, message)
    }
}

/// The status code of an engine error kind.
pub fn status_of(kind: ErrorKind) -> DsStatus {
    match kind {
        ErrorKind::Cancelled => DS_E_CANCELLED,
        ErrorKind::NotFound => DS_E_NOT_FOUND,
        ErrorKind::AccessDenied => DS_E_ACCESS_DENIED,
        ErrorKind::SharingViolation => DS_E_SHARING_VIOLATION,
        ErrorKind::AlreadyExists => DS_E_ALREADY_EXISTS,
        ErrorKind::DiskFull => DS_E_DISK_FULL,
        ErrorKind::PathTooLong => DS_E_PATH_TOO_LONG,
        ErrorKind::ChangedSinceCompare => DS_E_CHANGED_SINCE_COMPARE,
        ErrorKind::InvalidArgument => DS_E_INVALID_ARG,
        ErrorKind::Encoding => DS_E_ENCODING,
        ErrorKind::Unsupported => DS_E_UNSUPPORTED,
        ErrorKind::Io => DS_E_IO,
        ErrorKind::Internal => DS_E_INTERNAL,
        ErrorKind::InvalidName => DS_E_INVALID_NAME,
    }
}

impl From<Error> for Fail {
    fn from(e: Error) -> Fail {
        Fail::new(status_of(e.kind()), e.to_string())
    }
}

pub type Res<T = ()> = Result<T, Fail>;

thread_local! {
    static LAST_ERROR: RefCell<String> = const { RefCell::new(String::new()) };
}

pub fn set_last_error(message: &str) {
    LAST_ERROR.with(|e| {
        let mut e = e.borrow_mut();
        e.clear();
        e.push_str(message);
    });
}

pub fn last_error() -> String {
    LAST_ERROR.with(|e| e.borrow().clone())
}

/// Runs an export body: an `Err` sets the last error and returns its status; a panic becomes
/// `DS_E_INTERNAL` with the panic message as the last error. A success leaves the last error
/// alone (docs/abi.md). While the body runs, the thread is marked as being inside an API call,
/// so events produced on it are delivered from another thread.
pub fn guard(body: impl FnOnce() -> Res) -> DsStatus {
    let was = crate::events::IN_API.with(|c| c.replace(true));
    let r = catch_unwind(AssertUnwindSafe(body));
    crate::events::IN_API.with(|c| c.set(was));
    match r {
        Ok(Ok(())) => DS_OK,
        Ok(Err(f)) => {
            set_last_error(&f.message);
            f.status
        }
        Err(payload) => {
            let msg = payload
                .downcast_ref::<&str>()
                .map(|s| (*s).to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".to_string());
            set_last_error(&format!("internal error (panic): {msg}"));
            DS_E_INTERNAL
        }
    }
}

/// Wraps an export body in [`guard`].
macro_rules! ffi_entry {
    ($body:expr) => {
        $crate::status::guard(|| $body)
    };
}
pub(crate) use ffi_entry;
