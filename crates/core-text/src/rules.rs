//! Line rules: from a line to its *comparable text* (plan.md §4.2, backlog E1-S4).
//!
//! Pipeline order: remove "unimportant" regex matches → apply the whitespace mode → fold
//! case. A line whose comparable text is empty is *insignificant* (blank or only
//! unimportant text). Lines that differ only in ignored whitespace or unimportant text become
//! **Unimportant** rows downstream.

use std::ops::Range;

use core_base::{Error, Result};
use regex::Regex;

use crate::fold;

/// Whitespace handling (decision D3). The default is `IgnoreLeadingTrailing`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WhitespaceMode {
    /// Every whitespace character counts.
    CompareAll,
    /// Indentation and trailing blanks are unimportant.
    #[default]
    IgnoreLeadingTrailing,
    /// Inner blank runs compare as one space; leading and trailing blanks are dropped.
    IgnoreChanges,
    /// Whitespace is ignored completely.
    IgnoreAll,
}

/// The characters whitespace rules treat as blanks (decision D4): the Delphi set (space, TAB,
/// VT, FF, NBSP, ZWNBSP) plus the other Unicode space separators. CR and LF never occur inside
/// a line.
#[inline]
pub const fn is_blank(c: char) -> bool {
    matches!(
        c,
        ' ' | '\t' | '\u{0B}' | '\u{0C}' | '\u{A0}' | '\u{FEFF}' | '\u{1680}' | '\u{2000}'
            ..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}'
    )
}

/// Reusable buffers for [`LineRules::normalize`]. Keep one per thread and pass it to every
/// call: after warm-up no call allocates.
#[derive(Debug, Default)]
pub struct Scratch {
    cur: String,
    tmp: String,
}

impl Scratch {
    pub fn new() -> Self {
        Self::default()
    }
}

/// How line endings take part in the comparison (decision: D55). A visual compare ignores them; a
/// patch has to carry them, so `diff`-style output compares them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EolCompare {
    /// Endings are not compared (the default).
    #[default]
    Ignore,
    /// CRLF, LF, CR and "no ending" (the last line of a file) are four different endings.
    Strict,
    /// CRLF and LF are the same (`--strip-trailing-cr`); a missing final ending still differs.
    CrLfAsLf,
}

/// The rules that decide what part of a line is important.
#[derive(Debug, Clone, Default)]
pub struct LineRules {
    ignore_case: bool,
    whitespace: WhitespaceMode,
    unimportant: Vec<Regex>,
    eol: EolCompare,
}

impl LineRules {
    /// Default rules: `IgnoreLeadingTrailing`, case-sensitive, no unimportant patterns.
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with_eol(mut self, eol: EolCompare) -> Self {
        self.eol = eol;
        self
    }

    pub fn eol(&self) -> EolCompare {
        self.eol
    }

    /// A number mixed into the line hash so that lines with different endings differ (0 when
    /// endings are not compared). Decision: D55.
    pub fn eol_salt(&self, e: crate::lines::LineEnding) -> u32 {
        use crate::lines::LineEnding;
        let code: u32 = match (self.eol, e) {
            (EolCompare::Ignore, _) => return 0,
            (_, LineEnding::None) => 4,
            (EolCompare::CrLfAsLf, LineEnding::CrLf) | (_, LineEnding::Lf) => 2,
            (_, LineEnding::CrLf) => 1,
            (_, LineEnding::Cr) => 3,
        };
        code.wrapping_mul(0x9E37_79B1)
    }

    #[must_use]
    pub fn with_ignore_case(mut self, on: bool) -> Self {
        self.ignore_case = on;
        self
    }

    #[must_use]
    pub fn with_whitespace(mut self, mode: WhitespaceMode) -> Self {
        self.whitespace = mode;
        self
    }

    /// Adds regexes whose matches are unimportant text. They are applied in order, each to
    /// the result of the previous one. An invalid pattern is an `InvalidArgument` error.
    pub fn with_unimportant<I, S>(mut self, patterns: I) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        for p in patterns {
            let p = p.as_ref();
            let re = Regex::new(p).map_err(|e| {
                Error::invalid_argument(format!("bad unimportant pattern `{p}`: {e}"))
            })?;
            self.unimportant.push(re);
        }
        Ok(self)
    }

    pub fn ignore_case(&self) -> bool {
        self.ignore_case
    }

    pub fn whitespace(&self) -> WhitespaceMode {
        self.whitespace
    }

    pub fn unimportant_patterns(&self) -> impl Iterator<Item = &str> {
        self.unimportant.iter().map(Regex::as_str)
    }

    /// The comparable text of `line`. It borrows `line` when no rule changes it (or only
    /// trims it) and otherwise lives in `scratch`, so a line the rules leave alone costs no
    /// allocation. An empty result means the line is insignificant.
    pub fn normalize<'a>(&self, line: &'a str, scratch: &'a mut Scratch) -> &'a str {
        let Scratch { cur, tmp } = scratch;
        // The current text is `line[range]` while `in_cur` is false, else `cur[range]`.
        let mut in_cur = false;
        let mut range = 0..line.len();

        macro_rules! text {
            () => {
                if in_cur {
                    &cur[range.clone()]
                } else {
                    &line[range.clone()]
                }
            };
        }
        // Rewrites the text into `tmp` with `$fill`, then makes `tmp` the current text.
        macro_rules! rewrite {
            ($fill:expr) => {{
                tmp.clear();
                let text: &str = text!();
                #[allow(clippy::redundant_closure_call)]
                ($fill)(text, &mut *tmp);
                std::mem::swap(cur, tmp);
                in_cur = true;
                range = 0..cur.len();
            }};
        }

        // 1. unimportant text (spec ALIGN-1.2, `Normalize` of Dc.TextLines)
        for re in &self.unimportant {
            if re.is_match(text!()) {
                rewrite!(|t: &str, out: &mut String| remove_matches(re, t, out));
            }
        }

        // 2. whitespace (decision D3)
        match self.whitespace {
            WhitespaceMode::CompareAll => {}
            WhitespaceMode::IgnoreLeadingTrailing => {
                let t: &str = text!();
                let start = t.len() - t.trim_start_matches(is_blank).len();
                let end = t.trim_end_matches(is_blank).len();
                range = if start >= end {
                    range.start..range.start
                } else {
                    range.start + start..range.start + end
                };
            }
            WhitespaceMode::IgnoreAll => {
                if text!().chars().any(is_blank) {
                    rewrite!(
                        |t: &str, out: &mut String| out.extend(t.chars().filter(|&c| !is_blank(c)))
                    );
                }
            }
            WhitespaceMode::IgnoreChanges => {
                if collapse_needed(text!()) {
                    rewrite!(|t: &str, out: &mut String| collapse_into(t, out));
                }
            }
        }

        // 3. case (decision D5): simple folding, one code point at a time
        if self.ignore_case && fold::needs_fold(text!()) {
            rewrite!(|t: &str, out: &mut String| fold::fold_into(t, out));
        }

        if in_cur { &cur[range] } else { &line[range] }
    }

    /// The parts of `line` the rules treat as unimportant, as UTF-8 byte ranges of `line`,
    /// sorted and merged. Used to colour in-line differences.
    ///
    /// Included: text removed by the unimportant regexes, and the blanks the whitespace mode
    /// ignores (leading and trailing blanks; every blank for `IgnoreAll`; for `IgnoreChanges`
    /// also inner runs other than a single plain space). Case folding removes nothing.
    pub fn unimportant_ranges(&self, line: &str, out: &mut Vec<Range<u32>>) {
        out.clear();
        if line.is_empty() {
            return;
        }
        // `kept`: the parts of `line` still in the comparable text. `cur` is their concatenation.
        let mut kept: Vec<Range<usize>> = Vec::with_capacity(4);
        kept.push(0..line.len());
        let mut removed: Vec<Range<usize>> = Vec::new();
        let mut cur = line.to_owned();
        for re in &self.unimportant {
            let cuts: Vec<Range<usize>> = re
                .find_iter(&cur)
                .map(|m| m.range())
                .filter(|r| !r.is_empty())
                .collect();
            if !cuts.is_empty() {
                apply_cuts(&mut kept, &mut removed, &cuts);
                cur = join(line, &kept);
            }
        }
        let cuts = whitespace_cuts(&cur, self.whitespace);
        if !cuts.is_empty() {
            apply_cuts(&mut kept, &mut removed, &cuts);
        }
        removed.sort_by_key(|r| r.start);
        let mut merged: Vec<Range<usize>> = Vec::with_capacity(removed.len());
        for r in removed {
            match merged.last_mut() {
                Some(last) if r.start <= last.end => last.end = last.end.max(r.end),
                _ => merged.push(r),
            }
        }
        out.extend(merged.into_iter().map(|r| r.start as u32..r.end as u32));
    }
}

fn remove_matches(re: &Regex, text: &str, out: &mut String) {
    let mut last = 0;
    for m in re.find_iter(text) {
        out.push_str(&text[last..m.start()]);
        last = m.end();
    }
    out.push_str(&text[last..]);
}

/// True when `IgnoreChanges` would rewrite `text`: a leading or trailing blank, a blank that
/// is not a plain space, or two blanks in a row.
fn collapse_needed(text: &str) -> bool {
    let mut prev_blank = false;
    let mut first = true;
    for c in text.chars() {
        let blank = is_blank(c);
        if blank && (first || prev_blank || c != ' ') {
            return true;
        }
        prev_blank = blank;
        first = false;
    }
    prev_blank
}

fn collapse_into(text: &str, out: &mut String) {
    let mut in_blank = false;
    for c in text.chars() {
        if is_blank(c) {
            in_blank = true;
        } else {
            if in_blank && !out.is_empty() {
                out.push(' ');
            }
            in_blank = false;
            out.push(c);
        }
    }
}

/// Blank runs of `text` as byte ranges.
fn blank_runs(text: &str) -> Vec<Range<usize>> {
    let mut runs = Vec::new();
    let mut start: Option<usize> = None;
    for (i, c) in text.char_indices() {
        match (is_blank(c), start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                runs.push(s..i);
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        runs.push(s..text.len());
    }
    runs
}

fn whitespace_cuts(text: &str, mode: WhitespaceMode) -> Vec<Range<usize>> {
    let runs = blank_runs(text);
    match mode {
        WhitespaceMode::CompareAll => Vec::new(),
        WhitespaceMode::IgnoreAll => runs,
        WhitespaceMode::IgnoreLeadingTrailing => runs
            .into_iter()
            .filter(|r| r.start == 0 || r.end == text.len())
            .collect(),
        WhitespaceMode::IgnoreChanges => runs
            .into_iter()
            .filter(|r| r.start == 0 || r.end == text.len() || &text[r.clone()] != " ")
            .collect(),
    }
}

/// Removes `cuts` (sorted, non-overlapping ranges of the *current* text) from `kept` (ranges
/// of the original line whose concatenation is the current text) and records the removed
/// parts, in original coordinates, in `removed`.
fn apply_cuts(
    kept: &mut Vec<Range<usize>>,
    removed: &mut Vec<Range<usize>>,
    cuts: &[Range<usize>],
) {
    let mut new_kept = Vec::with_capacity(kept.len() + cuts.len());
    let mut ci = 0;
    let mut pos = 0; // offset in the current text where `seg` starts
    for seg in kept.iter() {
        let seg_cur = pos..pos + seg.len();
        let mut a = seg.start;
        while ci < cuts.len() && cuts[ci].start < seg_cur.end {
            let c = &cuts[ci];
            let s = c.start.max(seg_cur.start);
            let e = c.end.min(seg_cur.end);
            if s < e {
                let os = seg.start + (s - seg_cur.start);
                let oe = seg.start + (e - seg_cur.start);
                if os > a {
                    new_kept.push(a..os);
                }
                removed.push(os..oe);
                a = oe;
            }
            if c.end <= seg_cur.end {
                ci += 1;
            } else {
                break; // the cut continues in the next segment
            }
        }
        if a < seg.end {
            new_kept.push(a..seg.end);
        }
        pos = seg_cur.end;
    }
    *kept = new_kept;
}

fn join(line: &str, kept: &[Range<usize>]) -> String {
    kept.iter().map(|r| &line[r.clone()]).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn norm(rules: &LineRules, line: &str) -> String {
        let mut s = Scratch::new();
        rules.normalize(line, &mut s).to_owned()
    }

    fn rules(mode: WhitespaceMode) -> LineRules {
        LineRules::new().with_whitespace(mode)
    }

    /// The §4.2 table (decision D3) for `"  a   b  "`.
    #[test]
    fn whitespace_mode_table() {
        let line = "  a   b  ";
        assert_eq!(norm(&rules(WhitespaceMode::CompareAll), line), "  a   b  ");
        assert_eq!(
            norm(&rules(WhitespaceMode::IgnoreLeadingTrailing), line),
            "a   b"
        );
        assert_eq!(norm(&rules(WhitespaceMode::IgnoreChanges), line), "a b");
        assert_eq!(norm(&rules(WhitespaceMode::IgnoreAll), line), "ab");
    }

    #[test]
    fn default_mode_is_ignore_leading_trailing() {
        assert_eq!(
            LineRules::new().whitespace(),
            WhitespaceMode::IgnoreLeadingTrailing
        );
        assert_eq!(norm(&LineRules::new(), "\t x \t"), "x");
    }

    #[test]
    fn blank_only_lines_are_insignificant_in_every_mode_but_compare_all() {
        for mode in [
            WhitespaceMode::IgnoreLeadingTrailing,
            WhitespaceMode::IgnoreChanges,
            WhitespaceMode::IgnoreAll,
        ] {
            assert_eq!(norm(&rules(mode), " \t  "), "", "{mode:?}");
            assert_eq!(norm(&rules(mode), ""), "", "{mode:?}");
        }
        assert_eq!(norm(&rules(WhitespaceMode::CompareAll), "  "), "  ");
    }

    #[test]
    fn blank_set_is_the_delphi_set_plus_unicode_spaces() {
        // D4: SP TAB VT FF NBSP ZWNBSP + U+1680, U+2000..U+200A, U+202F, U+205F, U+3000.
        let blanks = [
            ' ', '\t', '\u{0B}', '\u{0C}', '\u{A0}', '\u{FEFF}', '\u{1680}', '\u{2000}',
            '\u{2005}', '\u{200A}', '\u{202F}', '\u{205F}', '\u{3000}',
        ];
        for c in blanks {
            assert!(is_blank(c), "U+{:04X} should be blank", c as u32);
            let line = format!("{c}x{c}");
            assert_eq!(
                norm(&rules(WhitespaceMode::IgnoreLeadingTrailing), &line),
                "x"
            );
        }
        // Not blank: zero width space (not a separator), NEL, line separator, letters.
        for c in ['\u{200B}', '\u{85}', '\u{2028}', 'x', '0', '\u{2E80}'] {
            assert!(!is_blank(c), "U+{:04X} should not be blank", c as u32);
        }
    }

    #[test]
    fn nbsp_and_ideographic_space_count_as_whitespace() {
        let r = rules(WhitespaceMode::IgnoreAll);
        assert_eq!(norm(&r, "a\u{A0}b\u{3000}c"), "abc");
        let r = rules(WhitespaceMode::IgnoreChanges);
        assert_eq!(norm(&r, "a\u{A0}\u{A0}b"), "a b");
    }

    #[test]
    fn ignore_changes_keeps_a_single_plain_space_unchanged() {
        let r = rules(WhitespaceMode::IgnoreChanges);
        assert_eq!(norm(&r, "a b c"), "a b c");
        assert_eq!(norm(&r, "a\tb"), "a b"); // a tab is a blank that must become a space
        assert_eq!(norm(&r, "a  b"), "a b");
    }

    #[test]
    fn regexes_are_applied_first_and_in_order() {
        let r = LineRules::new()
            .with_unimportant(["/\\*.*?\\*/", "//.*$"])
            .unwrap();
        assert_eq!(norm(&r, "  int x; /* c */ // tail"), "int x;");
        // Comment-only lines become empty (insignificant).
        assert_eq!(norm(&r, "   // just a comment"), "");
        assert_eq!(norm(&r, "/* a */ /* b */"), "");
    }

    #[test]
    fn regex_output_feeds_the_next_regex() {
        // The second pattern only matches once the first removed the text between the Xs.
        let r = LineRules::new().with_unimportant(["b+", "aa"]).unwrap();
        assert_eq!(norm(&r, "abbba"), "");
    }

    #[test]
    fn invalid_pattern_is_an_invalid_argument_error() {
        let err = LineRules::new()
            .with_unimportant(["(unclosed"])
            .unwrap_err();
        assert_eq!(err.kind(), core_base::ErrorKind::InvalidArgument);
        assert!(err.message().contains("(unclosed"));
    }

    #[test]
    fn ignore_case_uses_simple_folding() {
        let r = LineRules::new()
            .with_ignore_case(true)
            .with_whitespace(WhitespaceMode::CompareAll);
        assert_eq!(norm(&r, "HeLLo"), "hello");
        assert_eq!(norm(&r, "ΣΑΣ"), norm(&r, "σας"));
        assert_eq!(norm(&r, "İ"), "İ");
        assert_eq!(norm(&r, "Straße"), "straße");
        assert_eq!(norm(&r, "\u{212A}"), "k"); // KELVIN SIGN
    }

    #[test]
    fn pipeline_order_is_regex_then_whitespace_then_case() {
        let r = LineRules::new()
            .with_ignore_case(true)
            .with_whitespace(WhitespaceMode::IgnoreAll)
            .with_unimportant(["#.*$"])
            .unwrap();
        assert_eq!(norm(&r, "  A B  # NOTE"), "ab");
    }

    #[test]
    fn unchanged_lines_are_borrowed_from_the_input() {
        let r = LineRules::new();
        let mut s = Scratch::new();
        let line = String::from("already trimmed");
        let out = r.normalize(&line, &mut s);
        assert_eq!(
            out.as_ptr(),
            line.as_ptr(),
            "no copy when no rule changes the line"
        );
        // Trimming slices the input, it does not copy either.
        let line = String::from("  padded  ");
        let out = r.normalize(&line, &mut s);
        assert_eq!(out, "padded");
        assert_eq!(out.as_ptr() as usize, line.as_ptr() as usize + 2);
    }

    #[test]
    fn scratch_is_reusable_across_calls() {
        let r = LineRules::new()
            .with_ignore_case(true)
            .with_whitespace(WhitespaceMode::IgnoreAll);
        let mut s = Scratch::new();
        assert_eq!(r.normalize("A B", &mut s), "ab");
        assert_eq!(r.normalize("C D E", &mut s), "cde");
        assert_eq!(r.normalize("", &mut s), "");
    }

    fn ranges(r: &LineRules, line: &str) -> Vec<Range<u32>> {
        let mut out = Vec::new();
        r.unimportant_ranges(line, &mut out);
        out
    }

    /// Text outside the ranges, joined, equals the comparable text before case folding.
    fn outside(line: &str, ranges: &[Range<u32>]) -> String {
        let mut s = String::new();
        let mut at = 0usize;
        for r in ranges {
            s.push_str(&line[at..r.start as usize]);
            at = r.end as usize;
        }
        s.push_str(&line[at..]);
        s
    }

    #[test]
    fn ranges_cover_exactly_the_removed_text() {
        let r = LineRules::new().with_unimportant(["//.*$"]).unwrap();
        let line = "  x = 1; // note";
        let got = ranges(&r, line);
        // the comment and the two leading blanks; the blank before `//` stays part of the
        // comparable text until trimming removes it as a trailing blank
        assert_eq!(outside(line, &got), norm(&r, line));
        assert_eq!(got, vec![0..2, 8..line.len() as u32]);
    }

    #[test]
    fn ranges_for_every_mode_reproduce_the_normalized_text() {
        let lines = [
            "  a   b  ",
            "x",
            "",
            "   ",
            "a\t b",
            "/* c */ x /* d */ y",
            " é  Ω  ",
        ];
        for mode in [
            WhitespaceMode::IgnoreLeadingTrailing,
            WhitespaceMode::IgnoreAll,
            WhitespaceMode::CompareAll,
        ] {
            let r = LineRules::new()
                .with_whitespace(mode)
                .with_unimportant(["/\\*.*?\\*/"])
                .unwrap();
            for line in lines {
                let got = ranges(&r, line);
                assert_eq!(outside(line, &got), norm(&r, line), "{mode:?} {line:?}");
                // sorted, non-empty, non-overlapping, on char boundaries
                for w in got.windows(2) {
                    assert!(w[0].end < w[1].start, "{got:?}");
                }
                for g in &got {
                    assert!(g.start < g.end);
                    assert!(line.is_char_boundary(g.start as usize));
                    assert!(line.is_char_boundary(g.end as usize));
                }
            }
        }
    }

    #[test]
    fn ranges_with_ignore_changes_mark_long_runs_but_not_single_spaces() {
        let r = rules(WhitespaceMode::IgnoreChanges);
        assert_eq!(ranges(&r, "a b"), vec![]);
        assert_eq!(ranges(&r, "a   b"), vec![1..4]);
        assert_eq!(ranges(&r, " a\tb "), vec![0..1, 2..3, 4..5]);
    }

    #[test]
    fn ranges_of_a_regex_inside_a_regex_result_map_back_to_the_original() {
        // First pattern removes `[..]`, then `ab` matches across the cut.
        let r = LineRules::new()
            .with_unimportant(["\\[.*?\\]", "ab"])
            .unwrap();
        let line = "xa[mid]by";
        let got = ranges(&r, line);
        assert_eq!(outside(line, &got), "xy");
        assert_eq!(got, vec![1..8]);
    }

    #[test]
    fn ranges_of_an_empty_line_are_empty() {
        assert!(ranges(&LineRules::new(), "").is_empty());
    }
}
