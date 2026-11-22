//! Saving a file the user edited (backlog E11.5-S14.2, decision D63): the bytes of a text side, or
//! any bytes, written so that a crash, a full disk or another program never leaves half a file.
//!
//! * the data goes to a temporary sibling, is flushed, and replaces the file by a rename: the file
//!   is the old one or the new one, never a mix (Notepad++ truncates the file and writes into it;
//!   a crash then loses the text);
//! * the file must still be the one that was loaded: if its size or time differ from what the
//!   caller read, nothing is written (`ChangedSinceCompare`) unless the caller says to overwrite;
//! * the attributes (hidden, system, archive, read-only) and the creation time of the old file
//!   are kept; a read-only file is refused unless the caller says to write it anyway;
//! * a link is never replaced by a file: saving to a symlink or junction is refused (§9.5);
//! * an optional backup (`name.bak`) is a safe copy of the old file made before the replacement.
//!
//! Known limit of the replace-by-rename way: alternate data streams, hard links and the security
//! descriptor of the old file are not carried over. [`save_in_place`] writes into the existing
//! file instead for the cases where that matters (local files only).

use std::io::Write as _;

use core_base::{CancelToken, Error, ErrorKind, Result, Ticks};
use core_vfs::{
    CopyOptions, Kind, Name, VPath, Vfs, attr, check_destination_unchanged, copy_between,
};

/// How a save behaves.
#[derive(Debug, Clone, Default)]
pub struct SaveOptions {
    /// What the file looked like when it was read: size and modification time. `None` for a new
    /// file or a "Save As" that expects nothing there.
    pub expect: Option<(u64, Ticks)>,
    /// Write although the file changed after it was read.
    pub overwrite_changed: bool,
    /// Write although the file is read-only (it stays read-only afterwards).
    pub write_readonly: bool,
    /// Keep the old file as `<name><suffix>` (for example `.bak`).
    pub backup_suffix: Option<String>,
}

/// What a save did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaveReport {
    pub bytes: u64,
    /// The path of the backup that was made.
    pub backup: Option<String>,
    /// Size and time of the file as written, for the next `expect`.
    pub size: u64,
    pub mtime: Ticks,
    /// An existing file was replaced (false: the file is new).
    pub replaced: bool,
}

fn within(a: Ticks, b: Ticks, resolution: Ticks) -> bool {
    a.abs_diff(b) <= resolution.max(1) as u64
}

/// Writes `bytes` to `path` as the rules above say.
pub fn save_file(vfs: &dyn Vfs, path: &VPath, bytes: &[u8], o: &SaveOptions) -> Result<SaveReport> {
    let name = path
        .file_name()
        .cloned()
        .ok_or_else(|| Error::invalid_argument("the root cannot be a file"))?;
    let at = |e: Error| e.with_path(path.display());
    let old = vfs.stat(path).map_err(at)?;
    if let Some(e) = &old {
        match e.kind {
            Kind::Dir => {
                return Err(
                    Error::invalid_argument("the target is a folder").with_path(path.display())
                );
            }
            Kind::Link => {
                return Err(Error::new(
                    ErrorKind::Unsupported,
                    "the target is a link; save to the file it points to, a save would replace the link",
                )
                .with_path(path.display()));
            }
            Kind::File => {}
        }
        if e.is_offline() {
            return Err(Error::new(
                ErrorKind::Unsupported,
                "the file is not stored locally (cloud placeholder); download it first",
            )
            .with_path(path.display()));
        }
        if e.is_readonly() && !o.write_readonly {
            return Err(Error::new(ErrorKind::AccessDenied, "the file is read-only")
                .with_path(path.display()));
        }
    }
    if let Some((size, mtime)) = o.expect {
        let res = vfs.caps().time_resolution;
        let same = old
            .as_ref()
            .is_some_and(|e| e.size == size && within(e.mtime, mtime, res));
        if !same && !o.overwrite_changed {
            return Err(Error::new(
                ErrorKind::ChangedSinceCompare,
                if old.is_some() {
                    "the file changed on disk after it was loaded; nothing was written"
                } else {
                    "the file was deleted after it was loaded; nothing was written"
                },
            )
            .with_path(path.display()));
        }
    }
    let mut backup = None;
    if let (Some(suffix), Some(_)) = (&o.backup_suffix, &old) {
        if suffix.is_empty() || suffix.contains(['/', '\\', ':']) {
            return Err(Error::invalid_argument("the backup suffix is not valid"));
        }
        let dir = path.parent().unwrap_or_else(VPath::root);
        let bpath = dir.join(Name::new(&format!("{}{suffix}", name.to_string_lossy())));
        copy_between(
            vfs,
            path,
            vfs,
            &bpath,
            &CopyOptions {
                overwrite: true,
                overwrite_readonly: true,
                ..CopyOptions::default()
            },
            &|_, _| {},
            &CancelToken::new(),
        )
        .map_err(|e| {
            Error::new(
                e.kind(),
                format!("the backup could not be made, nothing was written: {e}"),
            )
        })?;
        backup = Some(bpath.display());
    }
    // the data first, next to the target
    let (tmp, mut w) = vfs.create_temp_sibling(path).map_err(at)?;
    let cleanup = |vfs: &dyn Vfs, tmp: &VPath| {
        let _ = vfs.delete(tmp, core_vfs::DeleteMode::Permanent);
    };
    let written = w
        .write_all(bytes)
        .and_then(|()| w.flush())
        .map_err(|e| Error::from_io(&e, path.display()));
    drop(w);
    if let Err(e) = written {
        cleanup(vfs, &tmp);
        return Err(e);
    }
    let finish = || -> Result<()> {
        // the flush first (a read-only attribute set below would stop it), then the target is looked
        // at once more: it must still be what it was when this save began (decision D64)
        vfs.sync_file(&tmp)?;
        check_destination_unchanged(vfs, path, old.as_ref())?;
        if let Some(e) = &old {
            if vfs.caps().has_attributes {
                // the temp file is hidden; the target's own attributes (read-only included) apply
                vfs.set_attrs(&tmp, e.attrs & !(attr::DIRECTORY | attr::OFFLINE))?;
            }
            vfs.set_times(&tmp, None, Some(e.ctime))?;
        } else if vfs.caps().has_attributes {
            vfs.set_attrs(&tmp, attr::ARCHIVE)?;
        }
        // a read-only target must be writable for the rename to replace it
        if let Some(e) = &old {
            if e.is_readonly() {
                vfs.set_attrs(path, e.attrs & !attr::READONLY)?;
            }
        }
        vfs.rename(&tmp, path, true)?;
        let _ = vfs.sync_parent(path);
        Ok(())
    };
    if let Err(e) = finish() {
        cleanup(vfs, &tmp);
        if let Some(e0) = &old {
            if e0.is_readonly() {
                let _ = vfs.set_attrs(path, e0.attrs);
            }
        }
        return Err(at(e));
    }
    let now = vfs.stat(path).map_err(at)?;
    Ok(SaveReport {
        bytes: bytes.len() as u64,
        backup,
        size: now.as_ref().map_or(bytes.len() as u64, |e| e.size),
        mtime: now.as_ref().map_or(0, |e| e.mtime),
        replaced: old.is_some(),
    })
}

/// Writes into the existing local file instead of replacing it, which keeps its alternate data
/// streams, hard links, owner and permissions. The price is the crash window of any in-place
/// write, so a backup is made first when asked. The same checks as [`save_file`] apply.
pub fn save_in_place(path: &std::path::Path, bytes: &[u8], o: &SaveOptions) -> Result<SaveReport> {
    let err = |e: &std::io::Error| Error::from_io(e, path);
    let md = std::fs::metadata(path).map_err(|e| err(&e))?;
    if !md.is_file() {
        return Err(Error::invalid_argument("the target is not a file").with_path(path));
    }
    if md.permissions().readonly() && !o.write_readonly {
        return Err(Error::new(ErrorKind::AccessDenied, "the file is read-only").with_path(path));
    }
    if let Some((size, mtime)) = o.expect {
        let cur = md
            .modified()
            .ok()
            .map(core_base::ticks::ticks_from_system_time);
        if (md.len() != size || cur.is_none_or(|c| !within(c, mtime, 20_000_000)))
            && !o.overwrite_changed
        {
            return Err(Error::new(
                ErrorKind::ChangedSinceCompare,
                "the file changed on disk after it was loaded; nothing was written",
            )
            .with_path(path));
        }
    }
    let mut backup = None;
    if let Some(suffix) = &o.backup_suffix {
        if suffix.is_empty() || suffix.contains(['/', '\\', ':']) {
            return Err(Error::invalid_argument("the backup suffix is not valid"));
        }
        let mut b = path.as_os_str().to_owned();
        b.push(suffix);
        let b = std::path::PathBuf::from(b);
        std::fs::copy(path, &b).map_err(|e| {
            Error::new(
                Error::from_io(&e, &b).kind(),
                format!("the backup could not be made, nothing was written: {e}"),
            )
            .with_path(&b)
        })?;
        backup = Some(b.display().to_string());
    }
    let mut perms = None;
    if md.permissions().readonly() {
        perms = Some(md.permissions());
        let mut p = md.permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        p.set_readonly(false);
        std::fs::set_permissions(path, p).map_err(|e| err(&e))?;
    }
    let result = (|| -> std::io::Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(path)?;
        f.write_all(bytes)?;
        f.sync_all()
    })();
    if let Some(p) = perms {
        let _ = std::fs::set_permissions(path, p);
    }
    result.map_err(|e| err(&e))?;
    let now = std::fs::metadata(path).map_err(|e| err(&e))?;
    Ok(SaveReport {
        bytes: bytes.len() as u64,
        backup,
        size: now.len(),
        mtime: now
            .modified()
            .map_or(0, core_base::ticks::ticks_from_system_time),
        replaced: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_vfs::MemVfs;

    fn vfs() -> MemVfs {
        let v = MemVfs::new();
        v.add_file("a.txt", b"old", 1_000_000_000);
        v
    }

    fn p(s: &str) -> VPath {
        VPath::root().join(Name::new(s))
    }

    #[test]
    fn the_file_is_replaced_whole_and_the_new_stamp_is_reported() {
        let v = vfs();
        let r = save_file(
            &v,
            &p("a.txt"),
            b"new text",
            &SaveOptions {
                expect: Some((3, 1_000_000_000)),
                ..SaveOptions::default()
            },
        )
        .unwrap();
        assert!(r.replaced);
        assert_eq!(
            (v.read("a.txt").unwrap(), r.size),
            (b"new text".to_vec(), 8)
        );
        // no temp file is left behind
        assert_eq!(v.paths().len(), 1, "{:?}", v.paths());
    }

    #[test]
    fn a_file_that_changed_since_it_was_read_is_not_overwritten() {
        let v = vfs();
        let o = SaveOptions {
            expect: Some((99, 1_000_000_000)),
            ..SaveOptions::default()
        };
        let e = save_file(&v, &p("a.txt"), b"x", &o).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::ChangedSinceCompare);
        assert_eq!(v.read("a.txt").unwrap(), b"old");
        // a deleted file likewise
        let e = save_file(&v, &p("gone.txt"), b"x", &o).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::ChangedSinceCompare);
        // unless the caller says so
        let ok = save_file(
            &v,
            &p("a.txt"),
            b"x",
            &SaveOptions {
                overwrite_changed: true,
                ..o
            },
        );
        assert!(ok.is_ok());
        // a new file with nothing expected is created
        assert!(
            !save_file(&v, &p("n.txt"), b"n", &SaveOptions::default())
                .unwrap()
                .replaced
        );
    }

    #[test]
    fn read_only_links_and_folders_are_refused_and_attributes_survive() {
        let v = vfs();
        v.set_raw_attrs("a.txt", attr::READONLY | attr::HIDDEN | attr::ARCHIVE);
        let e = save_file(&v, &p("a.txt"), b"x", &SaveOptions::default()).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::AccessDenied);
        save_file(
            &v,
            &p("a.txt"),
            b"x",
            &SaveOptions {
                write_readonly: true,
                ..SaveOptions::default()
            },
        )
        .unwrap();
        let st = v.stat(&p("a.txt")).unwrap().unwrap();
        assert!(st.is_readonly() && st.is_hidden(), "{:#x}", st.attrs);
        v.add_link("l", Kind::File);
        assert_eq!(
            save_file(&v, &p("l"), b"x", &SaveOptions::default())
                .unwrap_err()
                .kind(),
            ErrorKind::Unsupported
        );
        v.add_dir("d", 1);
        assert!(save_file(&v, &p("d"), b"x", &SaveOptions::default()).is_err());
    }

    #[test]
    fn a_backup_is_made_first_and_a_failed_write_leaves_the_old_file() {
        let v = vfs();
        let r = save_file(
            &v,
            &p("a.txt"),
            b"new",
            &SaveOptions {
                backup_suffix: Some(".bak".into()),
                ..SaveOptions::default()
            },
        )
        .unwrap();
        assert!(
            r.backup
                .as_deref()
                .is_some_and(|b| b.ends_with("a.txt.bak")),
            "{:?}",
            r.backup
        );
        assert_eq!(v.read("a.txt.bak").unwrap(), b"old");
        assert_eq!(v.read("a.txt").unwrap(), b"new");
        assert!(
            save_file(
                &v,
                &p("a.txt"),
                b"x",
                &SaveOptions {
                    backup_suffix: Some("/x".into()),
                    ..SaveOptions::default()
                }
            )
            .is_err()
        );
        // a write that fails half way: the old file is untouched, no temp file remains
        let f = core_vfs::FaultVfs::new(vfs());
        f.fail_writes_after(0);
        let e = save_file(&f, &p("a.txt"), b"a long new text", &SaveOptions::default());
        assert!(e.is_err());
        assert_eq!(f.inner().read("a.txt").unwrap(), b"old");
        assert_eq!(f.inner().paths().len(), 1, "{:?}", f.inner().paths());
    }

    #[test]
    fn saving_in_place_keeps_the_file_and_makes_the_backup() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("f.txt");
        std::fs::write(&f, "old").unwrap();
        let md = std::fs::metadata(&f).unwrap();
        let o = SaveOptions {
            expect: Some((
                md.len(),
                core_base::ticks::ticks_from_system_time(md.modified().unwrap()),
            )),
            backup_suffix: Some(".bak".into()),
            ..SaveOptions::default()
        };
        let r = save_in_place(&f, b"new!", &o).unwrap();
        assert_eq!((std::fs::read(&f).unwrap(), r.size), (b"new!".to_vec(), 4));
        assert_eq!(std::fs::read(d.path().join("f.txt.bak")).unwrap(), b"old");
        // the stamp no longer matches: refused
        let e = save_in_place(&f, b"x", &o).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::ChangedSinceCompare);
        assert_eq!(std::fs::read(&f).unwrap(), b"new!");
    }

    #[test]
    fn a_target_that_changes_just_before_the_rename_is_not_replaced() {
        let m = vfs();
        let mc = m.clone();
        let f = core_vfs::FaultVfs::new(m);
        f.on_nth(
            core_vfs::Op::CreateTemp,
            1,
            std::sync::Arc::new(move |_| {
                mc.add_file("a.txt", b"another program wrote this", 2_000_000_000);
            }),
        );
        let e = save_file(&f, &p("a.txt"), b"mine", &SaveOptions::default()).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::ChangedSinceCompare, "{e}");
        assert_eq!(
            f.inner().read("a.txt").unwrap(),
            b"another program wrote this"
        );
        assert_eq!(f.inner().paths().len(), 1, "no temp file is left");
    }
}
