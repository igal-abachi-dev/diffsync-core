//! Error type shared by every crate (plan.md §3).
//!
//! One `Error` carries a machine-readable [`ErrorKind`], the raw OS error code and the path
//! when there is one, and a message. `Display` always includes the path and the OS message
//! so a log line is useful on its own.

use std::fmt;
use std::path::{Path, PathBuf};

/// What went wrong, coarsely. The numbering is not part of any ABI here; `core-ffi` maps
/// each kind to its own status code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorKind {
    Cancelled,
    NotFound,
    AccessDenied,
    SharingViolation,
    AlreadyExists,
    DiskFull,
    PathTooLong,
    /// A destination or source changed after the compare (plan.md §9.1).
    ChangedSinceCompare,
    InvalidArgument,
    /// Text could not be decoded or encoded.
    Encoding,
    Unsupported,
    Io,
    Internal,
    /// A name the target cannot hold (plan.md §9.8); added last so no number moves.
    InvalidName,
}

impl ErrorKind {
    /// Short lowercase label used by `Display`.
    pub const fn label(self) -> &'static str {
        match self {
            ErrorKind::Cancelled => "cancelled",
            ErrorKind::NotFound => "not found",
            ErrorKind::AccessDenied => "access denied",
            ErrorKind::SharingViolation => "sharing violation",
            ErrorKind::AlreadyExists => "already exists",
            ErrorKind::DiskFull => "disk full",
            ErrorKind::PathTooLong => "path too long",
            ErrorKind::ChangedSinceCompare => "changed since compare",
            ErrorKind::InvalidArgument => "invalid argument",
            ErrorKind::Encoding => "encoding error",
            ErrorKind::Unsupported => "unsupported",
            ErrorKind::Io => "i/o error",
            ErrorKind::Internal => "internal error",
            ErrorKind::InvalidName => "invalid name",
        }
    }

    /// Maps a Win32 error code (`GetLastError`) to a kind. The table is shared with
    /// `core-vfs`; codes that have no better home map to [`ErrorKind::Io`].
    pub const fn from_win32(code: u32) -> ErrorKind {
        match code {
            // ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, ERROR_INVALID_DRIVE,
            // ERROR_BAD_NETPATH, ERROR_BAD_NET_NAME
            2 | 3 | 15 | 53 | 67 => ErrorKind::NotFound,
            // ERROR_ACCESS_DENIED, ERROR_WRITE_PROTECT, ERROR_NETWORK_ACCESS_DENIED,
            // ERROR_PRIVILEGE_NOT_HELD
            5 | 19 | 65 | 1314 => ErrorKind::AccessDenied,
            // ERROR_SHARING_VIOLATION, ERROR_LOCK_VIOLATION
            32 | 33 => ErrorKind::SharingViolation,
            // ERROR_FILE_EXISTS, ERROR_ALREADY_EXISTS
            80 | 183 => ErrorKind::AlreadyExists,
            // ERROR_HANDLE_DISK_FULL, ERROR_DISK_FULL, ERROR_DISK_QUOTA_EXCEEDED
            39 | 112 | 1295 => ErrorKind::DiskFull,
            // ERROR_BUFFER_OVERFLOW, ERROR_FILENAME_EXCED_RANGE
            111 | 206 => ErrorKind::PathTooLong,
            // ERROR_INVALID_PARAMETER, ERROR_INVALID_NAME, ERROR_BAD_PATHNAME
            87 | 123 | 161 => ErrorKind::InvalidArgument,
            // ERROR_NOT_SUPPORTED, ERROR_CALL_NOT_IMPLEMENTED
            50 | 120 => ErrorKind::Unsupported,
            // ERROR_OPERATION_ABORTED, ERROR_CANCELLED, ERROR_REQUEST_ABORTED
            995 | 1223 | 1235 => ErrorKind::Cancelled,
            _ => ErrorKind::Io,
        }
    }
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// The error type of every fallible function in the workspace.
#[derive(Debug, Clone)]
pub struct Error {
    kind: ErrorKind,
    os_code: Option<u32>,
    path: Option<PathBuf>,
    msg: String,
}

/// `Result` with [`Error`] as the default error type.
pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    pub fn new(kind: ErrorKind, msg: impl Into<String>) -> Self {
        Error {
            kind,
            os_code: None,
            path: None,
            msg: msg.into(),
        }
    }

    /// `Cancelled` with the standard message.
    pub fn cancelled() -> Self {
        Error::new(ErrorKind::Cancelled, "operation cancelled")
    }

    pub fn invalid_argument(msg: impl Into<String>) -> Self {
        Error::new(ErrorKind::InvalidArgument, msg)
    }

    pub fn internal(msg: impl Into<String>) -> Self {
        Error::new(ErrorKind::Internal, msg)
    }

    /// Builds an error from a Win32 code; the kind comes from [`ErrorKind::from_win32`].
    pub fn from_win32(code: u32, msg: impl Into<String>) -> Self {
        Error {
            kind: ErrorKind::from_win32(code),
            os_code: Some(code),
            path: None,
            msg: msg.into(),
        }
    }

    #[must_use]
    pub fn with_path(mut self, path: impl AsRef<Path>) -> Self {
        self.path = Some(path.as_ref().to_path_buf());
        self
    }

    #[must_use]
    pub fn with_os_code(mut self, code: u32) -> Self {
        self.os_code = Some(code);
        self
    }

    #[must_use]
    pub fn with_kind(mut self, kind: ErrorKind) -> Self {
        self.kind = kind;
        self
    }

    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    pub fn os_code(&self) -> Option<u32> {
        self.os_code
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn message(&self) -> &str {
        &self.msg
    }

    pub fn is_cancelled(&self) -> bool {
        self.kind == ErrorKind::Cancelled
    }

    /// Wraps an `std::io::Error` and attaches the path it happened on. The OS code is kept
    /// and, on Windows, drives the kind through the Win32 table.
    pub fn from_io(err: &std::io::Error, path: impl AsRef<Path>) -> Self {
        Error::from(err).with_path(path)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.kind)?;
        if !self.msg.is_empty() {
            write!(f, ": {}", self.msg)?;
        }
        if let Some(p) = &self.path {
            write!(f, " ({})", p.display())?;
        }
        if let Some(code) = self.os_code {
            // The OS text (e.g. "The system cannot find the file specified. (os error 2)").
            write!(f, " [{}]", std::io::Error::from_raw_os_error(code as i32))?;
        }
        Ok(())
    }
}

impl std::error::Error for Error {}

impl From<&std::io::Error> for Error {
    fn from(e: &std::io::Error) -> Self {
        let code = e.raw_os_error().map(|c| c as u32);
        let kind = match code {
            #[cfg(windows)]
            Some(c) => ErrorKind::from_win32(c),
            _ => kind_from_io(e.kind()),
        };
        // The OS text is added by `Display` from the code; keep the message free of it. For
        // errors without a code (synthetic ones) the message is the whole text.
        let msg = if code.is_some() {
            String::new()
        } else {
            e.to_string()
        };
        Error {
            kind,
            os_code: code,
            path: None,
            msg,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::from(&e)
    }
}

fn kind_from_io(k: std::io::ErrorKind) -> ErrorKind {
    use std::io::ErrorKind as K;
    match k {
        K::NotFound => ErrorKind::NotFound,
        K::PermissionDenied => ErrorKind::AccessDenied,
        K::AlreadyExists => ErrorKind::AlreadyExists,
        K::InvalidInput | K::InvalidData => ErrorKind::InvalidArgument,
        K::Unsupported => ErrorKind::Unsupported,
        K::StorageFull | K::QuotaExceeded => ErrorKind::DiskFull,
        K::Interrupted => ErrorKind::Cancelled,
        _ => ErrorKind::Io,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn win32_table_covers_each_kind() {
        let cases: &[(u32, ErrorKind)] = &[
            (2, ErrorKind::NotFound),
            (3, ErrorKind::NotFound),
            (5, ErrorKind::AccessDenied),
            (19, ErrorKind::AccessDenied),
            (32, ErrorKind::SharingViolation),
            (33, ErrorKind::SharingViolation),
            (80, ErrorKind::AlreadyExists),
            (183, ErrorKind::AlreadyExists),
            (39, ErrorKind::DiskFull),
            (112, ErrorKind::DiskFull),
            (111, ErrorKind::PathTooLong),
            (206, ErrorKind::PathTooLong),
            (87, ErrorKind::InvalidArgument),
            (123, ErrorKind::InvalidArgument),
            (50, ErrorKind::Unsupported),
            (995, ErrorKind::Cancelled),
            (1223, ErrorKind::Cancelled),
            (31, ErrorKind::Io), // ERROR_GEN_FAILURE: unknown codes fall back to Io
            (0, ErrorKind::Io),
        ];
        for &(code, kind) in cases {
            assert_eq!(ErrorKind::from_win32(code), kind, "win32 code {code}");
        }
    }

    #[test]
    fn display_includes_path_and_os_message() {
        let e = Error::from_win32(2, "open failed").with_path("C:\\x\\y.txt");
        let s = e.to_string();
        assert!(s.starts_with("not found: open failed"), "{s}");
        assert!(s.contains("C:\\x\\y.txt"), "{s}");
        // The OS text is appended from the code; exact wording is platform specific, but
        // the numeric code is always present in std's rendering.
        assert!(s.contains("os error 2"), "{s}");
        assert_eq!(e.os_code(), Some(2));
        assert_eq!(e.kind(), ErrorKind::NotFound);
    }

    #[test]
    fn display_without_path_or_code() {
        let e = Error::new(ErrorKind::Encoding, "bad bytes");
        assert_eq!(e.to_string(), "encoding error: bad bytes");
        assert!(e.path().is_none());
        assert!(e.os_code().is_none());
    }

    #[test]
    fn cancelled_helper_and_predicates() {
        let e = Error::cancelled();
        assert!(e.is_cancelled());
        assert_eq!(e.kind(), ErrorKind::Cancelled);
        assert!(!Error::internal("x").is_cancelled());
    }

    #[test]
    fn io_error_conversion_keeps_code_and_path() {
        let io = std::io::Error::from_raw_os_error(2);
        let e = Error::from_io(&io, "/some/file");
        assert_eq!(e.kind(), ErrorKind::NotFound);
        assert_eq!(e.os_code(), Some(2));
        assert_eq!(e.path(), Some(Path::new("/some/file")));
    }

    #[test]
    fn io_kind_fallback_without_code() {
        let io = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "nope");
        let e = Error::from(io);
        assert_eq!(e.kind(), ErrorKind::AccessDenied);
        assert!(e.os_code().is_none());
        assert!(e.to_string().contains("nope"));
    }

    #[test]
    fn builders_override_kind() {
        let e = Error::new(ErrorKind::Io, "x").with_kind(ErrorKind::ChangedSinceCompare);
        assert_eq!(e.kind(), ErrorKind::ChangedSinceCompare);
    }
}
