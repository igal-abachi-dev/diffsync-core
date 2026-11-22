//! `dsdev`: developer CLI for debugging, golden values and review pages (plan.md §12.3).
//! It is a thin layer over the Rust API and is **not** the product CLI.

mod review;

use std::path::Path;
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use core_base::{CancelToken, Result};
use core_diff::options::AlignmentName;
use core_diff::{AlignOp, AlignRange, DocSource, TextOptions, align};
use core_text::TextDocument;

#[derive(Parser)]
#[command(
    name = "dsdev",
    version,
    about = "diffsync developer CLI (not the product CLI)"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, ValueEnum)]
enum Kind {
    Standard,
    Patience,
    Myers,
    Histogram,
    None,
}

impl From<Kind> for AlignmentName {
    fn from(k: Kind) -> Self {
        match k {
            Kind::Standard => AlignmentName::Standard,
            Kind::Patience => AlignmentName::Patience,
            Kind::Myers => AlignmentName::Myers,
            Kind::Histogram => AlignmentName::Histogram,
            Kind::None => AlignmentName::None,
        }
    }
}

#[derive(Subcommand)]
enum Command {
    /// Align two text files and print the ops or the rows.
    Align {
        left: String,
        right: String,
        /// Options JSON file (the keys of plan.md §5.4; only differences from the defaults).
        #[arg(long)]
        options: Option<String>,
        /// Overrides the alignment kind of the options.
        #[arg(long, value_enum)]
        kind: Option<Kind>,
        /// Overrides the skew tolerance of the options.
        #[arg(long)]
        skew: Option<u32>,
        /// Print the op stream (default).
        #[arg(long)]
        ops: bool,
        /// Print the display rows.
        #[arg(long)]
        rows: bool,
        #[arg(long)]
        json: bool,
    },
    /// Dump per-line hash, weight and significance of a file.
    Hash { file: String },
    /// Compare two folders (epic E7).
    Folders {
        a: String,
        b: String,
        #[arg(long)]
        content: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Plan (and with --run, execute) a sync (epic E8).
    Sync {
        a: String,
        b: String,
        preset: String,
        #[arg(long)]
        run: bool,
    },
    /// Check a golden case, or `all`, against the plan.md §11.3 values.
    Golden { case: String },
    /// Write the side-by-side review page of a case (plan.md §11.4).
    Render {
        case: String,
        #[arg(long)]
        html: bool,
    },
}

fn not_yet(what: &str, epic: &str) -> ExitCode {
    eprintln!("dsdev {what}: not implemented yet (backlog {epic})");
    ExitCode::from(2)
}

fn options_from(path: Option<&str>) -> Result<TextOptions> {
    match path {
        None => Ok(TextOptions::default()),
        Some(p) => {
            let json = std::fs::read_to_string(p)
                .map_err(|e| core_base::Error::from_io(&e, Path::new(p)))?;
            TextOptions::from_json(&json)
        }
    }
}

fn op_json(op: &AlignOp) -> String {
    let side = |v: Option<u32>| v.map_or_else(|| "null".to_string(), |n| n.to_string());
    format!(
        "[{},{},{}]",
        side(op.src_opt()),
        side(op.tgt_opt()),
        op.amount
    )
}

#[allow(clippy::too_many_arguments)]
fn cmd_align(
    left: &str,
    right: &str,
    options: Option<&str>,
    kind: Option<Kind>,
    skew: Option<u32>,
    rows: bool,
    json: bool,
) -> Result<()> {
    let mut opts = options_from(options)?;
    if let Some(k) = kind {
        opts.alignment = k.into();
    }
    if let Some(s) = skew {
        opts.skew_tolerance = s;
    }
    let rules = opts.line_rules()?;
    let l = TextDocument::open(Path::new(left))?;
    let r = TextDocument::open(Path::new(right))?;
    let source = DocSource::new(&l, &r, &rules, None);
    let mut ops = Vec::new();
    align(
        &source,
        &opts.align_config(),
        AlignRange::full(),
        &CancelToken::new(),
        &mut |op| ops.push(op),
    )?;
    if rows {
        let tc = core_diff::TextCompare::start(
            core_diff::DocSpec::Path(left.into()),
            core_diff::DocSpec::Path(right.into()),
            opts,
            None,
        )?;
        tc.wait();
        if let core_diff::Status::Failed { error } = tc.status() {
            return Err(core_base::Error::internal(error));
        }
        let all = review::fetch_rows(&tc);
        if json {
            let parts: Vec<String> = all.iter().map(review::row_json).collect();
            println!("{{\"rows\":[{}]}}", parts.join(","));
        } else {
            println!("# expected.rows");
            for r in &all {
                println!("{}", review::row_text(r));
            }
        }
        return Ok(());
    }
    if json {
        let parts: Vec<String> = ops.iter().map(op_json).collect();
        println!("{{\"ops\":[{}]}}", parts.join(","));
    } else {
        println!("# expected.ops");
        for op in &ops {
            println!("{op}");
        }
    }
    Ok(())
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Align {
            left,
            right,
            options,
            kind,
            skew,
            ops: _,
            rows,
            json,
        } => cmd_align(&left, &right, options.as_deref(), kind, skew, rows, json),
        Command::Hash { .. } => return not_yet("hash", "E1"),
        Command::Folders { .. } => return not_yet("folders", "E7"),
        Command::Sync { .. } => return not_yet("sync", "E8"),
        Command::Golden { case } => review::cmd_golden(&case),
        Command::Render { case, html } => {
            if html {
                review::cmd_render(&case)
            } else {
                Err(core_base::Error::invalid_argument(
                    "only --html is supported",
                ))
            }
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("dsdev: {e}");
            ExitCode::FAILURE
        }
    }
}
