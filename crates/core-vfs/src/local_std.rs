//! `LocalStd`: a `std::fs` backend (plan.md §6.2, backlog E5-S10). It is the backend for Linux
//! and macOS and also works on Windows (without the Win32 listing details such as reparse tags
//! beyond what `std` exposes). Names split only on `/` in `VPath`; a backslash in a Unix name
//! is an ordinary character. Dot files are flagged hidden on non-Windows systems (D18).

use std::fs;
use std::io::Write;
use std::path::PathBuf;

use core_base::ticks::{ticks_from_system_time, ticks_to_system_time};
use core_base::{CancelToken, Error, ErrorKind, Result, Ticks};

use crate::copy::{copy_stream, temp_sibling_name};
use crate::path::{Name, VPath};
use crate::vfs::{CopyOptions, DeleteMode, Entry, Kind, Progress, ReadSeek, Vfs, VfsCaps, attr};

/// A `std::fs` backend rooted at a folder.
#[derive(Debug, Clone)]
pub struct LocalStd {
    root: PathBuf,
    /// Whether the root's file system is case sensitive: probed once, on first use.
    root_case: std::sync::Arc<std::sync::OnceLock<bool>>,
    /// Whether the root's disk is a rotating one (decision: D54): asked once, on first use.
    root_seek_penalty: std::sync::Arc<std::sync::OnceLock<bool>>,
}

/// Major and minor numbers of a Linux `dev_t` (the glibc encoding). Only Linux uses it outside
/// the unit test, which runs everywhere.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn dev_major_minor(dev: u64) -> (u64, u64) {
    (
        ((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff),
        (dev & 0xff) | ((dev >> 12) & !0xff),
    )
}

/// decision: D54: Linux says whether a block device rotates in `queue/rotational`, found through
/// `/sys/dev/block/<major>:<minor>`; for a partition the flag is one level up. Anything missing
/// (macOS, a network or overlay file system, a container) is "not known to rotate".
fn rotational_root(root: &std::path::Path) -> bool {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::MetadataExt;
        let Ok(md) = fs::metadata(root) else {
            return false;
        };
        let (major, minor) = dev_major_minor(md.dev());
        let base = format!("/sys/dev/block/{major}:{minor}");
        for rel in ["queue/rotational", "../queue/rotational"] {
            if let Ok(v) = fs::read_to_string(format!("{base}/{rel}")) {
                return v.trim() == "1";
            }
        }
        false
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = root;
        false
    }
}

/// The platform's usual answer, used when a folder gives no evidence (nothing in it has a letter
/// whose case could be swapped): Linux is case sensitive, Windows and macOS usually are not.
const DEFAULT_CASE_SENSITIVE: bool = !cfg!(any(windows, target_os = "macos"));

/// Swaps the case of the ASCII letters of `name`; `None` when it has none. (Only ASCII: a
/// Unicode case swap can change the normalisation form and so prove nothing.)
fn swap_ascii_case(name: &str) -> Option<String> {
    name.chars().any(|c| c.is_ascii_alphabetic()).then(|| {
        name.chars()
            .map(|c| {
                if c.is_ascii_lowercase() {
                    c.to_ascii_uppercase()
                } else {
                    c.to_ascii_lowercase()
                }
            })
            .collect()
    })
}

/// decision: D52: case sensitivity of a folder is measured, not assumed from the OS (APFS can be
/// case sensitive, ext4 folders can be casefolded, FAT/exFAT/SMB mounts are not). Without writing
/// anything: take entries of the folder and look up their case-swapped name. A swapped name that
/// is listed as its own entry, or that does not exist, means "sensitive"; one that resolves although
/// it is not listed means "insensitive". `None` when no entry has a letter to swap.
fn probe_dir(dir: &std::path::Path) -> Option<bool> {
    let names: Vec<std::ffi::OsString> = fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.file_name()))
        .take(256)
        .collect();
    for n in &names {
        let Some(swapped) = n.to_str().and_then(swap_ascii_case) else {
            continue;
        };
        if names.iter().any(|o| o.to_str() == Some(&swapped)) {
            return Some(true); // both spellings are separate entries
        }
        return Some(match fs::symlink_metadata(dir.join(&swapped)) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
            Err(_) => continue, // permission and the like say nothing about case
            Ok(_) => false,     // resolves although it is not listed: the lookup folds case
        });
    }
    None
}

impl LocalStd {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        LocalStd {
            root: root.into(),
            root_case: std::sync::Arc::default(),
            root_seek_penalty: std::sync::Arc::default(),
        }
    }

    pub fn root(&self) -> &std::path::Path {
        &self.root
    }

    /// The OS path of `p`. Built from components, never by string concatenation.
    pub fn os_path(&self, p: &VPath) -> PathBuf {
        let mut out = self.root.clone();
        for n in p.components() {
            out.push(n.to_os_string());
        }
        out
    }

    fn err(&self, e: &std::io::Error, p: &VPath) -> Error {
        Error::from_io(e, self.os_path(p))
    }

    fn entry(&self, name: Name, md: &fs::Metadata, path: &std::path::Path) -> Entry {
        let kind = if md.file_type().is_symlink() {
            Kind::Link
        } else if md.is_dir() {
            Kind::Dir
        } else {
            Kind::File
        };
        let mtime = md.modified().map_or(0, ticks_from_system_time);
        let ctime = md.created().map_or(mtime, ticks_from_system_time);
        let mut attrs = attrs_of(md, &name);
        if kind == Kind::Dir {
            attrs |= attr::DIRECTORY;
        }
        let link_target_kind = (kind == Kind::Link).then(|| {
            fs::metadata(path).map_or(None, |t| {
                Some(if t.is_dir() { Kind::Dir } else { Kind::File })
            })
        });
        Entry {
            name,
            kind,
            size: if kind == Kind::File { md.len() } else { 0 },
            mtime,
            ctime,
            attrs,
            link_target_kind: link_target_kind.flatten(),
            file_id: file_id_of(md),
        }
    }
}

#[cfg(windows)]
fn attrs_of(md: &fs::Metadata, _name: &Name) -> u32 {
    use std::os::windows::fs::MetadataExt;
    md.file_attributes() & !attr::DIRECTORY
}

#[cfg(not(windows))]
fn attrs_of(md: &fs::Metadata, name: &Name) -> u32 {
    let mut a = attr::ARCHIVE;
    if md.permissions().readonly() {
        a |= attr::READONLY;
    }
    if name.units().first() == Some(&u16::from(b'.')) {
        a |= attr::HIDDEN; // decision: D18: leading dot on Unix-like sources
    }
    a
}

#[cfg(unix)]
fn file_id_of(md: &fs::Metadata) -> Option<(u64, u128)> {
    use std::os::unix::fs::MetadataExt;
    Some((md.dev(), u128::from(md.ino())))
}

#[cfg(not(unix))]
fn file_id_of(_md: &fs::Metadata) -> Option<(u64, u128)> {
    None
}

impl Vfs for LocalStd {
    fn caps(&self) -> VfsCaps {
        VfsCaps {
            case_sensitive: *self.root_case.get_or_init(|| {
                self.dir_case_sensitive(&VPath::root())
                    .unwrap_or(DEFAULT_CASE_SENSITIVE)
            }),
            seek_penalty: *self
                .root_seek_penalty
                .get_or_init(|| rotational_root(&self.root)),
            time_resolution: 1,
            has_attributes: cfg!(windows),
            windows_names: cfg!(windows),
            ..VfsCaps::default()
        }
    }

    fn dir_case_sensitive(&self, dir: &VPath) -> Result<bool> {
        // the folder itself, then its ancestors up to the root (a folder without letters in its
        // entries says nothing, but the file system around it does), then the platform default
        let mut d = dir.clone();
        loop {
            if let Some(v) = probe_dir(&self.os_path(&d)) {
                return Ok(v);
            }
            match d.parent() {
                Some(p) => d = p,
                None => return Ok(DEFAULT_CASE_SENSITIVE),
            }
        }
    }

    fn stat(&self, p: &VPath) -> Result<Option<Entry>> {
        let path = self.os_path(p);
        match fs::symlink_metadata(&path) {
            Ok(md) => Ok(Some(self.entry(
                p.file_name().cloned().unwrap_or_default(),
                &md,
                &path,
            ))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(self.err(&e, p)),
        }
    }

    fn list(&self, dir: &VPath, out: &mut Vec<Entry>) -> Result<()> {
        let path = self.os_path(dir);
        for e in fs::read_dir(&path).map_err(|e| self.err(&e, dir))? {
            let e = e.map_err(|e| self.err(&e, dir))?;
            // `DirEntry::metadata` does not follow links; on Windows it comes from the listing
            let md = match e.metadata() {
                Ok(m) => m,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue, // vanished
                Err(err) => return Err(self.err(&err, dir)),
            };
            out.push(self.entry(Name::from_os_str(&e.file_name()), &md, &e.path()));
        }
        Ok(())
    }

    fn open_read(&self, p: &VPath) -> Result<Box<dyn ReadSeek>> {
        Ok(Box::new(
            fs::File::open(self.os_path(p)).map_err(|e| self.err(&e, p))?,
        ))
    }

    fn create_temp_sibling(&self, p: &VPath) -> Result<(VPath, Box<dyn Write + Send>)> {
        let name = p
            .file_name()
            .ok_or_else(|| Error::invalid_argument("the root has no siblings"))?;
        let tmp = p.parent().unwrap_or_default().join(temp_sibling_name(name));
        let f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(self.os_path(&tmp))
            .map_err(|e| self.err(&e, &tmp))?;
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
        copy_stream(self, src, self, dst, o, prog, cancel)
    }

    fn rename(&self, from: &VPath, to: &VPath, replace: bool) -> Result<()> {
        let dst = self.os_path(to);
        if !replace && fs::symlink_metadata(&dst).is_ok() {
            return Err(Error::new(ErrorKind::AlreadyExists, "destination exists").with_path(dst));
        }
        fs::rename(self.os_path(from), &dst).map_err(|e| self.err(&e, to))
    }

    fn delete(&self, p: &VPath, mode: DeleteMode) -> Result<()> {
        if mode == DeleteMode::RecycleBin {
            return Err(Error::new(
                ErrorKind::Unsupported,
                "the Recycle Bin is only available through the Windows backend",
            ));
        }
        if p.is_root() {
            return Err(Error::invalid_argument("cannot delete the root"));
        }
        // iterative, never follows a link: a link is removed itself
        let mut stack = vec![(p.clone(), false)];
        while let Some((cur, visited)) = stack.pop() {
            let path = self.os_path(&cur);
            let md = fs::symlink_metadata(&path).map_err(|e| self.err(&e, &cur))?;
            let is_link = md.file_type().is_symlink();
            if md.is_dir() && !is_link {
                if visited {
                    clear_readonly(&path, &md);
                    fs::remove_dir(&path).map_err(|e| self.err(&e, &cur))?;
                } else {
                    stack.push((cur.clone(), true));
                    let mut entries = Vec::new();
                    self.list(&cur, &mut entries)?;
                    stack.extend(entries.into_iter().map(|e| (cur.join(e.name), false)));
                }
            } else {
                clear_readonly(&path, &md);
                // Unix links (even to folders) are removed with `unlink`; on Windows a link to a
                // folder (junction or dir symlink) carries the DIRECTORY attribute
                let target_is_dir = is_link && is_dir_link(&md);
                let r = if target_is_dir {
                    fs::remove_dir(&path)
                } else {
                    fs::remove_file(&path)
                };
                r.map_err(|e| self.err(&e, &cur))?;
            }
        }
        Ok(())
    }

    fn create_dir(&self, p: &VPath) -> Result<()> {
        match fs::create_dir(self.os_path(p)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && self.os_path(p).is_dir() => {
                Ok(())
            }
            Err(e) => Err(self.err(&e, p)),
        }
    }

    fn set_times(&self, p: &VPath, mtime: Option<Ticks>, _ctime: Option<Ticks>) -> Result<()> {
        let Some(m) = mtime else { return Ok(()) };
        let t =
            ticks_to_system_time(m).ok_or_else(|| Error::invalid_argument("time out of range"))?;
        let mut oo = fs::OpenOptions::new();
        oo.write(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            oo.custom_flags(0x0200_0000); // FILE_FLAG_BACKUP_SEMANTICS: needed to open folders
        }
        let f = oo.open(self.os_path(p)).or_else(|e| {
            // folders cannot be opened for writing on Unix; open read-only there
            if cfg!(unix) {
                fs::File::open(self.os_path(p))
            } else {
                Err(e)
            }
        });
        f.and_then(|f| f.set_modified(t))
            .map_err(|e| self.err(&e, p))
    }

    fn set_attrs(&self, p: &VPath, attrs: u32) -> Result<()> {
        // only READONLY can be set through std; HIDDEN/SYSTEM need the Win32 backend
        let path = self.os_path(p);
        let md = fs::symlink_metadata(&path).map_err(|e| self.err(&e, p))?;
        let mut perm = md.permissions();
        perm.set_readonly(attrs & attr::READONLY != 0);
        fs::set_permissions(&path, perm).map_err(|e| self.err(&e, p))
    }

    fn file_id(&self, p: &VPath) -> Result<Option<(u64, u128)>> {
        let md = fs::symlink_metadata(self.os_path(p)).map_err(|e| self.err(&e, p))?;
        Ok(file_id_of(&md))
    }

    fn storage_id(&self) -> usize {
        // identifies the root: two handles to the same folder share storage (native copy is not
        // used, `native_copy` is false)
        crate::vfs::hash_id(self.root.to_string_lossy().as_bytes())
    }
}

#[cfg(windows)]
fn is_dir_link(md: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    md.file_attributes() & attr::DIRECTORY != 0
}

#[cfg(not(windows))]
fn is_dir_link(_md: &fs::Metadata) -> bool {
    false
}

fn clear_readonly(path: &std::path::Path, md: &fs::Metadata) {
    // only Windows refuses to delete a read-only file; on Unix this would make it world-writable
    if cfg!(windows) && md.permissions().readonly() {
        let mut p = md.permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        // intended: a read-only file must be deletable
        p.set_readonly(false);
        let _ = fs::set_permissions(path, p);
    }
}

#[cfg(test)]
mod tests {
    use super::dev_major_minor;

    /// The glibc `dev_t` layout: /dev/sda1 is 8:1, /dev/nvme0n1p2 is 259:2, a large minor spills
    /// into the high bits.
    #[test]
    fn dev_numbers_decode_like_glibc() {
        assert_eq!(dev_major_minor(0x0801), (8, 1));
        assert_eq!(dev_major_minor((259 << 8) | 2), (259, 2));
        assert_eq!(
            // makedev(8, 0x12310): the minor bits above 8 sit at bit 20
            dev_major_minor((8 << 8) | 0x10 | (0x12300 << 12)),
            (8, 0x1_2310)
        );
    }
}
