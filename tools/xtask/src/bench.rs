//! `cargo xtask bench` (backlog E11-S2, plan.md §12.2): runs every performance bench, checks the
//! budgets, and optionally compares with a saved baseline.
//!
//! Each bench prints `BUDGET <name> <value> <limit> PASS|FAIL` lines and exits non-zero when a
//! budget fails; this runner collects them, prints a table and fails when any is `FAIL`.
//! Criterion benches also leave `target/criterion/**/new/estimates.json`; their medians are the
//! other half of the regression data.
//!
//! * `--save FILE` writes the current figures (criterion medians in ns, budget values) as JSON.
//! * `--compare FILE` fails when a figure is more than 10% worse than in FILE (a missing FILE is a
//!   note, not an error: the first run on a fresh runner has nothing to compare with).
//!
//! The baseline is only meaningful on the machine that wrote it, so CI keeps one per runner
//! (see the `bench` job in `.github/workflows/ci.yml`).

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use serde_json::{Value, json};

/// A regression above this fraction fails the run (plan.md §12.2).
const TOLERANCE: f64 = 0.10;

/// Budget figures where a bigger number is better (everything else: smaller is better).
const HIGHER_IS_BETTER: &[&str] = &[
    "line_index_gbs",
    "scan_entries_per_s",
    "content_binary",
    "content_crc32",
];

/// Budgets that stay absolute-only: the figure depends on an external process (robocopy) and
/// moved by 20% between two identical runs, so a 10% relative gate would only produce noise.
const NOT_COMPARED: &[&str] = &["copy_1gib_vs_robocopy"];

/// Every budget the suite must print. A step that exits 0 without printing one (a renamed bench,
/// a test filter that matches nothing) would otherwise drop the budget silently.
const REQUIRED: &[&str] = &[
    "line_index_gbs",
    "first200_ms",
    "full_alignment_s",
    "viewport_pull_ms",
    "worst_case_s",
    "worst_case_max_read_ms",
    "content_binary",
    "content_crc32",
    "scan_entries_per_s",
    "real_disk_scan_entries_per_s",
    "get_rows_ms",
    "tree_mb_1m_nodes",
];

/// Required only where the step runs.
const REQUIRED_WINDOWS: &[&str] = &["copy_1gib_vs_robocopy"];

/// Needs a platform that can report the process's peak memory (Windows and Linux).
const REQUIRED_PEAK: &str = "worst_case_peak_mb";

struct Step {
    name: &'static str,
    args: Vec<&'static str>,
    env: Vec<(&'static str, &'static str)>,
    windows_only: bool,
}

fn steps() -> Vec<Step> {
    let bench = |name, pkg, b, env| Step {
        name,
        args: vec!["bench", "-p", pkg, "--bench", b],
        env,
        windows_only: false,
    };
    vec![
        bench("line indexing", "core-text", "line_index", vec![]),
        bench(
            "align (logs, all-different)",
            "core-diff",
            "align",
            vec![("ALIGN_BENCH_MB", "100")],
        ),
        bench(
            "text session",
            "core-diff",
            "session",
            vec![("SESSION_BENCH_MB", "100")],
        ),
        bench(
            "worst case, 50k x 50k, peak memory",
            "core-diff",
            "worst_case",
            vec![],
        ),
        bench("content compare", "core-folder", "content", vec![]),
        Step {
            name: "folder scan, 1M nodes",
            args: vec![
                "test",
                "-p",
                "core-folder",
                "--release",
                "--test",
                "scale",
                "--",
                "--ignored",
                "--nocapture",
                "scan_one_million_nodes",
            ],
            env: vec![],
            windows_only: false,
        },
        Step {
            name: "folder scan, real disk (100k files)",
            args: vec![
                "test",
                "-p",
                "core-folder",
                "--release",
                "--test",
                "scale",
                "--",
                "--ignored",
                "--nocapture",
                "scan_real_disk",
            ],
            env: vec![],
            windows_only: false,
        },
        Step {
            name: "copy vs robocopy",
            args: vec![
                "test",
                "-p",
                "core-vfs",
                "--release",
                "--test",
                "win32",
                "--",
                "--ignored",
                "--nocapture",
                "copy_vs_robocopy",
            ],
            env: vec![],
            windows_only: true,
        },
    ]
}

struct Budget {
    value: f64,
    limit: f64,
    pass: bool,
}

fn parse_budgets(out: &str, into: &mut BTreeMap<String, Budget>) {
    for line in out.lines() {
        let mut t = line.split_whitespace();
        if t.next() != Some("BUDGET") {
            continue;
        }
        let (Some(name), Some(v), Some(l), Some(verdict)) =
            (t.next(), t.next(), t.next(), t.next())
        else {
            continue;
        };
        if let (Ok(value), Ok(limit)) = (v.parse(), l.parse()) {
            into.insert(
                name.to_string(),
                Budget {
                    value,
                    limit,
                    pass: verdict == "PASS",
                },
            );
        }
    }
}

/// `target/criterion/<group>/<id>/new/estimates.json` -> median in ns, keyed `<group>/<id>`.
fn criterion_medians(root: &Path) -> BTreeMap<String, f64> {
    fn walk(dir: &Path, root: &Path, out: &mut BTreeMap<String, f64>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                if p.file_name().is_some_and(|n| n == "report") {
                    continue;
                }
                walk(&p, root, out);
            } else if p.file_name().is_some_and(|n| n == "estimates.json")
                && p.parent()
                    .and_then(|d| d.file_name())
                    .is_some_and(|n| n == "new")
            {
                let Ok(text) = std::fs::read_to_string(&p) else {
                    continue;
                };
                let Ok(v) = serde_json::from_str::<Value>(&text) else {
                    continue;
                };
                let Some(ns) = v["median"]["point_estimate"].as_f64() else {
                    continue;
                };
                let id = p
                    .parent()
                    .and_then(Path::parent)
                    .and_then(|d| d.strip_prefix(root).ok())
                    .map(|d| d.to_string_lossy().replace('\\', "/"))
                    .unwrap_or_default();
                out.insert(id, ns);
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

pub fn run(save: Option<&Path>, compare: Option<&Path>) -> Result<(), String> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    // stale estimates of benches that no longer exist must not be compared
    // the target folder honours CARGO_TARGET_DIR and does not depend on the current directory
    let target_dir = crate::cargo_metadata()?["target_directory"]
        .as_str()
        .map(std::path::PathBuf::from)
        .ok_or("cargo metadata has no target_directory")?;
    let crit_root = target_dir.join("criterion");
    let _ = std::fs::remove_dir_all(&crit_root);

    let mut budgets: BTreeMap<String, Budget> = BTreeMap::new();
    let mut failed_steps = Vec::new();
    for s in steps() {
        if s.windows_only && !cfg!(windows) {
            println!("== {}: skipped (Windows only)", s.name);
            continue;
        }
        println!("== {}", s.name);
        let mut cmd = Command::new(&cargo);
        cmd.args(&s.args);
        for (k, v) in &s.env {
            cmd.env(k, v);
        }
        let out = cmd.output().map_err(|e| format!("cannot run cargo: {e}"))?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        parse_budgets(&stdout, &mut budgets);
        if !out.status.success() {
            // a failed budget exits non-zero by design; anything else is a real error: show it
            let has_fail = stdout
                .lines()
                .any(|l| l.starts_with("BUDGET") && l.ends_with("FAIL"));
            if !has_fail {
                eprintln!("{}", String::from_utf8_lossy(&out.stderr));
                eprintln!("{stdout}");
            }
            failed_steps.push(s.name);
        }
    }

    println!(
        "\n{:<28} {:>12} {:>12}  verdict",
        "budget", "value", "limit"
    );
    let mut problems = Vec::new();
    for (name, b) in &budgets {
        println!(
            "{name:<28} {:>12.4} {:>12.4}  {}",
            b.value,
            b.limit,
            if b.pass { "PASS" } else { "FAIL" }
        );
        if !b.pass {
            problems.push(format!("budget {name}: {} against {}", b.value, b.limit));
        }
    }
    let peak = (cfg!(windows) || cfg!(target_os = "linux")).then_some(REQUIRED_PEAK);
    for name in REQUIRED.iter().copied().chain(peak).chain(
        if cfg!(windows) { REQUIRED_WINDOWS } else { &[] }
            .iter()
            .copied(),
    ) {
        if !budgets.contains_key(name) {
            problems.push(format!(
                "budget {name} was not reported (a bench or filter changed?)"
            ));
        }
    }
    for s in &failed_steps {
        problems.push(format!("step `{s}` failed"));
    }

    let medians = criterion_medians(&crit_root);
    let current = json!({
        "note": "written by `cargo xtask bench --save`; valid for the machine that wrote it",
        "criterion_ns": medians,
        "budgets": budgets.iter().map(|(k, b)| (k.clone(), b.value)).collect::<BTreeMap<_, _>>(),
    });

    if let Some(file) = compare {
        match std::fs::read_to_string(file) {
            Err(_) => println!(
                "\nno baseline at {}: nothing to compare with (run with --save)",
                file.display()
            ),
            Ok(text) => {
                let base: Value =
                    serde_json::from_str(&text).map_err(|e| format!("{}: {e}", file.display()))?;
                println!(
                    "\nregression check against {} (tolerance {:.0}%)",
                    file.display(),
                    TOLERANCE * 100.0
                );
                for (id, ns) in &medians {
                    if let Some(old) = base["criterion_ns"][id].as_f64() {
                        let worse = ns / old - 1.0;
                        println!("  {id:<44} {worse:+.1}%");
                        if worse > TOLERANCE {
                            problems.push(format!(
                                "{id}: {:.1}% slower than the baseline",
                                worse * 100.0
                            ));
                        }
                    }
                }
                for (name, b) in &budgets {
                    if NOT_COMPARED.contains(&name.as_str()) {
                        continue;
                    }
                    // a baseline of 0 (a figure below the print precision) cannot regress
                    if let Some(old) = base["budgets"][name]
                        .as_f64()
                        .filter(|o| *o > 0.0 && b.value > 0.0)
                    {
                        let worse = if HIGHER_IS_BETTER.contains(&name.as_str()) {
                            old / b.value - 1.0
                        } else {
                            b.value / old - 1.0
                        };
                        println!("  {name:<44} {worse:+.1}%");
                        if worse > TOLERANCE {
                            problems.push(format!(
                                "{name}: {:.1}% worse than the baseline",
                                worse * 100.0
                            ));
                        }
                    }
                }
            }
        }
    }
    if let Some(file) = save {
        let text = serde_json::to_string_pretty(&current).map_err(|e| e.to_string())?;
        std::fs::write(file, text + "\n").map_err(|e| format!("{}: {e}", file.display()))?;
        println!("\nsaved {}", file.display());
    }
    if problems.is_empty() {
        println!("\nbench: all budgets met");
        Ok(())
    } else {
        Err(format!("bench failed:\n  {}", problems.join("\n  ")))
    }
}
