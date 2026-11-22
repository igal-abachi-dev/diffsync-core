//! SEARCH/REPLACE blocks (backlog E11.5-S16): the edit format language models write and read best
//! (Aider's), produced from a diff and applied to a text.
//!
//! ```text
//! src/app.py
//! <<<<<<< SEARCH
//! def main():
//!     x = 1
//! =======
//! def main():
//!     x = 2
//! >>>>>>> REPLACE
//! ```
//!
//! Rendering ([`render_hunks`]) writes one block per hunk and grows the SEARCH part with the
//! lines around it until it occurs once in the old file, so a block can always be applied to the
//! place it was made for. Applying ([`apply_blocks`]) is stricter than a model's habits: a block
//! applies only where its SEARCH text is found, and when it is found in several places it is
//! refused (decision D59), not applied to the first one. The match is exact; if that finds
//! nothing, trailing white space and then indentation are ignored, and the REPLACE text is
//! re-indented by the difference. The report says which kind of match was used.

use serde::Serialize;

use crate::patch::{Eol, FLine, PatchError, join_lines, split_lines};
use crate::udiff::{Hunk, LineOp};

/// One edit: replace the lines `search` with the lines `replace` in the file `path`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SrBlock {
    /// The file the block is for; `None` when the text had no name above it and no earlier block.
    pub path: Option<String>,
    /// Lines separated by `\n`, each ending in one; empty for "create or append".
    pub search: String,
    pub replace: String,
}

fn marker(l: &str, ch: char, word: &str) -> bool {
    let l = l.trim_end();
    let n = l.chars().take_while(|c| *c == ch).count();
    if !(5..=9).contains(&n) {
        return false;
    }
    let rest = l[n..].trim();
    rest == word || (word == "SEARCH" && rest == "SEARCH>") || (word.is_empty() && rest.is_empty())
}

fn clean_path(l: &str) -> Option<String> {
    if l.trim_start().starts_with("```") {
        return None; // a code fence (with or without a language), not a file name
    }
    let p = l
        .trim()
        .trim_matches(|c| c == '`' || c == '*' || c == '"' || c == '\'')
        .trim_end_matches(':')
        .trim();
    if p.is_empty() {
        None
    } else {
        Some(p.to_string())
    }
}

/// Finds every block in `text` (a whole model answer is fine: prose and code fences between the
/// blocks are skipped). A block that is not closed is an error, so half an edit is never taken
/// for a whole one.
pub fn parse_blocks(text: &str) -> Result<Vec<SrBlock>, PatchError> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out: Vec<SrBlock> = Vec::new();
    let (mut candidate, mut last_path): (Option<String>, Option<String>) = (None, None);
    let mut i = 0;
    while i < lines.len() {
        let l = lines[i];
        if !marker(l, '<', "SEARCH") {
            if let Some(p) = clean_path(l) {
                if !marker(l, '=', "") && !marker(l, '>', "REPLACE") {
                    candidate = Some(p);
                }
            }
            i += 1;
            continue;
        }
        let at = i;
        let path = candidate.take().or_else(|| last_path.clone());
        let (mut search, mut replace) = (String::new(), String::new());
        i += 1;
        loop {
            let Some(l) = lines.get(i) else {
                return Err(PatchError {
                    line: at + 1,
                    message: "SEARCH block without a ======= divider".into(),
                });
            };
            i += 1;
            if marker(l, '=', "") {
                break;
            }
            search.push_str(l);
            search.push('\n');
        }
        loop {
            let Some(l) = lines.get(i) else {
                return Err(PatchError {
                    line: at + 1,
                    message: "SEARCH/REPLACE block without a >>>>>>> REPLACE end".into(),
                });
            };
            i += 1;
            if marker(l, '>', "REPLACE") {
                break;
            }
            replace.push_str(l);
            replace.push('\n');
        }
        last_path.clone_from(&path);
        out.push(SrBlock {
            path,
            search,
            replace,
        });
    }
    if out.is_empty() {
        return Err(PatchError {
            line: 1,
            message: "no <<<<<<< SEARCH block found".into(),
        });
    }
    Ok(out)
}

/// Writes one block.
pub fn render_block(path: Option<&str>, search: &str, replace: &str) -> String {
    let mut s = String::new();
    if let Some(p) = path {
        s.push_str(p);
        s.push('\n');
    }
    s.push_str("<<<<<<< SEARCH\n");
    s.push_str(search);
    if !search.is_empty() && !search.ends_with('\n') {
        s.push('\n');
    }
    s.push_str("=======\n");
    s.push_str(replace);
    if !replace.is_empty() && !replace.ends_with('\n') {
        s.push('\n');
    }
    s.push_str(">>>>>>> REPLACE\n");
    s
}

fn count_at(old: &[String], pat: &[String]) -> usize {
    if pat.is_empty() || pat.len() > old.len() {
        return 0;
    }
    old.windows(pat.len()).filter(|w| *w == pat).count()
}

/// The blocks for `hunks` of the file `old_text`: one per hunk, with enough surrounding lines
/// in each that its SEARCH text is unique in the old file. A new file is one block with an empty
/// SEARCH; a deleted file is one block that replaces all of its text with nothing.
pub fn render_hunks(path: &str, old_text: &str, hunks: &[Hunk]) -> String {
    let old: Vec<String> = split_lines(old_text).into_iter().map(|l| l.text).collect();
    let mut out = String::new();
    for h in hunks {
        let hs = (h.old_first as usize).min(old.len());
        let he = (hs + h.old_count as usize).min(old.len());
        let new_side: Vec<String> = h
            .lines
            .iter()
            .filter(|l| l.op != LineOp::Delete)
            .map(|l| l.text.clone())
            .collect();
        let (mut s, mut e) = (hs, he);
        // a hunk with no old lines at all (pure insertion, -U0) has nothing to search for yet
        while (e - s == 0 && !old.is_empty() || count_at(&old, &old[s..e]) > 1)
            && (s > 0 || e < old.len())
        {
            s = s.saturating_sub(1);
            if e < old.len() {
                e += 1;
            }
        }
        let mut search = String::new();
        for l in &old[s..e] {
            search.push_str(l);
            search.push('\n');
        }
        let mut replace = String::new();
        for l in old[s..hs]
            .iter()
            .chain(new_side.iter())
            .chain(old[he..e].iter())
        {
            replace.push_str(l);
            replace.push('\n');
        }
        out.push_str(&render_block(Some(path), &search, &replace));
    }
    out
}

/// How a block was matched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum MatchKind {
    Exact,
    /// Trailing white space ignored.
    TrailingWhitespace,
    /// Leading and trailing white space ignored; the REPLACE text was re-indented.
    Indentation,
    /// An empty SEARCH: the REPLACE text became the file or was appended to it.
    Append,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockResult {
    pub index: usize,
    pub applied: bool,
    pub matched: Option<MatchKind>,
    /// 1-based line where the block was applied.
    pub line: usize,
    pub message: String,
}

fn lead_ws(s: &str) -> &str {
    &s[..s.len() - s.trim_start().len()]
}

fn norm(tier: u8, s: &str) -> &str {
    match tier {
        0 => s,
        1 => s.trim_end(),
        _ => s.trim(),
    }
}

fn find_all(lines: &[FLine], pat: &[&str], tier: u8) -> Vec<usize> {
    if pat.is_empty() || pat.len() > lines.len() {
        return Vec::new();
    }
    (0..=lines.len() - pat.len())
        .filter(|&i| {
            pat.iter()
                .enumerate()
                .all(|(k, p)| norm(tier, &lines[i + k].text) == norm(tier, p))
        })
        .collect()
}

/// Applies the blocks one after the other, each to the text the earlier ones left. A block that
/// cannot be applied (not found, found in several places) is reported and skipped.
pub fn apply_blocks(text: &str, blocks: &[&SrBlock]) -> (String, Vec<BlockResult>) {
    let mut lines = split_lines(text);
    let mut results = Vec::with_capacity(blocks.len());
    for (index, b) in blocks.iter().enumerate() {
        let fail = |m: String| BlockResult {
            index,
            applied: false,
            matched: None,
            line: 0,
            message: m,
        };
        let crlf = lines.iter().filter(|l| l.eol == Eol::CrLf).count()
            > lines.iter().filter(|l| l.eol == Eol::Lf).count();
        let eol = if crlf { Eol::CrLf } else { Eol::Lf };
        let search: Vec<&str> = b.search.lines().collect();
        let mut repl: Vec<String> = b.replace.lines().map(str::to_string).collect();
        if search.is_empty() {
            let at = lines.len();
            if let Some(last) = lines.last_mut() {
                if last.eol == Eol::None {
                    last.eol = eol;
                }
            }
            lines.extend(repl.into_iter().map(|t| FLine { text: t, eol }));
            results.push(BlockResult {
                index,
                applied: true,
                matched: Some(MatchKind::Append),
                line: at + 1,
                message: String::new(),
            });
            continue;
        }
        let mut hit: Option<(usize, u8)> = None;
        for tier in 0..3u8 {
            let all = find_all(&lines, &search, tier);
            match all.len() {
                0 => {}
                1 => {
                    hit = Some((all[0], tier));
                    break;
                }
                n => {
                    results.push(fail(format!(
                        "block #{}: the SEARCH text is found in {n} places (first at line {}); add lines around it",
                        index + 1,
                        all[0] + 1
                    )));
                    hit = Some((usize::MAX, tier));
                    break;
                }
            }
        }
        let Some((at, tier)) = hit else {
            results.push(fail(format!(
                "block #{}: the SEARCH text was not found in the file",
                index + 1
            )));
            continue;
        };
        if at == usize::MAX {
            continue;
        }
        if tier == 2 {
            // re-indent REPLACE by the difference between the first lines that have text
            if let Some((sl, fl)) = search
                .iter()
                .enumerate()
                .find(|(_, s)| !s.trim().is_empty())
                .map(|(k, s)| {
                    (
                        lead_ws(s).to_string(),
                        lead_ws(&lines[at + k].text).to_string(),
                    )
                })
            {
                if fl != sl && fl.ends_with(&sl) {
                    let pre = &fl[..fl.len() - sl.len()];
                    for r in &mut repl {
                        if !r.trim().is_empty() {
                            *r = format!("{pre}{r}");
                        }
                    }
                } else if fl != sl && sl.ends_with(&fl) {
                    let cut = &sl[..sl.len() - fl.len()];
                    for r in &mut repl {
                        if let Some(rest) = r.strip_prefix(cut) {
                            *r = rest.to_string();
                        }
                    }
                }
            }
        }
        let end = at + search.len();
        let keep_open_end = end == lines.len() && lines[end - 1].eol == Eol::None;
        let n = repl.len();
        let new_lines: Vec<FLine> = repl
            .into_iter()
            .enumerate()
            .map(|(k, t)| FLine {
                text: t,
                eol: if keep_open_end && k + 1 == n {
                    Eol::None
                } else {
                    eol
                },
            })
            .collect();
        lines.splice(at..end, new_lines);
        results.push(BlockResult {
            index,
            applied: true,
            matched: Some(match tier {
                0 => MatchKind::Exact,
                1 => MatchKind::TrailingWhitespace,
                _ => MatchKind::Indentation,
            }),
            line: at + 1,
            message: String::new(),
        });
    }
    (join_lines(&lines), results)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::udiff::HunkLine;
    use core_text::LineEnding;

    const ANSWER: &str = "Sure, here is the change:\n\n**app.py**\n```python\n<<<<<<< SEARCH\ndef main():\n    x = 1\n    print(x)\n=======\ndef main():\n    x = 2\n    print(x)\n>>>>>>> REPLACE\n```\n";

    #[test]
    fn blocks_are_found_in_prose_and_fences() {
        let b = parse_blocks(ANSWER).unwrap();
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].path.as_deref(), Some("app.py"));
        assert_eq!(b[0].search, "def main():\n    x = 1\n    print(x)\n");
        assert_eq!(b[0].replace, "def main():\n    x = 2\n    print(x)\n");
        assert!(parse_blocks("<<<<<<< SEARCH\na\n=======\nb\n").is_err());
        assert!(parse_blocks("nothing here").is_err());
        // the path is carried to the next block that has none
        let two = parse_blocks("f.txt\n<<<<<<< SEARCH\na\n=======\nb\n>>>>>>> REPLACE\n<<<<<<< SEARCH\nc\n=======\nd\n>>>>>>> REPLACE\n").unwrap();
        assert_eq!(two[1].path.as_deref(), Some("f.txt"));
    }

    #[test]
    fn an_exact_block_applies_once_and_only_where_unique() {
        let b = parse_blocks(ANSWER).unwrap();
        let src = "import os\ndef main():\n    x = 1\n    print(x)\nmain()\n";
        let (t, r) = apply_blocks(src, &[&b[0]]);
        assert!(r[0].applied, "{r:?}");
        assert_eq!(r[0].matched, Some(MatchKind::Exact));
        assert_eq!(r[0].line, 2);
        assert_eq!(
            t,
            "import os\ndef main():\n    x = 2\n    print(x)\nmain()\n"
        );
        // twice in the file: refused, text unchanged
        let twice = format!("{src}{src}");
        let (t2, r2) = apply_blocks(&twice, &[&b[0]]);
        assert!(
            !r2[0].applied && r2[0].message.contains("2 places"),
            "{r2:?}"
        );
        assert_eq!(t2, twice);
        // not present
        let (_, r3) = apply_blocks("nothing\n", &[&b[0]]);
        assert!(r3[0].message.contains("not found"));
    }

    #[test]
    fn indentation_and_trailing_blanks_are_forgiven_and_reported() {
        let blk = SrBlock {
            path: None,
            search: "x = 1  \nprint(x)\n".into(),
            replace: "x = 2\nprint(x)\n".into(),
        };
        let (t, r) = apply_blocks("def f():\n    x = 1\n    print(x)\n", &[&blk]);
        assert!(r[0].applied);
        assert_eq!(r[0].matched, Some(MatchKind::Indentation));
        assert_eq!(t, "def f():\n    x = 2\n    print(x)\n");
        let ws = SrBlock {
            path: None,
            search: "a  \nb\n".into(),
            replace: "A\nb\n".into(),
        };
        let (t, r) = apply_blocks("a\nb\n", &[&ws]);
        assert_eq!(r[0].matched, Some(MatchKind::TrailingWhitespace));
        assert_eq!(t, "A\nb\n");
    }

    #[test]
    fn empty_search_creates_or_appends_and_crlf_is_kept() {
        let add = SrBlock {
            path: None,
            search: String::new(),
            replace: "new\n".into(),
        };
        assert_eq!(apply_blocks("", &[&add]).0, "new\n");
        assert_eq!(apply_blocks("a\nb", &[&add]).0, "a\nb\nnew\n");
        let edit = SrBlock {
            path: None,
            search: "b\n".into(),
            replace: "B\nB2\n".into(),
        };
        assert_eq!(
            apply_blocks("a\r\nb\r\nc\r\n", &[&edit]).0,
            "a\r\nB\r\nB2\r\nc\r\n"
        );
        // the last line has no newline: the replaced last line has none either
        assert_eq!(apply_blocks("a\nb", &[&edit]).0, "a\nB\nB2");
    }

    fn hunk(old_first: u32, old_count: u32, lines: &[(LineOp, &str)]) -> Hunk {
        let new_count = lines.iter().filter(|l| l.0 != LineOp::Delete).count() as u32;
        Hunk {
            old_first,
            old_count,
            new_first: old_first,
            new_count,
            func: None,
            lines: lines
                .iter()
                .map(|(op, t)| HunkLine {
                    op: *op,
                    text: (*t).to_string(),
                    eol: LineEnding::Lf,
                })
                .collect(),
        }
    }

    #[test]
    fn rendering_grows_the_search_text_until_it_is_unique() {
        // the changed line `x` appears twice; the context of one line each side is the same too
        let old = "a\nx\nb\nc\na\nx\nb\nd\n";
        let h = hunk(5, 1, &[(LineOp::Delete, "x"), (LineOp::Insert, "y")]);
        let s = render_hunks("f.txt", old, &[h]);
        assert!(s.starts_with("f.txt\n<<<<<<< SEARCH\n"), "{s}");
        let blocks = parse_blocks(&s).unwrap();
        let (t, r) = apply_blocks(old, &[&blocks[0]]);
        assert!(r[0].applied, "{r:?}\n{s}");
        assert_eq!(t, "a\nx\nb\nc\na\ny\nb\nd\n");
        // a new file
        let n = render_hunks("n.txt", "", &[hunk(0, 0, &[(LineOp::Insert, "one")])]);
        assert_eq!(n, "n.txt\n<<<<<<< SEARCH\n=======\none\n>>>>>>> REPLACE\n");
    }
}
