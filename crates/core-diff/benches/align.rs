//! Aligner benchmarks (backlog E2-S10.1, plan.md §12.2).
//!
//! Budgets (reference machine, 8-core NVMe Windows 11):
//! * two 100 MB near-identical logs: first 200 ops <= 100 ms after open, full <= 1.5 s;
//! * two 50k-line files with nothing in common: <= 3 s and <= 300 MB.
//!
//! `ALIGN_BENCH_MB` sets the size of the log pair (default 16; use 100 for the budget figure).
//! The first-200-ops latency is printed once to stderr before the criterion groups run.

use std::hint::black_box;
use std::time::Instant;

use core_base::CancelToken;
use core_diff::{
    AlignConfig, AlignOp, AlignRange, AlignScratch, DocSource, LineSource, Side, align_with_scratch,
};
use core_text::{LineRules, TextDocument};
use criterion::{Criterion, criterion_group, criterion_main};

/// A log-like text of about `mb` megabytes (~75 bytes per line).
fn log(mb: usize, variant: bool) -> String {
    let target = mb * 1024 * 1024;
    let mut out = String::with_capacity(target + 128);
    let mut n = 0u64;
    while out.len() < target {
        // the variant differs in every 5000th line, and drops every 20000th, inserts after
        // every 30000th: a few real differences in a big shared body
        if variant && n % 20_000 == 7 {
            n += 1;
            continue;
        }
        let tag = if variant && n % 5000 == 3 {
            "CHANGED"
        } else {
            "INFO "
        };
        out.push_str(&format!(
            "2026-11-21 15:{:02}:{:02}.{:03} {tag} worker-{} request {} handled in {} ms\n",
            n % 60,
            (n / 60) % 60,
            n % 1000,
            n % 8,
            n,
            n % 250
        ));
        if variant && n % 30_000 == 11 {
            out.push_str(&format!("inserted line after {n}\n"));
        }
        n += 1;
    }
    out
}

fn doc(text: &str) -> TextDocument {
    TextDocument::from_text(text).expect("document")
}

fn run(left: &TextDocument, right: &TextDocument, scratch: &mut AlignScratch) -> usize {
    let rules = LineRules::new();
    let source = DocSource::new(left, right, &rules, None);
    let mut ops = 0usize;
    align_with_scratch(
        &source,
        &AlignConfig::default(),
        AlignRange::full(),
        &CancelToken::new(),
        &mut |_: AlignOp| ops += 1,
        scratch,
    )
    .expect("align");
    ops
}

/// Time from the start of the alignment until the 200th op is emitted.
fn first_200_ops(left: &TextDocument, right: &TextDocument) -> std::time::Duration {
    let rules = LineRules::new();
    let source = DocSource::new(left, right, &rules, None);
    let cancel = CancelToken::new();
    let started = Instant::now();
    let mut seen = 0;
    let mut at = None;
    let _ = align_with_scratch(
        &source,
        &AlignConfig::default(),
        AlignRange::full(),
        &cancel,
        &mut |_: AlignOp| {
            seen += 1;
            if seen == 200 {
                at = Some(started.elapsed());
                cancel.cancel();
            }
        },
        &mut AlignScratch::new(),
    );
    at.unwrap_or_else(|| started.elapsed())
}

fn bench_logs(c: &mut Criterion) {
    let mb: usize = std::env::var("ALIGN_BENCH_MB")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(16);
    let (a, b) = (log(mb, false), log(mb, true));
    let (left, right) = (doc(&a), doc(&b));
    eprintln!(
        "near-identical logs: {mb} MB, {} / {} lines; first 200 ops after {:?} (budget 100 ms at 100 MB)",
        left.line_count(),
        right.line_count(),
        first_200_ops(&left, &right)
    );
    let mut scratch = AlignScratch::new();
    let mut g = c.benchmark_group("near_identical_logs");
    g.sample_size(10);
    g.bench_function(format!("full_{mb}mb"), |bch| {
        bch.iter(|| black_box(run(&left, &right, &mut scratch)));
    });
    g.finish();
}

fn bench_all_different(c: &mut Criterion) {
    // 50k lines per side that share nothing: every window is a full gap DP
    let n: usize = 50_000;
    let words = [
        "amber", "birch", "cedar", "dune", "ember", "fjord", "glade", "heath",
    ];
    let make = |salt: usize| -> String {
        (0..n)
            .map(|k| {
                let w = |i: usize| {
                    words[(k.wrapping_mul(2_654_435_761).rotate_left(i as u32 * 5) ^ salt) % 8]
                };
                format!("{salt}-{k} {} {} {} {}\n", w(1), w(2), w(3), w(4))
            })
            .collect()
    };
    let (left, right) = (doc(&make(1)), doc(&make(2)));
    let mut scratch = AlignScratch::new();
    let started = Instant::now();
    run(&left, &right, &mut scratch);
    let (took, mb) = (started.elapsed(), scratch.capacity_bytes() / (1024 * 1024));
    eprintln!("all-different 50k x 50k: {took:?}, scratch {mb} MB (budget 3 s, 300 MB)");
    // the budget gate for this case is the isolated `worst_case` bench (it also measures the
    // process's peak memory); this one keeps the timing history for the regression check
    let mut g = c.benchmark_group("all_different");
    g.sample_size(10);
    g.bench_function("50k_lines", |bch| {
        bch.iter(|| black_box(run(&left, &right, &mut scratch)));
    });
    g.finish();
}

/// Cost of one metadata read through the document source (lock + chunked cursor).
fn bench_meta(c: &mut Criterion) {
    let text = log(4, false);
    let (l, r) = (doc(&text), doc(&text));
    let rules = LineRules::new();
    let source = DocSource::new(&l, &r, &rules, None);
    let n = l.line_count() as u32;
    c.bench_function("doc_source_meta_per_line", |bch| {
        let mut i = 0;
        bch.iter(|| {
            i = (i + 1) % n;
            black_box(source.meta(Side::Left, i))
        });
    });
}

criterion_group!(benches, bench_logs, bench_all_different, bench_meta);
criterion_main!(benches);
