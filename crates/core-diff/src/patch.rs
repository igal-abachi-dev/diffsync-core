//! Reading and applying patches (backlog E11.5-S16): unified diffs as `diff -u` and `git diff`
//! print them, with git's extended headers, and git mailbox patches (`git format-patch`).
//!
//! Applying is `patch` / `git apply` in memory: [`apply_file_patch`] takes the text of one file and
//! its [`FilePatch`] and returns the new text plus a verdict per hunk. Nothing here touches the file
//! system; `core-tools` does the reading, the all-or-nothing decision and the safe writes.
//!
//! What a hunk must satisfy (decision D58): its context and deleted lines match the file, at the
//! place the header says or at the nearest place that matches (the way GNU patch carries an offset
//! forward); if nothing matches, up to `fuzz` outer context lines are dropped (GNU patch's `-F`).
//! A hunk never applies where it does not match, and hunks never overlap or reorder. A line ending
//! that differs (CRLF in the file, LF in the patch) is accepted in a second pass and the inserted
//! lines then take the ending of the file.
//!
//! Not supported, and reported as such: context (`*** `) and normal diffs, combined (`@@@`) diffs,
//! git binary patches.

use serde::Serialize;

use crate::udiff::LineOp;

/// One patch: the text of a mail, or the whole input when it is not a mailbox.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Patch {
    pub mail: Option<MailHeader>,
    pub files: Vec<FilePatch>,
}

/// The header of a `git format-patch` mail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct MailHeader {
    pub from: String,
    pub date: String,
    /// With the `[PATCH n/m]` tag removed.
    pub subject: String,
    /// The commit message below the subject, before the `---` line.
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum FileKind {
    Modify,
    Create,
    Delete,
    Rename,
}

/// The changes of one file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct FilePatch {
    /// As written in the patch (`a/src/x.rs`); `None` is `/dev/null`.
    pub old_path: Option<String>,
    pub new_path: Option<String>,
    pub binary: bool,
    pub old_mode: Option<String>,
    pub new_mode: Option<String>,
    /// `rename from` / `rename to` were present.
    pub renamed: bool,
    pub hunks: Vec<PatchHunk>,
}

impl FilePatch {
    pub fn kind(&self) -> FileKind {
        match (&self.old_path, &self.new_path) {
            (None, _) => FileKind::Create,
            (_, None) => FileKind::Delete,
            (Some(a), Some(b)) if self.renamed || a != b => FileKind::Rename,
            _ => FileKind::Modify,
        }
    }

    /// The path the patch speaks of with `strip` leading components removed (`patch -p`), the new
    /// name when there is one.
    pub fn path(&self, strip: usize) -> Option<String> {
        self.new_path
            .as_deref()
            .or(self.old_path.as_deref())
            .map(|p| strip_components(p, strip))
    }

    pub fn new_path_stripped(&self, strip: usize) -> Option<String> {
        self.new_path.as_deref().map(|p| strip_components(p, strip))
    }

    pub fn old_path_stripped(&self, strip: usize) -> Option<String> {
        self.old_path.as_deref().map(|p| strip_components(p, strip))
    }

    pub fn additions(&self) -> u32 {
        self.hunks.iter().map(PatchHunk::additions).sum()
    }

    pub fn deletions(&self) -> u32 {
        self.hunks.iter().map(PatchHunk::deletions).sum()
    }
}

/// Removes `n` leading path components (`patch -pN`).
pub fn strip_components(p: &str, n: usize) -> String {
    let mut rest = p;
    for _ in 0..n {
        match rest.find(['/', '\\']) {
            Some(i) => rest = &rest[i + 1..],
            None => break,
        }
    }
    rest.to_string()
}

/// One line of a hunk. `text` has no line ending; `cr` says the patch line ended in CR LF.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PatchLine {
    pub op: LineOp,
    pub text: String,
    pub cr: bool,
    /// `\ No newline at end of file` followed this line.
    pub no_eol: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PatchHunk {
    /// As in the header: 1-based, and for an empty range the line before it.
    pub old_start: u32,
    pub old_count: u32,
    pub new_start: u32,
    pub new_count: u32,
    pub lines: Vec<PatchLine>,
    /// The header counts did not match the body (an editor or a model wrote the patch); the counts
    /// were taken from the body.
    pub recounted: bool,
}

impl PatchHunk {
    pub fn additions(&self) -> u32 {
        self.lines.iter().filter(|l| l.op == LineOp::Insert).count() as u32
    }

    pub fn deletions(&self) -> u32 {
        self.lines.iter().filter(|l| l.op == LineOp::Delete).count() as u32
    }
}

/// The input is not a patch this module reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchError {
    /// 1-based line of the input.
    pub line: usize,
    pub message: String,
}

impl std::fmt::Display for PatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "patch line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for PatchError {}

fn perr<T>(line: usize, m: impl Into<String>) -> Result<T, PatchError> {
    Err(PatchError {
        line: line + 1,
        message: m.into(),
    })
}

fn t(l: &str) -> &str {
    l.strip_suffix('\r').unwrap_or(l)
}

fn is_mbox_from(l: &str) -> bool {
    // `From <40 hex> Mon Sep 17 00:00:00 2001`: git's fixed separator
    let Some(rest) = t(l).strip_prefix("From ") else {
        return false;
    };
    let id = rest.split(' ').next().unwrap_or("");
    id.len() >= 7 && id.bytes().all(|b| b.is_ascii_hexdigit())
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    let Some(inner) = s.strip_prefix('"').and_then(|x| x.strip_suffix('"')) else {
        return s.to_string();
    };
    let b = inner.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 1 < b.len() {
            i += 1;
            match b[i] {
                b'n' => out.push(b'\n'),
                b't' => out.push(b'\t'),
                b'0'..=b'7' if i + 2 < b.len() && b[i + 1].is_ascii_digit() => {
                    let v = u32::from(b[i] - b'0') * 64
                        + u32::from(b[i + 1] - b'0') * 8
                        + u32::from(b.get(i + 2).map_or(0, |c| c - b'0'));
                    out.push(v as u8);
                    i += 2;
                }
                c => out.push(c),
            }
        } else {
            out.push(b[i]);
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The file name of a `---` / `+++` line: quotes removed, the tab-separated time dropped.
fn header_path(rest: &str) -> Option<String> {
    let rest = t(rest);
    let name = rest.split('\t').next().unwrap_or(rest);
    let name = unquote(name);
    if name == "/dev/null" || name.is_empty() {
        None
    } else {
        Some(name)
    }
}

fn parse_range(s: &str) -> Option<(u32, u32)> {
    let s = s.strip_prefix(['-', '+'])?;
    match s.split_once(',') {
        Some((a, b)) => Some((a.parse().ok()?, b.parse().ok()?)),
        None => Some((s.parse().ok()?, 1)),
    }
}

fn parse_hunk_header(l: &str) -> Option<(u32, u32, u32, u32)> {
    let rest = t(l).strip_prefix("@@ ")?;
    let (ranges, _) = rest.split_once(" @@")?;
    let (a, b) = ranges.split_once(' ')?;
    let (os, oc) = parse_range(a)?;
    let (ns, nc) = parse_range(b)?;
    Some((os, oc, ns, nc))
}

/// A line that ends a hunk body: the next hunk, the next file, the next mail.
fn is_boundary(lines: &[&str], i: usize) -> bool {
    let l = lines[i];
    l.starts_with("@@ ")
        || l.starts_with("diff --git ")
        || l.starts_with("diff --cc ")
        || is_mbox_from(l)
        || (l.starts_with("--- ") && lines.get(i + 1).is_some_and(|n| n.starts_with("+++ ")))
}

fn parse_hunk(
    lines: &[&str],
    start: usize,
    hdr: (u32, u32, u32, u32),
) -> Result<(PatchHunk, usize), PatchError> {
    let (os, oc, ns, nc) = hdr;
    let mut h = PatchHunk {
        old_start: os,
        old_count: oc,
        new_start: ns,
        new_count: nc,
        ..PatchHunk::default()
    };
    let (mut old_seen, mut new_seen) = (0u32, 0u32);
    let mut i = start + 1;
    while i < lines.len() {
        let raw = lines[i];
        if old_seen >= oc && new_seen >= nc {
            // counts met: only a "no newline" marker can still belong to the hunk
            if raw.starts_with('\\') {
                if let Some(last) = h.lines.last_mut() {
                    last.no_eol = true;
                }
                i += 1;
            }
            break;
        }
        if raw.starts_with('\\') {
            if let Some(last) = h.lines.last_mut() {
                last.no_eol = true;
            }
            i += 1;
            continue;
        }
        if is_boundary(lines, i) {
            break; // fewer lines than the header says
        }
        let (op, body) = match raw.as_bytes().first() {
            Some(b' ') => (LineOp::Equal, &raw[1..]),
            Some(b'-') => (LineOp::Delete, &raw[1..]),
            Some(b'+') => (LineOp::Insert, &raw[1..]),
            // an empty line is a context line whose blank was stripped (or "\r" alone)
            None => (LineOp::Equal, ""),
            Some(b'\r') if raw.len() == 1 => (LineOp::Equal, raw),
            _ => break,
        };
        let cr = body.ends_with('\r');
        let text = if cr { &body[..body.len() - 1] } else { body };
        match op {
            LineOp::Equal => {
                old_seen += 1;
                new_seen += 1;
            }
            LineOp::Delete => old_seen += 1,
            LineOp::Insert => new_seen += 1,
        }
        h.lines.push(PatchLine {
            op,
            text: text.to_string(),
            cr,
            no_eol: false,
        });
        i += 1;
    }
    // the last line of the input may be the empty string after the final newline
    while h.lines.last().is_some_and(|l| {
        l.op == LineOp::Equal && l.text.is_empty() && !l.cr && (old_seen > oc || new_seen > nc)
    }) {
        h.lines.pop();
        old_seen -= 1;
        new_seen -= 1;
    }
    if old_seen != oc || new_seen != nc {
        h.recounted = true;
        h.old_count = old_seen;
        h.new_count = new_seen;
    }
    if h.lines.is_empty() && (oc != 0 || nc != 0) {
        return perr(start, "hunk without lines");
    }
    Ok((h, i))
}

/// Reads a patch or a mailbox of patches. Text that has no file header at all is an error, so a
/// wrong input is reported instead of "applied" as nothing.
pub fn parse_patches(text: &str) -> Result<Vec<Patch>, PatchError> {
    let lines: Vec<&str> = {
        let mut v: Vec<&str> = text.split('\n').collect();
        if v.last() == Some(&"") {
            v.pop();
        }
        v
    };
    let mut patches: Vec<Patch> = Vec::new();
    let mut cur: Option<Patch> = None;
    let mut i = 0;
    while i < lines.len() {
        let l = lines[i];
        if is_mbox_from(l) {
            if let Some(p) = cur.take() {
                patches.push(p);
            }
            let (mail, next) = parse_mail_header(&lines, i + 1);
            cur = Some(Patch {
                mail: Some(mail),
                files: Vec::new(),
            });
            i = next;
            continue;
        }
        if l.starts_with("diff --cc ") || l.starts_with("diff --combined ") || l.starts_with("@@@ ")
        {
            return perr(i, "combined diffs (diff --cc) are not supported");
        }
        if l.starts_with("*** ") && lines.get(i + 1).is_some_and(|n| n.starts_with("--- ")) {
            return perr(i, "context diffs (diff -c) are not supported; use unified");
        }
        if l.starts_with("diff --git ") {
            let fp = parse_git_header(&lines, &mut i);
            cur.get_or_insert_with(Patch::default).files.push(fp);
            continue;
        }
        if l.starts_with("--- ") && lines.get(i + 1).is_some_and(|n| n.starts_with("+++ ")) {
            let p = cur.get_or_insert_with(Patch::default);
            let fp = FilePatch {
                old_path: header_path(&l[4..]),
                new_path: header_path(&lines[i + 1][4..]),
                ..FilePatch::default()
            };
            p.files.push(fp);
            i += 2;
            continue;
        }
        if let Some(hdr) = parse_hunk_header(l) {
            let Some(fp) = cur.as_mut().and_then(|p| p.files.last_mut()) else {
                return perr(i, "hunk before any file header");
            };
            let (h, next) = parse_hunk(&lines, i, hdr)?;
            fp.hunks.push(h);
            i = next;
            continue;
        }
        i += 1; // commit message, diffstat, signature, anything between files
    }
    if let Some(p) = cur.take() {
        patches.push(p);
    }
    if patches.iter().all(|p| p.files.is_empty()) {
        return perr(0, "no file header (--- / +++ or diff --git) found");
    }
    Ok(patches)
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// A header value with non-ASCII text as an RFC 2047 encoded word (`=?UTF-8?B?...?=`), as `git
/// format-patch` writes it; ASCII text is returned as it is.
pub fn encode_header_words(s: &str) -> String {
    if s.is_ascii() {
        return s.to_string();
    }
    let mut out = String::new();
    for c in s.as_bytes().chunks(3) {
        let n = (u32::from(c[0]) << 16)
            | (u32::from(*c.get(1).unwrap_or(&0)) << 8)
            | u32::from(*c.get(2).unwrap_or(&0));
        for k in 0..4 {
            if k <= c.len() {
                out.push(B64[(n >> (18 - 6 * k) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    format!("=?UTF-8?B?{out}?=")
}

fn b64_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let (mut acc, mut bits) = (0u32, 0);
    for c in s.bytes().filter(|c| *c != b'=') {
        let v = B64.iter().position(|b| *b == c)? as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// Decodes the RFC 2047 encoded words (`=?UTF-8?B?...?=`, `=?UTF-8?q?...?=`) of a header value.
pub fn decode_header_words(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find("=?") {
        out.push_str(&rest[..i]);
        let tail = &rest[i + 2..];
        let decoded = (|| {
            let (charset, r) = tail.split_once('?')?;
            let (enc, r) = r.split_once('?')?;
            let (text, after) = r.split_once("?=")?;
            if !charset.eq_ignore_ascii_case("utf-8") {
                return None;
            }
            let bytes = if enc.eq_ignore_ascii_case("b") {
                b64_decode(text)?
            } else if enc.eq_ignore_ascii_case("q") {
                let b = text.as_bytes();
                let mut v = Vec::new();
                let mut k = 0;
                while k < b.len() {
                    match b[k] {
                        b'_' => v.push(b' '),
                        b'=' if k + 2 < b.len() => {
                            v.push(u8::from_str_radix(text.get(k + 1..k + 3)?, 16).ok()?);
                            k += 2;
                        }
                        c => v.push(c),
                    }
                    k += 1;
                }
                v
            } else {
                return None;
            };
            Some((String::from_utf8(bytes).ok()?, after))
        })();
        if let Some((t, after)) = decoded {
            out.push_str(&t);
            // white space between two encoded words is not part of the text
            let trimmed = after.trim_start_matches([' ', '\t']);
            rest = if trimmed.starts_with("=?") {
                trimmed
            } else {
                after
            };
        } else {
            out.push_str("=?");
            rest = tail;
        }
    }
    out.push_str(rest);
    out
}

fn parse_mail_header(lines: &[&str], mut i: usize) -> (MailHeader, usize) {
    let mut m = MailHeader::default();
    let mut last: Option<&'static str> = None;
    while i < lines.len() {
        let l = t(lines[i]);
        if l.is_empty() {
            i += 1;
            break;
        }
        if (l.starts_with(' ') || l.starts_with('\t')) && last.is_some() {
            let add = format!(" {}", l.trim());
            match last {
                Some("subject") => m.subject.push_str(&add),
                Some("from") => m.from.push_str(&add),
                _ => {}
            }
        } else if let Some(v) = l.strip_prefix("From: ") {
            m.from = v.to_string();
            last = Some("from");
        } else if let Some(v) = l.strip_prefix("Date: ") {
            m.date = v.to_string();
            last = Some("date");
        } else if let Some(v) = l.strip_prefix("Subject: ") {
            m.subject = v.to_string();
            last = Some("subject");
        } else {
            last = None;
        }
        i += 1;
    }
    m.subject = decode_header_words(&m.subject);
    m.from = decode_header_words(&m.from);
    // `[PATCH 2/5] ` and `[PATCH v2] ` tags are not part of the subject
    if let Some(rest) = m.subject.strip_prefix('[') {
        if let Some((_, after)) = rest.split_once(']') {
            m.subject = after.trim_start().to_string();
        }
    }
    let mut body: Vec<&str> = Vec::new();
    while i < lines.len() {
        let l = t(lines[i]);
        if l == "---" || lines[i].starts_with("diff --") || is_mbox_from(lines[i]) {
            break;
        }
        body.push(l);
        i += 1;
    }
    m.message = body.join("\n").trim().to_string();
    (m, i)
}

fn parse_git_header(lines: &[&str], i: &mut usize) -> FilePatch {
    let first = t(lines[*i]);
    // `diff --git a/x b/y`: the names again follow in ---/+++ unless the file is only renamed
    // or binary; take them from here as the fallback (split in the middle when unquoted)
    let rest = &first["diff --git ".len()..];
    let (mut old, mut new) = (None, None);
    if let Some(mid) = rest.find(" b/") {
        old = Some(unquote(&rest[..mid]));
        new = Some(unquote(&rest[mid + 1..]));
    }
    let mut fp = FilePatch {
        old_path: old,
        new_path: new,
        ..FilePatch::default()
    };
    *i += 1;
    while *i < lines.len() {
        let l = t(lines[*i]);
        if let Some(old) = l.strip_prefix("--- ") {
            fp.old_path = header_path(old);
            if let Some(n) = lines.get(*i + 1).and_then(|n| n.strip_prefix("+++ ")) {
                fp.new_path = header_path(n);
                *i += 1;
            }
            *i += 1;
            break;
        } else if l.starts_with("@@ ") || l.starts_with("diff --git ") || is_mbox_from(l) {
            break;
        } else if let Some(v) = l.strip_prefix("new file mode ") {
            fp.old_path = None;
            fp.new_mode = Some(v.to_string());
        } else if let Some(v) = l.strip_prefix("deleted file mode ") {
            fp.new_path = None;
            fp.old_mode = Some(v.to_string());
        } else if let Some(v) = l.strip_prefix("old mode ") {
            fp.old_mode = Some(v.to_string());
        } else if let Some(v) = l.strip_prefix("new mode ") {
            fp.new_mode = Some(v.to_string());
        } else if let Some(v) = l.strip_prefix("rename from ") {
            // `rename from` has no a/ prefix; keep the longer name of the diff --git line
            let v = unquote(v);
            if !fp.old_path.as_deref().is_some_and(|p| p.ends_with(&v)) {
                fp.old_path = Some(v);
            }
            fp.renamed = true;
        } else if let Some(v) = l.strip_prefix("rename to ") {
            let v = unquote(v);
            if !fp.new_path.as_deref().is_some_and(|p| p.ends_with(&v)) {
                fp.new_path = Some(v);
            }
            fp.renamed = true;
        } else if l.starts_with("Binary files ") || l.starts_with("GIT binary patch") {
            fp.binary = true;
        }
        *i += 1;
    }
    fp
}

// ---------------------------------------------------------------------------------------------
// applying

/// How a hunk is placed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyOptions {
    /// Undo the patch (`patch -R`).
    pub reverse: bool,
    /// Outer context lines that may be dropped to find a place (default 2, as `patch`).
    pub fuzz: u32,
    /// Compare lines with all white space ignored (`patch -l`).
    pub ignore_whitespace: bool,
}

impl Default for ApplyOptions {
    fn default() -> Self {
        ApplyOptions {
            reverse: false,
            fuzz: 2,
            ignore_whitespace: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HunkResult {
    /// 0-based index in the file patch.
    pub index: usize,
    pub applied: bool,
    /// Lines between the place in the header and the place found.
    pub offset: i64,
    /// Context lines dropped at each end to find the place.
    pub fuzz: u32,
    /// The ending of some lines differed (CRLF against LF) and was accepted.
    pub loose_endings: bool,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Applied {
    /// The text with every applicable hunk applied. Equal to the input when none was.
    pub text: String,
    pub hunks: Vec<HunkResult>,
}

impl Applied {
    pub fn all_applied(&self) -> bool {
        self.hunks.iter().all(|h| h.applied)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Eol {
    None,
    Lf,
    CrLf,
}

#[derive(Debug, Clone)]
pub(crate) struct FLine {
    pub(crate) text: String,
    pub(crate) eol: Eol,
}

pub(crate) fn split_lines(text: &str) -> Vec<FLine> {
    let mut v = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        if let Some(i) = rest.find('\n') {
            let (body, eol) = match rest[..i].strip_suffix('\r') {
                Some(b) => (b, Eol::CrLf),
                None => (&rest[..i], Eol::Lf),
            };
            v.push(FLine {
                text: body.to_string(),
                eol,
            });
            rest = &rest[i + 1..];
        } else {
            v.push(FLine {
                text: rest.to_string(),
                eol: Eol::None,
            });
            break;
        }
    }
    v
}

pub(crate) fn join_lines(lines: &[FLine]) -> String {
    let mut s = String::new();
    for l in lines {
        s.push_str(&l.text);
        match l.eol {
            Eol::None => {}
            Eol::Lf => s.push('\n'),
            Eol::CrLf => s.push_str("\r\n"),
        }
    }
    s
}

fn squash(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn line_matches(f: &FLine, p: &PatchLine, strict: bool, ws: bool) -> bool {
    let same = if ws {
        squash(&f.text) == squash(&p.text)
    } else {
        f.text == p.text
    };
    if !same {
        return false;
    }
    if !strict {
        return true;
    }
    if p.no_eol {
        f.eol == Eol::None
    } else {
        f.eol != Eol::None && p.cr == (f.eol == Eol::CrLf)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Anchor {
    None,
    Start,
    End,
}

struct Edit {
    start: usize,
    len: usize,
    repl: Vec<FLine>,
}

/// Applies the hunks of `fp` to `text`. Hunks that do not fit are reported and left out; the
/// caller decides whether a partial result is acceptable.
pub fn apply_file_patch(text: &str, fp: &FilePatch, o: &ApplyOptions) -> Applied {
    let lines = split_lines(text);
    let crlf_dominant = {
        let c = lines.iter().filter(|l| l.eol == Eol::CrLf).count();
        let l = lines.iter().filter(|l| l.eol == Eol::Lf).count();
        c > l
    };
    let mut results = Vec::with_capacity(fp.hunks.len());
    let mut edits: Vec<Edit> = Vec::new();
    let mut prev_end = 0usize;
    let mut carry: i64 = 0;
    for (index, h) in fp.hunks.iter().enumerate() {
        let (old_side, new_side, start, count): (Vec<&PatchLine>, Vec<&PatchLine>, u32, u32) =
            if o.reverse {
                (
                    h.lines.iter().filter(|l| l.op != LineOp::Delete).collect(),
                    h.lines.iter().filter(|l| l.op != LineOp::Insert).collect(),
                    h.new_start,
                    h.new_count,
                )
            } else {
                (
                    h.lines.iter().filter(|l| l.op != LineOp::Insert).collect(),
                    h.lines.iter().filter(|l| l.op != LineOp::Delete).collect(),
                    h.old_start,
                    h.old_count,
                )
            };
        // 0-based line the header expects the first old line at
        let header_idx = if count == 0 {
            i64::from(start)
        } else {
            i64::from(start) - 1
        };
        let lead_ctx = h.lines.iter().take_while(|l| l.op == LineOp::Equal).count();
        let trail_ctx = h
            .lines
            .iter()
            .rev()
            .take_while(|l| l.op == LineOp::Equal)
            .count();
        let only_ctx = lead_ctx == h.lines.len();
        // GNU patch: a hunk with less context after the change than before it ends the file, one
        // with less before than after starts it (the diff only cut the context at the edge)
        let anchor = if only_ctx || lead_ctx == trail_ctx {
            Anchor::None
        } else if trail_ctx < lead_ctx {
            Anchor::End
        } else {
            Anchor::Start
        };
        let mut found: Option<(usize, u32, bool, usize, usize)> = None;
        'fuzz: for fuzz in 0..=o.fuzz {
            let lead = (fuzz as usize).min(lead_ctx);
            let trail = if only_ctx {
                0
            } else {
                (fuzz as usize).min(trail_ctx)
            };
            if lead + trail > old_side.len() {
                continue;
            }
            let core = &old_side[lead..old_side.len() - trail];
            // the anchor holds until the fuzz has used up the missing context (GNU patch)
            let deficit = lead_ctx.abs_diff(trail_ctx);
            let anchor_now = if (fuzz as usize) < deficit {
                anchor
            } else {
                Anchor::None
            };
            for strict in [true, false] {
                let expected = header_idx + carry + lead as i64;
                if let Some(p) = find_place(
                    &lines,
                    core,
                    expected,
                    strict,
                    o.ignore_whitespace,
                    (anchor_now, lead, trail),
                ) {
                    found = Some((p, fuzz, !strict, lead, trail));
                    break 'fuzz;
                }
            }
        }
        let Some((p, fuzz, loose, lead, trail)) = found else {
            results.push(HunkResult {
                index,
                applied: false,
                offset: 0,
                fuzz: 0,
                loose_endings: false,
                message: format!(
                    "hunk #{} (@@ -{},{} @@) does not match the file",
                    index + 1,
                    h.old_start,
                    h.old_count
                ),
            });
            continue;
        };
        if p < prev_end {
            results.push(HunkResult {
                index,
                applied: false,
                offset: 0,
                fuzz,
                loose_endings: loose,
                message: format!("hunk #{} overlaps the previous hunk", index + 1),
            });
            continue;
        }
        let offset = p as i64 - lead as i64 - header_idx;
        carry = offset;
        let core_len = old_side.len() - lead - trail;
        let repl: Vec<FLine> = new_side[lead..new_side.len() - trail]
            .iter()
            .map(|pl| FLine {
                text: pl.text.clone(),
                eol: if pl.no_eol {
                    Eol::None
                } else if pl.cr || (loose && crlf_dominant) {
                    Eol::CrLf
                } else {
                    Eol::Lf
                },
            })
            .collect();
        prev_end = p + core_len;
        edits.push(Edit {
            start: p,
            len: core_len,
            repl,
        });
        results.push(HunkResult {
            index,
            applied: true,
            offset,
            fuzz,
            loose_endings: loose,
            message: String::new(),
        });
    }
    let mut out: Vec<FLine> = Vec::with_capacity(lines.len());
    let mut pos = 0usize;
    for e in edits {
        out.extend(lines[pos..e.start].iter().cloned());
        out.extend(e.repl);
        pos = e.start + e.len;
    }
    out.extend(lines[pos..].iter().cloned());
    // a line that is no longer the last one must end somehow
    let n = out.len();
    for l in out.iter_mut().take(n.saturating_sub(1)) {
        if l.eol == Eol::None {
            l.eol = if crlf_dominant { Eol::CrLf } else { Eol::Lf };
        }
    }
    Applied {
        text: join_lines(&out),
        hunks: results,
    }
}

/// The index in `lines` where `core` matches, the one nearest to `expected`.
fn find_place(
    lines: &[FLine],
    core: &[&PatchLine],
    expected: i64,
    strict: bool,
    ws: bool,
    (anchor, lead, trail): (Anchor, usize, usize),
) -> Option<usize> {
    let max_start = lines.len().checked_sub(core.len())? as i64;
    let at = |p: i64| -> bool {
        (0..=max_start).contains(&p)
            && match anchor {
                Anchor::None => true,
                Anchor::Start => p as usize == lead,
                Anchor::End => p as usize + core.len() + trail == lines.len(),
            }
            && core
                .iter()
                .enumerate()
                .all(|(k, pl)| line_matches(&lines[p as usize + k], pl, strict, ws))
    };
    if core.is_empty() {
        let p = expected.clamp(0, lines.len() as i64) as usize;
        return match anchor {
            Anchor::Start if p != lead => None,
            Anchor::End if p + trail != lines.len() => None,
            _ => Some(p),
        };
    }
    let mut d = 0i64;
    loop {
        let (a, b) = (expected + d, expected - d);
        if a > max_start && b < 0 {
            return None;
        }
        if at(a) {
            return Some(a as usize);
        }
        if d > 0 && at(b) {
            return Some(b as usize);
        }
        d += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PATCH: &str = "--- a/f.txt\t2020-01-01 00:00:00\n+++ b/f.txt\t2020-01-02 00:00:00\n@@ -2,3 +2,3 @@\n b\n-c\n+C\n d\n";

    fn one(p: &str) -> FilePatch {
        parse_patches(p).unwrap().remove(0).files.remove(0)
    }

    #[test]
    fn a_unified_diff_is_read_and_applied() {
        let fp = one(PATCH);
        assert_eq!(fp.old_path.as_deref(), Some("a/f.txt"));
        assert_eq!(fp.path(1).as_deref(), Some("f.txt"));
        assert_eq!(fp.hunks.len(), 1);
        assert_eq!((fp.additions(), fp.deletions()), (1, 1));
        let r = apply_file_patch("a\nb\nc\nd\ne\n", &fp, &ApplyOptions::default());
        assert!(r.all_applied());
        assert_eq!(r.text, "a\nb\nC\nd\ne\n");
        let back = apply_file_patch(
            &r.text,
            &fp,
            &ApplyOptions {
                reverse: true,
                ..ApplyOptions::default()
            },
        );
        assert_eq!(back.text, "a\nb\nc\nd\ne\n");
    }

    #[test]
    fn the_nearest_place_is_found_and_the_offset_reported() {
        let fp = one(PATCH);
        // three lines inserted above: the hunk moves down by three
        let r = apply_file_patch("x\ny\nz\na\nb\nc\nd\ne\n", &fp, &ApplyOptions::default());
        assert!(r.all_applied());
        assert_eq!(r.hunks[0].offset, 3);
        assert_eq!(r.text, "x\ny\nz\na\nb\nC\nd\ne\n");
    }

    #[test]
    fn a_hunk_that_does_not_match_is_refused_not_forced() {
        let fp = one(PATCH);
        let r = apply_file_patch("a\nb\nQ\nd\ne\n", &fp, &ApplyOptions::default());
        assert!(!r.all_applied());
        assert_eq!(r.text, "a\nb\nQ\nd\ne\n");
        assert!(r.hunks[0].message.contains("does not match"));
    }

    #[test]
    fn fuzz_drops_outer_context_only() {
        let fp = one(PATCH);
        // `b` and `d` (the context) differ, the changed line is still there
        let text = "a\nB\nc\nD\ne\n";
        let none = apply_file_patch(
            text,
            &fp,
            &ApplyOptions {
                fuzz: 0,
                ..ApplyOptions::default()
            },
        );
        assert!(!none.all_applied());
        let r = apply_file_patch(text, &fp, &ApplyOptions::default());
        assert!(r.all_applied());
        assert_eq!(r.hunks[0].fuzz, 1);
        assert_eq!(r.text, "a\nB\nC\nD\ne\n");
    }

    #[test]
    fn crlf_files_take_an_lf_patch_and_keep_their_endings() {
        let fp = one(PATCH);
        let r = apply_file_patch("a\r\nb\r\nc\r\nd\r\ne\r\n", &fp, &ApplyOptions::default());
        assert!(r.all_applied());
        assert!(r.hunks[0].loose_endings);
        assert_eq!(r.text, "a\r\nb\r\nC\r\nd\r\ne\r\n");
    }

    #[test]
    fn no_newline_markers_are_kept() {
        let p = "--- a\n+++ b\n@@ -1,2 +1,2 @@\n x\n-y\n\\ No newline at end of file\n+z\n\\ No newline at end of file\n";
        let fp = one(p);
        let r = apply_file_patch("x\ny", &fp, &ApplyOptions::default());
        assert!(r.all_applied());
        assert_eq!(r.text, "x\nz");
        let r = apply_file_patch("x\ny\n", &fp, &ApplyOptions::default());
        // the file has a newline the patch does not expect: the loose pass takes it
        assert_eq!(r.text, "x\nz");
    }

    #[test]
    fn wrong_counts_and_stripped_blanks_are_recounted() {
        let p = "--- a\n+++ b\n@@ -1,9 +1,9 @@\n x\n\n-y\n+Y\n";
        let fp = one(p);
        assert!(fp.hunks[0].recounted);
        let r = apply_file_patch("x\n\ny\nz\n", &fp, &ApplyOptions::default());
        assert!(r.all_applied(), "{r:?}");
        assert_eq!(r.text, "x\n\nY\nz\n");
    }

    #[test]
    fn two_hunks_and_creation_and_deletion() {
        let p = "diff --git a/n.txt b/n.txt\nnew file mode 100644\nindex 0000000..1111111\n--- /dev/null\n+++ b/n.txt\n@@ -0,0 +1,2 @@\n+one\n+two\ndiff --git a/o.txt b/o.txt\ndeleted file mode 100644\n--- a/o.txt\n+++ /dev/null\n@@ -1 +0,0 @@\n-gone\n";
        let ps = parse_patches(p).unwrap();
        let (n, o) = (&ps[0].files[0], &ps[0].files[1]);
        assert_eq!((n.kind(), o.kind()), (FileKind::Create, FileKind::Delete));
        assert_eq!(
            apply_file_patch("", n, &ApplyOptions::default()).text,
            "one\ntwo\n"
        );
        assert_eq!(
            apply_file_patch("gone\n", o, &ApplyOptions::default()).text,
            ""
        );
    }

    #[test]
    fn a_mailbox_has_a_header_a_message_and_several_patches() {
        let m = "From 0123456789abcdef0123456789abcdef01234567 Mon Sep 17 00:00:00 2001\nFrom: A B <a@b.c>\nDate: Tue, 1 Jan 2030 10:00:00 +0000\nSubject: [PATCH 1/2] fix the\n thing\n\nLonger text\nof the commit.\n---\n f.txt | 2 +-\n 1 file changed\n\ndiff --git a/f.txt b/f.txt\nindex 1..2 100644\n--- a/f.txt\n+++ b/f.txt\n@@ -1 +1 @@\n-a\n+b\n-- \n2.43.0\n\nFrom 1123456789abcdef0123456789abcdef01234567 Mon Sep 17 00:00:00 2001\nSubject: [PATCH 2/2] second\n\n---\ndiff --git a/g b/g\n--- a/g\n+++ b/g\n@@ -1 +1 @@\n-1\n+2\n-- \n2.43.0\n\n";
        let ps = parse_patches(m).unwrap();
        assert_eq!(ps.len(), 2);
        let h = ps[0].mail.as_ref().unwrap();
        assert_eq!(
            (h.subject.as_str(), h.from.as_str()),
            ("fix the thing", "A B <a@b.c>")
        );
        assert_eq!(h.message, "Longer text\nof the commit.");
        assert_eq!(ps[0].files[0].hunks[0].lines.len(), 2);
        assert_eq!(ps[1].files[0].path(1).as_deref(), Some("g"));
    }

    #[test]
    fn unsupported_and_empty_inputs_are_errors() {
        assert!(parse_patches("hello\nworld\n").is_err());
        assert!(
            parse_patches("*** a\n--- b\n")
                .unwrap_err()
                .message
                .contains("context")
        );
        assert!(
            parse_patches("diff --cc x\n@@@ -1 -1 +1 @@@\n")
                .unwrap_err()
                .message
                .contains("combined")
        );
    }

    #[test]
    fn encoded_header_words_round_trip() {
        for s in ["plain", "Zażółć gęślą jaźń", "日本語のタイトル", "é"] {
            assert_eq!(decode_header_words(&encode_header_words(s)), s);
        }
        assert_eq!(
            decode_header_words("=?UTF-8?q?caf=C3=A9_au_lait?= tail"),
            "café au lait tail"
        );
        assert_eq!(
            decode_header_words("=?UTF-8?B?w6k=?= =?UTF-8?B?w6k=?="),
            "éé"
        );
        assert_eq!(decode_header_words("a =? b"), "a =? b");
    }

    #[test]
    fn rename_and_quoted_names_and_strip() {
        let p = "diff --git a/old name.txt b/new name.txt\nsimilarity index 90%\nrename from old name.txt\nrename to new name.txt\n--- a/old name.txt\n+++ b/new name.txt\n@@ -1 +1 @@\n-a\n+b\n";
        let fp = one(p);
        assert_eq!(fp.kind(), FileKind::Rename);
        assert_eq!(fp.path(1).as_deref(), Some("new name.txt"));
        assert_eq!(unquote("\"a\\tb\\303\\251\""), "a\tbé");
        assert_eq!(strip_components("a/b/c.txt", 2), "c.txt");
    }
}
