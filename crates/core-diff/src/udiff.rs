//! Patch-style output of a finished text compare: unified (`diff -u`, `git diff`), context
//! (`diff -c`) and normal (`diff`) formats, and a JSON hunk model (backlog E11.5-S1).
//!
//! The rows of a [`TextCompare`] are turned into *change blocks* (a run of lines deleted on the
//! left and inserted on the right, between two equal lines), the blocks are grouped into hunks, and
//! the hunks are rendered. The rules follow GNU diffutils 3.12 and git's xdiff:
//!
//! * two changes are in one hunk when at most `2 * context` equal lines lie between them;
//! * a block prints all its deletions, then all its insertions;
//! * context lines are taken from the *left* file (they may differ from the right one under
//!   `-w`, `-b`, `-i`);
//! * `@@ -a,b +c,d @@`: a count of 1 is written without `,1`, and an empty range starts at the line
//!   before it;
//! * a file whose last line has no line ending gets `\ No newline at end of file`, and a changed
//!   ending (CRLF against LF, or a final newline added or removed) is a change of that line.
//!
//! Text is rendered as UTF-8 (decoded from whatever encoding the file had); line endings are kept
//! as they were in the file.

use std::fmt::Write as _;

use core_base::Result;
use core_text::LineEnding;
use serde::Serialize;

use crate::options::{AlignmentName, EolName, TextOptions, WhitespaceName};
use crate::session::{DiffRow, DocSpec, RowKind, Status, TextCompare};
use crate::{ORPHAN, Side};

/// What goes into the hunks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct HunkOptions {
    /// Lines of context around a change (`-U N`, default 3).
    pub context: u32,
    /// `-B`: changes that only insert or delete blank lines are dropped.
    pub ignore_blank_lines: bool,
    /// Take the context lines from the new file (git does; GNU diff takes them from the old one,
    /// which only shows when whitespace or case is ignored).
    pub context_from_new: bool,
    /// Fill [`Hunk::func`] with the last "function" line above the hunk (git's `@@ ... @@ name`,
    /// GNU's `-p`).
    pub func_header: bool,
}

impl Default for HunkOptions {
    fn default() -> Self {
        HunkOptions {
            context: 3,
            ignore_blank_lines: false,
            context_from_new: false,
            func_header: false,
        }
    }
}

/// The text options a diff output wants: every difference counts unless the caller asks to ignore
/// it, line endings included (decision: D55). `diff` is not a "similar lines" aligner, so the
/// default engine is the minimal one; pass `Standard` for the pairing of similar lines that the
/// visual compare uses.
pub fn diff_text_options(
    alignment: AlignmentName,
    whitespace: WhitespaceName,
    ignore_case: bool,
) -> TextOptions {
    TextOptions {
        alignment,
        whitespace,
        ignore_case,
        eol: EolName::Strict,
        // an inserted blank line is a real insertion in a patch
        orphans_always_sig: true,
        ..TextOptions::default()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum LineOp {
    #[serde(rename = " ")]
    Equal,
    #[serde(rename = "-")]
    Delete,
    #[serde(rename = "+")]
    Insert,
}

/// One printed line. `text` has no line ending; `eol` is the ending the line had in its file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HunkLine {
    pub op: LineOp,
    pub text: String,
    #[serde(serialize_with = "ser_eol")]
    pub eol: LineEnding,
}

fn ser_eol<S: serde::Serializer>(e: &LineEnding, s: S) -> std::result::Result<S::Ok, S::Error> {
    s.serialize_str(match e {
        LineEnding::None => "none",
        LineEnding::CrLf => "crlf",
        LineEnding::Lf => "lf",
        LineEnding::Cr => "cr",
    })
}

/// A run of changed lines, 0-based: `old_first..old_first + old_count` on the left is replaced by
/// `new_first..new_first + new_count` on the right.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Change {
    pub old_first: u32,
    pub old_count: u32,
    pub new_first: u32,
    pub new_count: u32,
}

/// A group of changes with their context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Hunk {
    /// 0-based index of the first old line in the hunk (for an empty range: the line it follows).
    pub old_first: u32,
    pub old_count: u32,
    pub new_first: u32,
    pub new_count: u32,
    /// The "function" line above the hunk, when asked for.
    pub func: Option<String>,
    pub lines: Vec<HunkLine>,
}

impl Hunk {
    /// The 1-based start printed in a unified header: an empty range starts at the line before.
    pub fn old_start(&self) -> u32 {
        start_of(self.old_first, self.old_count)
    }

    pub fn new_start(&self) -> u32 {
        start_of(self.new_first, self.new_count)
    }

    pub fn additions(&self) -> u32 {
        self.lines.iter().filter(|l| l.op == LineOp::Insert).count() as u32
    }

    pub fn deletions(&self) -> u32 {
        self.lines.iter().filter(|l| l.op == LineOp::Delete).count() as u32
    }
}

fn start_of(first: u32, count: u32) -> u32 {
    if count == 0 { first } else { first + 1 }
}

/// Reads the rows of a finished compare into change blocks. `Status::Binary` is the caller's
/// business; any other state than `Done` is an error.
pub fn changes(tc: &TextCompare, _opts: &HunkOptions) -> Result<Vec<Change>> {
    if tc.status() != Status::Done {
        return Err(core_base::Error::invalid_argument(
            "the compare is not finished",
        ));
    }
    let mut out: Vec<Change> = Vec::new();
    let mut open: Option<Change> = None;
    let (mut li, mut ri) = (0u32, 0u32);
    let total = tc.row_count();
    let mut rows = vec![
        DiffRow {
            left: ORPHAN,
            right: ORPHAN,
            kind: RowKind::Same
        };
        4096
    ];
    let mut first = 0u32;
    while first < total {
        let n = tc.rows(first, &mut rows);
        if n == 0 {
            break;
        }
        for r in &rows[..n] {
            let (l, rr) = (r.left_opt(), r.right_opt());
            // endings are part of the line hash when the options say so (D55), so an equal row is
            // equal in every way the caller asked for
            let equal = matches!(r.kind, RowKind::Same | RowKind::Unimportant)
                && l.is_some()
                && rr.is_some();
            if equal {
                if let Some(c) = open.take() {
                    out.push(c);
                }
                li += 1;
                ri += 1;
                continue;
            }
            let c = open.get_or_insert(Change {
                old_first: li,
                old_count: 0,
                new_first: ri,
                new_count: 0,
            });
            if l.is_some() {
                c.old_count += 1;
                li += 1;
            }
            if rr.is_some() {
                c.new_count += 1;
                ri += 1;
            }
        }
        first += n as u32;
    }
    if let Some(c) = open {
        out.push(c);
    }
    Ok(out)
}

/// The result of diffing two documents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiffBody {
    /// Either side is a binary file: no hunks, only "Binary files differ" when they are not equal.
    pub binary: bool,
    /// Whether the two documents differ at all (for a binary pair: whether the bytes differ).
    pub differ: bool,
    pub hunks: Vec<Hunk>,
}

/// Compares two documents and builds their hunks in one call. `text` should come from
/// [`diff_text_options`] (or the caller's own choice); the compare runs on a worker thread and is
/// waited for.
pub fn diff_specs(
    old: DocSpec,
    new: DocSpec,
    text: &TextOptions,
    opts: &HunkOptions,
) -> Result<DiffBody> {
    let tc = TextCompare::start(old, new, text.clone(), None)?;
    tc.wait();
    match tc.status() {
        Status::Done => {
            let hunks = hunks(&tc, opts)?;
            Ok(DiffBody {
                binary: false,
                differ: !hunks.is_empty(),
                hunks,
            })
        }
        Status::Binary => Ok(DiffBody {
            binary: true,
            // the session does not say whether two binary files are equal; the caller compares
            // the bytes (or knows from a folder compare)
            differ: true,
            hunks: Vec::new(),
        }),
        Status::Failed { error } => {
            Err(core_base::Error::new(core_base::ErrorKind::Internal, error))
        }
        other => Err(core_base::Error::new(
            core_base::ErrorKind::Internal,
            format!("the compare ended as {other:?}"),
        )),
    }
}

/// Builds the hunks of a finished text compare.
pub fn hunks(tc: &TextCompare, opts: &HunkOptions) -> Result<Vec<Hunk>> {
    let ch = changes(tc, opts)?;
    let line = |side: Side, i: u32| -> (String, LineEnding) {
        let mut buf = Vec::new();
        tc.line_utf16(side, i, &mut buf);
        (
            String::from_utf16_lossy(&buf),
            tc.line_ending(side, i).unwrap_or(LineEnding::None),
        )
    };
    // `-B`: a change whose every line, on both sides, is blank is "ignorable". As in GNU diff it
    // is only dropped when no other change shares its hunk, and an ignorable change joins a
    // neighbour only when fewer than `context` lines lie between them.
    let ignorable: Vec<bool> = ch
        .iter()
        .map(|c| {
            opts.ignore_blank_lines
                && c.old_count + c.new_count > 0
                && (c.old_first..c.old_first + c.old_count)
                    .all(|i| line(Side::Left, i).0.trim().is_empty())
                && (c.new_first..c.new_first + c.new_count)
                    .all(|i| line(Side::Right, i).0.trim().is_empty())
        })
        .collect();
    let old_total = tc.doc_info(Side::Left).map_or(0, |d| d.line_count);
    let ctx = opts.context;
    let mut out = Vec::new();
    let mut i = 0;
    while i < ch.len() {
        // group: the next change joins while few enough equal lines separate them
        let mut j = i;
        while j + 1 < ch.len() {
            let gap = ch[j + 1].old_first - (ch[j].old_first + ch[j].old_count);
            let joins = if ignorable[j] || ignorable[j + 1] {
                gap < ctx
            } else {
                gap <= 2 * ctx
            };
            if !joins {
                break;
            }
            j += 1;
        }
        let all_ignorable = ignorable[i..=j].iter().all(|x| *x);
        let (a, b) = (ch[i], ch[j]);
        let hunk_changes = &ch[i..=j];
        i = j + 1;
        if all_ignorable {
            continue;
        }
        let before = a.old_first.min(ctx);
        let after = (old_total - (b.old_first + b.old_count)).min(ctx);
        let (old_first, new_first) = (a.old_first - before, a.new_first - before);
        let old_end = b.old_first + b.old_count + after;
        let new_end = b.new_first + b.new_count + after;
        let mut lines = Vec::new();
        // context lines come from the old file (GNU diff) or the new one (git); `delta` is how far
        // the new line number is from the old one at this point of the hunk
        let equal = |lines: &mut Vec<HunkLine>, from: u32, to: u32, delta: i64| {
            for k in from..to {
                let (text, eol) = if opts.context_from_new {
                    line(Side::Right, (i64::from(k) + delta) as u32)
                } else {
                    line(Side::Left, k)
                };
                lines.push(HunkLine {
                    op: LineOp::Equal,
                    text,
                    eol,
                });
            }
        };
        let delta_before = |c: &Change| i64::from(c.new_first) - i64::from(c.old_first);
        let delta_after = |c: &Change| {
            i64::from(c.new_first + c.new_count) - i64::from(c.old_first + c.old_count)
        };
        equal(&mut lines, old_first, a.old_first, delta_before(&a));
        for (n, c) in hunk_changes.iter().enumerate() {
            if n > 0 {
                let p = hunk_changes[n - 1];
                equal(
                    &mut lines,
                    p.old_first + p.old_count,
                    c.old_first,
                    delta_after(&p),
                );
            }
            for k in c.old_first..c.old_first + c.old_count {
                let (text, eol) = line(Side::Left, k);
                lines.push(HunkLine {
                    op: LineOp::Delete,
                    text,
                    eol,
                });
            }
            for k in c.new_first..c.new_first + c.new_count {
                let (text, eol) = line(Side::Right, k);
                lines.push(HunkLine {
                    op: LineOp::Insert,
                    text,
                    eol,
                });
            }
        }
        equal(
            &mut lines,
            b.old_first + b.old_count,
            b.old_first + b.old_count + after,
            delta_after(&b),
        );
        let func = opts
            .func_header
            .then(|| function_line(|k| line(Side::Left, k).0, old_first))
            .flatten();
        out.push(Hunk {
            old_first,
            old_count: old_end - old_first,
            new_first,
            new_count: new_end - new_first,
            func,
            lines,
        });
    }
    Ok(out)
}

/// git's default "function" rule (and GNU `-p`): a line that starts with a letter, `_` or `$`.
/// The search goes up from the line before the hunk. The text is cut at 80 bytes (on a character
/// boundary) and trailing blanks are dropped.
fn function_line(get: impl Fn(u32) -> String, hunk_first: u32) -> Option<String> {
    let mut k = hunk_first;
    while k > 0 {
        k -= 1;
        let t = get(k);
        if t.chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || c == '$')
        {
            let mut end = t.len().min(80);
            while !t.is_char_boundary(end) {
                end -= 1;
            }
            return Some(t[..end].trim_end().to_string());
        }
    }
    None
}

/// The two file names (and optional times) in the header of a patch.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Labels {
    pub old: String,
    pub new: String,
    /// Printed after a tab, like GNU diff does (`2024-05-01 12:00:00.000000000 +0000`).
    pub old_time: Option<String>,
    pub new_time: Option<String>,
}

fn push_line(out: &mut String, prefix: &str, l: &HunkLine) {
    out.push_str(prefix);
    out.push_str(&l.text);
    if l.eol == LineEnding::None {
        out.push_str("\n\\ No newline at end of file\n");
    } else {
        out.push_str(l.eol.as_str());
    }
}

fn header_line(out: &mut String, sign: &str, name: &str, time: Option<&str>) {
    out.push_str(sign);
    out.push(' ');
    out.push_str(name);
    if let Some(t) = time {
        out.push('\t');
        out.push_str(t);
    }
    out.push('\n');
}

/// Unified format. Nothing is written for no hunks (and no header either, like `diff -u` on equal
/// files).
pub fn render_unified(hunks: &[Hunk], labels: &Labels) -> String {
    let mut out = String::new();
    if hunks.is_empty() {
        return out;
    }
    header_line(&mut out, "---", &labels.old, labels.old_time.as_deref());
    header_line(&mut out, "+++", &labels.new, labels.new_time.as_deref());
    for h in hunks {
        out.push_str("@@ -");
        range(&mut out, h.old_start(), h.old_count);
        out.push_str(" +");
        range(&mut out, h.new_start(), h.new_count);
        out.push_str(" @@");
        if let Some(f) = &h.func {
            out.push(' ');
            out.push_str(f);
        }
        out.push('\n');
        for l in &h.lines {
            let p = match l.op {
                LineOp::Equal => " ",
                LineOp::Delete => "-",
                LineOp::Insert => "+",
            };
            push_line(&mut out, p, l);
        }
    }
    out
}

fn range(out: &mut String, start: u32, count: u32) {
    let _ = if count == 1 {
        write!(out, "{start}")
    } else {
        write!(out, "{start},{count}")
    };
}

/// Context format (`diff -c`). The old half lists lines only when the hunk deletes or changes
/// something, the new half only when it inserts or changes something (as GNU diff does).
pub fn render_context(hunks: &[Hunk], labels: &Labels) -> String {
    let mut out = String::new();
    if hunks.is_empty() {
        return out;
    }
    header_line(&mut out, "***", &labels.old, labels.old_time.as_deref());
    header_line(&mut out, "---", &labels.new, labels.new_time.as_deref());
    for h in hunks {
        out.push_str("***************");
        if let Some(f) = &h.func {
            out.push(' ');
            out.push_str(f);
        }
        out.push('\n');
        // a block that deletes and inserts marks both sides `!`; otherwise `-` / `+`
        let mut mark_old = vec![" "; h.lines.len()];
        let mut mark_new = vec![" "; h.lines.len()];
        let mut i = 0;
        while i < h.lines.len() {
            if h.lines[i].op == LineOp::Equal {
                i += 1;
                continue;
            }
            let mut j = i;
            while j < h.lines.len() && h.lines[j].op != LineOp::Equal {
                j += 1;
            }
            let blk = &h.lines[i..j];
            let (has_del, has_ins) = (
                blk.iter().any(|l| l.op == LineOp::Delete),
                blk.iter().any(|l| l.op == LineOp::Insert),
            );
            for k in i..j {
                match h.lines[k].op {
                    LineOp::Delete => mark_old[k] = if has_ins { "!" } else { "-" },
                    LineOp::Insert => mark_new[k] = if has_del { "!" } else { "+" },
                    LineOp::Equal => {}
                }
            }
            i = j;
        }
        let any_old = mark_old.iter().any(|m| *m != " ");
        let any_new = mark_new.iter().any(|m| *m != " ");
        out.push_str("*** ");
        context_range(&mut out, h.old_start(), h.old_count);
        out.push_str(" ****\n");
        if any_old {
            for (k, l) in h.lines.iter().enumerate() {
                if l.op != LineOp::Insert {
                    push_line(&mut out, &format!("{} ", mark_old[k]), l);
                }
            }
        }
        out.push_str("--- ");
        context_range(&mut out, h.new_start(), h.new_count);
        out.push_str(" ----\n");
        if any_new {
            for (k, l) in h.lines.iter().enumerate() {
                if l.op != LineOp::Delete {
                    push_line(&mut out, &format!("{} ", mark_new[k]), l);
                }
            }
        }
    }
    out
}

/// `a,b` with `b` inclusive; a single line is just `a`; an empty range is the line before it.
fn context_range(out: &mut String, start: u32, count: u32) {
    let _ = match count {
        0 | 1 => write!(out, "{start}"),
        _ => write!(out, "{start},{}", start + count - 1),
    };
}

/// Normal format (the default of `diff`): `5c5,6`, `3a4`, `7,8d6`. Built from hunks made with
/// `context = 0`.
pub fn render_normal(hunks: &[Hunk]) -> String {
    let mut out = String::new();
    for h in hunks {
        let dels: Vec<&HunkLine> = h.lines.iter().filter(|l| l.op == LineOp::Delete).collect();
        let ins: Vec<&HunkLine> = h.lines.iter().filter(|l| l.op == LineOp::Insert).collect();
        let (os, ns) = (h.old_start(), h.new_start());
        let nr = |out: &mut String, s: u32, c: u32| {
            let _ = if c <= 1 {
                write!(out, "{s}")
            } else {
                write!(out, "{s},{}", s + c - 1)
            };
        };
        if dels.is_empty() {
            let _ = write!(out, "{os}a");
            nr(&mut out, ns, h.new_count);
        } else if ins.is_empty() {
            nr(&mut out, os, h.old_count);
            let _ = write!(out, "d{ns}");
        } else {
            nr(&mut out, os, h.old_count);
            out.push('c');
            nr(&mut out, ns, h.new_count);
        }
        out.push('\n');
        for l in &dels {
            push_line(&mut out, "< ", l);
        }
        if !dels.is_empty() && !ins.is_empty() {
            out.push_str("---\n");
        }
        for l in &ins {
            push_line(&mut out, "> ", l);
        }
    }
    out
}

/// Added and deleted line counts of a set of hunks (`--stat`, `--numstat`).
pub fn counts(hunks: &[Hunk]) -> (u32, u32) {
    hunks
        .iter()
        .fold((0, 0), |(a, d), h| (a + h.additions(), d + h.deletions()))
}

/// GNU diff's time stamp: `2024-05-01 12:34:56.123456789 +0000` (UTC), from 100 ns ticks since
/// 1601 (a `FILETIME`).
pub fn format_gnu_time(ticks: i64) -> String {
    const TICKS_PER_S: i64 = 10_000_000;
    const UNIX_EPOCH_TICKS: i64 = 11_644_473_600 * TICKS_PER_S;
    let t = ticks - UNIX_EPOCH_TICKS;
    let (secs, frac) = (t.div_euclid(TICKS_PER_S), t.rem_euclid(TICKS_PER_S));
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // civil date from days since 1970-01-01 (Howard Hinnant's algorithm)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}.{:09} +0000",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60,
        frac * 100
    )
}

// 1970-01-01 was a Thursday
const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// The time stamp GNU diff prints in the *context* format headers: `Sun Nov 22 07:58:24 2026`
/// (UTC), from 100 ns ticks since 1601.
pub fn format_context_time(ticks: i64) -> String {
    const UNIX_EPOCH_TICKS: i64 = 11_644_473_600 * 10_000_000;
    let secs = (ticks - UNIX_EPOCH_TICKS).div_euclid(10_000_000);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let wd = DAYS[days.rem_euclid(7) as usize];
    // the civil date comes from the same algorithm as `format_gnu_time`
    let iso = format_gnu_time(ticks);
    let (y, m, d) = (
        &iso[0..4],
        iso[5..7].parse::<usize>().unwrap_or(1),
        iso[8..10].parse::<u32>().unwrap_or(1),
    );
    format!(
        "{wd} {} {d:>2} {:02}:{:02}:{:02} {y}",
        MONTHS[m - 1],
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// The date of a mail header (RFC 2822, as `git format-patch` writes it): `Tue, 1 Jan 2030
/// 10:00:00 +0000` (UTC), from 100 ns ticks since 1601.
pub fn format_mail_time(ticks: i64) -> String {
    let ctx = format_context_time(ticks); // `Sun Nov 22 07:58:24 2026`
    let parts: Vec<&str> = ctx.split_whitespace().collect();
    format!(
        "{}, {} {} {} {} +0000",
        parts[0],
        parts[2].parse::<u32>().unwrap_or(1),
        parts[1],
        parts[4],
        parts[3]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mail_time_has_the_rfc_2822_shape() {
        // 2030-01-01 10:00:00 UTC, a Tuesday
        let ticks = (1_893_492_000 + 11_644_473_600) * 10_000_000;
        assert_eq!(format_mail_time(ticks), "Tue, 1 Jan 2030 10:00:00 +0000");
    }

    fn compare(a: &str, b: &str, opts: &TextOptions) -> std::sync::Arc<TextCompare> {
        let tc = TextCompare::start(
            DocSpec::Bytes(a.as_bytes().to_vec()),
            DocSpec::Bytes(b.as_bytes().to_vec()),
            opts.clone(),
            None,
        )
        .unwrap();
        tc.wait();
        tc
    }

    fn unified(a: &str, b: &str, ctx: u32) -> String {
        let tc = compare(
            a,
            b,
            &diff_text_options(AlignmentName::Myers, WhitespaceName::All, false),
        );
        let h = hunks(
            &tc,
            &HunkOptions {
                context: ctx,
                ..HunkOptions::default()
            },
        )
        .unwrap();
        render_unified(
            &h,
            &Labels {
                old: "a".into(),
                new: "b".into(),
                ..Labels::default()
            },
        )
    }

    #[test]
    fn a_single_change_with_context() {
        let a = "1\n2\n3\n4\n5\n6\n7\n8\n9\n";
        let b = "1\n2\n3\n4\nFIVE\n6\n7\n8\n9\n";
        assert_eq!(
            unified(a, b, 3),
            "--- a\n+++ b\n@@ -2,7 +2,7 @@\n 2\n 3\n 4\n-5\n+FIVE\n 6\n 7\n 8\n"
        );
    }

    #[test]
    fn equal_files_print_nothing() {
        assert_eq!(unified("x\ny\n", "x\ny\n", 3), "");
    }

    #[test]
    fn counts_of_one_drop_the_comma_and_empty_ranges_start_before() {
        // insert into an empty file: -0,0 +1
        assert_eq!(
            unified("", "new\n", 3),
            "--- a\n+++ b\n@@ -0,0 +1 @@\n+new\n"
        );
        // delete everything: -1 +0,0
        assert_eq!(
            unified("old\n", "", 3),
            "--- a\n+++ b\n@@ -1 +0,0 @@\n-old\n"
        );
    }

    #[test]
    fn changes_within_twice_the_context_share_a_hunk_and_further_ones_split() {
        let base: String = (1..=30)
            .map(|i| {
                format!(
                    "line{i}
"
                )
            })
            .collect();
        // changes at 5 and 12: six equal lines between them (6..=11): at most 2 * 3, one hunk
        let near = base
            .replace(
                "line5
", "X
",
            )
            .replace(
                "line12
", "Y
",
            );
        assert_eq!(unified(&base, &near, 3).matches("@@ -").count(), 1);
        // changes at 5 and 13: seven equal lines (6..=12) is more than 2 * 3: two hunks
        let far = base
            .replace(
                "line5
", "X
",
            )
            .replace(
                "line13
", "Y
",
            );
        assert_eq!(unified(&base, &far, 3).matches("@@ -").count(), 2);
    }

    #[test]
    fn a_missing_final_newline_is_marked_and_is_itself_a_change() {
        assert_eq!(
            unified("a\nb", "a\nb\n", 3),
            "--- a\n+++ b\n@@ -1,2 +1,2 @@\n a\n-b\n\\ No newline at end of file\n+b\n"
        );
        assert_eq!(
            unified("a\nb\n", "a\nc", 3),
            "--- a\n+++ b\n@@ -1,2 +1,2 @@\n a\n-b\n+c\n\\ No newline at end of file\n"
        );
    }

    #[test]
    fn crlf_against_lf_is_a_change_unless_asked_to_ignore_it() {
        let strict = diff_text_options(AlignmentName::Myers, WhitespaceName::All, false);
        let tc = compare("a\r\nb\r\n", "a\nb\n", &strict);
        assert_eq!(
            counts(&hunks(&tc, &HunkOptions::default()).unwrap()),
            (2, 2)
        );
        let lax = TextOptions {
            eol: EolName::CrLfAsLf,
            ..strict.clone()
        };
        let tc = compare("a\r\nb\r\n", "a\nb\n", &lax);
        assert!(hunks(&tc, &HunkOptions::default()).unwrap().is_empty());
        // a missing final newline still counts with CrLfAsLf
        let tc = compare("a\r\nb", "a\nb\n", &lax);
        assert_eq!(
            counts(&hunks(&tc, &HunkOptions::default()).unwrap()),
            (1, 1)
        );
        // and nothing about endings counts when they are not compared
        let ignore = TextOptions {
            eol: EolName::Ignore,
            ..strict
        };
        let tc = compare("a\r\nb", "a\nb\n", &ignore);
        assert!(hunks(&tc, &HunkOptions::default()).unwrap().is_empty());
    }

    #[test]
    fn whitespace_options_make_lines_equal_and_context_comes_from_the_left() {
        let tc = compare(
            "a  b\nkeep\nold\n",
            "a b\nkeep\nnew\n",
            &diff_text_options(AlignmentName::Myers, WhitespaceName::Changes, false),
        );
        let h = hunks(
            &tc,
            &HunkOptions {
                context: 1,
                ..HunkOptions::default()
            },
        )
        .unwrap();
        let text = render_unified(
            &h,
            &Labels {
                old: "a".into(),
                new: "b".into(),
                ..Labels::default()
            },
        );
        assert_eq!(text, "--- a\n+++ b\n@@ -2,2 +2,2 @@\n keep\n-old\n+new\n");
    }

    #[test]
    fn ignore_blank_lines_drops_blank_only_changes() {
        let tc = compare(
            "a\nb\n",
            "a\n\nb\n",
            &diff_text_options(AlignmentName::Myers, WhitespaceName::All, false),
        );
        let kept = hunks(&tc, &HunkOptions::default()).unwrap();
        assert_eq!(counts(&kept), (1, 0));
        let dropped = hunks(
            &tc,
            &HunkOptions {
                ignore_blank_lines: true,
                ..HunkOptions::default()
            },
        )
        .unwrap();
        assert!(dropped.is_empty());
    }

    #[test]
    fn the_function_header_is_the_last_function_like_line_above_the_hunk() {
        let a = "int main() {\n  x();\n  y();\n  z();\n  w();\n  v();\n  u();\n}\n";
        let b = a.replace("  v();", "  V();");
        let tc = compare(
            a,
            &b,
            &diff_text_options(AlignmentName::Myers, WhitespaceName::All, false),
        );
        let h = hunks(
            &tc,
            &HunkOptions {
                context: 1,
                func_header: true,
                ..HunkOptions::default()
            },
        )
        .unwrap();
        assert_eq!(h[0].func.as_deref(), Some("int main() {"));
        assert!(render_unified(&h, &Labels::default()).contains("@@ -5,3 +5,3 @@ int main() {\n"));
    }

    #[test]
    fn normal_format_uses_a_c_and_d() {
        let tc = compare(
            "1\n2\n3\n4\n",
            "1\nTWO\n3\n4\n5\n",
            &diff_text_options(AlignmentName::Myers, WhitespaceName::All, false),
        );
        let h = hunks(
            &tc,
            &HunkOptions {
                context: 0,
                ..HunkOptions::default()
            },
        )
        .unwrap();
        assert_eq!(render_normal(&h), "2c2\n< 2\n---\n> TWO\n4a5\n> 5\n");
        let tc = compare(
            "1\n2\n3\n",
            "1\n",
            &diff_text_options(AlignmentName::Myers, WhitespaceName::All, false),
        );
        let h = hunks(
            &tc,
            &HunkOptions {
                context: 0,
                ..HunkOptions::default()
            },
        )
        .unwrap();
        assert_eq!(render_normal(&h), "2,3d1\n< 2\n< 3\n");
    }

    #[test]
    fn context_time_format() {
        let ticks = (1_714_566_896i64 + 11_644_473_600) * 10_000_000;
        assert_eq!(format_context_time(ticks), "Wed May  1 12:34:56 2024");
    }

    #[test]
    fn gnu_time_format() {
        // 2024-05-01 12:34:56.5 UTC
        let unix = 1_714_566_896i64;
        let ticks = (unix + 11_644_473_600) * 10_000_000 + 5_000_000;
        assert_eq!(
            format_gnu_time(ticks),
            "2024-05-01 12:34:56.500000000 +0000"
        );
    }
}
