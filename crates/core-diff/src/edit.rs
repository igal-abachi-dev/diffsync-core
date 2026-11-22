//! Editing and copying between the two sides of a text compare (backlog E11.5-S14.1, S14.2):
//! the model behind the "copy to right / copy to left" arrows, line editing, undo and redo, and
//! the conversion of encoding, byte order mark and line endings.
//!
//! * [`EditDoc`] is one side as a list of lines with their own endings, an undo history and the
//!   format it must be written in. It does no I/O; [`EditDoc::to_bytes`] gives the bytes and the
//!   caller writes them (`core-sync::save`).
//! * [`Editor`] ties two of them to a running [`TextCompare`]. Each copy or edit changes the
//!   documents and restarts the compare on the new text, so the rows, sections and statistics the
//!   host reads are always those of the text on screen (the generation counter changes).
//!
//! Rules (decision D62): an edit never touches lines it was not asked to touch, except that a last
//! line without a line ending gets one when something is added after it; text that cannot be
//! written back in the document's encoding is refused when saving and when converting, never
//! replaced silently; a document that was loaded with malformed text is read-only until it is
//! converted, because writing it back would change bytes the user never saw.

use std::sync::Arc;

use core_base::{Error, ErrorKind, Result};
use core_text::{DecodeOptions, Encoding, LineEnding, TextDocument, TextEncoding};
use serde::Serialize;

use crate::Side;
use crate::session::{DiffRow, DocSpec, Status, TextCompare};

/// A line as an editor holds it: the text without its ending, and the ending.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditLine {
    pub text: String,
    pub eol: LineEnding,
}

/// The line endings a document can be converted to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
pub enum EolKind {
    #[serde(rename = "crlf")]
    CrLf,
    #[serde(rename = "lf")]
    Lf,
    #[serde(rename = "cr")]
    Cr,
}

impl EolKind {
    pub fn ending(self) -> LineEnding {
        match self {
            EolKind::CrLf => LineEnding::CrLf,
            EolKind::Lf => LineEnding::Lf,
            EolKind::Cr => LineEnding::Cr,
        }
    }
}

#[derive(Debug, Clone)]
enum Change {
    /// `removed` lines at `first` were replaced by `inserted`.
    Splice {
        first: usize,
        removed: Vec<EditLine>,
        inserted: Vec<EditLine>,
    },
    Eols {
        old: Vec<LineEnding>,
        new: Vec<LineEnding>,
    },
    Format {
        old: TextEncoding,
        new: TextEncoding,
    },
}

/// One user action: all its changes are undone together.
type Action = Vec<Change>;

/// The most lines the undo history may hold; the oldest actions go first.
const MAX_HISTORY_LINES: usize = 4_000_000;

/// An edit as a text buffer with a piece tree reports it (VS Code's `IIdentifiedSingleEditOperation`,
/// Avalonia's `TextDocument` changes): replace the text between two positions by `text`. Lines are
/// 0-based and columns count UTF-16 code units from the start of the line's text (the line ending
/// is not part of a line); `end_column` may equal the line's length. Line breaks in `text` become
/// the document's line ending.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RangeEdit {
    pub start_line: usize,
    pub start_column: usize,
    pub end_line: usize,
    pub end_column: usize,
    pub text: String,
}

/// The byte index of UTF-16 column `col` in `text`; `None` inside a surrogate pair or past the end.
fn byte_at(text: &str, col: usize) -> Option<usize> {
    let mut units = 0;
    for (i, c) in text.char_indices() {
        if units == col {
            return Some(i);
        }
        units += c.len_utf16();
        if units > col {
            return None;
        }
    }
    (units == col).then_some(text.len())
}

/// One side of a compare as an editable list of lines.
#[derive(Debug, Clone)]
pub struct EditDoc {
    lines: Vec<EditLine>,
    format: TextEncoding,
    /// Malformed bytes were replaced when the file was read.
    lossy: bool,
    undo: Vec<(u64, Action)>,
    redo: Vec<(u64, Action)>,
    history_lines: usize,
    /// Counts every change of the text or format (also undo and redo): a cache key for the host.
    revision: u64,
    next_id: u64,
    /// The id of the last action when the document was saved; the document is dirty when the
    /// last action is another one (so undoing back to the saved text is clean again).
    saved_id: u64,
}

/// What [`EditDoc`] says about itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DocState {
    pub lines: u32,
    pub dirty: bool,
    pub can_undo: bool,
    pub can_redo: bool,
    pub encoding: String,
    pub bom: bool,
    /// Lines per ending: `[none, CRLF, LF, CR]`.
    pub ending_counts: [u64; 4],
    /// More than one kind of line ending is in use.
    pub mixed_endings: bool,
    /// The ending a new line gets.
    pub default_eol: EolKind,
    /// Malformed text was replaced on loading; saving is refused until the encoding is converted.
    pub lossy: bool,
    pub revision: u64,
}

fn unmappable_error(line: usize, col: usize, c: char, enc: &str) -> Error {
    Error::new(
        ErrorKind::Encoding,
        format!(
            "line {}, column {}: '{c}' (U+{:04X}) cannot be written in {enc}; convert the file to UTF-8 or change the text",
            line + 1,
            col + 1,
            c as u32
        ),
    )
}

impl EditDoc {
    /// The lines of a loaded document. A binary document has no lines to edit.
    pub fn from_document(d: &TextDocument) -> Result<EditDoc> {
        if d.is_binary() {
            return Err(Error::new(
                ErrorKind::Unsupported,
                "a binary file cannot be edited as text",
            ));
        }
        let lines = (0..d.line_count())
            .map(|i| EditLine {
                text: d.line(i).to_string(),
                eol: d.line_ending(i),
            })
            .collect();
        Ok(EditDoc::new(
            lines,
            TextEncoding::new(d.encoding(), d.has_bom()),
            d.is_lossy(),
        ))
    }

    /// A document from text (UTF-8, no byte order mark).
    pub fn from_text(text: &str) -> EditDoc {
        EditDoc::new(
            split_text(text, LineEnding::None),
            TextEncoding::new(Encoding::Utf8, false),
            false,
        )
    }

    /// Decodes `bytes`. With `force`, the bytes are read as that encoding whatever they look like
    /// ("Encode in" of Notepad++: the same bytes, another reading); without it the encoding is
    /// detected.
    pub fn from_bytes(
        bytes: Vec<u8>,
        opts: &DecodeOptions,
        force: Option<TextEncoding>,
    ) -> Result<EditDoc> {
        let doc = match force {
            None => TextDocument::from_bytes(bytes, opts)?,
            Some(t) => {
                let sniff = core_text::encoding::Sniff {
                    encoding: t.encoding,
                    bom_len: bom_len(&bytes, t),
                    is_binary: false,
                };
                let dec = core_text::encoding::decode(bytes, &sniff);
                let mut d =
                    EditDoc::new(split_text(&dec.text, LineEnding::None), t, dec.had_errors);
                // a decoded U+FEFF of a UTF-8 BOM is not text
                if t.encoding == Encoding::Utf8 && t.bom {
                    if let Some(l) = d.lines.first_mut() {
                        if let Some(r) = l.text.strip_prefix('\u{FEFF}') {
                            l.text = r.to_string();
                        }
                    }
                }
                return Ok(d);
            }
        };
        EditDoc::from_document(&doc)
    }

    fn new(lines: Vec<EditLine>, format: TextEncoding, lossy: bool) -> EditDoc {
        EditDoc {
            lines,
            format,
            lossy,
            undo: Vec::new(),
            redo: Vec::new(),
            history_lines: 0,
            revision: 0,
            next_id: 1,
            saved_id: 0,
        }
    }

    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    pub fn line(&self, i: usize) -> Option<&EditLine> {
        self.lines.get(i)
    }

    pub fn lines(&self) -> &[EditLine] {
        &self.lines
    }

    pub fn format(&self) -> TextEncoding {
        self.format
    }

    pub fn is_lossy(&self) -> bool {
        self.lossy
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    fn top_id(&self) -> u64 {
        self.undo.last().map_or(0, |a| a.0)
    }

    pub fn is_dirty(&self) -> bool {
        self.top_id() != self.saved_id
    }

    /// Records that the current text is what is on disk (after a successful save).
    pub fn mark_saved(&mut self) {
        self.saved_id = self.top_id();
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// Lines per ending: `[none, CRLF, LF, CR]`.
    pub fn ending_counts(&self) -> [u64; 4] {
        let mut c = [0u64; 4];
        for l in &self.lines {
            c[l.eol as usize] += 1;
        }
        c
    }

    /// The ending most lines use; LF for an empty or one-line document without ending (the
    /// platform's own convention is the host's to ask for with [`convert_eol`](Self::convert_eol)).
    pub fn default_eol(&self) -> EolKind {
        let c = self.ending_counts();
        if c[LineEnding::CrLf as usize] > c[LineEnding::Lf as usize]
            && c[LineEnding::CrLf as usize] >= c[LineEnding::Cr as usize]
        {
            EolKind::CrLf
        } else if c[LineEnding::Cr as usize] > c[LineEnding::Lf as usize] {
            EolKind::Cr
        } else {
            EolKind::Lf
        }
    }

    pub fn is_mixed(&self) -> bool {
        self.ending_counts()[1..].iter().filter(|n| **n > 0).count() > 1
    }

    pub fn state(&self) -> DocState {
        DocState {
            lines: self.lines.len() as u32,
            dirty: self.is_dirty(),
            can_undo: self.can_undo(),
            can_redo: self.can_redo(),
            encoding: self.format.label(),
            bom: self.format.bom,
            ending_counts: self.ending_counts(),
            mixed_endings: self.is_mixed(),
            default_eol: self.default_eol(),
            lossy: self.lossy,
            revision: self.revision,
        }
    }

    /// The whole text, every line with its own ending.
    pub fn text(&self) -> String {
        let mut s = String::with_capacity(self.lines.iter().map(|l| l.text.len() + 2).sum());
        for l in &self.lines {
            s.push_str(&l.text);
            s.push_str(l.eol.as_str());
        }
        s
    }

    /// The bytes to write: the text in the document's encoding, with the byte order mark if it
    /// has one. Refused for a document that was read lossily and for text the encoding cannot
    /// hold; the error names the line and column.
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        if self.lossy {
            return Err(Error::new(
                ErrorKind::Encoding,
                "the file had malformed text that was replaced while loading; convert it to UTF-8 to save, or reopen it with the right encoding",
            ));
        }
        self.check_encodable(self.format)?;
        core_text::encoding::encode(&self.text(), self.format.encoding, self.format.bom)
    }

    /// Fails when a character of the document has no representation in `enc`.
    pub fn check_encodable(&self, enc: TextEncoding) -> Result<()> {
        if !matches!(enc.encoding, Encoding::Ansi(_)) {
            return Ok(());
        }
        for (i, l) in self.lines.iter().enumerate() {
            if let Some((col, c)) = core_text::first_unmappable(&l.text, enc.encoding) {
                return Err(unmappable_error(i, col, c, &enc.label()));
            }
        }
        Ok(())
    }

    fn push_action(&mut self, a: Action) {
        let weight: usize = a
            .iter()
            .map(|c| match c {
                Change::Splice {
                    removed, inserted, ..
                } => removed.len() + inserted.len() + 1,
                Change::Eols { old, .. } => old.len() / 4 + 1,
                Change::Format { .. } => 1,
            })
            .sum();
        self.history_lines += weight;
        let id = self.next_id;
        self.next_id += 1;
        self.undo.push((id, a));
        self.redo.clear();
        while self.history_lines > MAX_HISTORY_LINES && self.undo.len() > 1 {
            let old = self.undo.remove(0).1;
            let w: usize = old
                .iter()
                .map(|c| match c {
                    Change::Splice {
                        removed, inserted, ..
                    } => removed.len() + inserted.len() + 1,
                    Change::Eols { old, .. } => old.len() / 4 + 1,
                    Change::Format { .. } => 1,
                })
                .sum();
            self.history_lines = self.history_lines.saturating_sub(w);
        }
        self.revision += 1;
    }

    fn check_writable(&self) -> Result<()> {
        if self.lossy {
            return Err(Error::new(
                ErrorKind::Encoding,
                "this file had malformed text that was replaced while loading; it cannot be edited until it is reopened with the right encoding or converted",
            ));
        }
        Ok(())
    }

    /// Replaces `remove` lines at `first` with `new`. Lines must not contain a line break. When
    /// lines are added after a last line that has no ending, that line gets the document's ending
    /// (the one change outside the range).
    pub fn splice(&mut self, first: usize, remove: usize, new: Vec<EditLine>) -> Result<()> {
        self.check_writable()?;
        let n = self.lines.len();
        if first > n || remove > n - first {
            return Err(Error::invalid_argument(format!(
                "lines {first}..{} are outside the document ({n} lines)",
                first.saturating_add(remove)
            )));
        }
        if new.iter().any(|l| l.text.contains(['\r', '\n'])) {
            return Err(Error::invalid_argument(
                "a line must not contain a line break",
            ));
        }
        let mut new = new;
        let (mut first, mut remove) = (first, remove);
        // the new lines end the document, or they do not: only the last of the document may be open
        let after = first + remove;
        if after < n {
            if let Some(l) = new.last_mut() {
                if l.eol == LineEnding::None {
                    l.eol = self.default_eol().ending();
                }
            }
        }
        // only the last line can be open, so this is an insertion at the very end
        let tail_open =
            first > 0 && self.lines[first - 1].eol == LineEnding::None && !new.is_empty();
        let mut removed: Vec<EditLine> = self.lines[first..after].to_vec();
        let mut inserted = new;
        if tail_open {
            // the line before the range was the open last line; it is not the last any more
            first -= 1;
            remove += 1;
            let mut prev = self.lines[first].clone();
            prev.eol = self.default_eol().ending();
            removed.insert(0, self.lines[first].clone());
            inserted.insert(0, prev);
        }
        if removed == inserted {
            return Ok(());
        }
        self.lines.splice(first..first + remove, inserted.clone());
        self.push_action(vec![Change::Splice {
            first,
            removed,
            inserted,
        }]);
        Ok(())
    }

    /// Replaces `remove` whole lines at `first` with the lines of `text` (split at `\r\n`, `\n` and
    /// `\r`). A last line of `text` without an ending takes the ending of the last line it
    /// replaces, or the document's.
    pub fn replace_text(&mut self, first: usize, remove: usize, text: &str) -> Result<()> {
        let n = self.lines.len();
        if first > n || remove > n - first {
            return Err(Error::invalid_argument(format!(
                "lines {first}..{} are outside the document ({n} lines)",
                first.saturating_add(remove)
            )));
        }
        let fallback = if remove > 0 {
            self.lines[first + remove - 1].eol
        } else if first == n && n > 0 && self.lines[n - 1].eol == LineEnding::None {
            LineEnding::None // appending to a file that has no final newline keeps it that way
        } else {
            self.default_eol().ending()
        };
        let mut new = split_text(text, fallback);
        if let Some(l) = new.last_mut() {
            if l.eol == LineEnding::None && fallback == LineEnding::None && first + remove < n {
                l.eol = self.default_eol().ending();
            }
        }
        self.splice(first, remove, new)
    }

    /// Applies one [`RangeEdit`]: the lines it touches are rebuilt from the text before the range,
    /// the new text and the text after the range; lines outside the range are not touched.
    pub fn replace_range(&mut self, e: &RangeEdit) -> Result<()> {
        let (first, remove, new) = self.range_edit_lines(e)?;
        self.splice(first, remove, new)
    }

    fn range_edit_lines(&self, e: &RangeEdit) -> Result<(usize, usize, Vec<EditLine>)> {
        let bad = |m: &str| Error::invalid_argument(format!("range edit: {m}"));
        let ins = e.text.replace("\r\n", "\n").replace('\r', "\n");
        let n = self.lines.len();
        if n == 0 {
            // an empty document is one empty line without ending
            if (e.start_line, e.start_column, e.end_line, e.end_column) != (0, 0, 0, 0) {
                return Err(bad("the document is empty"));
            }
            return Ok((0, 0, self.join_edit(&ins, "", LineEnding::None)));
        }
        if e.start_line > e.end_line || e.end_line >= n {
            return Err(bad(&format!(
                "lines {}..={} are outside the document ({n} lines)",
                e.start_line, e.end_line
            )));
        }
        let (a, b) = (&self.lines[e.start_line], &self.lines[e.end_line]);
        let bs = byte_at(&a.text, e.start_column)
            .ok_or_else(|| bad("the start column is outside the line"))?;
        let be = byte_at(&b.text, e.end_column)
            .ok_or_else(|| bad("the end column is outside the line"))?;
        if e.start_line == e.end_line && be < bs {
            return Err(bad("the end is before the start"));
        }
        let combined = format!("{}{ins}", &a.text[..bs]);
        Ok((
            e.start_line,
            e.end_line - e.start_line + 1,
            self.join_edit(&combined, &b.text[be..], b.eol),
        ))
    }

    /// `head` + `tail` split at `\n` into lines; the last gets `last`, the others the document's
    /// line ending.
    fn join_edit(&self, head: &str, tail: &str, last: LineEnding) -> Vec<EditLine> {
        let all = format!("{head}{tail}");
        let parts: Vec<&str> = all.split('\n').collect();
        let eol = self.default_eol().ending();
        let n = parts.len();
        parts
            .into_iter()
            .enumerate()
            .map(|(i, t)| EditLine {
                text: t.to_string(),
                eol: if i + 1 == n { last } else { eol },
            })
            .collect()
    }

    /// Applies several edits that refer to the *current* text (as a piece tree reports a batch):
    /// they must not overlap; they are applied from the end so positions stay valid, and undo takes
    /// them back together.
    pub fn apply_edits(&mut self, edits: &[RangeEdit]) -> Result<()> {
        let mut order: Vec<&RangeEdit> = edits.iter().collect();
        order.sort_by_key(|e| (e.start_line, e.start_column));
        for w in order.windows(2) {
            if (w[0].end_line, w[0].end_column) > (w[1].start_line, w[1].start_column) {
                return Err(Error::invalid_argument("range edits overlap"));
            }
        }
        // validate everything first: nothing is applied when one edit is wrong
        for e in &order {
            self.range_edit_lines(e)?;
        }
        // the actions of this batch are the ones with an id from here on (the history may be trimmed
        // at the front meanwhile, so positions cannot be used)
        let first_id = self.next_id;
        for e in order.into_iter().rev() {
            self.replace_range(e)?;
        }
        // one undo step for the batch
        let start = self.undo.iter().position(|a| a.0 >= first_id);
        if let Some(start) = start {
            if self.undo.len() - start > 1 {
                let mut group: Action = Vec::new();
                let mut id = 0;
                for a in self.undo.drain(start..) {
                    id = a.0;
                    group.extend(a.1);
                }
                self.undo.push((id, group));
            }
        }
        Ok(())
    }

    /// Replaces the whole text (a host that owns its buffer and wants the core to catch up), keeping
    /// the encoding and byte order mark; the text's own line endings are kept. One undo step.
    pub fn set_text(&mut self, text: &str) -> Result<()> {
        let n = self.lines.len();
        self.splice(0, n, split_text(text, LineEnding::None))
    }

    /// Gives every line `kind` as its ending, except that a last line without one stays open
    /// (Notepad++'s "Edit > EOL Conversion"). Returns false when nothing changed.
    pub fn convert_eol(&mut self, kind: EolKind) -> Result<bool> {
        self.check_writable()?;
        let want = kind.ending();
        let old: Vec<LineEnding> = self.lines.iter().map(|l| l.eol).collect();
        let new: Vec<LineEnding> = old
            .iter()
            .map(|e| if *e == LineEnding::None { *e } else { want })
            .collect();
        if old == new {
            return Ok(false);
        }
        for (l, e) in self.lines.iter_mut().zip(&new) {
            l.eol = *e;
        }
        self.push_action(vec![Change::Eols { old, new }]);
        Ok(true)
    }

    /// Changes the encoding and byte order mark the document is written in ("Convert to ..."): the
    /// text stays, the bytes change. Refused when the text has characters the encoding cannot
    /// hold. Converting a lossily read document is the way to make it writable: its replacement
    /// characters become part of the text.
    pub fn convert_encoding(&mut self, to: TextEncoding) -> Result<bool> {
        self.check_encodable(to)?;
        if to == self.format && !self.lossy {
            return Ok(false);
        }
        let old = self.format;
        self.format = to;
        self.lossy = false;
        self.push_action(vec![Change::Format { old, new: to }]);
        Ok(true)
    }

    fn apply(&mut self, c: &Change, forward: bool) {
        match c {
            Change::Splice {
                first,
                removed,
                inserted,
            } => {
                let (drop_n, put) = if forward {
                    (removed.len(), inserted)
                } else {
                    (inserted.len(), removed)
                };
                self.lines
                    .splice(*first..*first + drop_n, put.iter().cloned());
            }
            Change::Eols { old, new } => {
                let src = if forward { new } else { old };
                for (l, e) in self.lines.iter_mut().zip(src) {
                    l.eol = *e;
                }
            }
            Change::Format { old, new } => self.format = if forward { *new } else { *old },
        }
    }

    /// Undoes the last action. Returns false when there is none.
    pub fn undo(&mut self) -> bool {
        let Some(a) = self.undo.pop() else {
            return false;
        };
        for c in a.1.iter().rev() {
            self.apply(c, false);
        }
        self.redo.push(a);
        self.revision += 1;
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(a) = self.redo.pop() else {
            return false;
        };
        for c in &a.1 {
            self.apply(c, true);
        }
        self.undo.push(a);
        self.revision += 1;
        true
    }
}

fn bom_len(bytes: &[u8], t: TextEncoding) -> usize {
    // a UTF-8 BOM stays in the decoded text as U+FEFF and is removed by the caller
    match (t.encoding, bytes) {
        (Encoding::Utf16Le, [0xFF, 0xFE, ..]) | (Encoding::Utf16Be, [0xFE, 0xFF, ..]) => 2,
        _ => 0,
    }
}

/// Splits text at `\r\n`, `\n` and `\r`; the last line gets `last` as its ending when the text
/// does not end with one (`LineEnding::None` keeps it open).
pub fn split_text(text: &str, last: LineEnding) -> Vec<EditLine> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let (mut start, mut i) = (0usize, 0usize);
    while i < b.len() {
        match b[i] {
            b'\n' => {
                out.push(EditLine {
                    text: text[start..i].to_string(),
                    eol: LineEnding::Lf,
                });
                i += 1;
                start = i;
            }
            b'\r' => {
                let crlf = b.get(i + 1) == Some(&b'\n');
                out.push(EditLine {
                    text: text[start..i].to_string(),
                    eol: if crlf {
                        LineEnding::CrLf
                    } else {
                        LineEnding::Cr
                    },
                });
                i += if crlf { 2 } else { 1 };
                start = i;
            }
            _ => i += 1,
        }
    }
    if start < b.len() {
        out.push(EditLine {
            text: text[start..].to_string(),
            eol: last,
        });
    }
    out
}

// ---------------------------------------------------------------------------------------------
// the editor: two documents and the compare that shows them

/// What an edit did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EditOutcome {
    /// The documents changed (a copy of identical text changes nothing).
    pub changed: bool,
    /// Lines of the target that were replaced, and lines that replaced them.
    pub removed: u32,
    pub inserted: u32,
}

/// The two documents of a compare and the compare that shows them.
#[derive(Debug)]
pub struct Editor {
    tc: Arc<TextCompare>,
    docs: [EditDoc; 2],
    /// The documents changed and the compare has not been restarted on them yet.
    stale: bool,
    /// Copy the endings of the source lines too (when line endings take part in the compare).
    keep_source_eol: bool,
}

fn idx(side: Side) -> usize {
    usize::from(side == Side::Right)
}

fn other(side: Side) -> Side {
    if side == Side::Left {
        Side::Right
    } else {
        Side::Left
    }
}

impl Editor {
    /// Takes over the documents of a finished compare. Fails while the compare is loading, when it
    /// failed, or when either side is binary.
    pub fn begin(tc: Arc<TextCompare>) -> Result<Editor> {
        tc.wait();
        match tc.status() {
            Status::Done => {}
            Status::Binary => {
                return Err(Error::new(
                    ErrorKind::Unsupported,
                    "a binary file cannot be edited as text; copy the whole file instead",
                ));
            }
            s => {
                return Err(Error::invalid_argument(format!(
                    "the compare is not ready for editing ({s:?})"
                )));
            }
        }
        let [l, r] = tc
            .documents()
            .ok_or_else(|| Error::internal("no documents loaded"))?;
        let keep_source_eol = tc.options().eol == crate::options::EolName::Strict;
        Ok(Editor {
            stale: false,
            docs: [EditDoc::from_document(&l)?, EditDoc::from_document(&r)?],
            tc,
            keep_source_eol,
        })
    }

    /// The compare shows older text than the documents hold (edits were made with `refresh` off).
    pub fn is_stale(&self) -> bool {
        self.stale
    }

    /// Restarts the compare when edits are waiting; copy operations call this first, because they
    /// read the rows.
    fn ensure_fresh(&mut self) -> Result<()> {
        if self.stale {
            self.refresh()?;
        }
        Ok(())
    }

    pub fn compare(&self) -> &Arc<TextCompare> {
        &self.tc
    }

    pub fn doc(&self, side: Side) -> &EditDoc {
        &self.docs[idx(side)]
    }

    pub fn doc_mut(&mut self, side: Side) -> &mut EditDoc {
        &mut self.docs[idx(side)]
    }

    /// Restarts the compare on the current text. The rows the host holds are stale afterwards (the
    /// generation changes); the host pulls them again as it does after a recompare.
    pub fn refresh(&mut self) -> Result<()> {
        self.stale = false;
        self.tc.replace_docs(
            DocSpec::Text(self.docs[0].text()),
            DocSpec::Text(self.docs[1].text()),
        );
        Ok(())
    }

    fn rows(&self, first: u32, last: u32) -> Result<Vec<DiffRow>> {
        self.tc.wait();
        if self.tc.status() != Status::Done {
            return Err(Error::invalid_argument(
                "the compare is not finished (it is aligning the last edit); try again when it is",
            ));
        }
        let total = self.tc.row_count();
        if first > last || last >= total {
            return Err(Error::invalid_argument(format!(
                "rows {first}..={last} are outside the compare ({total} rows)"
            )));
        }
        let mut out = vec![
            DiffRow {
                left: 0,
                right: 0,
                kind: crate::RowKind::Same
            };
            (last - first + 1) as usize
        ];
        let n = self.tc.rows(first, &mut out);
        out.truncate(n);
        Ok(out)
    }

    /// The line index in `side` where a range of rows starting at `row` begins: the first line of
    /// the side at or after `row`, or the end of the side.
    fn target_start(&self, side: Side, row: u32, in_range: &[DiffRow]) -> Result<usize> {
        if let Some(l) = in_range.iter().find_map(|r| line_of(r, side)) {
            return Ok(l as usize);
        }
        // nothing of this side in the range: the insertion point is after the last line before it
        let mut at = row;
        while at > 0 {
            let from = at.saturating_sub(4096);
            let rows = self.rows(from, at - 1)?;
            if let Some(l) = rows.iter().rev().find_map(|r| line_of(r, side)) {
                return Ok(l as usize + 1);
            }
            at = from;
        }
        Ok(0)
    }

    /// Copies what `from` has in rows `first..=last` onto the other side: the lines of `from` in
    /// those rows replace the other side's lines in those rows. A row where `from` has no line (a
    /// gap) deletes the other side's line, as the arrow of a gap does in Beyond Compare.
    pub fn copy_rows(&mut self, from: Side, first: u32, last: u32) -> Result<EditOutcome> {
        self.ensure_fresh()?;
        let rows = self.rows(first, last)?;
        let to = other(from);
        let src: Vec<u32> = rows.iter().filter_map(|r| line_of(r, from)).collect();
        let dst: Vec<u32> = rows.iter().filter_map(|r| line_of(r, to)).collect();
        let start = self.target_start(to, first, &rows)?;
        let new: Vec<EditLine> = {
            let src_doc = &self.docs[idx(from)];
            let dst_doc = &self.docs[idx(to)];
            let dst_end = start + dst.len();
            let dst_eol = dst_doc.default_eol().ending();
            src.iter()
                .enumerate()
                .map(|(k, &l)| {
                    let line = &src_doc.lines[l as usize];
                    let at_end = dst_end >= dst_doc.lines.len() && k + 1 == src.len();
                    let eol = if self.keep_source_eol && line.eol != LineEnding::None {
                        line.eol
                    } else if line.eol == LineEnding::None && at_end {
                        LineEnding::None // the open last line stays the open last line
                    } else {
                        dst_eol
                    };
                    EditLine {
                        text: line.text.clone(),
                        eol,
                    }
                })
                .collect()
        };
        let (removed, inserted) = (dst.len() as u32, new.len() as u32);
        let before = self.docs[idx(to)].revision;
        self.docs[idx(to)].splice(start, dst.len(), new)?;
        let changed = self.docs[idx(to)].revision != before;
        if changed {
            self.refresh()?;
        }
        Ok(EditOutcome {
            changed,
            removed,
            inserted,
        })
    }

    /// Copies one difference section (as the host numbers them: `sections` of the compare).
    pub fn copy_section(&mut self, from: Side, section: u32) -> Result<EditOutcome> {
        self.ensure_fresh()?;
        self.tc.wait();
        let mut s = [crate::session::Section {
            first_row: 0,
            row_count: 0,
            important: false,
        }];
        if self.tc.sections(section, &mut s) == 0 {
            return Err(Error::invalid_argument(format!(
                "there is no difference section {section}"
            )));
        }
        let s = s[0];
        self.copy_rows(from, s.first_row, s.first_row + s.row_count - 1)
    }

    /// Makes the other side's text equal to `from`'s, line endings included. With `with_format`
    /// its encoding and byte order mark follow too.
    pub fn copy_all(&mut self, from: Side, with_format: bool) -> Result<EditOutcome> {
        let to = other(from);
        let lines = self.docs[idx(from)].lines.clone();
        let fmt = self.docs[idx(from)].format;
        let (removed, inserted) = (self.docs[idx(to)].lines.len() as u32, lines.len() as u32);
        let before = self.docs[idx(to)].revision;
        let n = self.docs[idx(to)].lines.len();
        self.docs[idx(to)].splice(0, n, lines)?;
        if with_format {
            self.docs[idx(to)].convert_encoding(fmt)?;
        }
        let changed = self.docs[idx(to)].revision != before;
        if changed {
            self.refresh()?;
        }
        Ok(EditOutcome {
            changed,
            removed,
            inserted,
        })
    }

    /// Replaces `remove` lines of `side` at `first` with the lines of `text`.
    pub fn replace_lines(
        &mut self,
        side: Side,
        first: usize,
        remove: usize,
        text: &str,
        refresh: bool,
    ) -> Result<EditOutcome> {
        let before = self.docs[idx(side)].revision;
        self.docs[idx(side)].replace_text(first, remove, text)?;
        self.after_edit(side, before, refresh)?;
        Ok(EditOutcome {
            changed: self.docs[idx(side)].revision != before,
            removed: remove as u32,
            inserted: split_text(text, LineEnding::None).len() as u32,
        })
    }

    fn after_edit(&mut self, side: Side, before: u64, refresh: bool) -> Result<()> {
        if self.docs[idx(side)].revision != before {
            self.stale = true;
            if refresh {
                self.refresh()?;
            }
        }
        Ok(())
    }

    /// One range edit of a host buffer (see [`RangeEdit`]). With `refresh` off the compare is not
    /// restarted: a host that types into a piece-tree buffer sends its changes this way and asks
    /// for a refresh when typing pauses.
    pub fn replace_range(
        &mut self,
        side: Side,
        edit: &RangeEdit,
        refresh: bool,
    ) -> Result<EditOutcome> {
        let before = self.docs[idx(side)].revision;
        self.docs[idx(side)].replace_range(edit)?;
        self.after_edit(side, before, refresh)?;
        Ok(EditOutcome {
            changed: self.docs[idx(side)].revision != before,
            removed: 0,
            inserted: 0,
        })
    }

    /// A batch of range edits against the current text, as one undo step.
    pub fn apply_edits(
        &mut self,
        side: Side,
        edits: &[RangeEdit],
        refresh: bool,
    ) -> Result<EditOutcome> {
        let before = self.docs[idx(side)].revision;
        self.docs[idx(side)].apply_edits(edits)?;
        self.after_edit(side, before, refresh)?;
        Ok(EditOutcome {
            changed: self.docs[idx(side)].revision != before,
            removed: 0,
            inserted: 0,
        })
    }

    /// Replaces the text of a side by the host's buffer.
    pub fn set_text(&mut self, side: Side, text: &str, refresh: bool) -> Result<EditOutcome> {
        let before = self.docs[idx(side)].revision;
        self.docs[idx(side)].set_text(text)?;
        self.after_edit(side, before, refresh)?;
        Ok(EditOutcome {
            changed: self.docs[idx(side)].revision != before,
            removed: 0,
            inserted: 0,
        })
    }

    pub fn undo(&mut self, side: Side) -> Result<bool> {
        let r = self.docs[idx(side)].undo();
        if r {
            self.refresh()?;
        }
        Ok(r)
    }

    pub fn redo(&mut self, side: Side) -> Result<bool> {
        let r = self.docs[idx(side)].redo();
        if r {
            self.refresh()?;
        }
        Ok(r)
    }

    /// Converts the endings and/or the encoding of `side`.
    pub fn convert(
        &mut self,
        side: Side,
        eol: Option<EolKind>,
        encoding: Option<TextEncoding>,
    ) -> Result<bool> {
        let before = self.docs[idx(side)].revision;
        // check first, so a refused conversion changes nothing
        if let Some(e) = encoding {
            self.docs[idx(side)].check_encodable(e)?;
        }
        let d = &mut self.docs[idx(side)];
        if let Some(k) = eol {
            d.convert_eol(k)?;
        }
        if let Some(e) = encoding {
            d.convert_encoding(e)?;
        }
        let changed = self.docs[idx(side)].revision != before;
        if changed {
            self.refresh()?;
        }
        Ok(changed)
    }

    /// Replaces a side by a document read again, for "Encode in X": the same bytes decoded as
    /// another encoding. The undo history of that side starts over, as in Notepad++.
    pub fn reinterpret(&mut self, side: Side, doc: EditDoc) -> Result<()> {
        self.docs[idx(side)] = doc;
        self.refresh()
    }

    pub fn states(&self) -> [DocState; 2] {
        [self.docs[0].state(), self.docs[1].state()]
    }
}

fn line_of(r: &DiffRow, side: Side) -> Option<u32> {
    if side == Side::Left {
        r.left_opt()
    } else {
        r.right_opt()
    }
}
