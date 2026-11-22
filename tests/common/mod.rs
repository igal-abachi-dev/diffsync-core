//! Fixture loader shared by integration tests and `dsdev` (backlog E0-S6.1).
//!
//! Include it from a test or binary with
//! `#[path = "../../../tests/common/mod.rs"] mod common;` (the relative path is the same
//! from `crates/<crate>/tests/x.rs` and `tools/dsdev/src/main.rs`). It depends on `std`
//! only, so every crate can use it.
//!
//! # Layout (`tests/fixtures/text/<case>/`)
//!
//! | File | Content |
//! |---|---|
//! | `left`, `right` | the two inputs, as raw bytes |
//! | `options.json` | optional; only the keys that differ from the defaults |
//! | `expected.ops` | one op per line, `src tgt amount`; `-` marks the orphan side |
//! | `expected.rows` | one row per line, `left right Kind`; `-` marks an absent line |
//!
//! Blank lines and lines starting with `#` are ignored in both expected files.
#![allow(dead_code)]

use std::fmt;
use std::path::{Path, PathBuf};

/// One expected alignment op. `None` is the orphan side (`-`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExpOp {
    pub src: Option<u32>,
    pub tgt: Option<u32>,
    pub amount: u32,
}

/// One expected display row. `kind` is the `RowKind` name, e.g. `RightOnly`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpRow {
    pub left: Option<u32>,
    pub right: Option<u32>,
    pub kind: String,
}

impl fmt::Display for ExpOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {} {}", side(self.src), side(self.tgt), self.amount)
    }
}

impl fmt::Display for ExpRow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {} {}", side(self.left), side(self.right), self.kind)
    }
}

fn side(v: Option<u32>) -> String {
    v.map_or_else(|| "-".to_string(), |n| n.to_string())
}

/// The kind names accepted in `expected.rows`.
pub const ROW_KINDS: [&str; 7] = [
    "Same",
    "Unimportant",
    "Different",
    "LeftOnly",
    "RightOnly",
    "UnimportantLeftOnly",
    "UnimportantRightOnly",
];

fn parse_side(tok: &str, what: &str, line_no: usize) -> Result<Option<u32>, String> {
    if tok == "-" {
        return Ok(None);
    }
    tok.parse::<u32>()
        .map(Some)
        .map_err(|_| format!("line {line_no}: bad {what} `{tok}`"))
}

fn content_lines(text: &str) -> impl Iterator<Item = (usize, &str)> {
    text.lines()
        .enumerate()
        .map(|(i, l)| (i + 1, l.trim()))
        .filter(|(_, l)| !l.is_empty() && !l.starts_with('#'))
}

/// Parses `expected.ops` text.
pub fn parse_ops(text: &str) -> Result<Vec<ExpOp>, String> {
    let mut out = Vec::new();
    for (n, line) in content_lines(text) {
        let toks: Vec<&str> = line.split_whitespace().collect();
        let [src, tgt, amount] = toks[..] else {
            return Err(format!("line {n}: expected `src tgt amount`, got `{line}`"));
        };
        let op = ExpOp {
            src: parse_side(src, "src", n)?,
            tgt: parse_side(tgt, "tgt", n)?,
            amount: amount
                .parse()
                .map_err(|_| format!("line {n}: bad amount `{amount}`"))?,
        };
        if op.src.is_none() && op.tgt.is_none() {
            return Err(format!("line {n}: both sides are orphans"));
        }
        if op.amount == 0 {
            return Err(format!("line {n}: amount must be at least 1"));
        }
        out.push(op);
    }
    Ok(out)
}

/// Parses `expected.rows` text.
pub fn parse_rows(text: &str) -> Result<Vec<ExpRow>, String> {
    let mut out = Vec::new();
    for (n, line) in content_lines(text) {
        let toks: Vec<&str> = line.split_whitespace().collect();
        let [left, right, kind] = toks[..] else {
            return Err(format!(
                "line {n}: expected `left right Kind`, got `{line}`"
            ));
        };
        if !ROW_KINDS.contains(&kind) {
            return Err(format!("line {n}: unknown row kind `{kind}`"));
        }
        out.push(ExpRow {
            left: parse_side(left, "left", n)?,
            right: parse_side(right, "right", n)?,
            kind: kind.to_string(),
        });
    }
    Ok(out)
}

/// A loaded golden case.
#[derive(Debug, Clone)]
pub struct Case {
    pub name: String,
    pub dir: PathBuf,
    pub left: Vec<u8>,
    pub right: Vec<u8>,
    /// Raw `options.json` text, if the case has one.
    pub options_json: Option<String>,
    pub ops: Vec<ExpOp>,
    pub rows: Vec<ExpRow>,
}

/// `<repo>/tests/fixtures/text`, found from a crate's `CARGO_MANIFEST_DIR`
/// (`crates/<crate>` or `tools/<tool>`).
pub fn fixtures_root(manifest_dir: &str) -> PathBuf {
    Path::new(manifest_dir).join("../../tests/fixtures/text")
}

/// Loads one case directory. Missing `expected.*` files are errors.
pub fn load_case(dir: &Path) -> Result<Case, String> {
    let name = dir
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    let read = |f: &str| std::fs::read(dir.join(f)).map_err(|e| format!("{name}/{f}: {e}"));
    let read_text = |f: &str| -> Result<String, String> {
        String::from_utf8(read(f)?).map_err(|e| format!("{name}/{f}: not UTF-8: {e}"))
    };
    let options_path = dir.join("options.json");
    let options_json = if options_path.exists() {
        Some(read_text("options.json")?)
    } else {
        None
    };
    Ok(Case {
        name: name.clone(),
        dir: dir.to_path_buf(),
        left: read("left")?,
        right: read("right")?,
        options_json,
        ops: parse_ops(&read_text("expected.ops")?)
            .map_err(|e| format!("{name}/expected.ops: {e}"))?,
        rows: parse_rows(&read_text("expected.rows")?)
            .map_err(|e| format!("{name}/expected.rows: {e}"))?,
    })
}

/// Every case directory under `root`, sorted by name.
pub fn list_cases(root: &Path) -> Result<Vec<PathBuf>, String> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(root)
        .map_err(|e| format!("{}: {e}", root.display()))?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    Ok(dirs)
}

/// Readable first-difference report between expected and actual lists.
pub fn diff_report<T: fmt::Display + PartialEq>(
    what: &str,
    expected: &[T],
    actual: &[T],
) -> String {
    let mut s = format!(
        "{what}: expected {} entries, got {}\n",
        expected.len(),
        actual.len()
    );
    let n = expected.len().max(actual.len());
    for i in 0..n {
        let (e, a) = (expected.get(i), actual.get(i));
        if e == a {
            continue;
        }
        let show = |v: Option<&T>| v.map_or_else(|| "<none>".to_string(), ToString::to_string);
        s.push_str(&format!(
            "  #{i}: expected `{}`  actual `{}`\n",
            show(e),
            show(a)
        ));
        if s.lines().count() > 12 {
            s.push_str("  ...\n");
            break;
        }
    }
    s
}
