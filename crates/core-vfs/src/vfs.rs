//! The `Vfs` trait and its types (plan.md §6.1, backlog E5-S1).
//!
//! The trait is synchronous: concurrency comes from the core's own pools. Paths are
//! root-relative [`VPath`]s; each `Vfs` is bound to one root.

use std::io::{Read, Seek, Write};

use core_base::{CancelToken, Result, Ticks};

use crate::path::{Name, VPath};

/// FILE_ATTRIBUTE_* values (the same numbers on every backend).
pub mod attr {
    pub const READONLY: u32 = 0x1;
    pub const HIDDEN: u32 = 0x2;
    pub const SYSTEM: u32 = 0x4;
    pub const DIRECTORY: u32 = 0x10;
    pub const ARCHIVE: u32 = 0x20;
    pub const OFFLINE: u32 = 0x1000;
    pub const RECALL_ON_OPEN: u32 = 0x40000;
    pub const RECALL_ON_DATA_ACCESS: u32 = 0x400000;
}

/// What an entry is. Only real links (symlink, junction, WSL symlink) are `Link` (D16).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    File,
    Dir,
    Link,
}

/// One directory entry, from the listing itself (no per-entry stat on Windows).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: Name,
    pub kind: Kind,
    pub size: u64,
    pub mtime: Ticks,
    pub ctime: Ticks,
    pub attrs: u32,
    /// For links: what the target is, when known.
    pub link_target_kind: Option<Kind>,
    /// (volume serial, file id) when the backend knows it.
    pub file_id: Option<(u64, u128)>,
}

impl Entry {
    pub fn file(name: &str, size: u64, mtime: Ticks) -> Self {
        Entry {
            name: Name::new(name),
            kind: Kind::File,
            size,
            mtime,
            ctime: mtime,
            attrs: attr::ARCHIVE,
            link_target_kind: None,
            file_id: None,
        }
    }

    pub fn dir(name: &str, mtime: Ticks) -> Self {
        Entry {
            name: Name::new(name),
            kind: Kind::Dir,
            size: 0,
            mtime,
            ctime: mtime,
            attrs: attr::DIRECTORY,
            link_target_kind: None,
            file_id: None,
        }
    }

    /// D17: the content is not local; reading it would download it.
    pub fn is_offline(&self) -> bool {
        self.attrs & (attr::OFFLINE | attr::RECALL_ON_OPEN | attr::RECALL_ON_DATA_ACCESS) != 0
    }

    /// D18: hidden or system by attribute. (Unix-like backends set HIDDEN for dot files.)
    pub fn is_hidden(&self) -> bool {
        self.attrs & (attr::HIDDEN | attr::SYSTEM) != 0
    }

    pub fn is_readonly(&self) -> bool {
        self.attrs & attr::READONLY != 0
    }
}

/// Properties of a backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VfsCaps {
    pub case_sensitive: bool,
    /// Smallest distinguishable time step (e.g. 2 s on FAT, 100 ns on NTFS).
    pub time_resolution: Ticks,
    pub has_attributes: bool,
    pub has_recycle_bin: bool,
    pub native_copy: bool,
    pub atomic_rename: bool,
    /// File names are limited to this many UTF-16 units.
    pub max_name_units: usize,
    /// Windows naming rules apply to names written here (invalid characters, device names).
    pub windows_names: bool,
    /// The storage is a rotating disk (it pays a seek penalty): two files read at the same time
    /// make the head travel between them, so readers should take turns (decision: D54). `false`
    /// when it is not known: the answer for an SSD, a network share and memory.
    pub seek_penalty: bool,
}

impl Default for VfsCaps {
    fn default() -> Self {
        VfsCaps {
            case_sensitive: true,
            time_resolution: 10_000_000,
            has_attributes: false,
            has_recycle_bin: false,
            native_copy: false,
            atomic_rename: true,
            max_name_units: 255,
            windows_names: false,
            seek_penalty: false,
        }
    }
}

pub trait ReadSeek: Read + Seek + Send {}
impl<T: Read + Seek + Send> ReadSeek for T {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteMode {
    Permanent,
    RecycleBin,
}

/// How to copy one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CopyOptions {
    /// Replace an existing destination. Without it an existing destination is `AlreadyExists`.
    pub overwrite: bool,
    /// On `AccessDenied`, clear READONLY/HIDDEN/SYSTEM on the destination and retry once
    /// (fix B5).
    pub overwrite_readonly: bool,
    /// Write to a `.dctmp~` sibling and rename (fix B4, decision D13: on by default).
    pub safe_copy: bool,
    /// Also copy the creation time.
    pub copy_ctime: bool,
    /// Read the copy back and compare checksums before the final rename (fix B6).
    pub verify: bool,
}

impl Default for CopyOptions {
    fn default() -> Self {
        CopyOptions {
            overwrite: false,
            overwrite_readonly: false,
            safe_copy: true,
            copy_ctime: false,
            verify: false,
        }
    }
}

/// Progress callback: `(bytes done, bytes total)`.
pub type Progress<'a> = &'a dyn Fn(u64, u64);

/// A file system. Implementations are `Send + Sync` and take `&self` everywhere.
pub trait Vfs: Send + Sync {
    fn caps(&self) -> VfsCaps;

    /// `Ok(None)` when the path does not exist. A link is reported as a link (not followed).
    fn stat(&self, p: &VPath) -> Result<Option<Entry>>;

    /// Lists a directory (unsorted). Names are returned exactly as the file system has them.
    fn list(&self, dir: &VPath, out: &mut Vec<Entry>) -> Result<()>;

    fn open_read(&self, p: &VPath) -> Result<Box<dyn ReadSeek>>;

    /// Creates a new temp file next to `p` (`<name>.<6 hex>.dctmp~`, hidden) and returns its
    /// path and a writer. The data is durable after `flush`/drop of the writer.
    fn create_temp_sibling(&self, p: &VPath) -> Result<(VPath, Box<dyn Write + Send>)>;

    /// Copies a file inside this backend (a native fast path where there is one).
    fn copy_file(
        &self,
        src: &VPath,
        dst: &VPath,
        o: &CopyOptions,
        prog: Progress<'_>,
        cancel: &CancelToken,
    ) -> Result<()>;

    /// Renames; with `replace`, an existing destination file is replaced atomically.
    fn rename(&self, from: &VPath, to: &VPath, replace: bool) -> Result<()>;

    /// Deletes a file, link or folder. **Never follows links**: deleting a junction removes the
    /// junction only. Folders are deleted recursively.
    fn delete(&self, p: &VPath, mode: DeleteMode) -> Result<()>;

    fn create_dir(&self, p: &VPath) -> Result<()>;

    fn set_times(&self, p: &VPath, mtime: Option<Ticks>, ctime: Option<Ticks>) -> Result<()>;

    fn set_attrs(&self, p: &VPath, attrs: u32) -> Result<()>;

    /// (volume serial, file id) of an existing path; `None` for backends without IDs.
    fn file_id(&self, _p: &VPath) -> Result<Option<(u64, u128)>> {
        Ok(None)
    }

    /// Whether names inside `dir` are case sensitive (per-directory on NTFS, plan.md §6.4).
    fn dir_case_sensitive(&self, _dir: &VPath) -> Result<bool> {
        Ok(self.caps().case_sensitive)
    }

    /// An identifier of the backend instance's storage, equal for two handles to the same
    /// storage: copies between them can take the native fast path.
    fn storage_id(&self) -> usize;

    /// File IDs of the OS-level ancestors of this backend's root (parent first, volume root
    /// last), so root overlap can be detected between two backends (plan.md §9.7). Backends
    /// without IDs return nothing.
    fn ancestor_ids(&self) -> Result<Vec<(u64, u128)>> {
        Ok(Vec::new())
    }
}

/// A stable pseudo-identifier for a byte string (FNV-1a), used for `storage_id`.
pub fn hash_id(bytes: &[u8]) -> usize {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h = (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
    }
    h as usize
}

/// `Some(Entry)` or `NotFound`.
pub fn stat_required(v: &dyn Vfs, p: &VPath) -> Result<Entry> {
    v.stat(p)?.ok_or_else(|| {
        core_base::Error::new(core_base::ErrorKind::NotFound, "no such file or folder")
            .with_path(p.display())
    })
}
