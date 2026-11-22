//! Row output, golden checks and the side-by-side review page (plan.md §11.4, backlog E4-S9.1).

use std::path::Path;

use core_base::{Error, Result};
use core_diff::session::CharRange;
use core_diff::{DiffRow, DocSpec, RowKind, Status, TextCompare, TextOptions};
use core_text::TextDocument;

pub fn fetch_rows(tc: &TextCompare) -> Vec<DiffRow> {
    let n = tc.row_count() as usize;
    let mut out = vec![
        DiffRow {
            left: 0,
            right: 0,
            kind: RowKind::Same
        };
        n
    ];
    tc.rows(0, &mut out);
    out
}

pub fn row_text(r: &DiffRow) -> String {
    let side = |v: Option<u32>| v.map_or_else(|| "-".to_string(), |n| n.to_string());
    format!(
        "{} {} {}",
        side(r.left_opt()),
        side(r.right_opt()),
        r.kind.name()
    )
}

pub fn row_json(r: &DiffRow) -> String {
    let side = |v: Option<u32>| v.map_or_else(|| "null".to_string(), |n| n.to_string());
    format!(
        "[{},{},\"{}\"]",
        side(r.left_opt()),
        side(r.right_opt()),
        r.kind.name()
    )
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

const STYLE: &str = "body{font:13px monospace}table{border-collapse:collapse;width:100%}\
td{vertical-align:top;white-space:pre-wrap;padding:0 4px;border-bottom:1px solid #eee;width:50%}\
.n{color:#999;width:3em;text-align:right}.Different{background:#fdd}.LeftOnly,.RightOnly{background:#fed}\
.Unimportant,.UnimportantLeftOnly,.UnimportantRightOnly{background:#eef}mark{background:#f66}mark.u{background:#cce}";

fn mark(line: &str, ranges: &[CharRange]) -> String {
    let units: Vec<u16> = line.encode_utf16().collect();
    let (mut out, mut at) = (String::new(), 0usize);
    for c in ranges {
        let (a, b) = (c.start as usize, (c.start + c.len) as usize);
        out += &html_escape(&String::from_utf16_lossy(&units[at..a]));
        let class = if c.important { "" } else { " class=u" };
        out += &format!(
            "<mark{class}>{}</mark>",
            html_escape(&String::from_utf16_lossy(&units[a..b]))
        );
        at = b;
    }
    out + &html_escape(&String::from_utf16_lossy(&units[at..]))
}

/// A side-by-side page of the rows with intraline highlights.
pub fn render_html(left: &Path, right: &Path, opts: TextOptions) -> Result<String> {
    let tc = TextCompare::start(
        DocSpec::Path(left.into()),
        DocSpec::Path(right.into()),
        opts,
        None,
    )?;
    tc.wait();
    if let Status::Failed { error } = tc.status() {
        return Err(Error::internal(error));
    }
    let (l, r) = (TextDocument::open(left)?, TextDocument::open(right)?);
    let mut out = format!(
        "<!doctype html><meta charset=utf-8><title>diffsync review</title><style>{STYLE}</style><table>"
    );
    for (i, row) in fetch_rows(&tc).iter().enumerate() {
        let intra = tc.intraline(i as u32);
        let cell = |text: Option<&str>, ranges: Option<&[CharRange]>| match text {
            Some(t) => ranges.map_or_else(|| html_escape(t), |rg| mark(t, rg)),
            None => String::new(),
        };
        let num = |v: Option<u32>| v.map_or(String::new(), |n| (n + 1).to_string());
        out += &format!(
            "<tr class={}><td class=n>{}</td><td>{}</td><td class=n>{}</td><td>{}</td></tr>\n",
            row.kind.name(),
            num(row.left_opt()),
            cell(
                row.left_opt().map(|n| l.line(n as usize)),
                intra.as_ref().map(|d| d.left.as_slice())
            ),
            num(row.right_opt()),
            cell(
                row.right_opt().map(|n| r.line(n as usize)),
                intra.as_ref().map(|d| d.right.as_slice())
            ),
        );
    }
    Ok(out + "</table>")
}

fn case_dir(case: &str) -> std::path::PathBuf {
    let p = Path::new(case);
    if p.is_dir() {
        p.to_path_buf()
    } else {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/text")
            .join(case)
    }
}

pub fn options_in(dir: &Path) -> Result<TextOptions> {
    match std::fs::read_to_string(dir.join("options.json")) {
        Ok(json) => TextOptions::from_json(&json),
        Err(_) => Ok(TextOptions::default()),
    }
}

/// `dsdev render <case> --html`: writes `review.html` next to the case files.
pub fn cmd_render(case: &str) -> Result<()> {
    let dir = case_dir(case);
    let page = render_html(&dir.join("left"), &dir.join("right"), options_in(&dir)?)?;
    let out = dir.join("review.html");
    std::fs::write(&out, page).map_err(|e| Error::from_io(&e, &out))?;
    println!("{}", out.display());
    Ok(())
}

/// `dsdev golden <case|all>`: every fixture's rows through the session against `expected.rows`.
pub fn cmd_golden(case: &str) -> Result<()> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/text");
    let (mut failed, mut total) = (0, 0);
    for entry in std::fs::read_dir(&root).map_err(|e| Error::from_io(&e, &root))? {
        let dir = entry.map_err(|e| Error::from_io(&e, &root))?.path();
        let name = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if !dir.is_dir() || (case != "all" && case != name) {
            continue;
        }
        total += 1;
        let read = |f: &str| std::fs::read(dir.join(f)).unwrap_or_default();
        let tc = TextCompare::start(
            DocSpec::Bytes(read("left")),
            DocSpec::Bytes(read("right")),
            options_in(&dir)?,
            None,
        )?;
        tc.wait();
        let got: Vec<String> = fetch_rows(&tc).iter().map(row_text).collect();
        let want: Vec<String> = String::from_utf8_lossy(&read("expected.rows"))
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(ToString::to_string)
            .collect();
        if got == want {
            println!("ok    {name}");
        } else {
            failed += 1;
            println!(
                "FAIL  {name}: {} rows expected, {} got",
                want.len(),
                got.len()
            );
        }
    }
    if total == 0 {
        return Err(Error::invalid_argument(format!("no golden case `{case}`")));
    }
    if failed > 0 {
        return Err(Error::internal(format!(
            "{failed} of {total} golden cases failed"
        )));
    }
    Ok(())
}
