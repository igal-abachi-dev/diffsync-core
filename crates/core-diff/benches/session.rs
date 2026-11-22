//! Session-level text budgets (backlog E4-S10, plan.md §12.2), measured through the public
//! `TextCompare` API on two near-identical log files written to a temp folder:
//!
//! * viewport pull (100 rows + their text + intraline of the changed ones) <= 0.2 ms;
//! * first 200 rows available <= 100 ms after `start`;
//! * full alignment <= 1.5 s (for the 2 x 100 MB pair).
//!
//! `SESSION_BENCH_MB` sets the size of each file (default 16; use 100 for the budget figure).
//! The first-rows and full-alignment budgets scale with the size; the figure printed is checked
//! against the 100 MB budget scaled linearly. Run: `cargo bench -p core-diff --bench session`.

use std::fmt::Write as _;
use std::time::{Duration, Instant};

use core_diff::session::{DiffRow, RowKind};
use core_diff::{DocSpec, Side, Status, TextCompare, TextOptions};

fn log(mb: usize, variant: bool) -> String {
    let target = mb * 1024 * 1024;
    let mut out = String::with_capacity(target + 128);
    let mut n = 0u64;
    while out.len() < target {
        if variant && n % 20_000 == 7 {
            n += 1;
            continue;
        }
        // the variant also differs early, so the first 200 rows have real differences
        let tag = if variant && (n % 5000 == 3 || (n < 400 && n % 2 == 1)) {
            "CHANGED"
        } else {
            "INFO "
        };
        let _ = writeln!(
            out,
            "2026-11-21 15:{:02}:{:02}.{:03} {tag} worker-{} request {} handled in {} ms",
            n % 60,
            (n / 60) % 60,
            n % 1000,
            n % 8,
            n,
            n % 250
        );
        n += 1;
    }
    out
}

fn main() {
    let mb: usize = std::env::var("SESSION_BENCH_MB")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(16);
    let dir = std::env::temp_dir().join(format!("ds-session-bench-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let (lp, rp) = (dir.join("left.log"), dir.join("right.log"));
    std::fs::write(&lp, log(mb, false)).expect("write left");
    std::fs::write(&rp, log(mb, true)).expect("write right");

    let started = Instant::now();
    let tc = TextCompare::start(
        DocSpec::Path(lp.clone()),
        DocSpec::Path(rp.clone()),
        TextOptions::default(),
        None,
    )
    .expect("start");
    let mut first200 = None;
    while !matches!(
        tc.status(),
        Status::Done | Status::Failed { .. } | Status::Cancelled | Status::Binary
    ) {
        if first200.is_none() && tc.row_count() >= 200 {
            first200 = Some(started.elapsed());
        }
        std::thread::sleep(Duration::from_micros(200));
    }
    let full = started.elapsed();
    assert_eq!(tc.status(), Status::Done, "the session must finish");
    let first200 = first200.unwrap_or(full);
    let rows = tc.row_count();

    // viewport pulls: 100 rows, their text, intraline for paired changed rows
    let mut buf = vec![
        DiffRow {
            left: 0,
            right: 0,
            kind: RowKind::Same
        };
        100
    ];
    let mut text = Vec::new();
    let pulls = 2_000u32;
    let t = Instant::now();
    let mut sink = 0usize;
    for k in 0..pulls {
        let first = k.wrapping_mul(2_654_435_761) % rows.saturating_sub(100).max(1);
        let n = tc.rows(first, &mut buf);
        for (i, r) in buf[..n].iter().enumerate() {
            if let Some(l) = r.left_opt() {
                tc.line_utf16(Side::Left, l, &mut text);
                sink += text.len();
            }
            if let Some(rr) = r.right_opt() {
                tc.line_utf16(Side::Right, rr, &mut text);
                sink += text.len();
            }
            if r.kind != RowKind::Same {
                sink += tc.intraline(first + i as u32).map_or(0, |x| x.left.len());
            }
        }
    }
    let per_pull = t.elapsed().as_secs_f64() * 1e3 / f64::from(pulls);
    std::hint::black_box(sink);

    let scale = mb as f64 / 100.0;
    // linear scaling of the 100 MB budgets; the latency to first rows does not grow with size
    let full_budget = 1.5 * scale.max(0.01);
    println!(
        "{mb} MB x 2, {rows} rows: first 200 rows after {first200:?} (budget 100 ms), full alignment {full:?} \
         (budget {full_budget:.2} s), viewport pull {per_pull:.3} ms (budget 0.2 ms)",
    );
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        first200 <= Duration::from_millis(100),
        "first 200 rows too slow"
    );
    let verdict = |ok: bool| if ok { "PASS" } else { "FAIL" };
    println!(
        "BUDGET first200_ms {:.1} 100.0 {}",
        first200.as_secs_f64() * 1e3,
        verdict(first200.as_millis() <= 100)
    );
    println!(
        "BUDGET full_alignment_s {:.2} {full_budget:.2} {}",
        full.as_secs_f64(),
        verdict(full.as_secs_f64() <= full_budget)
    );
    println!(
        "BUDGET viewport_pull_ms {per_pull:.4} 0.2000 {}",
        verdict(per_pull <= 0.2)
    );
    assert!(per_pull <= 0.2, "viewport pull too slow");
    assert!(full.as_secs_f64() <= full_budget, "full alignment too slow");
}
