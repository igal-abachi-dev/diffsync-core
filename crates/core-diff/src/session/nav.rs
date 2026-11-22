//! Navigation helpers the compare windows of Beyond Compare and Araxis Merge show (backlog
//! E11.5-S9.2, S13.1, S13.2): the folded regions of the *Context* view ("36 filtered lines"), the
//! kind of a difference section (next/previous *replacement*), and find over both sides.

use core_base::{Error, Result};
use regex::RegexBuilder;
use serde::Serialize;

use super::TextCompare;
use super::rows::{DiffRow, Dir, RowKind, Section};
use crate::{ORPHAN, Side};

/// A run of rows the Context view hides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Fold {
    pub first_row: u32,
    pub row_count: u32,
}

/// What a difference section does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SectionKind {
    /// Lines only on the right.
    Insert,
    /// Lines only on the left.
    Delete,
    /// Changed lines (or lines removed on one side and added on the other).
    Replace,
}

/// How [`TextCompare::find`] looks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FindOptions {
    pub case_sensitive: bool,
    pub whole_word: bool,
    /// The query is a regular expression (otherwise a plain string).
    pub regex: bool,
    pub left: bool,
    pub right: bool,
}

impl Default for FindOptions {
    fn default() -> Self {
        FindOptions {
            case_sensitive: false,
            whole_word: false,
            regex: false,
            left: true,
            right: true,
        }
    }
}

/// A match: the row, the side and line it was found on, and the range in UTF-16 units.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct FindHit {
    pub row: u32,
    pub left_side: bool,
    pub line: u32,
    pub start: u32,
    pub end: u32,
}

impl TextCompare {
    /// The runs of rows the *Context* view hides: rows farther than `context` rows from any
    /// difference (`important_only`: from any *important* difference). A file without differences
    /// folds completely. The regions are in row order and never overlap.
    pub fn folds(&self, context: u32, important_only: bool) -> Vec<Fold> {
        let total = self.row_count();
        let interesting = |k: RowKind| {
            if important_only {
                k.is_important()
            } else {
                k.is_difference()
            }
        };
        let mut out = Vec::new();
        let mut rows = vec![
            DiffRow {
                left: ORPHAN,
                right: ORPHAN,
                kind: RowKind::Same
            };
            4096
        ];
        // `run_start`: first row of the current run of boring rows; `has_prev`: an interesting row
        // came before it (its `context` rows after it stay visible)
        let (mut run_start, mut has_prev, mut first) = (0u32, false, 0u32);
        let mut push = |from: u32, to: u32| {
            if to > from {
                out.push(Fold {
                    first_row: from,
                    row_count: to - from,
                });
            }
        };
        while first < total {
            let n = self.rows(first, &mut rows);
            if n == 0 {
                break;
            }
            for (k, r) in rows[..n].iter().enumerate() {
                if !interesting(r.kind) {
                    continue;
                }
                let at = first + k as u32;
                let from = run_start + if has_prev { context } else { 0 };
                let to = at.saturating_sub(context);
                push(from, to);
                run_start = at + 1;
                has_prev = true;
            }
            first += n as u32;
        }
        let from = run_start + if has_prev { context } else { 0 };
        push(from, total);
        out
    }

    /// The kind of a section (as returned by `sections`).
    pub fn section_kind(&self, s: &Section) -> SectionKind {
        let mut rows = vec![
            DiffRow {
                left: ORPHAN,
                right: ORPHAN,
                kind: RowKind::Same
            };
            1024
        ];
        let (mut left_only, mut right_only, mut paired) = (false, false, false);
        let (mut at, end) = (s.first_row, s.first_row.saturating_add(s.row_count));
        while at < end {
            let n = self.rows(at, &mut rows);
            if n == 0 {
                break;
            }
            for r in &rows[..n.min((end - at) as usize)] {
                match (r.left_opt(), r.right_opt()) {
                    (Some(_), Some(_)) => paired = true,
                    (Some(_), None) => left_only = true,
                    (None, Some(_)) => right_only = true,
                    (None, None) => {}
                }
            }
            at += n as u32;
        }
        match (paired, left_only, right_only) {
            (false, true, false) => SectionKind::Delete,
            (false, false, true) => SectionKind::Insert,
            _ => SectionKind::Replace,
        }
    }

    /// The first row at or after `from_row` (`Dir::Next`) or at or before it (`Dir::Prev`) whose
    /// section has this kind; the row is the first row of that section. `None` at the ends.
    pub fn find_section_of_kind(&self, from_row: u32, dir: Dir, kind: SectionKind) -> Option<u32> {
        let total = self.stats().sections;
        let mut secs = vec![
            Section {
                first_row: 0,
                row_count: 0,
                important: false
            };
            256
        ];
        let mut all: Vec<Section> = Vec::new();
        let mut first = 0u32;
        while first < total {
            let n = self.sections(first, &mut secs);
            if n == 0 {
                break;
            }
            all.extend_from_slice(&secs[..n]);
            first += n as u32;
        }
        let ok = |s: &&Section| self.section_kind(s) == kind;
        match dir {
            Dir::Next => all
                .iter()
                .filter(|s| s.first_row + s.row_count > from_row)
                .find(ok)
                .map(|s| s.first_row.max(from_row)),
            Dir::Prev => all
                .iter()
                .rev()
                .filter(|s| s.first_row <= from_row)
                .find(ok)
                .map(|s| s.first_row),
        }
    }

    /// Finds `query` in the lines of the rows, starting at `from_row` (inclusive) and going
    /// forwards or backwards. Within a row the left side is searched before the right one. A bad
    /// regular expression is `InvalidArgument`.
    pub fn find(
        &self,
        query: &str,
        o: &FindOptions,
        from_row: u32,
        dir: Dir,
    ) -> Result<Option<FindHit>> {
        if query.is_empty() {
            return Ok(None);
        }
        let pat = if o.regex {
            query.to_string()
        } else {
            regex::escape(query)
        };
        let pat = if o.whole_word {
            format!(r"\b(?:{pat})\b")
        } else {
            pat
        };
        let re = RegexBuilder::new(&pat)
            .case_insensitive(!o.case_sensitive)
            .build()
            .map_err(|e| Error::invalid_argument(format!("bad search pattern: {e}")))?;
        let total = self.row_count();
        if total == 0 {
            return Ok(None);
        }
        let mut row = DiffRow {
            left: ORPHAN,
            right: ORPHAN,
            kind: RowKind::Same,
        };
        let mut buf = Vec::new();
        let mut at = i64::from(from_row.min(total - 1));
        let step: i64 = if dir == Dir::Next { 1 } else { -1 };
        while (0..i64::from(total)).contains(&at) {
            if self.rows(at as u32, std::slice::from_mut(&mut row)) == 1 {
                for (left_side, idx, wanted) in [
                    (true, row.left_opt(), o.left),
                    (false, row.right_opt(), o.right),
                ] {
                    let (Some(line), true) = (idx, wanted) else {
                        continue;
                    };
                    let side = if left_side { Side::Left } else { Side::Right };
                    if !self.line_utf16(side, line, &mut buf) {
                        continue;
                    }
                    let text = String::from_utf16_lossy(&buf);
                    if let Some(m) = re.find(&text) {
                        let utf16 = |byte: usize| text[..byte].encode_utf16().count() as u32;
                        return Ok(Some(FindHit {
                            row: at as u32,
                            left_side,
                            line,
                            start: utf16(m.start()),
                            end: utf16(m.end()),
                        }));
                    }
                }
            }
            at += step;
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::options::{AlignmentName, WhitespaceName};
    use crate::session::DocSpec;
    use crate::udiff::diff_text_options;

    fn cmp(a: &str, b: &str) -> std::sync::Arc<TextCompare> {
        let tc = TextCompare::start(
            DocSpec::Text(a.into()),
            DocSpec::Text(b.into()),
            diff_text_options(AlignmentName::Myers, WhitespaceName::All, false),
            None,
        )
        .unwrap();
        tc.wait();
        tc
    }

    fn lines(n: u32) -> String {
        (1..=n).map(|i| format!("line {i}\n")).collect()
    }

    #[test]
    fn folds_hide_the_rows_far_from_a_difference() {
        let a = lines(30);
        let b = a.replace("line 15\n", "CHANGED\n");
        let tc = cmp(&a, &b);
        // row 14 (0-based) differs: with 3 rows of context rows 11..=17 stay, 0..11 and 18..30 fold
        assert_eq!(
            tc.folds(3, false),
            [
                Fold {
                    first_row: 0,
                    row_count: 11
                },
                Fold {
                    first_row: 18,
                    row_count: 12
                }
            ]
        );
        assert_eq!(tc.folds(100, false), [], "everything is within the context");
        // two differences close together leave nothing folded between them
        let c = a.replace("line 15\n", "X\n").replace("line 19\n", "Y\n");
        let tc = cmp(&a, &c);
        // differences at rows 14 and 18: rows 19..=21 stay visible, 22.. folds
        assert_eq!(
            tc.folds(3, false),
            [
                Fold {
                    first_row: 0,
                    row_count: 11
                },
                Fold {
                    first_row: 22,
                    row_count: 8
                }
            ]
        );
        // equal files fold completely
        let tc = cmp(&a, &a);
        assert_eq!(
            tc.folds(3, false),
            [Fold {
                first_row: 0,
                row_count: 30
            }]
        );
        assert!(cmp("", "").folds(3, false).is_empty());
    }

    #[test]
    fn section_kinds_tell_insertions_deletions_and_replacements_apart() {
        let tc = cmp("a\nb\nc\nd\ne\nf\ng\n", "a\nB\nc\nd\nf\ng\nh\ni\n");
        let mut secs = [Section {
            first_row: 0,
            row_count: 0,
            important: false,
        }; 8];
        let n = tc.sections(0, &mut secs);
        let kinds: Vec<SectionKind> = secs[..n].iter().map(|s| tc.section_kind(s)).collect();
        assert_eq!(
            kinds,
            [
                SectionKind::Replace,
                SectionKind::Delete,
                SectionKind::Insert
            ]
        );
        assert_eq!(
            tc.find_section_of_kind(0, Dir::Next, SectionKind::Delete),
            Some(secs[1].first_row)
        );
        assert_eq!(
            tc.find_section_of_kind(100, Dir::Prev, SectionKind::Replace),
            Some(secs[0].first_row)
        );
        assert_eq!(
            tc.find_section_of_kind(secs[2].first_row + 5, Dir::Next, SectionKind::Insert),
            None
        );
    }

    #[test]
    fn find_walks_rows_and_sides_with_utf16_offsets() {
        let tc = cmp("alpha\nbeta\ngamma delta\n", "alpha\nBETA x\ngamma\n");
        let o = FindOptions::default();
        let hit = tc.find("beta", &o, 0, Dir::Next).unwrap().unwrap();
        assert_eq!(
            (hit.line, hit.left_side, hit.start, hit.end),
            (1, true, 0, 4)
        );
        let next = tc.find("beta", &o, hit.row, Dir::Next).unwrap();
        assert!(next.is_some());
        let cs = FindOptions {
            case_sensitive: true,
            ..o
        };
        assert!(
            tc.find("BETA", &cs, 0, Dir::Next)
                .unwrap()
                .is_some_and(|h| !h.left_side)
        );
        let right_only = FindOptions { left: false, ..o };
        assert_eq!(tc.find("delta", &right_only, 0, Dir::Next).unwrap(), None);
        // backwards, whole words, regular expressions and a bad pattern
        assert_eq!(
            tc.find(
                "a",
                &FindOptions {
                    whole_word: true,
                    ..o
                },
                5,
                Dir::Prev
            )
            .unwrap(),
            None
        );
        let re = FindOptions { regex: true, ..o };
        assert_eq!(
            tc.find("g.mma", &re, 0, Dir::Next).unwrap().unwrap().start,
            0
        );
        assert!(tc.find("(", &re, 0, Dir::Next).is_err());
        // a character outside the BMP counts as two UTF-16 units
        let tc = cmp("x\u{1F600}needle\n", "x\u{1F600}needle\n");
        assert_eq!(
            tc.find("needle", &o, 0, Dir::Next).unwrap().unwrap().start,
            3
        );
    }
}
