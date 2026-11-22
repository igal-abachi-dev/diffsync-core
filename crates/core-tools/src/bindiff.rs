//! Byte-level compare of two files (backlog E11.5-S16): which bytes differ and where, the way
//! `fc /b`, `cmp -l` and offset patch scripts (`00001A4F:74->75`) show it, plus applying such a
//! script with a check of the old byte (decision D61).
//!
//! This is the report a person or a model reads for a binary file that a line diff calls "Binary
//! files differ". It is not a delta format: a shifted file differs in nearly every byte after the
//! shift (the delta formats of backlog E13 are for that).

use serde::Serialize;

/// One run of consecutive differing bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ByteRun {
    /// 0-based offset of the first byte.
    pub offset: u64,
    pub left: Vec<u8>,
    pub right: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ByteDiff {
    pub left_len: u64,
    pub right_len: u64,
    /// Differing bytes in the part both files have.
    pub differing_bytes: u64,
    /// The runs listed (at most the requested limit).
    pub runs: Vec<ByteRun>,
    /// More runs exist than were listed.
    pub truncated: bool,
}

impl ByteDiff {
    pub fn identical(&self) -> bool {
        self.left_len == self.right_len && self.differing_bytes == 0
    }
}

/// Compares two byte strings. `max_runs` bounds the list (the counts stay exact).
pub fn compare(left: &[u8], right: &[u8], max_runs: usize) -> ByteDiff {
    let n = left.len().min(right.len());
    let mut runs: Vec<ByteRun> = Vec::new();
    let (mut differing, mut truncated) = (0u64, false);
    let mut i = 0;
    while i < n {
        if left[i] == right[i] {
            i += 1;
            continue;
        }
        let s = i;
        while i < n && left[i] != right[i] {
            i += 1;
        }
        differing += (i - s) as u64;
        if runs.len() < max_runs {
            runs.push(ByteRun {
                offset: s as u64,
                left: left[s..i].to_vec(),
                right: right[s..i].to_vec(),
            });
        } else {
            truncated = true;
        }
    }
    ByteDiff {
        left_len: left.len() as u64,
        right_len: right.len() as u64,
        differing_bytes: differing,
        runs,
        truncated,
    }
}

/// The text forms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ByteFormat {
    /// Windows `fc /b`: `00001A4F: 74 75`, one line per differing byte.
    Fc,
    /// `cmp -l`: 1-based decimal offset and the two bytes in octal.
    CmpList,
    /// `00001A4F:74->75`, one line per byte; [`apply_script`] reads it back.
    Script,
}

/// Renders the differences. `left_name` and `right_name` appear in the `fc` header only.
pub fn render(d: &ByteDiff, f: ByteFormat, left_name: &str, right_name: &str) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    if f == ByteFormat::Fc {
        let _ = writeln!(s, "Comparing files {left_name} and {right_name}");
    }
    for r in &d.runs {
        for (k, (a, b)) in r.left.iter().zip(&r.right).enumerate() {
            let off = r.offset + k as u64;
            let _ = match f {
                ByteFormat::Fc => writeln!(s, "{off:08X}: {a:02X} {b:02X}"),
                ByteFormat::CmpList => writeln!(s, "{} {a:3o} {b:3o}", off + 1),
                ByteFormat::Script => writeln!(s, "{off:08X}:{a:02X}->{b:02X}"),
            };
        }
    }
    if d.truncated {
        let _ = writeln!(s, "... more differences not listed");
    }
    if f == ByteFormat::Fc {
        match d.left_len.cmp(&d.right_len) {
            std::cmp::Ordering::Greater => {
                let _ = writeln!(s, "FC: {left_name} longer than {right_name}");
            }
            std::cmp::Ordering::Less => {
                let _ = writeln!(s, "FC: {right_name} longer than {left_name}");
            }
            std::cmp::Ordering::Equal if d.differing_bytes == 0 => {
                s.push_str("FC: no differences encountered\n");
            }
            std::cmp::Ordering::Equal => {}
        }
    }
    s
}

/// A script line that could not be applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptError {
    /// 1-based line of the script.
    pub line: usize,
    pub message: String,
}

impl std::fmt::Display for ScriptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "script line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for ScriptError {}

fn hex_byte(s: &str) -> Option<u8> {
    (s.len() == 2)
        .then(|| u8::from_str_radix(s, 16).ok())
        .flatten()
}

/// Reads `OFFSET:OLD->NEW` and `OFFSET: OLD NEW` lines (hex; `>file` header lines, blank lines
/// and `#`/`;` comments are skipped) and applies them to `data`. Every old byte must be the byte
/// found at the offset, otherwise nothing is changed: a script made for another build of a file is
/// refused, not forced. Returns the number of bytes changed.
pub fn apply_script(data: &mut [u8], script: &str, reverse: bool) -> Result<usize, ScriptError> {
    let mut edits: Vec<(usize, u8)> = Vec::new();
    for (n, raw) in script.lines().enumerate() {
        let l = raw.trim();
        if l.is_empty()
            || l.starts_with(['>', '#', ';'])
            || l.starts_with("Comparing files")
            || l.starts_with("FC:")
        {
            continue;
        }
        let err = |m: &str| ScriptError {
            line: n + 1,
            message: m.into(),
        };
        let (off, rest) = l
            .split_once(':')
            .ok_or_else(|| err("expected OFFSET:OLD->NEW"))?;
        let off =
            usize::from_str_radix(off.trim(), 16).map_err(|_| err("the offset is not hex"))?;
        let (a, b) = if let Some((a, b)) = rest.split_once("->") {
            (a.trim(), b.trim())
        } else {
            let mut it = rest.split_whitespace();
            (it.next().unwrap_or(""), it.next().unwrap_or(""))
        };
        let (Some(old), Some(new)) = (hex_byte(a), hex_byte(b)) else {
            return Err(err("expected two hex bytes"));
        };
        let (old, new) = if reverse { (new, old) } else { (old, new) };
        match data.get(off) {
            None => return Err(err("the offset is beyond the end of the file")),
            Some(&have) if have != old => {
                return Err(err(&format!(
                    "the byte at that offset is {have:02X}, the script expects {old:02X}"
                )));
            }
            Some(_) => edits.push((off, new)),
        }
    }
    for (o, v) in &edits {
        data[*o] = *v;
    }
    Ok(edits.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_counts_and_the_three_text_forms() {
        let a = [1u8, 2, 3, 4, 5, 6, 0x74];
        let b = [1u8, 9, 9, 4, 5, 6, 0x75, 7];
        let d = compare(&a, &b, 100);
        assert_eq!(
            (d.differing_bytes, d.runs.len(), d.identical()),
            (3, 2, false)
        );
        assert_eq!(d.runs[0].offset, 1);
        assert_eq!(d.runs[0].left, [2, 3]);
        let fc = render(&d, ByteFormat::Fc, "a.bin", "b.bin");
        assert_eq!(
            fc,
            "Comparing files a.bin and b.bin\n00000001: 02 09\n00000002: 03 09\n00000006: 74 75\nFC: b.bin longer than a.bin\n"
        );
        assert_eq!(
            render(&d, ByteFormat::CmpList, "", ""),
            "2   2  11\n3   3  11\n7 164 165\n"
        );
        assert_eq!(
            render(&d, ByteFormat::Script, "", ""),
            "00000001:02->09\n00000002:03->09\n00000006:74->75\n"
        );
        assert!(compare(&a, &a, 1).identical());
        assert!(
            render(&compare(&a, &a, 1), ByteFormat::Fc, "x", "y")
                .ends_with("FC: no differences encountered\n")
        );
        let t = compare(&a, &b, 1);
        assert!(t.truncated && t.differing_bytes == 3);
    }

    #[test]
    fn a_script_applies_only_where_the_old_byte_is_there() {
        let mut data = vec![0u8; 0x20];
        data[0x1A] = 0x74;
        let n = apply_script(&mut data, ">app.exe\n0000001A:74->75\n", false).unwrap();
        assert_eq!((n, data[0x1A]), (1, 0x75));
        // the same script again: the old byte is gone, nothing changes
        let e = apply_script(&mut data, "0000001A:74->75\n00000001:00->01\n", false).unwrap_err();
        assert!(e.message.contains("75"), "{e}");
        assert_eq!(data[1], 0);
        // fc-style lines and undo
        apply_script(&mut data, "0000001A: 74 75\n", true).unwrap();
        assert_eq!(data[0x1A], 0x74);
        assert!(apply_script(&mut data, "00000100:00->01\n", false).is_err());
        assert!(apply_script(&mut data, "nonsense\n", false).is_err());
    }
}
