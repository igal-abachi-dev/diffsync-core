//! Line indexing throughput (backlog E1-S2.2, plan.md §12.2: >= 1 GB/s on UTF-8).
//!
//! `LINE_INDEX_MB` sets the synthetic file size (default 256; use 1024 for the 1 GB figure
//! of the budget table). The text mimics a log: ~80-byte lines, mostly LF, some CRLF.

use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};

fn synthetic(mb: usize) -> Vec<u8> {
    let target = mb * 1024 * 1024;
    let mut out = Vec::with_capacity(target + 128);
    let mut n = 0u64;
    while out.len() < target {
        let ending: &[u8] = if n % 97 == 0 { b"\r\n" } else { b"\n" };
        let line = format!(
            "2026-11-21 15:{:02}:{:02}.{:03} INFO  worker-{} request {} handled in {} ms",
            n % 60,
            (n / 60) % 60,
            n % 1000,
            n % 8,
            n,
            n % 250
        );
        out.extend_from_slice(line.as_bytes());
        out.extend_from_slice(ending);
        n += 1;
    }
    out
}

fn bench_index(c: &mut Criterion) {
    let mb = std::env::var("LINE_INDEX_MB")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(256);
    let data = synthetic(mb);
    // the budget gate of `cargo xtask bench` (backlog E11-S2): >= 1 GB/s, best of three
    let best = (0..3)
        .map(|_| {
            let t = std::time::Instant::now();
            black_box(core_text::index_lines(black_box(&data)).unwrap().len());
            t.elapsed().as_secs_f64()
        })
        .fold(f64::MAX, f64::min);
    let gbs = data.len() as f64 / best / 1e9;
    println!(
        "BUDGET line_index_gbs {gbs:.2} 1.00 {}",
        if gbs >= 1.0 { "PASS" } else { "FAIL" }
    );
    let mut g = c.benchmark_group("line_index");
    g.throughput(Throughput::Bytes(data.len() as u64));
    g.sample_size(10);
    g.bench_function(format!("utf8_{mb}mb"), |b| {
        b.iter(|| {
            let idx = core_text::index_lines(black_box(&data)).unwrap();
            black_box(idx.len())
        });
    });
    g.finish();
}

criterion_group!(benches, bench_index);
criterion_main!(benches);
