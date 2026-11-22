//! `LocalWin32`: the product backend on Windows (plan.md §6.3, backlog E5-S3..S8).
//!
//! * every call takes a `\\?\` wide path built by `VfsRoot::to_long_wide` (long paths, names
//!   with trailing dots or spaces and reserved names work unchanged);
//! * listing uses `FindFirstFileExW(FindExInfoBasic, LARGE_FETCH)`: size, times, attributes and
//!   the reparse tag come from the listing, no per-entry stat;
//! * only SYMLINK, MOUNT_POINT and LX_SYMLINK reparse points are links (D16); cloud, dedup and
//!   other reparse points are regular entries, and offline placeholders are flagged (D17);
//! * copy uses `CopyFileExW` with a progress routine (cancel through `PROGRESS_CANCEL`) into a
//!   temp sibling, then `MoveFileExW(REPLACE_EXISTING | WRITE_THROUGH)` (fix B4);
//! * delete never traverses a reparse point: a junction is removed itself.
//!
//! This is the only module of `core-vfs` that contains `unsafe`.
#![cfg(windows)]
#![allow(unsafe_code)]

use std::ffi::c_void;
use std::io::Write;
use std::os::windows::ffi::OsStringExt;

use core_base::ticks::ticks_from_filetime;
use core_base::{CancelToken, Error, ErrorKind, Result, Ticks};
use windows_sys::Win32::Foundation::{
    CloseHandle, FILETIME, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CopyFileExW, CreateDirectoryW, CreateFileW, DeleteFileW, FILE_ATTRIBUTE_DIRECTORY,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FIND_FIRST_EX_LARGE_FETCH, FindClose,
    FindExInfoBasic, FindExSearchNameMatch, FindFirstFileExW, FindNextFileW, GetFileAttributesW,
    GetFileInformationByHandleEx, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    OPEN_EXISTING, RemoveDirectoryW, SetFileAttributesW, SetFileTime, WIN32_FIND_DATAW,
};

use crate::copy::{check_destination_unchanged, rename_with_retry, temp_sibling_name, verify_crc};
use crate::path::{Name, VPath};
use crate::root::VfsRoot;
use crate::vfs::{CopyOptions, DeleteMode, Entry, Kind, Progress, ReadSeek, Vfs, VfsCaps, attr};

const IO_REPARSE_TAG_SYMLINK: u32 = 0xA000_000C;
const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;
const IO_REPARSE_TAG_LX_SYMLINK: u32 = 0xA000_001D;
const GENERIC_READ_ATTRS: u32 = 0x80; // FILE_READ_ATTRIBUTES
const FILE_WRITE_ATTRIBUTES: u32 = 0x100;
const FILE_ID_INFO_CLASS: i32 = 18;
const FILE_CASE_SENSITIVE_INFO_CLASS: i32 = 23;
const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;
const COPY_FILE_FAIL_IF_EXISTS: u32 = 1;
const COPY_FILE_NO_BUFFERING: u32 = 0x1000;

/// Files of at least this many bytes are copied with `COPY_FILE_NO_BUFFERING` (the system cache
/// is bypassed). `u64::MAX` = never; the shipped value comes from the robocopy benchmark
/// (backlog E5-S6.5, decision D38).
static NO_BUFFERING_THRESHOLD: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(u64::MAX);

/// Sets the `COPY_FILE_NO_BUFFERING` threshold in bytes (process-wide; for the benchmark).
pub fn set_no_buffering_threshold(bytes: u64) {
    NO_BUFFERING_THRESHOLD.store(bytes, std::sync::atomic::Ordering::Relaxed);
}
const PROGRESS_CONTINUE: u32 = 0;
const PROGRESS_CANCEL: u32 = 1;

/// The Windows backend, rooted at a parsed `VfsRoot`.
#[derive(Debug, Clone)]
pub struct LocalWin32 {
    root: VfsRoot,
    case_sensitive_volume: bool,
    /// The volume is a rotating disk (decision: D54).
    seek_penalty: bool,
}

/// Free and total space of the volume that holds `path` (backlog E11.5-S9.3: the status bar of a
/// folder compare shows "1.5 TB free on D:"). `free` is what the calling user may use (quotas
/// count), `total` the size of the volume.
pub fn volume_space(path: &std::path::Path) -> Result<(u64, u64)> {
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let mut wide: Vec<u16> =
        std::os::windows::ffi::OsStrExt::encode_wide(path.as_os_str()).collect();
    wide.push(0);
    let (mut free, mut total, mut total_free) = (0u64, 0u64, 0u64);
    // SAFETY: `wide` is NUL-terminated; the three out pointers are valid for a write.
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &raw mut free,
            &raw mut total,
            &raw mut total_free,
        )
    };
    if ok == 0 {
        return Err(
            last_error("GetDiskFreeSpaceExW", &VPath::root()).with_path(path.display().to_string())
        );
    }
    Ok((free, total))
}

/// decision: D54: whether the drive `letter` is a rotating disk, asked the way the OS defrag tool
/// asks: `IOCTL_STORAGE_QUERY_PROPERTY` with `StorageDeviceSeekPenaltyProperty`. Opening the
/// volume with no access rights needs no administrator. Anything that goes wrong (a mapped
/// network drive, a RAM disk, an unusual driver) is "no penalty known": readers overlap, which is
/// right for SSDs and network shares.
fn drive_has_seek_penalty(letter: char) -> bool {
    use windows_sys::Win32::System::IO::DeviceIoControl;
    use windows_sys::Win32::System::Ioctl::{
        DEVICE_SEEK_PENALTY_DESCRIPTOR, IOCTL_STORAGE_QUERY_PROPERTY, PropertyStandardQuery,
        STORAGE_PROPERTY_QUERY, StorageDeviceSeekPenaltyProperty,
    };
    let bs = '\u{5c}'; // a backslash
    // the device path of a volume: backslash, backslash, dot, backslash, `X:`
    let device = [bs, bs, '.', bs, letter, ':'].iter().collect::<String>();
    let path: Vec<u16> = device.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: `path` is NUL-terminated; no security attributes or template are passed.
    let h = unsafe {
        CreateFileW(
            path.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return false;
    }
    let query = STORAGE_PROPERTY_QUERY {
        PropertyId: StorageDeviceSeekPenaltyProperty,
        QueryType: PropertyStandardQuery,
        AdditionalParameters: [0],
    };
    // SAFETY: an all-zero descriptor is a valid value (integers and a BOOLEAN).
    let mut out: DEVICE_SEEK_PENALTY_DESCRIPTOR = unsafe { std::mem::zeroed() };
    let mut returned = 0u32;
    // SAFETY: `h` is a valid handle; the input and output buffers are valid for the sizes passed.
    let ok = unsafe {
        DeviceIoControl(
            h,
            IOCTL_STORAGE_QUERY_PROPERTY,
            std::ptr::addr_of!(query).cast::<c_void>(),
            size_of::<STORAGE_PROPERTY_QUERY>() as u32,
            std::ptr::addr_of_mut!(out).cast::<c_void>(),
            size_of::<DEVICE_SEEK_PENALTY_DESCRIPTOR>() as u32,
            &raw mut returned,
            std::ptr::null_mut(),
        )
    };
    // SAFETY: `h` is valid and closed exactly once.
    unsafe { CloseHandle(h) };
    ok != 0 && out.IncursSeekPenalty
}

#[repr(C)]
struct FileIdInfo {
    volume_serial: u64,
    file_id: [u8; 16],
}

#[repr(C)]
struct FileCaseSensitiveInfo {
    flags: u32,
}

fn last_error(what: &str, p: &VPath) -> Error {
    // SAFETY: GetLastError has no preconditions; it reads the calling thread's error slot.
    let code = unsafe { GetLastError() };
    Error::from_win32(code, what.to_string()).with_path(p.display())
}

fn units_to_os(units: &[u16]) -> std::ffi::OsString {
    std::ffi::OsString::from_wide(units)
}

impl LocalWin32 {
    /// A backend for the folder named by `root` (the root must be a Windows form).
    pub fn new(root: VfsRoot) -> Self {
        // `FILE_CASE_SENSITIVE_SEARCH` of the volume only says NTFS *supports* case-sensitive
        // names (it is always set), so the real answer is the per-directory flag of the root
        // folder (`FileCaseSensitiveInfo`); assume the NTFS default until it is read.
        let mut me = LocalWin32 {
            seek_penalty: match root.kind() {
                crate::root::RootKind::Drive(c) => drive_has_seek_penalty(*c),
                _ => false, // a share, a verbatim path: no seek-penalty data
            },
            root,
            case_sensitive_volume: false,
        };
        me.case_sensitive_volume = me.dir_case_sensitive(&VPath::root()).unwrap_or(false);
        me
    }

    fn full(&self, p: &VPath) -> VPath {
        let mut v = self.root.vpath();
        for c in p.components() {
            v = v.join(c.clone());
        }
        v
    }

    /// NUL-terminated `\\?\` path of `p`.
    fn wide(&self, p: &VPath) -> Vec<u16> {
        self.root.to_long_wide(&self.full(p))
    }

    fn os(&self, p: &VPath) -> std::ffi::OsString {
        let w = self.wide(p);
        units_to_os(&w[..w.len() - 1])
    }

    fn handle(&self, p: &VPath, access: u32, flags: u32) -> Result<HANDLE> {
        let w = self.wide(p);
        // SAFETY: `w` is a valid NUL-terminated wide string that outlives the call; the other
        // arguments are plain flags and null pointers allowed by CreateFileW.
        let h = unsafe {
            CreateFileW(
                w.as_ptr(),
                access,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | flags,
                std::ptr::null_mut(),
            )
        };
        if h == INVALID_HANDLE_VALUE {
            Err(last_error("cannot open", p))
        } else {
            Ok(h)
        }
    }

    fn find_entry(data: &WIN32_FIND_DATAW) -> Entry {
        let len = data
            .cFileName
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(data.cFileName.len());
        let a = data.dwFileAttributes;
        let is_link = a & FILE_ATTRIBUTE_REPARSE_POINT != 0
            && matches!(
                data.dwReserved0,
                IO_REPARSE_TAG_SYMLINK | IO_REPARSE_TAG_MOUNT_POINT | IO_REPARSE_TAG_LX_SYMLINK
            );
        let kind = if is_link {
            Kind::Link
        } else if a & FILE_ATTRIBUTE_DIRECTORY != 0 {
            Kind::Dir
        } else {
            Kind::File
        };
        let ft = |t: FILETIME| ticks_from_filetime(t.dwLowDateTime, t.dwHighDateTime);
        Entry {
            name: Name::from_units(&data.cFileName[..len]),
            kind,
            size: if kind == Kind::File {
                (u64::from(data.nFileSizeHigh) << 32) | u64::from(data.nFileSizeLow)
            } else {
                0
            },
            mtime: ft(data.ftLastWriteTime),
            ctime: ft(data.ftCreationTime),
            attrs: a,
            link_target_kind: None,
            file_id: None,
        }
    }

    /// Opens a find handle for `wide` (NUL-terminated, pattern or exact path).
    fn find_first(wide: &[u16]) -> std::result::Result<(HANDLE, WIN32_FIND_DATAW), u32> {
        // SAFETY: WIN32_FIND_DATAW is plain data; all-zero is a valid initial value.
        let mut data: WIN32_FIND_DATAW = unsafe { std::mem::zeroed() };
        // SAFETY: `wide` is NUL-terminated and valid for the call; `data` is a writable
        // WIN32_FIND_DATAW as required by FindExInfoBasic; the filter pointer may be null.
        let h = unsafe {
            FindFirstFileExW(
                wide.as_ptr(),
                FindExInfoBasic,
                std::ptr::addr_of_mut!(data).cast::<c_void>(),
                FindExSearchNameMatch,
                std::ptr::null(),
                FIND_FIRST_EX_LARGE_FETCH,
            )
        };
        if h == INVALID_HANDLE_VALUE {
            // SAFETY: no preconditions.
            Err(unsafe { GetLastError() })
        } else {
            Ok((h, data))
        }
    }
}

fn filetime_of(t: Ticks) -> FILETIME {
    let (lo, hi) = core_base::ticks::ticks_to_filetime(t);
    FILETIME {
        dwLowDateTime: lo,
        dwHighDateTime: hi,
    }
}

/// State passed to the CopyFileExW progress routine.
struct CopyCtx<'a> {
    prog: Progress<'a>,
    cancel: &'a CancelToken,
}

unsafe extern "system" fn copy_progress(
    total: i64,
    done: i64,
    _stream_total: i64,
    _stream_done: i64,
    _stream: u32,
    _reason: u32,
    _src: HANDLE,
    _dst: HANDLE,
    data: *const c_void,
) -> u32 {
    // SAFETY: `data` is the `&CopyCtx` that `copy_file` passed to CopyFileExW and keeps alive
    // for the whole call; it is only read.
    let ctx = unsafe { &*data.cast::<CopyCtx<'_>>() };
    if ctx.cancel.is_cancelled() {
        return PROGRESS_CANCEL;
    }
    (ctx.prog)(done.max(0) as u64, total.max(0) as u64);
    PROGRESS_CONTINUE
}

impl Vfs for LocalWin32 {
    fn caps(&self) -> VfsCaps {
        VfsCaps {
            case_sensitive: self.case_sensitive_volume,
            time_resolution: 1,
            has_attributes: true,
            has_recycle_bin: true,
            native_copy: true,
            atomic_rename: true,
            max_name_units: 255,
            windows_names: true,
            seek_penalty: self.seek_penalty,
        }
    }

    fn stat(&self, p: &VPath) -> Result<Option<Entry>> {
        if p.is_root() && self.root.parts().is_empty() {
            let w = self.wide(p);
            // SAFETY: `w` is NUL-terminated and valid for the call.
            let a = unsafe { GetFileAttributesW(w.as_ptr()) };
            return Ok((a != u32::MAX).then(|| Entry {
                attrs: a,
                ..Entry::dir("", 0)
            }));
        }
        // FindFirstFileExW treats `* ? < > "` as wildcards; no file can carry them, and a
        // wildcard must never answer for "the first match" (reviewer finding, D37)
        if p.file_name().is_some_and(|n| {
            n.units()
                .iter()
                .any(|&u| u < 0x80 && b"*?<>\"".contains(&(u as u8)))
        }) {
            return Ok(None);
        }
        match Self::find_first(&self.wide(p)) {
            Ok((h, data)) => {
                // SAFETY: `h` is the valid find handle returned above.
                unsafe { FindClose(h) };
                let mut e = Self::find_entry(&data);
                // the find data reports the name as the caller wrote it; keep the VPath's
                if let Some(n) = p.file_name() {
                    e.name = n.clone();
                }
                Ok(Some(e))
            }
            Err(2 | 3) => Ok(None),
            Err(code) => Err(Error::from_win32(code, "cannot stat").with_path(p.display())),
        }
    }

    fn list(&self, dir: &VPath, out: &mut Vec<Entry>) -> Result<()> {
        let mut pattern = self.wide(dir);
        pattern.pop(); // NUL
        if pattern.last() != Some(&u16::from(b'\\')) {
            pattern.push(u16::from(b'\\'));
        }
        pattern.extend([u16::from(b'*'), 0]);
        let (h, mut data) = match Self::find_first(&pattern) {
            Ok(v) => v,
            Err(code) => {
                return Err(Error::from_win32(code, "cannot list").with_path(dir.display()));
            }
        };
        loop {
            let e = Self::find_entry(&data);
            let u = e.name.units();
            if u != [u16::from(b'.')] && u != [u16::from(b'.'), u16::from(b'.')] {
                out.push(e);
            }
            // SAFETY: `h` is a valid find handle and `data` a writable WIN32_FIND_DATAW.
            if unsafe { FindNextFileW(h, std::ptr::addr_of_mut!(data)) } == 0 {
                // SAFETY: no preconditions.
                let code = unsafe { GetLastError() };
                // SAFETY: `h` is still valid and is closed exactly once here.
                unsafe { FindClose(h) };
                return if code == 18 {
                    Ok(())
                } else {
                    Err(Error::from_win32(code, "listing failed").with_path(dir.display()))
                };
            }
        }
    }

    fn open_read(&self, p: &VPath) -> Result<Box<dyn ReadSeek>> {
        let os = self.os(p);
        Ok(Box::new(
            std::fs::File::open(&os).map_err(|e| Error::from_io(&e, &os))?,
        ))
    }

    fn create_temp_sibling(&self, p: &VPath) -> Result<(VPath, Box<dyn Write + Send>)> {
        let name = p
            .file_name()
            .ok_or_else(|| Error::invalid_argument("the root has no siblings"))?;
        let tmp = p.parent().unwrap_or_default().join(temp_sibling_name(name));
        let os = self.os(&tmp);
        let f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&os)
            .map_err(|e| Error::from_io(&e, &os))?;
        Ok((tmp, Box::new(f)))
    }

    fn copy_file(
        &self,
        src: &VPath,
        dst: &VPath,
        o: &CopyOptions,
        prog: Progress<'_>,
        cancel: &CancelToken,
    ) -> Result<()> {
        cancel.check()?;
        let se = crate::vfs::stat_required(self, src)?;
        if se.kind != Kind::File {
            return Err(Error::new(
                ErrorKind::Unsupported,
                "only files are copied (links and folders are not)",
            )
            .with_path(src.display()));
        }
        let existing = self.stat(dst)?;
        if let Some(d) = &existing {
            if d.kind == Kind::Dir || !o.overwrite {
                return Err(Error::new(ErrorKind::AlreadyExists, "destination exists")
                    .with_path(dst.display()));
            }
        }
        let name = dst
            .file_name()
            .ok_or_else(|| Error::invalid_argument("bad destination"))?;
        let tmp = dst
            .parent()
            .unwrap_or_default()
            .join(temp_sibling_name(name));
        let (ws, wt) = (self.wide(src), self.wide(&tmp));
        let ctx = CopyCtx { prog, cancel };
        let mut cancel_flag = 0i32;
        // SAFETY: `ws`/`wt` are NUL-terminated and valid for the call; `copy_progress` matches
        // LPPROGRESS_ROUTINE; `ctx` and `cancel_flag` outlive the (synchronous) call and are only
        // accessed through the pointers passed here.
        let ok = unsafe {
            CopyFileExW(
                ws.as_ptr(),
                wt.as_ptr(),
                Some(copy_progress),
                std::ptr::addr_of!(ctx).cast::<c_void>(),
                std::ptr::addr_of_mut!(cancel_flag),
                COPY_FILE_FAIL_IF_EXISTS
                    | if se.size
                        >= NO_BUFFERING_THRESHOLD.load(std::sync::atomic::Ordering::Relaxed)
                    {
                        COPY_FILE_NO_BUFFERING
                    } else {
                        0
                    },
            )
        };
        let result = if ok == 0 {
            let e = last_error("copy failed", src);
            if cancel.is_cancelled() {
                Err(Error::cancelled())
            } else {
                Err(e)
            }
        } else {
            // decision: D28: the source must be what it was before the copy, or the destination
            // would carry new content with the old time
            match self.stat(src) {
                Ok(Some(now)) if (now.size, now.mtime) == (se.size, se.mtime) => {
                    self.finish_native(src, &tmp, dst, existing.as_ref(), o)
                }
                Ok(_) => Err(Error::new(
                    ErrorKind::ChangedSinceCompare,
                    "the source changed while it was copied",
                )
                .with_path(src.display())),
                Err(e) => Err(e),
            }
        };
        // a temp that was never created by this call (the name was taken) is not ours to delete
        if result.is_err() && !matches!(&result, Err(e) if e.kind() == ErrorKind::AlreadyExists) {
            let _ = self.delete(&tmp, DeleteMode::Permanent);
        }
        result
    }

    fn sync_file(&self, p: &VPath) -> Result<()> {
        let os = self.os(p);
        // a read-only file (a copy of a read-only source) cannot be opened for write: make it
        // writable for the moment and put the attribute back
        let mut restore = None;
        let f = match std::fs::OpenOptions::new().write(true).open(&os) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                let md = std::fs::metadata(&os).map_err(|e| Error::from_io(&e, &os))?;
                if !md.permissions().readonly() {
                    return Err(Error::from_io(&e, &os));
                }
                restore = Some(md.permissions());
                let mut w = md.permissions();
                #[allow(clippy::permissions_set_readonly_false)]
                // intended: a read-only file must be flushable
                w.set_readonly(false);
                std::fs::set_permissions(&os, w).map_err(|e| Error::from_io(&e, &os))?;
                std::fs::OpenOptions::new()
                    .write(true)
                    .open(&os)
                    .map_err(|e| Error::from_io(&e, &os))?
            }
            Err(e) => return Err(Error::from_io(&e, &os)),
        };
        let r = f.sync_all().map_err(|e| Error::from_io(&e, &os)); // FlushFileBuffers
        drop(f);
        if let Some(perm) = restore {
            let _ = std::fs::set_permissions(&os, perm);
        }
        r
    }

    fn remove_dir_if_empty(&self, p: &VPath) -> Result<bool> {
        if p.is_root() {
            return Err(Error::invalid_argument("cannot delete the root"));
        }
        let e = self.stat(p)?.ok_or_else(|| {
            Error::new(ErrorKind::NotFound, "no such folder").with_path(p.display())
        })?;
        if e.kind != Kind::Dir {
            return Err(Error::invalid_argument("not a folder").with_path(p.display()));
        }
        let w = self.wide(p);
        // SAFETY: `w` is NUL-terminated and valid for the call. RemoveDirectory fails with
        // ERROR_DIR_NOT_EMPTY when anything is inside, and never removes more than the folder.
        let mut ok = unsafe { RemoveDirectoryW(w.as_ptr()) };
        if ok == 0 && e.attrs & attr::READONLY != 0 {
            // SAFETY: as above.
            unsafe { SetFileAttributesW(w.as_ptr(), FILE_ATTRIBUTE_NORMAL) };
            // SAFETY: as above.
            ok = unsafe { RemoveDirectoryW(w.as_ptr()) };
        }
        if ok != 0 {
            return Ok(true);
        }
        // SAFETY: GetLastError has no preconditions.
        let code = unsafe { GetLastError() };
        if code == 145 {
            return Ok(false); // ERROR_DIR_NOT_EMPTY
        }
        Err(last_error("delete failed", p))
    }

    fn rename(&self, from: &VPath, to: &VPath, replace: bool) -> Result<()> {
        let (wf, wt) = (self.wide(from), self.wide(to));
        let flags = MOVEFILE_WRITE_THROUGH
            | if replace {
                MOVEFILE_REPLACE_EXISTING
            } else {
                0
            };
        // SAFETY: both strings are NUL-terminated and valid for the call.
        if unsafe { MoveFileExW(wf.as_ptr(), wt.as_ptr(), flags) } == 0 {
            Err(last_error("rename failed", to))
        } else {
            Ok(())
        }
    }

    fn delete(&self, p: &VPath, mode: DeleteMode) -> Result<()> {
        if mode == DeleteMode::RecycleBin {
            return self.recycle(p);
        }
        if p.is_root() {
            return Err(Error::invalid_argument("cannot delete the root"));
        }
        // iterative; a link is removed itself and never traversed
        let mut stack = vec![(p.clone(), None::<Entry>, false)];
        while let Some((cur, entry, visited)) = stack.pop() {
            let e = match entry {
                Some(e) => e,
                None => self.stat(&cur)?.ok_or_else(|| {
                    Error::new(ErrorKind::NotFound, "no such file or folder")
                        .with_path(cur.display())
                })?,
            };
            let w = self.wide(&cur);
            match e.kind {
                Kind::Dir if !visited => {
                    stack.push((cur.clone(), Some(e), true));
                    let mut children = Vec::new();
                    self.list(&cur, &mut children)?;
                    stack.extend(
                        children
                            .into_iter()
                            .map(|c| (cur.join(c.name.clone()), Some(c), false)),
                    );
                }
                Kind::Dir | Kind::Link => {
                    // a (dir-)link is removed with RemoveDirectory, a file symlink with DeleteFile
                    if e.attrs & attr::READONLY != 0 {
                        // SAFETY: `w` is NUL-terminated and valid for the call.
                        unsafe { SetFileAttributesW(w.as_ptr(), FILE_ATTRIBUTE_NORMAL) };
                    }
                    let is_dirlike = e.kind == Kind::Dir || e.attrs & FILE_ATTRIBUTE_DIRECTORY != 0;
                    // SAFETY: as above.
                    let ok = unsafe {
                        if is_dirlike {
                            RemoveDirectoryW(w.as_ptr())
                        } else {
                            DeleteFileW(w.as_ptr())
                        }
                    };
                    if ok == 0 {
                        return Err(last_error("delete failed", &cur));
                    }
                }
                Kind::File => {
                    if e.attrs & attr::READONLY != 0 {
                        // SAFETY: `w` is NUL-terminated and valid for the call.
                        unsafe { SetFileAttributesW(w.as_ptr(), FILE_ATTRIBUTE_NORMAL) };
                    }
                    // SAFETY: as above.
                    if unsafe { DeleteFileW(w.as_ptr()) } == 0 {
                        return Err(last_error("delete failed", &cur));
                    }
                }
            }
        }
        Ok(())
    }

    fn create_dir(&self, p: &VPath) -> Result<()> {
        let w = self.wide(p);
        // SAFETY: `w` is NUL-terminated; the security attributes pointer may be null.
        if unsafe { CreateDirectoryW(w.as_ptr(), std::ptr::null()) } != 0 {
            return Ok(());
        }
        let e = last_error("cannot create folder", p);
        if e.os_code() == Some(183) && self.stat(p)?.is_some_and(|x| x.kind == Kind::Dir) {
            Ok(())
        } else {
            Err(e)
        }
    }

    fn set_times(&self, p: &VPath, mtime: Option<Ticks>, ctime: Option<Ticks>) -> Result<()> {
        let h = self.handle(p, FILE_WRITE_ATTRIBUTES, FILE_FLAG_OPEN_REPARSE_POINT)?;
        let (c, m) = (ctime.map(filetime_of), mtime.map(filetime_of));
        // SAFETY: `h` is a valid handle opened with FILE_WRITE_ATTRIBUTES; each time pointer is
        // either null or points to a FILETIME that lives across the call.
        let ok = unsafe {
            SetFileTime(
                h,
                c.as_ref().map_or(std::ptr::null(), std::ptr::from_ref),
                std::ptr::null(),
                m.as_ref().map_or(std::ptr::null(), std::ptr::from_ref),
            )
        };
        let err = (ok == 0).then(|| last_error("cannot set times", p));
        // SAFETY: `h` is valid and closed exactly once.
        unsafe { CloseHandle(h) };
        err.map_or(Ok(()), Err)
    }

    fn set_attrs(&self, p: &VPath, attrs: u32) -> Result<()> {
        let w = self.wide(p);
        // SAFETY: `w` is NUL-terminated and valid for the call.
        let cur = unsafe { GetFileAttributesW(w.as_ptr()) };
        if cur == u32::MAX {
            return Err(last_error("cannot read attributes", p));
        }
        let mask = attr::READONLY | attr::HIDDEN | attr::SYSTEM | attr::ARCHIVE;
        let new = (cur & !mask) | (attrs & mask);
        // SAFETY: as above.
        if unsafe {
            SetFileAttributesW(
                w.as_ptr(),
                if new == 0 { FILE_ATTRIBUTE_NORMAL } else { new },
            )
        } == 0
        {
            return Err(last_error("cannot set attributes", p));
        }
        Ok(())
    }

    fn file_id(&self, p: &VPath) -> Result<Option<(u64, u128)>> {
        let h = self.handle(p, GENERIC_READ_ATTRS, 0)?;
        let mut info = FileIdInfo {
            volume_serial: 0,
            file_id: [0; 16],
        };
        // SAFETY: `h` is a valid handle; `info` is a writable FILE_ID_INFO-shaped buffer of the
        // size passed.
        let ok = unsafe {
            GetFileInformationByHandleEx(
                h,
                FILE_ID_INFO_CLASS,
                std::ptr::addr_of_mut!(info).cast::<c_void>(),
                size_of::<FileIdInfo>() as u32,
            )
        };
        // SAFETY: `h` is valid and closed exactly once.
        unsafe { CloseHandle(h) };
        Ok((ok != 0).then(|| (info.volume_serial, u128::from_le_bytes(info.file_id))))
    }

    fn dir_case_sensitive(&self, dir: &VPath) -> Result<bool> {
        let h = self.handle(dir, GENERIC_READ_ATTRS, 0)?;
        let mut info = FileCaseSensitiveInfo { flags: 0 };
        // SAFETY: `h` is a valid handle; `info` is a writable buffer of the size passed.
        let ok = unsafe {
            GetFileInformationByHandleEx(
                h,
                FILE_CASE_SENSITIVE_INFO_CLASS,
                std::ptr::addr_of_mut!(info).cast::<c_void>(),
                size_of::<FileCaseSensitiveInfo>() as u32,
            )
        };
        // SAFETY: `h` is valid and closed exactly once.
        unsafe { CloseHandle(h) };
        Ok(if ok != 0 {
            info.flags & 1 != 0
        } else {
            self.case_sensitive_volume
        })
    }

    fn storage_id(&self) -> usize {
        // per root: a native copy resolves both paths against *this* backend's root, so two
        // different roots must never look like the same storage
        crate::vfs::hash_id(self.root.long_form().to_ascii_lowercase().as_bytes())
    }

    fn ancestor_ids(&self) -> Result<Vec<(u64, u128)>> {
        let mut out = Vec::new();
        let mut cur = self.root.parent();
        while let Some(r) = cur {
            let anc = LocalWin32::new(r.clone());
            match anc.file_id(&VPath::root())? {
                Some(id) => out.push(id),
                None => break,
            }
            cur = r.parent();
        }
        // the volume root itself
        let top = LocalWin32::new(VfsRoot::parse_windows(self.root.root_string())?);
        if let Some(id) = top.file_id(&VPath::root())? {
            if !out.contains(&id) {
                out.push(id);
            }
        }
        Ok(out)
    }
}

impl LocalWin32 {
    /// After a successful CopyFileExW into `tmp`: verify (fix B6), then rename with the
    /// attribute-clear retry (fix B5).
    fn finish_native(
        &self,
        src: &VPath,
        tmp: &VPath,
        dst: &VPath,
        existing: Option<&Entry>,
        o: &CopyOptions,
    ) -> Result<()> {
        let old_attrs = existing.map(|e| e.attrs);
        if o.verify {
            let mut r = self.open_read(src)?;
            let mut crc = core_base::crc32::FileCrc::new();
            let mut buf = vec![0u8; crate::copy::COPY_BLOCK];
            loop {
                let n = std::io::Read::read(&mut r, &mut buf)
                    .map_err(|e| Error::from_io(&e, src.display()))?;
                if n == 0 {
                    break;
                }
                crc.update(&buf[..n]);
            }
            verify_crc(self, tmp, crc.finalize())?;
        }
        if o.copy_ctime {
            if let Some(e) = self.stat(src)? {
                self.set_times(tmp, None, Some(e.ctime))?;
            }
        }
        check_destination_unchanged(self, dst, existing)?;
        if o.durable {
            self.sync_file(tmp)?;
        }
        rename_with_retry(self, tmp, dst, old_attrs, o)
    }

    /// Moves an item to the Recycle Bin (decision: D37). The shell deletes permanently what it
    /// cannot recycle, so every case it would do that for is refused first with `Unsupported`:
    /// paths beyond MAX_PATH, network and removable drives, links, names the shell would read
    /// differently (trailing dot or space, device names, wildcards), a volume whose bin is
    /// switched off (`NukeOnDelete`) or forbidden by policy, and items larger than the volume's
    /// bin limit (`MaxCapacity`, else 10 % of the volume). As a last backstop the call carries
    /// `FOF_WANTNUKEWARNING`: a permanent delete the checks missed is never silent, the shell
    /// asks the user. Runs on its own COM apartment thread.
    fn recycle(&self, p: &VPath) -> Result<()> {
        use windows_sys::Win32::Storage::FileSystem::{GetDiskFreeSpaceExW, GetDriveTypeW};
        let unsupported =
            |why: &str| Error::new(ErrorKind::Unsupported, why.to_string()).with_path(p.display());
        if p.is_root() {
            return Err(Error::invalid_argument("cannot delete the root"));
        }
        let e = self.stat(p)?.ok_or_else(|| {
            Error::new(ErrorKind::NotFound, "no such file or folder").with_path(p.display())
        })?;
        if e.kind == Kind::Link {
            return Err(unsupported("links are not sent to the Recycle Bin"));
        }
        // the shell re-parses a plain path (no `\\?\` literal mode), so it must read every name
        // exactly as listed
        for n in p.components() {
            let ok = n.to_string_exact().is_some_and(|s| {
                !s.ends_with(['.', ' '])
                    && crate::validate::validate_name(&self.caps(), &s)
                        == crate::validate::NameCheck::Ok
            });
            if !ok {
                return Err(unsupported(
                    "the shell would read this name differently (trailing dot or space, device name, wildcard)",
                ));
            }
        }
        let long = self.wide(p);
        let long = &long[..long.len() - 1];
        let prefix: Vec<u16> = r"\\?\".encode_utf16().collect();
        let plain: &[u16] = match long.strip_prefix(prefix.as_slice()) {
            Some(rest) if rest.get(1) == Some(&u16::from(b':')) => rest,
            _ => {
                return Err(unsupported(
                    "only local drive paths can use the Recycle Bin",
                ));
            }
        };
        // the shell API predates long paths
        if plain.len() >= 260 {
            return Err(unsupported("the path is too long for the Recycle Bin"));
        }
        let mut drive: Vec<u16> = plain[..2].to_vec();
        drive.extend(r"\".encode_utf16());
        drive.push(0);
        // SAFETY: `drive` is NUL-terminated and valid for the call.
        if unsafe { GetDriveTypeW(drive.as_ptr()) } != 3 {
            return Err(unsupported("only fixed drives have a Recycle Bin"));
        }
        let size = self.tree_size(p, &e)?;
        let mut total = 0u64;
        // SAFETY: `drive` is NUL-terminated; the out pointers are null or valid for writes.
        let ok = unsafe {
            GetDiskFreeSpaceExW(
                drive.as_ptr(),
                std::ptr::null_mut(),
                &raw mut total,
                std::ptr::null_mut(),
            )
        };
        let limit = match bin_limit(&drive) {
            Ok(max) => max.map_or(total / 10, |m| m.min(total / 10)),
            Err(why) => return Err(unsupported(why)),
        };
        if ok == 0 || size > limit {
            return Err(unsupported(
                "the item is too large for the Recycle Bin and would be deleted permanently",
            ));
        }
        let mut from = plain.to_vec();
        from.extend([0, 0]); // double-NUL terminated list
        let code = std::thread::spawn(move || shell_delete(&from))
            .join()
            .map_err(|_| Error::internal("the Recycle Bin thread panicked"))?;
        match code {
            Ok(()) => {}
            Err(c) => {
                return Err(
                    Error::from_win32(c, "the Recycle Bin refused the item".to_string())
                        .with_path(p.display()),
                );
            }
        }
        if self.stat(p)?.is_some() {
            return Err(Error::new(
                ErrorKind::Io,
                "the item is still there after the Recycle Bin call",
            )
            .with_path(p.display()));
        }
        Ok(())
    }

    /// Total size in bytes below `p` (links count as zero and are not entered).
    fn tree_size(&self, p: &VPath, e: &Entry) -> Result<u64> {
        let mut total = 0u64;
        let mut stack = vec![(p.clone(), e.clone())];
        while let Some((cur, ent)) = stack.pop() {
            match ent.kind {
                Kind::File => total = total.saturating_add(ent.size),
                Kind::Dir => {
                    let mut kids = Vec::new();
                    self.list(&cur, &mut kids)?;
                    stack.extend(kids.into_iter().map(|k| (cur.join(k.name.clone()), k)));
                }
                Kind::Link => {}
            }
        }
        Ok(total)
    }
}

/// `SHFileOperationW(FO_DELETE, FOF_ALLOWUNDO)` on the calling thread, which gets its own
/// single-threaded COM apartment. `Err` carries the Win32 or shell error code.
fn shell_delete(from: &[u16]) -> std::result::Result<(), u32> {
    use windows_sys::Win32::System::Com::{
        COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize,
    };
    use windows_sys::Win32::UI::Shell::{
        FO_DELETE, FOF_ALLOWUNDO, FOF_NOERRORUI, FOF_SILENT, FOF_WANTNUKEWARNING, SHFILEOPSTRUCTW,
        SHFileOperationW,
    };
    // SAFETY: COM is initialised and uninitialised on this thread only; `op` and `from` live for
    // the whole call and `from` is double-NUL terminated as FO_DELETE requires.
    unsafe {
        let hr = CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32);
        let mut op: SHFILEOPSTRUCTW = std::mem::zeroed();
        op.wFunc = FO_DELETE;
        op.pFrom = from.as_ptr();
        // no FOF_NOCONFIRMATION: with FOF_WANTNUKEWARNING the shell must ask before it deletes
        // permanently, so that is never silent
        op.fFlags = (FOF_ALLOWUNDO | FOF_WANTNUKEWARNING | FOF_NOERRORUI | FOF_SILENT) as _;
        let rc = SHFileOperationW(&raw mut op);
        if hr >= 0 {
            CoUninitialize();
        }
        if rc != 0 {
            Err(rc as u32)
        } else if op.fAnyOperationsAborted != 0 {
            Err(1223) // ERROR_CANCELLED
        } else {
            Ok(())
        }
    }
}

/// Reads a DWORD registry value (`HKEY` root, sub key, value name).
fn reg_dword(
    root: windows_sys::Win32::System::Registry::HKEY,
    key: &str,
    name: &str,
) -> Option<u32> {
    use windows_sys::Win32::System::Registry::{RRF_RT_REG_DWORD, RegGetValueW};
    let wide = |t: &str| -> Vec<u16> { t.encode_utf16().chain([0]).collect() };
    let (k, n) = (wide(key), wide(name));
    let mut data = 0u32;
    let mut size = 4u32;
    // SAFETY: `k` and `n` are NUL-terminated; `data` and `size` are valid for writes and `size`
    // is the byte size of `data`.
    let rc = unsafe {
        RegGetValueW(
            root,
            k.as_ptr(),
            n.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            (&raw mut data).cast(),
            &raw mut size,
        )
    };
    (rc == 0).then_some(data)
}

/// What the Recycle Bin settings allow on the volume whose root is `drive` (NUL-terminated
/// `X:\`): `Err` = the bin cannot be used (switched off or forbidden, or the volume cannot be
/// identified), `Ok(max)` = the configured size limit in bytes, if any (decision: D37).
fn bin_limit(drive: &[u16]) -> std::result::Result<Option<u64>, &'static str> {
    use windows_sys::Win32::Storage::FileSystem::GetVolumeNameForVolumeMountPointW;
    use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
    const POLICY: &str = r"Software\Microsoft\Windows\CurrentVersion\Policies\Explorer";
    const BUCKET: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\BitBucket";
    for root in [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE] {
        if reg_dword(root, POLICY, "NoRecycleFiles").unwrap_or(0) != 0 {
            return Err("a policy forbids the Recycle Bin");
        }
    }
    let mut name = [0u16; 64];
    // SAFETY: `drive` is NUL-terminated; `name` is valid for 64 units.
    if unsafe { GetVolumeNameForVolumeMountPointW(drive.as_ptr(), name.as_mut_ptr(), 64) } == 0 {
        return Err("the volume cannot be identified");
    }
    let name = String::from_utf16_lossy(&name[..name.iter().position(|&u| u == 0).unwrap_or(64)]);
    let (Some(a), Some(b)) = (name.find('{'), name.find('}')) else {
        return Err("the volume cannot be identified");
    };
    let volume = format!(r"{BUCKET}\Volume\{}", &name[a..=b]);
    let get = |value: &str| {
        reg_dword(HKEY_CURRENT_USER, &volume, value)
            .or_else(|| reg_dword(HKEY_CURRENT_USER, BUCKET, value))
    };
    if get("NukeOnDelete").unwrap_or(0) != 0 {
        return Err("the Recycle Bin is switched off for this volume");
    }
    Ok(get("MaxCapacity").map(|mb| u64::from(mb) << 20))
}
