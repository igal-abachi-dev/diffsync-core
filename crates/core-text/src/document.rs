//! `TextDocument`: one UTF-8 buffer plus a line index (plan.md §4.1).
//!
//! Loading reads the file in 1 MiB blocks into **one** allocation and closes it again, so a
//! document never keeps a file locked (editors that save by replacing the file keep
//! working). Only UTF-8 files above [`LoadOptions::mmap_threshold`] are memory-mapped.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use core_base::{Error, ErrorKind, Result};

use crate::encoding::{self, DecodeOptions, Encoding};
use crate::lines::{LineEnding, LineIndex, LineSpan, index_lines};
use crate::mmap::MappedText;

/// Size of one read when loading a file.
pub const READ_BLOCK: usize = 1 << 20;
/// Default size above which a UTF-8 file is memory-mapped instead of read (512 MiB).
pub const DEFAULT_MMAP_THRESHOLD: u64 = 512 * 1024 * 1024;

/// How to load a file.
#[derive(Debug, Clone, Copy)]
pub struct LoadOptions {
    pub decode: DecodeOptions,
    /// UTF-8 files larger than this are mapped. `u64::MAX` disables mapping.
    pub mmap_threshold: u64,
}

impl Default for LoadOptions {
    fn default() -> Self {
        LoadOptions {
            decode: DecodeOptions::default(),
            mmap_threshold: DEFAULT_MMAP_THRESHOLD,
        }
    }
}

/// Identity of the file a document was loaded from: path, size and modification time at load.
/// A caller compares it with the disk to offer "file changed on disk → reload".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceId {
    pub path: Option<PathBuf>,
    pub size: u64,
    pub mtime: Option<SystemTime>,
}

impl SourceId {
    /// For documents that did not come from a file.
    pub fn none() -> Self {
        SourceId {
            path: None,
            size: 0,
            mtime: None,
        }
    }

    /// Reads the current size and mtime of `path`.
    pub fn capture(path: &Path) -> Result<Self> {
        let meta = std::fs::metadata(path).map_err(|e| Error::from_io(&e, path))?;
        Ok(SourceId {
            path: Some(path.to_path_buf()),
            size: meta.len(),
            mtime: meta.modified().ok(),
        })
    }

    /// True when the file is gone, or its size or modification time differs from this id.
    /// Always false for documents without a file.
    pub fn changed_on_disk(&self) -> bool {
        let Some(path) = &self.path else { return false };
        match std::fs::metadata(path) {
            Ok(meta) => meta.len() != self.size || meta.modified().ok() != self.mtime,
            Err(_) => true,
        }
    }
}

#[derive(Debug)]
enum Backing {
    /// All text is in one `String` (UTF-8 is used as read, others are transcoded once).
    Owned(String),
    Mapped(MappedText),
}

/// A loaded text file.
#[derive(Debug)]
pub struct TextDocument {
    backing: Backing,
    /// Where the text starts in the backing buffer (3 for a UTF-8 BOM, else 0).
    text_start: usize,
    index: LineIndex,
    encoding: Encoding,
    has_bom: bool,
    is_binary: bool,
    /// Malformed input was replaced with U+FFFD; the file cannot be rebuilt byte for byte.
    lossy: bool,
    byte_len: u64,
    source: SourceId,
}

impl TextDocument {
    /// Loads a file with the default options.
    pub fn open(path: &Path) -> Result<Self> {
        Self::load(path, &LoadOptions::default())
    }

    /// Loads a file: a buffered read in 1 MiB blocks, or a mapping for big UTF-8 files.
    /// The file handle is closed before this returns.
    pub fn load(path: &Path, opts: &LoadOptions) -> Result<Self> {
        let file = File::open(path).map_err(|e| Error::from_io(&e, path))?;
        let meta = file.metadata().map_err(|e| Error::from_io(&e, path))?;
        let source = SourceId {
            path: Some(path.to_path_buf()),
            size: meta.len(),
            mtime: meta.modified().ok(),
        };
        if meta.len() > opts.mmap_threshold && meta.len() > 0 {
            if let Some(doc) = Self::try_mapped(&file, &source)? {
                return Ok(doc);
            }
        }
        let bytes = read_all(file, meta.len(), path)?;
        Self::from_bytes_with_source(bytes, opts.decode, source)
    }

    fn try_mapped(file: &File, source: &SourceId) -> Result<Option<Self>> {
        let path = source.path.as_deref().unwrap_or(Path::new(""));
        let mapped = MappedText::try_new(file).map_err(|e| Error::from_io(&e, path))?;
        let Some(mapped) = mapped else {
            return Ok(None);
        };
        let bytes = mapped.as_bytes();
        // Valid UTF-8 without NULs means a UTF-8 file: UTF-16 text has NULs and ANSI text
        // with high bytes is not valid UTF-8, so both fall back to the buffered loader.
        let bom = bytes.starts_with(&[0xEF, 0xBB, 0xBF]);
        let text_start = if bom { 3 } else { 0 };
        let index = index_lines(&bytes[text_start..])?;
        Ok(Some(TextDocument {
            text_start,
            index,
            encoding: if bom {
                Encoding::Utf8Bom
            } else {
                Encoding::Utf8
            },
            has_bom: bom,
            is_binary: false,
            lossy: false,
            byte_len: bytes.len() as u64,
            source: source.clone(),
            backing: Backing::Mapped(mapped),
        }))
    }

    /// Builds a document from raw file bytes (BOM included).
    pub fn from_bytes(bytes: Vec<u8>, opts: &DecodeOptions) -> Result<Self> {
        Self::from_bytes_with_source(bytes, *opts, SourceId::none())
    }

    pub(crate) fn from_bytes_with_source(
        bytes: Vec<u8>,
        opts: DecodeOptions,
        source: SourceId,
    ) -> Result<Self> {
        let byte_len = bytes.len() as u64;
        let sniff = encoding::sniff(&bytes, &opts);
        if sniff.is_binary {
            // Binary files are not decoded and have no lines (plan.md §5.5).
            return Ok(TextDocument {
                backing: Backing::Owned(String::new()),
                text_start: 0,
                index: LineIndex::default(),
                encoding: sniff.encoding,
                has_bom: false,
                is_binary: true,
                lossy: false,
                byte_len,
                source,
            });
        }
        let decoded = encoding::decode(bytes, &sniff);
        let text_start = if sniff.encoding == Encoding::Utf8Bom {
            3
        } else {
            0
        };
        let index = index_lines(&decoded.text.as_bytes()[text_start..])?;
        Ok(TextDocument {
            backing: Backing::Owned(decoded.text),
            text_start,
            index,
            encoding: sniff.encoding,
            has_bom: sniff.bom_len > 0,
            is_binary: false,
            lossy: decoded.had_errors,
            byte_len,
            source,
        })
    }

    /// A UTF-8 document from text, without a file (tests, in-memory compare).
    pub fn from_text(text: &str) -> Result<Self> {
        let index = index_lines(text.as_bytes())?;
        Ok(TextDocument {
            backing: Backing::Owned(text.to_owned()),
            text_start: 0,
            index,
            encoding: Encoding::Utf8,
            has_bom: false,
            is_binary: false,
            lossy: false,
            byte_len: text.len() as u64,
            source: SourceId::none(),
        })
    }

    /// The whole text (after the BOM) as UTF-8 bytes. Empty for binary files.
    pub fn text_bytes(&self) -> &[u8] {
        match &self.backing {
            Backing::Owned(s) => &s.as_bytes()[self.text_start..],
            Backing::Mapped(m) => &m.as_bytes()[self.text_start..],
        }
    }

    pub fn line_count(&self) -> usize {
        self.index.spans.len()
    }

    pub fn spans(&self) -> &[LineSpan] {
        &self.index.spans
    }

    pub fn line_span(&self, i: usize) -> LineSpan {
        self.index.spans[i]
    }

    pub fn line_ending(&self, i: usize) -> LineEnding {
        self.index.spans[i].ending
    }

    /// The text of line `i` without its ending. Panics when `i` is out of range.
    pub fn line(&self, i: usize) -> &str {
        let span = self.index.spans[i];
        let (a, b) = (span.start as usize, span.start as usize + span.len as usize);
        match &self.backing {
            // Owned text was validated or transcoded to UTF-8, and lines are cut at ASCII
            // CR/LF, so these are always character boundaries.
            Backing::Owned(s) => &s[self.text_start..][a..b],
            // A mapped file can be modified behind our back; never trust it blindly.
            Backing::Mapped(m) => {
                std::str::from_utf8(&m.as_bytes()[self.text_start..][a..b]).unwrap_or("\u{FFFD}")
            }
        }
    }

    /// The bytes of line `i` without its ending.
    pub fn line_bytes(&self, i: usize) -> &[u8] {
        let span = self.index.spans[i];
        let a = span.start as usize;
        &self.text_bytes()[a..a + span.len as usize]
    }

    pub fn lines(&self) -> impl ExactSizeIterator<Item = &str> + '_ {
        (0..self.line_count()).map(|i| self.line(i))
    }

    pub fn encoding(&self) -> Encoding {
        self.encoding
    }

    pub fn has_bom(&self) -> bool {
        self.has_bom
    }

    pub fn is_binary(&self) -> bool {
        self.is_binary
    }

    /// True when malformed input was replaced and the file cannot be written back exactly.
    pub fn is_lossy(&self) -> bool {
        self.lossy
    }

    /// Size of the file in bytes at load time.
    pub fn byte_len(&self) -> u64 {
        self.byte_len
    }

    pub fn is_mapped(&self) -> bool {
        matches!(self.backing, Backing::Mapped(_))
    }

    /// Lines per ending: `[None, CrLf, Lf, Cr]`.
    pub fn ending_counts(&self) -> [u64; 4] {
        self.index.ending_counts
    }

    pub fn source(&self) -> &SourceId {
        &self.source
    }

    /// True when the file this document came from changed (or vanished) since loading.
    pub fn changed_on_disk(&self) -> bool {
        self.source.changed_on_disk()
    }

    /// Writes the document back in its original encoding and BOM, every line with its own
    /// ending (backlog E1-S7). Load → serialize is byte-identical for well-formed files.
    ///
    /// Fails with `Encoding` when the document is lossy (malformed input was replaced) or a
    /// character has no representation in the legacy code page, and with `Unsupported` for
    /// binary files. The atomic file write lives in `core-vfs`, not here.
    pub fn serialize(&self) -> Result<Vec<u8>> {
        if self.is_binary {
            return Err(Error::new(
                ErrorKind::Unsupported,
                "a binary file has no text to serialize",
            ));
        }
        if self.lossy {
            return Err(Error::new(
                ErrorKind::Encoding,
                "the file had malformed text that was replaced while loading; refusing to write it back",
            ));
        }
        let text = std::str::from_utf8(self.text_bytes()).map_err(|_| {
            Error::new(
                ErrorKind::Encoding,
                "the mapped file is no longer valid UTF-8",
            )
        })?;
        encoding::encode(text, self.encoding, self.has_bom)
    }
}

/// Reads a whole file in [`READ_BLOCK`] pieces into one buffer. Copes with a file that grows
/// or shrinks while it is read.
fn read_all(mut file: File, expected: u64, path: &Path) -> Result<Vec<u8>> {
    let io_err = |e: std::io::Error| Error::from_io(&e, path);
    let len = usize::try_from(expected).map_err(|_| {
        Error::new(ErrorKind::Unsupported, "file too large for this platform").with_path(path)
    })?;
    let mut buf = vec![0u8; len];
    let mut filled = 0usize;
    loop {
        if filled == buf.len() {
            // Exactly the expected size: probe for more data without growing the buffer.
            let mut probe = [0u8; 1];
            match file.read(&mut probe) {
                Ok(0) => break,
                Ok(_) => {
                    buf.push(probe[0]);
                    filled = buf.len();
                    buf.resize(filled + READ_BLOCK, 0);
                    continue;
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(io_err(e)),
            }
        }
        let end = (filled + READ_BLOCK).min(buf.len());
        match file.read(&mut buf[filled..end]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(io_err(e)),
        }
    }
    buf.truncate(filled);
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use encoding_rs::WINDOWS_1252;

    fn opts() -> DecodeOptions {
        DecodeOptions {
            ansi: Some(WINDOWS_1252),
        }
    }

    #[test]
    fn from_text_splits_lines_and_keeps_endings() {
        let d = TextDocument::from_text("a\r\nb\nc").unwrap();
        assert_eq!(d.line_count(), 3);
        assert_eq!(d.lines().collect::<Vec<_>>(), ["a", "b", "c"]);
        assert_eq!(d.line_ending(0), LineEnding::CrLf);
        assert_eq!(d.line_ending(1), LineEnding::Lf);
        assert_eq!(d.line_ending(2), LineEnding::None);
        assert_eq!(d.ending_counts(), [1, 1, 1, 0]);
    }

    #[test]
    fn utf8_bom_is_skipped_but_remembered() {
        let d = TextDocument::from_bytes(b"\xEF\xBB\xBFab\ncd".to_vec(), &opts()).unwrap();
        assert_eq!(d.encoding(), Encoding::Utf8Bom);
        assert!(d.has_bom());
        assert_eq!(d.line(0), "ab");
        assert_eq!(d.text_bytes(), b"ab\ncd");
        assert_eq!(d.byte_len(), 8);
    }

    #[test]
    fn binary_files_have_no_lines() {
        let d = TextDocument::from_bytes(b"\x00\x01\x02binary".to_vec(), &opts()).unwrap();
        assert!(d.is_binary());
        assert_eq!(d.line_count(), 0);
        assert_eq!(d.serialize().unwrap_err().kind(), ErrorKind::Unsupported);
    }

    #[test]
    fn empty_file_is_an_empty_utf8_document() {
        let d = TextDocument::from_bytes(Vec::new(), &opts()).unwrap();
        assert_eq!(d.line_count(), 0);
        assert!(!d.is_binary());
        assert_eq!(d.serialize().unwrap(), b"");
    }

    #[test]
    fn utf16_bom_is_not_part_of_the_text() {
        let mut bytes = vec![0xFF, 0xFE];
        bytes.extend("hi\r\nyo".encode_utf16().flat_map(u16::to_le_bytes));
        let d = TextDocument::from_bytes(bytes.clone(), &opts()).unwrap();
        assert_eq!(d.encoding(), Encoding::Utf16Le);
        assert!(d.has_bom());
        assert_eq!(d.lines().collect::<Vec<_>>(), ["hi", "yo"]);
        assert_eq!(d.serialize().unwrap(), bytes);
    }

    #[test]
    fn lossy_documents_refuse_to_serialize() {
        let d = TextDocument::from_bytes(b"\xEF\xBB\xBFab\xFFcd".to_vec(), &opts()).unwrap();
        assert!(d.is_lossy());
        assert_eq!(d.serialize().unwrap_err().kind(), ErrorKind::Encoding);
    }

    #[test]
    fn mapped_documents_match_buffered_ones() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.txt");
        let text = "first line\r\nשלום\nthird\rlast";
        std::fs::write(&path, text).unwrap();
        let buffered = TextDocument::load(&path, &LoadOptions::default()).unwrap();
        let mapped = TextDocument::load(
            &path,
            &LoadOptions {
                mmap_threshold: 0,
                ..LoadOptions::default()
            },
        )
        .unwrap();
        // threshold 0 maps every non-empty file
        assert!(mapped.is_mapped());
        assert!(!buffered.is_mapped());
        assert_eq!(mapped.line_count(), buffered.line_count());
        for i in 0..buffered.line_count() {
            assert_eq!(mapped.line(i), buffered.line(i));
            assert_eq!(mapped.line_ending(i), buffered.line_ending(i));
        }
        assert_eq!(mapped.serialize().unwrap(), text.as_bytes());
        drop(mapped);
    }

    #[test]
    fn non_utf8_files_above_the_threshold_fall_back_to_the_buffered_loader() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ansi.txt");
        std::fs::write(&path, b"caf\xE9\nnext").unwrap();
        let lo = LoadOptions {
            decode: opts(),
            mmap_threshold: 0,
        };
        let d = TextDocument::load(&path, &lo).unwrap();
        assert!(!d.is_mapped());
        assert_eq!(d.line(0), "café");
        // UTF-16 text has NULs: also not mapped, and still decoded.
        let path16 = dir.path().join("u16.txt");
        let mut bytes = vec![0xFF, 0xFE];
        bytes.extend("abc".encode_utf16().flat_map(u16::to_le_bytes));
        std::fs::write(&path16, bytes).unwrap();
        let d = TextDocument::load(&path16, &lo).unwrap();
        assert!(!d.is_mapped());
        assert_eq!(d.line(0), "abc");
    }

    #[test]
    fn the_file_is_not_held_open_after_loading() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "one\ntwo\n").unwrap();
        let d = TextDocument::open(&path).unwrap();
        // On Windows a held handle without FILE_SHARE_DELETE would block rename and delete.
        let moved = dir.path().join("b.txt");
        std::fs::rename(&path, &moved).unwrap();
        std::fs::write(&path, "replacement written by an editor\n").unwrap();
        std::fs::remove_file(&moved).unwrap();
        assert_eq!(d.lines().collect::<Vec<_>>(), ["one", "two"]);
        assert!(d.changed_on_disk(), "replaced file must be reported");
    }

    #[test]
    fn source_id_detects_modification_and_deletion() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "abc").unwrap();
        let d = TextDocument::open(&path).unwrap();
        assert!(!d.changed_on_disk());
        assert_eq!(d.source().size, 3);
        std::fs::write(&path, "abcdef").unwrap();
        assert!(d.changed_on_disk());
        let id = SourceId::capture(&path).unwrap();
        assert!(!id.changed_on_disk());
        std::fs::remove_file(&path).unwrap();
        assert!(id.changed_on_disk());
        assert!(!SourceId::none().changed_on_disk());
    }

    #[test]
    fn missing_file_reports_not_found_with_the_path() {
        let err = TextDocument::open(Path::new("definitely/not/here.txt")).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::NotFound);
        assert!(err.path().is_some());
    }

    #[test]
    fn read_all_copes_with_files_larger_than_one_block() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("multi.txt");
        let mut text = String::new();
        let mut n = 0;
        while text.len() < READ_BLOCK * 2 + 123 {
            text.push_str(&format!("line {n}\n"));
            n += 1;
        }
        std::fs::write(&path, &text).unwrap();
        let d = TextDocument::open(&path).unwrap();
        assert_eq!(d.line_count(), n);
        assert_eq!(d.serialize().unwrap(), text.as_bytes());
    }
}
