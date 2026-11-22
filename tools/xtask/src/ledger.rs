//! `cargo xtask ledger [--run]` (backlog E11-S4): every B and D row of `docs/decisions.md` must
//! name at least one test (or fixture, script or bench) that exists. A reference is a backtick
//! token: a file path (optionally `path::item`), or a bare item name that must be a function,
//! script function or fixture somewhere in the repository. With `--run` the whole workspace
//! test suite runs afterwards, so "pinned" also means "passing".

use std::collections::HashSet;
use std::path::{Path, PathBuf};

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        let name = e.file_name().to_string_lossy().into_owned();
        if p.is_dir() {
            if matches!(
                name.as_str(),
                "target" | ".git" | "bin" | "obj" | "node_modules" | "reference_source"
            ) {
                continue;
            }
            walk(&p, out);
        } else {
            out.push(p);
        }
    }
}

/// Backtick-quoted tokens of a cell.
fn tokens(cell: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = cell;
    while let Some(a) = rest.find('`') {
        let after = &rest[a + 1..];
        let Some(b) = after.find('`') else { break };
        out.push(after[..b].to_string());
        rest = &after[b + 1..];
    }
    out
}

/// Splits a table row into cells (the escaped `\|` stays inside a cell).
fn cells(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = line.trim().trim_start_matches('|').chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' && chars.peek() == Some(&'|') {
            cur.push('|');
            chars.next();
        } else if c == '|' {
            out.push(cur.trim().to_string());
            cur.clear();
        } else {
            cur.push(c);
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

struct Index {
    files: Vec<PathBuf>,
    /// `fn name` / `def name` / fixture names / test names in any source file.
    names: HashSet<String>,
}

impl Index {
    fn build(root: &Path) -> Index {
        let mut files = Vec::new();
        walk(root, &mut files);
        let mut names = HashSet::new();
        for f in &files {
            if let Some(stem) = f.file_stem().and_then(|s| s.to_str()) {
                names.insert(stem.to_string());
            }
            if let Some(d) = f
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|s| s.to_str())
            {
                names.insert(d.to_string()); // fixture folders
            }
            let ext = f.extension().and_then(|e| e.to_str()).unwrap_or("");
            if matches!(ext, "rs" | "py" | "cs" | "c" | "json" | "md" | "txt") {
                if let Ok(text) = std::fs::read_to_string(f) {
                    for line in text.lines() {
                        let t = line.trim_start();
                        for kw in ["fn ", "def "] {
                            if let Some(i) = t.find(kw) {
                                let name: String = t[i + kw.len()..]
                                    .chars()
                                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                                    .collect();
                                if !name.is_empty() {
                                    names.insert(name);
                                }
                            }
                        }
                        // golden fixture ids and test case names appear quoted in fixtures
                        if ext == "json" {
                            for tok in line.split('"') {
                                if tok.len() > 3
                                    && tok.chars().all(|c| c.is_alphanumeric() || c == '_')
                                {
                                    names.insert(tok.to_string());
                                }
                            }
                        }
                    }
                }
            }
        }
        Index { files, names }
    }

    fn file(&self, path: &str) -> Option<&PathBuf> {
        let p = path.replace('\\', "/");
        self.files.iter().find(|f| {
            let s = f.to_string_lossy().replace('\\', "/");
            s.ends_with(&format!("/{p}")) || s.ends_with(&format!("/crates/{p}"))
        })
    }

    /// Does a backtick token name something that exists?
    fn resolves(&self, tok: &str) -> bool {
        let tok = tok.trim();
        if tok.is_empty() || tok.contains(' ') && !tok.contains("::") && !tok.contains('/') {
            return false;
        }
        let mut parts = tok.split("::");
        let first = parts.next().unwrap_or("");
        let rest: Vec<&str> = parts.collect();
        let looks_like_path = first.contains('/')
            || first.ends_with(".rs")
            || first.ends_with(".py")
            || first.ends_with(".c")
            || first.ends_with(".cs");
        if looks_like_path {
            let Some(file) = self.file(first) else {
                return false;
            };
            return match rest.last() {
                None => true,
                Some(item) => std::fs::read_to_string(file).is_ok_and(|t| t.contains(item)),
            };
        }
        // `krate path::to::test_name`, `module::test_name` or a bare name
        let last = rest.last().copied().unwrap_or(first);
        self.names.contains(last)
    }
}

pub fn run(run_tests: bool) -> Result<(), String> {
    let root = repo();
    let text = std::fs::read_to_string(root.join("docs").join("decisions.md"))
        .map_err(|e| e.to_string())?;
    let index = Index::build(&root);
    let mut checked = 0;
    let mut bad: Vec<String> = Vec::new();
    for line in text.lines() {
        let c = cells(line);
        let Some(id) = c.first() else { continue };
        let is_row = (id.starts_with('B') || id.starts_with('D'))
            && id[1..].chars().all(|ch| ch.is_ascii_digit())
            && id.len() > 1;
        if !is_row {
            continue;
        }
        checked += 1;
        let pinned = c.last().map_or("", String::as_str);
        let toks = tokens(pinned);
        if toks.is_empty() {
            bad.push(format!("{id}: no `Pinned by` reference"));
        } else if !toks.iter().any(|t| index.resolves(t)) {
            bad.push(format!("{id}: none of {toks:?} exists in the repository"));
        }
    }
    println!("ledger: {checked} decision rows checked");
    if !bad.is_empty() {
        return Err(format!(
            "ledger: {} rows are not pinned:\n  {}",
            bad.len(),
            bad.join("\n  ")
        ));
    }
    if run_tests {
        println!("ledger: running the workspace tests (a pinned row must also pass)");
        let st = std::process::Command::new(env!("CARGO"))
            .args(["test", "--workspace"])
            .current_dir(&root)
            .status()
            .map_err(|e| e.to_string())?;
        if !st.success() {
            return Err("ledger: the workspace tests failed".into());
        }
    }
    println!("ledger: every row is pinned");
    Ok(())
}
