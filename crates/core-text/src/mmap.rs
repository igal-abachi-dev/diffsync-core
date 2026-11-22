//! Memory-mapped backing for very large UTF-8 files (plan.md §4.1, backlog E1-S3.2).
//!
//! Mapping is **not** the default: a mapped file cannot be truncated on Windows, so an editor
//! that saves by replacing the file can fail while a compare is open. Only UTF-8 files above
//! `LoadOptions::mmap_threshold` (512 MiB by default) are mapped, and documents built this
//! way are expected to be re-checked with `TextDocument::changed_on_disk` before reuse.
//!
//! This is the only module of `core-text` that contains `unsafe` besides the one-line
//! `GetACP` declaration in `ansi.rs`.
#![allow(unsafe_code)]

use std::fs::File;

use memmap2::Mmap;

/// A read-only mapping of a file that was verified to be valid UTF-8 without NUL bytes.
#[derive(Debug)]
pub struct MappedText {
    map: Mmap,
}

impl MappedText {
    /// Maps `file` and checks that it is UTF-8 text. Returns `Ok(None)` when the file is
    /// not mappable text (it contains NUL bytes or is not valid UTF-8), so the caller can
    /// fall back to the buffered loader and the normal encoding detection.
    pub fn try_new(file: &File) -> std::io::Result<Option<Self>> {
        // SAFETY: the mapping is read-only (`Mmap`, not `MmapMut`) and lives as long as this
        // value. The unavoidable hazard of any file mapping is another process modifying or
        // truncating the file while it is mapped; the OS may then deliver a fault when a
        // page is touched. We accept that for files above the mmap threshold, document it,
        // and never rely on the content staying valid for soundness: every access to the
        // bytes goes through `as_bytes` and is bounds checked, and text accessors re-validate
        // UTF-8 per line instead of trusting the check done here.
        let map = unsafe { Mmap::map(file)? };
        if memchr::memchr(0, &map).is_some() || std::str::from_utf8(&map).is_err() {
            return Ok(None);
        }
        Ok(Some(MappedText { map }))
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.map
    }
}
