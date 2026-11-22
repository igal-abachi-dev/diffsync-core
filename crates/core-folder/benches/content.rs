//! Content-compare throughput against plain sequential reading (backlog E11-S2, plan.md §12.2:
//! "Disk-bound, >= 90% of sequential read speed"). Two identical files of `CONTENT_BENCH_MB` MiB
//! (default 512) are compared through the real folder-compare path, once with `Binary` and once
//! with `Crc32`; the files are in the page cache, so the figure is the engine's own speed. The
//! reference is the speed of the disk: an unbuffered (`FILE_FLAG_NO_BUFFERING`) sequential read of
//! the same two files on Windows, a plain read elsewhere. "Disk-bound" holds when the engine is not
//! slower than 90% of that, i.e. it would saturate the disk (ratio = engine / disk).
//!
//! Prints `BUDGET name value limit PASS|FAIL` lines that `cargo xtask bench` collects, and exits
//! with an error when a budget fails. Run: `cargo bench -p core-folder --bench content`.

use std::io::Read;
use std::sync::Arc;
use std::time::Instant;

use core_base::CancelToken;
use core_folder::{ContentKind, FolderCompare, FolderRules};
use core_vfs::{VPath, Vfs};

fn make(path: &std::path::Path, mb: usize) {
    use std::io::Write;
    let mut f = std::io::BufWriter::with_capacity(1 << 20, std::fs::File::create(path).unwrap());
    let mut block = vec![0u8; 1 << 20];
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    for _ in 0..mb {
        for chunk in block.chunks_mut(8) {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            chunk.copy_from_slice(&x.to_le_bytes()[..chunk.len()]);
        }
        f.write_all(&block).unwrap();
    }
    f.flush().unwrap();
}

/// Cold sequential read: Windows bypasses the cache with unbuffered, sector-aligned reads.
#[cfg(windows)]
fn read_both_cold(a: &std::path::Path, b: &std::path::Path) -> f64 {
    use std::os::windows::fs::OpenOptionsExt;
    const NO_BUFFERING: u32 = 0x2000_0000;
    const BLOCK: usize = 8 << 20;
    let mut raw = vec![0u8; BLOCK + 4096];
    let off = raw.as_ptr().align_offset(4096);
    let buf = &mut raw[off..off + BLOCK];
    let t = Instant::now();
    for p in [a, b] {
        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(NO_BUFFERING)
            .open(p)
            .unwrap();
        while f.read(buf).unwrap() > 0 {}
    }
    t.elapsed().as_secs_f64()
}

#[cfg(not(windows))]
fn read_both_cold(a: &std::path::Path, b: &std::path::Path) -> f64 {
    read_both(a, b)
}

fn read_both(a: &std::path::Path, b: &std::path::Path) -> f64 {
    let t = Instant::now();
    let mut buf = vec![0u8; 1 << 20];
    for p in [a, b] {
        let mut f = std::fs::File::open(p).unwrap();
        while f.read(&mut buf).unwrap() > 0 {}
    }
    t.elapsed().as_secs_f64()
}

fn vfs(p: &std::path::Path) -> Arc<dyn Vfs> {
    #[cfg(windows)]
    {
        Arc::new(core_vfs::LocalWin32::new(
            core_vfs::VfsRoot::parse_windows(p.to_str().unwrap()).unwrap(),
        ))
    }
    #[cfg(not(windows))]
    {
        Arc::new(core_vfs::LocalStd::new(p))
    }
}

fn compare(l: &std::path::Path, r: &std::path::Path, kind: ContentKind) -> f64 {
    let rules = FolderRules {
        compare_content: true,
        content_kind: kind,
        content_overrides: true,
        ..FolderRules::default()
    };
    let cancel = CancelToken::new();
    let t = Instant::now();
    let mut fc = FolderCompare::compare(
        vfs(l),
        &VPath::root(),
        vfs(r),
        &VPath::root(),
        rules,
        &cancel,
    )
    .unwrap();
    let ids: Vec<u32> = (1..fc.tree.len() as u32).collect();
    fc.compare_content_now(&ids, &cancel).unwrap();
    t.elapsed().as_secs_f64()
}

fn main() {
    let mb: usize = std::env::var("CONTENT_BENCH_MB")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(512);
    let (dl, dr) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (fl, fr) = (dl.path().join("big.bin"), dr.path().join("big.bin"));
    make(&fl, mb);
    std::fs::copy(&fl, &fr).unwrap();
    // set both modification times equal so size/time say "same" and only content decides
    let _ = read_both(&fl, &fr);
    let warm = (0..3).map(|_| read_both(&fl, &fr)).fold(f64::MAX, f64::min);
    let base = (0..2)
        .map(|_| read_both_cold(&fl, &fr))
        .fold(f64::MAX, f64::min);
    let mut failed = false;
    for (name, kind) in [
        ("content_binary", ContentKind::Binary),
        ("content_crc32", ContentKind::Crc32),
    ] {
        let secs = (0..3)
            .map(|_| compare(dl.path(), dr.path(), kind))
            .fold(f64::MAX, f64::min);
        let ratio = base / secs; // engine speed / disk speed, as time ratio: >= 0.9 = disk-bound
        let pass = ratio >= 0.90;
        failed |= !pass;
        println!(
            "BUDGET {name} {ratio:.2} 0.90 {}",
            if pass { "PASS" } else { "FAIL" }
        );
        println!(
            "  {name}: {mb} MiB x 2 in {secs:.3} s = {:.0} MiB/s; disk (unbuffered) {base:.3} s = {:.0} MiB/s; warm plain read {warm:.3} s",
            2.0 * mb as f64 / secs,
            2.0 * mb as f64 / base
        );
    }
    if let Ok(dir) = std::env::var("CONTENT_BENCH_COLD_DIR") {
        cold(std::path::Path::new(&dir), mb);
    }
    if failed {
        std::process::exit(1);
    }
}

/// Writes `gb` GiB to a scratch file and deletes it: the cache manager gives its pages to the
/// new data, so the files measured before are no longer in memory (no admin rights needed).
fn evict_cache(dir: &std::path::Path, gb: u64) {
    use std::io::Write;
    let p = dir.join(format!("ds-evict-{}.bin", std::process::id()));
    let block = vec![0xA5u8; 8 << 20];
    {
        let mut f = std::fs::File::create(&p).unwrap();
        for _ in 0..gb * 128 {
            f.write_all(&block).unwrap();
        }
    }
    let _ = std::fs::remove_file(&p);
}

/// The cold-disk measurement (opt-in: `CONTENT_BENCH_COLD_DIR=<folder on the disk to test>`):
/// both files sit on that disk, which is the case that hurts a rotating disk most, because the
/// two overlapped readers make it seek between the files. The cache is emptied before every run
/// (`CONTENT_BENCH_EVICT_GB`, default 40: more than the RAM; `CONTENT_BENCH_EVICT_DIR`, default the
/// temp folder, should be a fast disk). Printed as `COLD` lines; the reference is a serial
/// unbuffered read of the same two files.
fn cold(dir: &std::path::Path, mb: usize) {
    let root = dir.join(format!("ds-content-bench-{}", std::process::id()));
    let (l, r) = (root.join("L"), root.join("R"));
    std::fs::create_dir_all(&l).unwrap();
    std::fs::create_dir_all(&r).unwrap();
    let (fl, fr) = (l.join("big.bin"), r.join("big.bin"));
    make(&fl, mb);
    std::fs::copy(&fl, &fr).unwrap();
    let gb: u64 = std::env::var("CONTENT_BENCH_EVICT_GB")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(40);
    let evict_dir = std::env::var("CONTENT_BENCH_EVICT_DIR")
        .map_or_else(|_| std::env::temp_dir(), std::path::PathBuf::from);
    let serial = (0..2)
        .map(|_| read_both_cold(&fl, &fr))
        .fold(f64::MAX, f64::min);
    println!(
        "COLD serial unbuffered read of both files: {serial:.2} s = {:.0} MiB/s",
        2.0 * mb as f64 / serial
    );
    for (name, kind) in [
        ("binary", ContentKind::Binary),
        ("crc32", ContentKind::Crc32),
    ] {
        evict_cache(&evict_dir, gb);
        let secs = compare(&l, &r, kind);
        println!(
            "COLD {name}: {secs:.2} s = {:.0} MiB/s; ratio to the serial read {:.2}",
            2.0 * mb as f64 / secs,
            serial / secs
        );
    }
    let _ = std::fs::remove_dir_all(&root);
}
