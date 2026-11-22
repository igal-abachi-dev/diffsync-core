//! The generic copy engine (plan.md §6.1, §6.3, §9.2; backlog E5-S6, E5-S8.2, E5-S12).
//!
//! * **Safe copy (B4)**: data goes to a `<name>.<6 hex>.dctmp~` sibling, times are set on it,
//!   then it is renamed over the destination. A failure or cancel removes the temp file and
//!   leaves the destination untouched.
//! * **Attribute retry (B5)**: when the final rename is refused with `AccessDenied` and
//!   `overwrite_readonly` is set, READONLY/HIDDEN/SYSTEM are cleared on the destination and the
//!   rename is retried once; the attributes are restored if it still fails.
//! * **Verify (B6)**: the temp file is read back and its CRC-32 compared with the streamed
//!   source before the rename.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};

use core_base::crc32::FileCrc;
use core_base::{CancelToken, Error, ErrorKind, Result, Ticks};

use crate::path::{Name, VPath};
use crate::vfs::{CopyOptions, DeleteMode, Kind, Progress, Vfs, attr, stat_required};

/// Block size of streamed copies (1 MiB, decision D11).
pub const COPY_BLOCK: usize = 1 << 20;
/// Suffix of temp files; leftovers are recognised by it.
pub const TEMP_SUFFIX: &str = ".dctmp~";

/// A temp-file name for `name`: `<name>.<6 hex>.dctmp~`. The tag mixes a process-wide counter and
/// the clock, so two jobs (or a crash leftover) never collide.
pub fn temp_sibling_name(name: &Name) -> Name {
    static COUNTER: AtomicU64 = AtomicU64::new(0x9E37);
    let n = COUNTER.fetch_add(0x1F3D5, Ordering::Relaxed);
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    let tag = (n.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ u64::from(t)) & 0xFF_FFFF;
    let mut units = name.units().to_vec();
    units.extend(format!(".{tag:06x}{TEMP_SUFFIX}").encode_utf16());
    Name::from_units(&units)
}

/// True for names of the exact shape of our temp files: `<name>.<6 hex digits>.dctmp~`. A user
/// file that merely ends in `.dctmp~` is not one (the sweep deletes what this accepts).
pub fn is_temp_name(name: &Name) -> bool {
    let text = name.to_string_lossy();
    let Some(stem) = text.strip_suffix(TEMP_SUFFIX) else {
        return false;
    };
    // `<name>.<6 hex>`: a non-empty name, a dot, exactly six hex digits
    stem.len() > 7
        && stem.is_char_boundary(stem.len() - 7)
        && stem.as_bytes()[stem.len() - 7] == b'.'
        && stem.as_bytes()[stem.len() - 6..]
            .iter()
            .all(u8::is_ascii_hexdigit)
}

fn cleanup(v: &dyn Vfs, tmp: &VPath) {
    let _ = v.delete(tmp, DeleteMode::Permanent);
}

/// Copies one file from `src` to `dst` by streaming it in 1 MiB blocks into a temp sibling and
/// renaming it (always the safe path). Used by every backend without a native copy.
pub fn copy_stream(
    src_vfs: &dyn Vfs,
    src: &VPath,
    dst_vfs: &dyn Vfs,
    dst: &VPath,
    o: &CopyOptions,
    prog: Progress<'_>,
    cancel: &CancelToken,
) -> Result<()> {
    cancel.check()?;
    let se = stat_required(src_vfs, src)?;
    match se.kind {
        Kind::File => {}
        Kind::Dir => {
            return Err(Error::invalid_argument("a folder is not a file").with_path(src.display()));
        }
        Kind::Link => {
            return Err(
                Error::new(ErrorKind::Unsupported, "links are not copied in v1")
                    .with_path(src.display()),
            );
        }
    }
    let existing = dst_vfs.stat(dst)?;
    if let Some(d) = &existing {
        if d.kind == Kind::Dir {
            return Err(
                Error::new(ErrorKind::AlreadyExists, "a folder is in the way")
                    .with_path(dst.display()),
            );
        }
        if !o.overwrite {
            return Err(
                Error::new(ErrorKind::AlreadyExists, "destination exists").with_path(dst.display())
            );
        }
    }

    let (tmp, mut writer) = dst_vfs.create_temp_sibling(dst)?;
    let result = stream_into(src_vfs, src, &mut *writer, se.size, prog, cancel).and_then(|crc| {
        drop(writer);
        // the source must not have changed while it was read (plan.md §9.2)
        let after = stat_required(src_vfs, src)?;
        if after.size != se.size || after.mtime != se.mtime {
            return Err(Error::new(
                ErrorKind::ChangedSinceCompare,
                "the source changed during the copy",
            )
            .with_path(src.display()));
        }
        finish(
            src_vfs,
            dst_vfs,
            &se,
            &tmp,
            dst,
            existing.as_ref().map(|e| e.attrs),
            crc,
            o,
        )
    });
    if result.is_err() {
        cleanup(dst_vfs, &tmp);
    }
    result
}

fn stream_into(
    src_vfs: &dyn Vfs,
    src: &VPath,
    writer: &mut dyn Write,
    total: u64,
    prog: Progress<'_>,
    cancel: &CancelToken,
) -> Result<u32> {
    let mut reader = src_vfs.open_read(src)?;
    let mut buf = vec![0u8; COPY_BLOCK];
    let (mut done, mut crc) = (0u64, FileCrc::new());
    prog(0, total);
    loop {
        cancel.check()?;
        let n = reader
            .read(&mut buf)
            .map_err(|e| Error::from_io(&e, src.display()))?;
        if n == 0 {
            break;
        }
        writer
            .write_all(&buf[..n])
            .map_err(|e| Error::from_io(&e, src.display()))?;
        crc.update(&buf[..n]);
        done += n as u64;
        prog(done, total);
    }
    writer
        .flush()
        .map_err(|e| Error::from_io(&e, src.display()))?;
    Ok(crc.finalize())
}

#[allow(clippy::too_many_arguments)]
fn finish(
    src_vfs: &dyn Vfs,
    dst_vfs: &dyn Vfs,
    se: &crate::vfs::Entry,
    tmp: &VPath,
    dst: &VPath,
    old_attrs: Option<u32>,
    crc: u32,
    o: &CopyOptions,
) -> Result<()> {
    let _ = src_vfs;
    dst_vfs.set_times(tmp, Some(se.mtime), o.copy_ctime.then_some(se.ctime))?;
    if dst_vfs.caps().has_attributes {
        dst_vfs.set_attrs(
            tmp,
            se.attrs & (attr::READONLY | attr::HIDDEN | attr::SYSTEM | attr::ARCHIVE),
        )?;
    }
    if o.verify {
        verify_crc(dst_vfs, tmp, crc)?; // fix: B6
    }
    rename_with_retry(dst_vfs, tmp, dst, old_attrs, o)
}

/// The final rename of a safe copy, with the attribute-clear retry of fix B5: when the rename is
/// refused with `AccessDenied` and `overwrite_readonly` is set, READONLY/HIDDEN/SYSTEM are cleared
/// on the destination and the rename is retried once; the attributes are restored if it still
/// fails.
pub fn rename_with_retry(
    dst_vfs: &dyn Vfs,
    tmp: &VPath,
    dst: &VPath,
    old_attrs: Option<u32>,
    o: &CopyOptions,
) -> Result<()> {
    match dst_vfs.rename(tmp, dst, o.overwrite) {
        Err(e) if e.kind() == ErrorKind::AccessDenied && o.overwrite_readonly => {
            let Some(old) = old_attrs else { return Err(e) };
            dst_vfs.set_attrs(dst, old & !(attr::READONLY | attr::HIDDEN | attr::SYSTEM))?;
            let r = dst_vfs.rename(tmp, dst, o.overwrite);
            if r.is_err() {
                let _ = dst_vfs.set_attrs(dst, old); // restore
            }
            r
        }
        other => other,
    }
}

pub fn verify_crc(v: &dyn Vfs, p: &VPath, want: u32) -> Result<()> {
    let mut r = v.open_read(p)?;
    let (mut buf, mut crc) = (vec![0u8; COPY_BLOCK], FileCrc::new());
    loop {
        let n = r
            .read(&mut buf)
            .map_err(|e| Error::from_io(&e, p.display()))?;
        if n == 0 {
            break;
        }
        crc.update(&buf[..n]);
    }
    if crc.finalize() == want {
        Ok(())
    } else {
        Err(Error::new(
            ErrorKind::Io,
            "verification failed: the copy does not match the source",
        )
        .with_path(p.display()))
    }
}

/// Copies a file between two backends (or within one): the native fast path when both sides are
/// the same storage and the backend has one, a streamed safe copy otherwise.
pub fn copy_between(
    src_vfs: &dyn Vfs,
    src: &VPath,
    dst_vfs: &dyn Vfs,
    dst: &VPath,
    o: &CopyOptions,
    prog: Progress<'_>,
    cancel: &CancelToken,
) -> Result<()> {
    if src_vfs.storage_id() == dst_vfs.storage_id() && src_vfs.caps().native_copy {
        return src_vfs.copy_file(src, dst, o, prog, cancel);
    }
    copy_stream(src_vfs, src, dst_vfs, dst, o, prog, cancel)
}

/// Writes `bytes` to `path` atomically: temp sibling, flush, rename over the target
/// (backlog E5-S8.2, used for document save). A failure leaves the old content.
pub fn write_atomic(v: &dyn Vfs, path: &VPath, bytes: &[u8]) -> Result<()> {
    let (tmp, mut w) = v.create_temp_sibling(path)?;
    let r = w
        .write_all(bytes)
        .and_then(|()| w.flush())
        .map_err(|e| Error::from_io(&e, path.display()));
    drop(w);
    let r = r.and_then(|()| v.rename(&tmp, path, true));
    if r.is_err() {
        cleanup(v, &tmp);
    }
    r
}

/// What `copy_tree` did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TreeReport {
    pub files: u64,
    pub dirs: u64,
    pub bytes: u64,
    /// Links that were skipped (decision D16: links are not copied in v1).
    pub skipped_links: Vec<VPath>,
}

/// Copies a folder tree: folders are created first, files copied, and folder times set last
/// (writing a file changes its folder's time). Links are skipped and reported.
pub fn copy_tree(
    src_vfs: &dyn Vfs,
    src: &VPath,
    dst_vfs: &dyn Vfs,
    dst: &VPath,
    o: &CopyOptions,
    cancel: &CancelToken,
) -> Result<TreeReport> {
    let mut report = TreeReport::default();
    let root = stat_required(src_vfs, src)?;
    if root.kind != Kind::Dir {
        return Err(Error::invalid_argument("copy_tree needs a folder").with_path(src.display()));
    }
    let mut dirs_done: Vec<(VPath, Ticks, Ticks)> = Vec::new();
    if src_vfs.storage_id() == dst_vfs.storage_id() && dst.starts_with(src) {
        // copying a folder into itself would never end (plan.md §9.7)
        return Err(
            Error::invalid_argument("the destination is inside the source folder")
                .with_path(dst.display()),
        );
    }
    let mut stack = vec![(src.clone(), dst.clone(), root.mtime, root.ctime)];
    while let Some((s, d, mtime, ctime)) = stack.pop() {
        cancel.check()?;
        // decision: D16: a destination that exists must be a real folder; a link there would carry
        // every write into its target (plan.md §9, rule 5)
        match dst_vfs.stat(&d)? {
            None => dst_vfs.create_dir(&d)?,
            Some(e) if e.kind == Kind::Dir => {}
            Some(_) => {
                return Err(Error::new(
                    ErrorKind::AlreadyExists,
                    "the destination exists and is not a folder (links are never entered)",
                )
                .with_path(d.display()));
            }
        }
        report.dirs += 1;
        dirs_done.push((d.clone(), mtime, ctime));
        let mut entries = Vec::new();
        src_vfs.list(&s, &mut entries)?;
        for e in entries {
            let (sp, dp) = (s.join(e.name.clone()), d.join(e.name.clone()));
            match e.kind {
                Kind::Dir => stack.push((sp, dp, e.mtime, e.ctime)),
                Kind::Link => report.skipped_links.push(sp),
                Kind::File => {
                    copy_between(src_vfs, &sp, dst_vfs, &dp, o, &|_, _| {}, cancel)?;
                    report.files += 1;
                    report.bytes += e.size;
                }
            }
        }
    }
    // deepest folders first, so a parent is not touched after its time was set
    for (d, mtime, ctime) in dirs_done.iter().rev() {
        dst_vfs.set_times(d, Some(*mtime), o.copy_ctime.then_some(*ctime))?;
    }
    Ok(report)
}

#[cfg(test)]
mod temp_name_tests {
    use super::*;

    /// The sweep deletes what `is_temp_name` accepts, so only the exact shape counts.
    #[test]
    fn only_the_exact_temp_shape_is_a_temp_name() {
        let n = |s: &str| is_temp_name(&Name::new(s));
        assert!(n("report.txt.a1b2c3.dctmp~"));
        assert!(n("x.000000.dctmp~"));
        assert!(!n("report.txt.dctmp~"), "no tag");
        assert!(!n("report.12345.dctmp~"), "five digits");
        assert!(!n("report.1234567.dctmp~"), "seven digits");
        assert!(!n("report.gggggg.dctmp~"), "not hex");
        assert!(!n(".a1b2c3.dctmp~"), "no name before the tag");
        assert!(!n("a1b2c3.dctmp~"), "no dot before the tag");
        assert!(!n("notes.dctmp~"));
        assert!(!n("photo.jpg"));
        // the name this module generates is always recognised
        assert!(is_temp_name(&temp_sibling_name(&Name::new("any name.txt"))));
        assert!(is_temp_name(&temp_sibling_name(&Name::new("é.txt"))));
    }
}
