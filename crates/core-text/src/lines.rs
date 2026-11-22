//! Line index: where each line starts and how it ends (plan.md §4.1).
//!
//! The index is built over the UTF-8 text with `memchr2` and keeps every line's own ending,
//! so mixed endings survive a load → save round trip. CRLF, LF and CR are recognised; a
//! final line without an ending gets [`LineEnding::None`]; a trailing ending does not create
//! an extra empty line; an empty text has no lines.

use core_base::{Error, ErrorKind, Result};

/// How a line is terminated. The numeric values index [`LineIndex::ending_counts`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum LineEnding {
    None = 0,
    CrLf = 1,
    Lf = 2,
    Cr = 3,
}

impl LineEnding {
    /// The ending as text (`""` for the last line of a file without a final newline).
    pub const fn as_str(self) -> &'static str {
        match self {
            LineEnding::None => "",
            LineEnding::CrLf => "\r\n",
            LineEnding::Lf => "\n",
            LineEnding::Cr => "\r",
        }
    }

    pub const fn len(self) -> usize {
        self.as_str().len()
    }

    pub const fn is_empty(self) -> bool {
        matches!(self, LineEnding::None)
    }
}

/// One line: byte offset of its text in the document text, text length (without the ending)
/// and the ending. 16 bytes per line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineSpan {
    pub start: u64,
    pub len: u32,
    pub ending: LineEnding,
}

const _: () = assert!(size_of::<LineSpan>() == 16);

impl LineSpan {
    /// Offset just after the line text (where its ending starts).
    pub fn text_end(&self) -> u64 {
        self.start + u64::from(self.len)
    }

    /// Offset of the next line (after the ending).
    pub fn next_start(&self) -> u64 {
        self.text_end() + self.ending.len() as u64
    }
}

/// The line spans of one text plus how many lines end each way.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LineIndex {
    pub spans: Vec<LineSpan>,
    /// Counts per [`LineEnding`] value: `[None, CrLf, Lf, Cr]`.
    pub ending_counts: [u64; 4],
}

impl LineIndex {
    pub fn len(&self) -> usize {
        self.spans.len()
    }

    pub fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }
}

/// Splits `text` into lines.
///
/// Fails with `Unsupported` when a single line is longer than 4 GiB or the file has more
/// than `u32::MAX - 1` lines (the ABI addresses lines with `u32`, and `u32::MAX` means
/// "absent").
pub fn index_lines(text: &[u8]) -> Result<LineIndex> {
    // Average line length guess keeps reallocation rare without over-reserving.
    let mut spans: Vec<LineSpan> = Vec::with_capacity(text.len() / 48 + 1);
    let mut counts = [0u64; 4];
    let mut start = 0usize;
    // Position of a `\n` that already belongs to a CRLF pair and must be skipped.
    let mut skip_nl = usize::MAX;

    let mut push = |start: usize, end: usize, ending: LineEnding| -> Result<()> {
        let len = u32::try_from(end - start)
            .map_err(|_| Error::new(ErrorKind::Unsupported, "a line is longer than 4 GiB"))?;
        spans.push(LineSpan {
            start: start as u64,
            len,
            ending,
        });
        counts[ending as usize] += 1;
        Ok(())
    };

    for p in memchr::memchr2_iter(b'\r', b'\n', text) {
        if p == skip_nl {
            continue;
        }
        if text[p] == b'\n' {
            push(start, p, LineEnding::Lf)?;
            start = p + 1;
        } else if text.get(p + 1) == Some(&b'\n') {
            push(start, p, LineEnding::CrLf)?;
            skip_nl = p + 1;
            start = p + 2;
        } else {
            push(start, p, LineEnding::Cr)?;
            start = p + 1;
        }
    }
    if start < text.len() {
        push(start, text.len(), LineEnding::None)?;
    }
    if spans.len() >= u32::MAX as usize {
        return Err(Error::new(
            ErrorKind::Unsupported,
            "more than 4 billion lines",
        ));
    }
    spans.shrink_to_fit();
    Ok(LineIndex {
        spans,
        ending_counts: counts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(text: &str) -> Vec<(String, LineEnding)> {
        let idx = index_lines(text.as_bytes()).unwrap();
        idx.spans
            .iter()
            .map(|s| {
                let a = s.start as usize;
                (text[a..a + s.len as usize].to_string(), s.ending)
            })
            .collect()
    }

    use LineEnding::{Cr, CrLf, Lf, None as NoEnd};

    #[test]
    fn empty_text_has_no_lines() {
        assert!(split("").is_empty());
        assert!(index_lines(b"").unwrap().is_empty());
    }

    #[test]
    fn single_endings() {
        assert_eq!(split("a\nb\n"), [("a".into(), Lf), ("b".into(), Lf)]);
        assert_eq!(
            split("a\r\nb\r\n"),
            [("a".into(), CrLf), ("b".into(), CrLf)]
        );
        assert_eq!(split("a\rb\r"), [("a".into(), Cr), ("b".into(), Cr)]);
    }

    #[test]
    fn final_line_without_ending() {
        assert_eq!(split("a\nb"), [("a".into(), Lf), ("b".into(), NoEnd)]);
        assert_eq!(split("x"), [("x".into(), NoEnd)]);
    }

    #[test]
    fn trailing_ending_does_not_add_an_empty_line() {
        assert_eq!(split("a\n").len(), 1);
        assert_eq!(split("\n").len(), 1);
        assert_eq!(split("\n\n").len(), 2);
    }

    #[test]
    fn empty_lines_are_lines() {
        assert_eq!(split("\r\n\r\n"), [("".into(), CrLf), ("".into(), CrLf)]);
        assert_eq!(
            split("a\n\nb"),
            [("a".into(), Lf), ("".into(), Lf), ("b".into(), NoEnd)]
        );
    }

    #[test]
    fn mixed_endings_are_preserved_per_line() {
        assert_eq!(
            split("a\r\nb\nc\rd"),
            [
                ("a".into(), CrLf),
                ("b".into(), Lf),
                ("c".into(), Cr),
                ("d".into(), NoEnd)
            ]
        );
    }

    #[test]
    fn lf_cr_sequences_are_two_endings() {
        // "\n\r" is LF then CR, never CRLF.
        assert_eq!(
            split("a\n\rb"),
            [("a".into(), Lf), ("".into(), Cr), ("b".into(), NoEnd)]
        );
        // "\r\r\n" is CR then CRLF.
        assert_eq!(
            split("a\r\r\nb"),
            [("a".into(), Cr), ("".into(), CrLf), ("b".into(), NoEnd)]
        );
    }

    #[test]
    fn cr_at_the_very_end_is_a_cr_ending() {
        assert_eq!(split("a\r"), [("a".into(), Cr)]);
    }

    #[test]
    fn counts_match_the_spans() {
        let idx = index_lines(b"a\r\nb\nc\rd\r\n").unwrap();
        assert_eq!(idx.ending_counts, [0, 2, 1, 1]);
        assert_eq!(idx.len(), 4);
    }

    #[test]
    fn span_is_16_bytes_and_helpers_walk_the_text() {
        assert_eq!(size_of::<LineSpan>(), 16);
        let idx = index_lines(b"ab\r\ncd\n").unwrap();
        let s = idx.spans[0];
        assert_eq!((s.start, s.text_end(), s.next_start()), (0, 2, 4));
        assert_eq!(idx.spans[1].start, 4);
    }

    #[test]
    fn spans_tile_the_text_exactly() {
        let text = "x\r\ny\n\nz\rq";
        let idx = index_lines(text.as_bytes()).unwrap();
        let mut rebuilt = String::new();
        for s in &idx.spans {
            let a = s.start as usize;
            rebuilt.push_str(&text[a..a + s.len as usize]);
            rebuilt.push_str(s.ending.as_str());
        }
        assert_eq!(rebuilt, text);
    }
}
