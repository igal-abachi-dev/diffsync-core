//! The worst case of plan §12.2 as one isolated process (backlog E11-S2): two files of 50 000 lines
//! that share nothing, compared through the public `TextCompare` API. Budget: <= 3 s and
//! <= 300 MB of memory for the process, with reads that never stall. The memory figure is the
//! process's own peak (`VmHWM` on Linux, `PeakWorkingSet64` on Windows), which is why this runs
//! alone: the other benches build 100 MB logs and would hide it.
//!
//! Prints `BUDGET worst_case_s ...` and `BUDGET worst_case_peak_mb ...` (a missing peak, on a
//! platform that cannot report it, prints only the time).

use std::time::{Duration, Instant};

use core_diff::{DiffRow, DocSpec, RowKind, Status, TextCompare, TextOptions};

fn peak_mb() -> Option<f64> {
    #[cfg(target_os = "linux")]
    {
        let s = std::fs::read_to_string("/proc/self/status").ok()?;
        let kb: f64 = s
            .lines()
            .find_map(|l| l.strip_prefix("VmHWM:"))?
            .trim()
            .trim_end_matches("kB")
            .trim()
            .parse()
            .ok()?;
        Some(kb / 1024.0)
    }
    #[cfg(windows)]
    {
        let cmd = format!("(Get-Process -Id {}).PeakWorkingSet64", std::process::id());
        let out = std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", &cmd])
            .output()
            .ok()?;
        let bytes: f64 = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
        Some(bytes / (1024.0 * 1024.0))
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        None
    }
}

fn main() {
    let n: usize = std::env::var("WORST_CASE_LINES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(50_000);
    let words = [
        "amber", "birch", "cedar", "dune", "ember", "fjord", "glade", "heath",
    ];
    let make = |salt: usize| -> String {
        (0..n)
            .map(|k| {
                let w =
                    |i: u32| words[(k.wrapping_mul(2_654_435_761).rotate_left(i * 5) ^ salt) % 8];
                format!("{salt}-{k} {} {} {} {}\n", w(1), w(2), w(3), w(4))
            })
            .collect()
    };
    let (a, b) = (make(1), make(2));
    let started = Instant::now();
    let tc = TextCompare::start(
        DocSpec::Text(a),
        DocSpec::Text(b),
        TextOptions::default(),
        None,
    )
    .unwrap();
    let mut rows = vec![
        DiffRow {
            left: 0,
            right: 0,
            kind: RowKind::Same
        };
        100
    ];
    let mut worst = Duration::ZERO;
    loop {
        let t = Instant::now();
        let _ = tc.row_count();
        let _ = tc.rows(0, &mut rows);
        let st = tc.status();
        worst = worst.max(t.elapsed());
        if matches!(st, Status::Done | Status::Failed { .. }) {
            assert_eq!(st, Status::Done);
            break;
        }
        std::thread::sleep(Duration::from_micros(200));
    }
    let secs = started.elapsed().as_secs_f64();
    println!(
        "BUDGET worst_case_s {secs:.2} 3.00 {}",
        if secs <= 3.0 { "PASS" } else { "FAIL" }
    );
    println!(
        "BUDGET worst_case_max_read_ms {:.3} 50.000 {}",
        worst.as_secs_f64() * 1e3,
        if worst.as_millis() < 50 {
            "PASS"
        } else {
            "FAIL"
        }
    );
    match peak_mb() {
        Some(mb) => println!(
            "BUDGET worst_case_peak_mb {mb:.0} 300 {}",
            if mb <= 300.0 { "PASS" } else { "FAIL" }
        ),
        None => eprintln!("peak memory is not available on this platform"),
    }
}
