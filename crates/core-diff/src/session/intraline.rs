//! Word-level (intraline) differences (plan.md §5.4, `DcIntraLineDiff`; backlog E4-S5).
//!
//! Tokens are words (letters, digits, `_`), whitespace runs and single punctuation marks
//! (surrogate pairs stay together). Myers runs over token hashes; unmatched tokens become
//! ranges, important unless the whole range is blank or unimportant text. Ranges are in
//! **UTF-16 code units**.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;

use core_base::crc32::line_hash_str;
use core_base::{CancelToken, Result};
use core_text::fold::fold_into;
use core_text::rules::{LineRules, WhitespaceMode, is_blank};

use crate::exact::myers_matches;

/// Lines with more tokens than this get one whole-line range (plan.md §5.4).
pub const MAX_TOKENS: usize = 10_000;
/// Rows kept in the intraline cache.
pub const CACHE_ROWS: usize = 4096;

/// A highlighted range of a line, in UTF-16 code units.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CharRange {
    pub start: u32,
    pub len: u32,
    pub important: bool,
}

/// The word-level differences of one paired row.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IntraLine {
    pub left: Vec<CharRange>,
    pub right: Vec<CharRange>,
}

#[derive(Debug, Clone, Copy)]
struct Token {
    /// UTF-8 byte range in the line.
    b0: u32,
    b1: u32,
    /// UTF-16 start and length.
    u0: u32,
    ulen: u32,
    blank: bool,
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn tokenize(s: &str) -> Vec<Token> {
    let mut out = Vec::new();
    let mut u = 0u32;
    let mut it = s.char_indices().peekable();
    while let Some((i, c)) = it.next() {
        let (blank, class): (bool, fn(char) -> bool) = if is_blank(c) {
            (true, is_blank)
        } else if is_word(c) {
            (false, is_word)
        } else {
            (false, |_| false) // a single punctuation mark
        };
        let mut end = i + c.len_utf8();
        let mut ulen = c.len_utf16() as u32;
        while let Some(&(j, d)) = it.peek() {
            if !class(d) {
                break;
            }
            end = j + d.len_utf8();
            ulen += d.len_utf16() as u32;
            it.next();
        }
        out.push(Token {
            b0: i as u32,
            b1: end as u32,
            u0: u,
            ulen,
            blank,
        });
        u += ulen;
    }
    out
}

fn token_hashes(s: &str, toks: &[Token], rules: &LineRules, scratch: &mut String) -> Vec<u32> {
    toks.iter()
        .map(|t| {
            let text = &s[t.b0 as usize..t.b1 as usize];
            if t.blank && rules.whitespace() != WhitespaceMode::CompareAll {
                line_hash_str(" ")
            } else if rules.ignore_case() {
                scratch.clear();
                fold_into(text, scratch);
                line_hash_str(scratch)
            } else {
                line_hash_str(text)
            }
        })
        .collect()
}

/// Ranges of unmatched tokens, merged when adjacent. `important` unless every token is blank
/// or the whole range lies inside unimportant text.
fn collect(toks: &[Token], matched: &[bool], unimportant: &[Range<u32>]) -> Vec<CharRange> {
    let mut out = Vec::new();
    // (utf16 start, utf16 len, utf8 start, utf8 end, any non-blank)
    let mut open: Option<(u32, u32, u32, u32, bool)> = None;
    let close = |o: (u32, u32, u32, u32, bool), out: &mut Vec<CharRange>| {
        let inside = unimportant.iter().any(|r| r.start <= o.2 && o.3 <= r.end);
        out.push(CharRange {
            start: o.0,
            len: o.1,
            important: o.4 && !inside,
        });
    };
    for (t, &m) in toks.iter().zip(matched) {
        if m {
            if let Some(o) = open.take() {
                close(o, &mut out);
            }
            continue;
        }
        match &mut open {
            Some(o) if o.0 + o.1 == t.u0 => {
                o.1 += t.ulen;
                o.3 = t.b1;
                o.4 |= !t.blank;
            }
            _ => {
                if let Some(o) = open.take() {
                    close(o, &mut out);
                }
                open = Some((t.u0, t.ulen, t.b0, t.b1, !t.blank));
            }
        }
    }
    if let Some(o) = open {
        close(o, &mut out);
    }
    out
}

fn whole_line(s: &str) -> Vec<CharRange> {
    if s.is_empty() {
        return Vec::new();
    }
    vec![CharRange {
        start: 0,
        len: s.encode_utf16().count() as u32,
        important: true,
    }]
}

/// Word-level differences between two lines under the line rules.
pub fn intraline_diff(
    a: &str,
    b: &str,
    rules: &LineRules,
    cancel: &CancelToken,
) -> Result<IntraLine> {
    let (ta, tb) = (tokenize(a), tokenize(b));
    if ta.len() > MAX_TOKENS || tb.len() > MAX_TOKENS {
        return Ok(IntraLine {
            left: whole_line(a),
            right: whole_line(b),
        });
    }
    let mut scratch = String::new();
    let (ha, hb) = (
        token_hashes(a, &ta, rules, &mut scratch),
        token_hashes(b, &tb, rules, &mut scratch),
    );
    let mut ma = vec![false; ta.len()];
    let mut mb = vec![false; tb.len()];
    for (i, j) in myers_matches(&ha, &hb, cancel)? {
        ma[i as usize] = true;
        mb[j as usize] = true;
    }
    // with every blank ignored, blank tokens are never differences
    if rules.whitespace() == WhitespaceMode::IgnoreAll {
        for (t, m) in ta.iter().zip(ma.iter_mut()) {
            *m |= t.blank;
        }
        for (t, m) in tb.iter().zip(mb.iter_mut()) {
            *m |= t.blank;
        }
    }
    let (mut ua, mut ub) = (Vec::new(), Vec::new());
    rules.unimportant_ranges(a, &mut ua);
    rules.unimportant_ranges(b, &mut ub);
    Ok(IntraLine {
        left: collect(&ta, &ma, &ua),
        right: collect(&tb, &mb, &ub),
    })
}

/// A small LRU of computed rows (about [`CACHE_ROWS`]).
#[derive(Debug, Default)]
pub struct IntraCache {
    map: HashMap<u32, (Arc<IntraLine>, u64)>,
    tick: u64,
}

impl IntraCache {
    pub fn get(&mut self, row: u32) -> Option<Arc<IntraLine>> {
        self.tick += 1;
        let tick = self.tick;
        self.map.get_mut(&row).map(|e| {
            e.1 = tick;
            e.0.clone()
        })
    }

    pub fn put(&mut self, row: u32, v: Arc<IntraLine>) {
        self.tick += 1;
        self.map.insert(row, (v, self.tick));
        if self.map.len() > CACHE_ROWS {
            // drop the least recently used quarter
            let mut ticks: Vec<u64> = self.map.values().map(|e| e.1).collect();
            ticks.sort_unstable();
            let cut = ticks[ticks.len() / 4];
            self.map.retain(|_, e| e.1 > cut);
        }
    }

    pub fn clear(&mut self) {
        self.map.clear();
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diff(a: &str, b: &str, rules: &LineRules) -> IntraLine {
        intraline_diff(a, b, rules, &CancelToken::new()).unwrap()
    }

    fn r(start: u32, len: u32, important: bool) -> CharRange {
        CharRange {
            start,
            len,
            important,
        }
    }

    #[test]
    fn tokens_are_words_blank_runs_and_punctuation() {
        let toks = tokenize("int  x=1;");
        let texts: Vec<&str> = toks
            .iter()
            .map(|t| &"int  x=1;"[t.b0 as usize..t.b1 as usize])
            .collect();
        assert_eq!(texts, ["int", "  ", "x", "=", "1", ";"]);
        assert!(toks[1].blank && !toks[0].blank);
        assert!(tokenize("").is_empty());
        // underscore and digits belong to words; punctuation is one token per mark
        let t: Vec<_> = tokenize("a_b1 ((")
            .iter()
            .map(|t| (t.b1 - t.b0, t.blank))
            .collect();
        assert_eq!(t, [(4, false), (1, true), (1, false), (1, false)]);
    }

    #[test]
    fn surrogate_pairs_stay_together_and_offsets_are_utf16() {
        let s = "a😀b";
        let toks = tokenize(s);
        assert_eq!(toks.len(), 3);
        assert_eq!(
            (toks[1].u0, toks[1].ulen),
            (1, 2),
            "the emoji is two UTF-16 units"
        );
        assert_eq!(toks[2].u0, 3);
    }

    /// Backlog E4-S5: `int x = 1;` vs `int x = 2;` highlights only the digits.
    #[test]
    fn only_the_changed_token_is_highlighted() {
        let d = diff("int x = 1;", "int x = 2;", &LineRules::new());
        assert_eq!(d.left, vec![r(8, 1, true)]);
        assert_eq!(d.right, vec![r(8, 1, true)]);
    }

    #[test]
    fn a_whitespace_only_change_gives_unimportant_ranges() {
        let d = diff(
            "a b",
            "a   b",
            &LineRules::new().with_whitespace(WhitespaceMode::CompareAll),
        );
        // CompareAll: the blank runs differ as tokens; both ranges are blank-only: unimportant
        assert_eq!(d.left, vec![r(1, 1, false)]);
        assert_eq!(d.right, vec![r(1, 3, false)]);
        // with the default mode the blank runs compare equal: no ranges at all
        let d = diff("a b", "a   b", &LineRules::new());
        assert!(d.left.is_empty() && d.right.is_empty());
    }

    #[test]
    fn ignore_all_never_reports_blank_tokens() {
        let rules = LineRules::new().with_whitespace(WhitespaceMode::IgnoreAll);
        let d = diff("a b c", "a c", &rules);
        // only the word `b` differs; the blanks around it are never differences
        assert_eq!(d.left, vec![r(2, 1, true)]);
        assert!(d.right.is_empty(), "{:?}", d.right);
    }

    #[test]
    fn ignore_case_matches_tokens_case_insensitively() {
        let d = diff(
            "Foo Bar",
            "foo baz",
            &LineRules::new().with_ignore_case(true),
        );
        assert_eq!(d.left, vec![r(4, 3, true)]);
        assert_eq!(d.right, vec![r(4, 3, true)]);
    }

    #[test]
    fn unimportant_text_makes_a_range_unimportant() {
        let rules = LineRules::new().with_unimportant(["//.*$"]).unwrap();
        let d = diff("x = 1; // old note", "x = 1; // new note", &rules);
        assert!(!d.left.is_empty());
        assert!(d.left.iter().all(|c| !c.important), "{:?}", d.left);
        let d = diff("x = 1; // note", "x = 2; // note", &rules);
        assert_eq!(d.left, vec![r(4, 1, true)]);
    }

    #[test]
    fn ranges_around_an_emoji_are_valid_utf16() {
        let d = diff("a😀b", "a😁b", &LineRules::new());
        assert_eq!(d.left, vec![r(1, 2, true)]);
        assert_eq!(d.right, vec![r(1, 2, true)]);
        let utf16: Vec<u16> = "a😀b".encode_utf16().collect();
        let c = d.left[0];
        assert!(String::from_utf16(&utf16[c.start as usize..(c.start + c.len) as usize]).is_ok());
    }

    #[test]
    fn adjacent_unmatched_tokens_merge_but_a_matched_blank_splits() {
        let d = diff("foo+ y", "baz- y", &LineRules::new());
        assert_eq!(d.left, vec![r(0, 4, true)], "foo and + are adjacent");
        // the blank between two changed words is equal, so two ranges
        let d = diff("x foo bar y", "x baz qux y", &LineRules::new());
        assert_eq!(d.left, vec![r(2, 3, true), r(6, 3, true)]);
    }

    #[test]
    fn lines_over_10k_tokens_get_a_whole_line_range() {
        let big_a = "a ".repeat(6000);
        let big_b = "b ".repeat(6000);
        let d = diff(&big_a, &big_b, &LineRules::new());
        assert_eq!(d.left, vec![r(0, 12_000, true)]);
        assert_eq!(d.right.len(), 1);
    }

    #[test]
    fn empty_sides() {
        let d = diff("", "abc", &LineRules::new());
        assert!(d.left.is_empty());
        assert_eq!(d.right, vec![r(0, 3, true)]);
    }

    #[test]
    fn the_cache_is_bounded_and_keeps_recent_rows() {
        let mut c = IntraCache::default();
        for i in 0..(CACHE_ROWS as u32 + 100) {
            c.put(i, Arc::new(IntraLine::default()));
        }
        assert!(c.len() <= CACHE_ROWS, "{}", c.len());
        assert!(
            c.get(CACHE_ROWS as u32 + 99).is_some(),
            "the newest row survives"
        );
        assert!(c.get(0).is_none(), "the oldest row was evicted");
        c.clear();
        assert!(c.is_empty());
    }
}
