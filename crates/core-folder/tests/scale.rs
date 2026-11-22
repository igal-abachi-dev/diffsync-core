//! Scale checks (backlog E7-S12.2): run with
//! `cargo test -p core-folder --release --test scale -- --ignored --nocapture`.
//! `DS_SCALE_FILES` sets the number of paired files (default 1,000,000).

use std::sync::Arc;
use std::time::Instant;

use core_base::CancelToken;
use core_folder::{FolderCompare, FolderRules, FolderView, ViewFilter};
use core_vfs::{MemVfs, VPath, Vfs};

fn build(total: usize) -> (MemVfs, MemVfs) {
    let (l, r) = (MemVfs::new(), MemVfs::new());
    let per = 1000;
    for i in 0..total {
        let (d, f) = (i / per, i % per);
        let path = format!("dir{d:05}/file{f:04}.txt");
        l.add_file(&path, b"x", 1000);
        // 1 in 10 differ, 1 in 50 are orphans on the right
        if i % 50 == 7 {
            continue;
        }
        r.add_file(&path, if i % 10 == 3 { b"yy" } else { b"x" }, 1000);
    }
    (l, r)
}

#[test]
#[ignore = "heavy: run in release mode"]
fn scan_one_million_nodes() {
    let total: usize = std::env::var("DS_SCALE_FILES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1_000_000);
    let t = Instant::now();
    let (l, r) = build(total);
    println!("built {total} pairs in {:?}", t.elapsed());
    let (lv, rv): (Arc<dyn Vfs>, Arc<dyn Vfs>) = (Arc::new(l), Arc::new(r));
    let t = Instant::now();
    let fc = FolderCompare::compare(
        lv,
        &VPath::root(),
        rv,
        &VPath::root(),
        FolderRules::default(),
        &CancelToken::new(),
    )
    .unwrap();
    let secs = t.elapsed().as_secs_f64();
    let nodes = fc.tree.len();
    let entries = (2 * total) as f64; // listed entries, both sides
    let mb = fc.tree.memory_bytes() as f64 / 1e6;
    println!(
        "scan: {nodes} nodes in {secs:.2}s = {:.0} entries/s ({:.0} nodes/s); tree {mb:.0} MB ({:.0} B/node); node = {} B",
        entries / secs,
        nodes as f64 / secs,
        mb * 1e6 / nodes as f64,
        size_of::<core_folder::Node>()
    );
    let c = fc.tree.node(0).counters;
    println!(
        "counters: diff {} left-orphan {} right-orphan {}",
        c.diff, c.left_orphan, c.right_orphan
    );

    let t = Instant::now();
    let mut v = FolderView::new();
    v.expand_all(&fc.tree);
    println!("view: {} rows built in {:?}", v.len(), t.elapsed());
    let t = Instant::now();
    let mut sum = 0u64;
    for k in 0..10_000 {
        sum += u64::from(v.get_rows(k * 97 % v.len().max(1), 60).len() as u32);
    }
    let per_call = t.elapsed().as_secs_f64() / 10_000.0 * 1e3;
    println!("get_rows(60): {per_call:.5} ms per call ({sum})");
    println!(
        "BUDGET get_rows_ms {per_call:.5} 0.2000 {}",
        if per_call < 0.2 { "PASS" } else { "FAIL" }
    );
    println!(
        "BUDGET scan_entries_per_s {:.0} 150000 {}",
        entries / secs,
        if entries / secs >= 150_000.0 {
            "PASS"
        } else {
            "FAIL"
        }
    );
    println!(
        "BUDGET tree_mb_1m_nodes {mb:.0} 160 {}",
        if mb <= 160.0 { "PASS" } else { "FAIL" }
    );
    assert!(per_call < 0.2, "get_rows budget is 0.2 ms");
    v.set_filter(&fc.tree, ViewFilter::Differences);
    println!("differences view: {} rows", v.len());
    assert!(entries / secs >= 150_000.0, "scan budget: 150k entries/s");
    assert!(mb <= 160.0, "memory budget: 160 MB");
}

/// Real disk (backlog E7-S12.2): `DS_SCALE_DISK_FILES` paired files (default 50,000) in two
/// temp folders, scanned through the Win32 backend on Windows.
#[test]
#[ignore = "heavy: creates many files; run in release mode"]
fn scan_real_disk() {
    let total: usize = std::env::var("DS_SCALE_DISK_FILES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(50_000);
    let (dl, dr) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let t = Instant::now();
    for (side, root) in [dl.path(), dr.path()].into_iter().enumerate() {
        for i in 0..total {
            let dir = root.join(format!("dir{:04}", i / 500));
            if i % 500 == 0 {
                std::fs::create_dir_all(&dir).unwrap();
            }
            if side == 1 && i % 50 == 7 {
                continue;
            }
            std::fs::write(dir.join(format!("file{:04}.txt", i % 500)), b"x").unwrap();
        }
    }
    println!("created {} files in {:?}", total * 2, t.elapsed());
    #[cfg(windows)]
    let mk = |p: &std::path::Path| -> Arc<dyn Vfs> {
        Arc::new(core_vfs::LocalWin32::new(
            core_vfs::VfsRoot::parse_windows(p.to_str().unwrap()).unwrap(),
        ))
    };
    #[cfg(not(windows))]
    let mk = |p: &std::path::Path| -> Arc<dyn Vfs> { Arc::new(core_vfs::LocalStd::new(p)) };
    let (l, r) = (mk(dl.path()), mk(dr.path()));
    let t = Instant::now();
    let fc = FolderCompare::compare(
        l,
        &VPath::root(),
        r,
        &VPath::root(),
        FolderRules::default(),
        &CancelToken::new(),
    )
    .unwrap();
    let secs = t.elapsed().as_secs_f64();
    let listed = (total * 2) as f64;
    println!(
        "real-disk scan: {} nodes in {secs:.2}s = {:.0} entries/s (warm cache); tree {:.0} MB",
        fc.tree.len(),
        listed / secs,
        fc.tree.memory_bytes() as f64 / 1e6
    );
    assert_eq!(fc.tree.node(0).counters.left_orphan as usize, total / 50);
    let rate = listed / secs;
    println!(
        "BUDGET real_disk_scan_entries_per_s {rate:.0} 150000 {}",
        if rate >= 150_000.0 { "PASS" } else { "FAIL" }
    );
    assert!(rate >= 150_000.0, "scan budget: 150k entries/s");
}
