//! Helpers at the ABI boundary: raw pointers become checked slices and out-parameters, strings
//! convert, and the two-call buffer pattern lives here.
#![allow(unsafe_code)]

use std::path::PathBuf;

use crate::status::{DS_E_BUFFER_TOO_SMALL, DS_OK, DsStatus, Fail, Res};
use crate::types::{DsBuf, DsStr8, DsStr16, DsStr16List};

/// A caller-provided array. A null pointer is only accepted with a length of 0.
///
/// # Safety
/// `ptr` must be valid for `len` reads of `T` for the duration of the call (the ABI contract).
pub unsafe fn slice<'a, T>(ptr: *const T, len: usize) -> Res<&'a [T]> {
    if len == 0 {
        return Ok(&[]);
    }
    if ptr.is_null() {
        return Err(Fail::invalid("a null pointer with a non-zero length"));
    }
    // SAFETY: non-null, and the caller guarantees `len` valid elements (checked above for null).
    Ok(unsafe { std::slice::from_raw_parts(ptr, len) })
}

/// An out-parameter. Null is an error.
///
/// # Safety
/// `ptr` must be valid for a write of `T` for the duration of the call.
pub unsafe fn out<'a, T>(ptr: *mut T) -> Res<&'a mut T> {
    // SAFETY: `as_mut` checks for null; validity is the caller's contract.
    unsafe { ptr.as_mut() }.ok_or_else(|| Fail::invalid("a required out-parameter is null"))
}

/// A UTF-8 JSON argument; empty means "defaults".
///
/// # Safety
/// `s.ptr` must be valid for `s.len` bytes.
pub unsafe fn json_arg<'a>(s: DsStr8) -> Res<&'a str> {
    // SAFETY: forwarded contract.
    let bytes = unsafe { slice(s.ptr, s.len) }?;
    std::str::from_utf8(bytes).map_err(|_| Fail::invalid("the JSON argument is not valid UTF-8"))
}

/// A UTF-16 argument as code units.
///
/// # Safety
/// `s.ptr` must be valid for `s.len` units.
pub unsafe fn units<'a>(s: DsStr16) -> Res<&'a [u16]> {
    // SAFETY: forwarded contract.
    unsafe { slice(s.ptr, s.len) }
}

/// A UTF-16 list.
///
/// # Safety
/// `l.items` must be valid for `l.count` entries, each a valid `DsStr16`.
pub unsafe fn unit_list(l: DsStr16List) -> Res<Vec<Vec<u16>>> {
    // SAFETY: forwarded contract.
    let items = unsafe { slice(l.items, l.count) }?;
    items
        .iter()
        // SAFETY: each entry is a valid view per the contract.
        .map(|s| unsafe { units(*s) }.map(<[u16]>::to_vec))
        .collect()
}

/// An OS path from UTF-16 (lossless on Windows; lossy elsewhere).
pub fn os_path(u: &[u16]) -> PathBuf {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt;
        PathBuf::from(std::ffi::OsString::from_wide(u))
    }
    #[cfg(not(windows))]
    {
        PathBuf::from(String::from_utf16_lossy(u))
    }
}

pub fn string16(u: &[u16]) -> String {
    String::from_utf16_lossy(u)
}

/// The two-call pattern: copies `min(src.len(), cap)` items into `buf`, always reports the full
/// length in `*needed`, and returns `DS_E_BUFFER_TOO_SMALL` when it did not all fit.
///
/// # Safety
/// `buf` must be valid for `cap` writes (or null with `cap == 0`); `needed` valid for a write.
pub unsafe fn write_buffer<T: Copy>(
    src: &[T],
    buf: *mut T,
    cap: usize,
    needed: *mut u32,
) -> Res<DsStatus> {
    // SAFETY: forwarded contract.
    let needed = unsafe { out(needed) }?;
    *needed = u32::try_from(src.len()).map_err(|_| Fail::invalid("too many items"))?;
    let n = src.len().min(cap);
    if n > 0 {
        if buf.is_null() {
            return Err(Fail::invalid("a null buffer with a non-zero capacity"));
        }
        // SAFETY: `buf` is valid for `cap >= n` writes and does not overlap `src`.
        unsafe { std::ptr::copy_nonoverlapping(src.as_ptr(), buf, n) };
    }
    Ok(if src.len() > cap {
        DS_E_BUFFER_TOO_SMALL
    } else {
        DS_OK
    })
}

/// Hands a byte vector to the caller as a `DsBuf` (free with `ds_buf_free`).
pub fn to_buf(v: Vec<u8>) -> DsBuf {
    let boxed = v.into_boxed_slice();
    let len = boxed.len();
    DsBuf {
        ptr: Box::into_raw(boxed).cast::<u8>(),
        len,
    }
}

/// Frees a buffer from [`to_buf`].
///
/// # Safety
/// `b` must come from `to_buf` and not have been freed before.
pub unsafe fn free_buf(b: DsBuf) {
    if b.ptr.is_null() {
        return;
    }
    // SAFETY: `to_buf` leaked a `Box<[u8]>` of exactly `len` bytes at `ptr`.
    drop(unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(b.ptr, b.len)) });
}

/// Writes a status-returning copy result: `Ok(code)` is the status to return.
pub fn finish(r: Res<DsStatus>) -> Res {
    match r? {
        DS_OK => Ok(()),
        s => Err(Fail::new(
            s,
            "the buffer is too small; the needed size was written",
        )),
    }
}
