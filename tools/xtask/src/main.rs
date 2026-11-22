//! `cargo xtask`: repository automation (plan.md §2).
//!
//! * `check-deps`: the workspace dependency direction (base → text/vfs → diff/glob →
//!   folder → sync → ffi) is enforced from `cargo metadata`.
//! * `check-release-deps`: `imara-diff` must not be in the release tree of `core-ffi`.
//! * `bindings`, `glob-diff`, `ledger`, `bench`: generated bindings, the Glob library diff, the
//!   decision ledger audit and the performance budgets.

mod bench;
mod bindings;
mod glob_diff;
mod ledger;

use std::collections::{BTreeMap, BTreeSet};
use std::process::{Command as Proc, ExitCode};

use clap::{Parser, Subcommand};
use serde_json::Value;

#[derive(Parser)]
#[command(name = "xtask", about = "diffsync repository automation")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Fail when a crate depends on a crate it must not (plan.md §1).
    CheckDeps,
    /// Fail when `imara-diff` is in the release dependency tree of `core-ffi`.
    CheckReleaseDeps,
    /// Regenerate include/diffsync.h and bindings/csharp (epic E9-S7).
    Bindings {
        /// Fail instead of writing when the generated files are stale.
        #[arg(long)]
        check: bool,
    },
    /// core-glob versus the C# Glob library (epic E6-S5).
    GlobDiff,
    /// Every B/D row of docs/decisions.md must be pinned by an existing test (epic E11-S4).
    Ledger {
        /// Also run the whole workspace test suite.
        #[arg(long)]
        run: bool,
    },
    /// Run every performance bench against the plan §12.2 budgets (epic E11-S2).
    Bench {
        /// Write the figures to this file (a baseline for `--compare`).
        #[arg(long)]
        save: Option<std::path::PathBuf>,
        /// Fail when a figure is more than 10% worse than in this baseline file.
        #[arg(long)]
        compare: Option<std::path::PathBuf>,
    },
}

const ALL_CORE: &[&str] = &[
    "core-base",
    "core-text",
    "core-diff",
    "core-vfs",
    "core-glob",
    "core-folder",
    "core-sync",
    "core-tools",
];

/// The allowed internal dependencies of every workspace crate. Every other internal edge is
/// a layering violation (`core-diff` in particular must never reach the file system).
fn allowed() -> BTreeMap<&'static str, BTreeSet<&'static str>> {
    let mut ffi: Vec<&str> = ALL_CORE.to_vec();
    ffi.retain(|c| *c != "core-ffi");
    let table: Vec<(&str, Vec<&str>)> = vec![
        ("core-base", vec![]),
        ("core-text", vec!["core-base"]),
        ("core-vfs", vec!["core-base"]),
        ("core-diff", vec!["core-base", "core-text"]),
        ("core-glob", vec!["core-base", "core-vfs"]),
        (
            "core-folder",
            vec![
                "core-base",
                "core-text",
                "core-vfs",
                "core-glob",
                "core-diff",
            ],
        ),
        (
            "core-sync",
            vec!["core-base", "core-vfs", "core-glob", "core-folder"],
        ),
        (
            "core-tools",
            vec![
                "core-base",
                "core-text",
                "core-vfs",
                "core-glob",
                "core-diff",
                "core-folder",
            ],
        ),
        ("core-ffi", ffi),
        ("dsdev", ALL_CORE.to_vec()),
        ("xtask", vec!["core-base", "core-vfs", "core-glob"]),
        (
            "readme-tests",
            vec![
                "core-base",
                "core-vfs",
                "core-diff",
                "core-folder",
                "core-sync",
            ],
        ),
    ];
    table
        .into_iter()
        .map(|(k, v)| (k, v.into_iter().collect()))
        .collect()
}

pub(crate) fn cargo_metadata() -> Result<Value, String> {
    let out = Proc::new("cargo")
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .output()
        .map_err(|e| format!("cannot run cargo metadata: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).into_owned());
    }
    serde_json::from_slice(&out.stdout).map_err(|e| format!("bad cargo metadata: {e}"))
}

fn check_deps() -> Result<(), String> {
    let meta = cargo_metadata()?;
    let allowed = allowed();
    let packages = meta["packages"]
        .as_array()
        .ok_or("no packages in metadata")?;
    let internal: BTreeSet<&str> = packages.iter().filter_map(|p| p["name"].as_str()).collect();
    let mut errors = Vec::new();
    let mut seen = BTreeSet::new();
    for p in packages {
        let name = p["name"].as_str().unwrap_or_default();
        seen.insert(name.to_string());
        let Some(ok) = allowed.get(name) else {
            errors.push(format!(
                "{name}: not in the xtask dependency table; add it deliberately"
            ));
            continue;
        };
        for d in p["dependencies"].as_array().into_iter().flatten() {
            let dep = d["name"].as_str().unwrap_or_default();
            if internal.contains(dep) && !ok.contains(dep) {
                let kind = d["kind"].as_str().unwrap_or("normal");
                errors.push(format!(
                    "{name} -> {dep} ({kind}) is not allowed by the layering"
                ));
            }
        }
    }
    for k in allowed.keys() {
        if !seen.contains(*k) {
            errors.push(format!(
                "{k}: listed in the table but missing from the workspace"
            ));
        }
    }
    if errors.is_empty() {
        println!("check-deps: ok ({} crates)", seen.len());
        Ok(())
    } else {
        Err(errors.join("\n"))
    }
}

fn check_release_deps() -> Result<(), String> {
    let out = Proc::new("cargo")
        .args([
            "tree",
            "-p",
            "core-ffi",
            "-e",
            "normal,build",
            "--prefix",
            "none",
        ])
        .output()
        .map_err(|e| format!("cannot run cargo tree: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).into_owned());
    }
    let tree = String::from_utf8_lossy(&out.stdout);
    if tree.lines().any(|l| l.starts_with("imara-diff ")) {
        return Err(
            "imara-diff is in the release dependency tree of core-ffi (plan.md §5.3)".into(),
        );
    }
    println!("check-release-deps: ok (imara-diff is dev-only)");
    Ok(())
}

fn main() -> ExitCode {
    let result = match Cli::parse().command {
        Cmd::CheckDeps => check_deps(),
        Cmd::CheckReleaseDeps => check_release_deps(),
        Cmd::Bindings { check } => bindings::run(check),
        Cmd::GlobDiff => glob_diff::run(),
        Cmd::Ledger { run } => ledger::run(run),
        Cmd::Bench { save, compare } => bench::run(save.as_deref(), compare.as_deref()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("{msg}");
            ExitCode::FAILURE
        }
    }
}
