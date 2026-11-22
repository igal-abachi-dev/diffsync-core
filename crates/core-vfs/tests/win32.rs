//! Windows integration tests (backlog E5-S3, S5, S6, S7, S13.2): the Win32 backend on a real
//! NTFS temp folder. Junctions are created with `mklink /J`, which needs no privilege;
//! symlink tests are skipped with a message when the privilege is missing.
#![cfg(windows)]

use std::io::Read;
use std::process::Command;

use core_base::{CancelToken, ErrorKind};
use core_vfs::identity::{RootsRelation, roots_relation};
use core_vfs::{
    CopyOptions, DeleteMode, Kind, LocalWin32, VPath, Vfs, VfsRoot, attr, copy_between,
    write_atomic,
};

fn p(s: &str) -> VPath {
    VPath::from_parts(s.split('/').filter(|x| !x.is_empty()))
}

fn backend(dir: &std::path::Path) -> LocalWin32 {
    LocalWin32::new(VfsRoot::parse_windows(dir.to_str().unwrap()).unwrap())
}

fn get(v: &dyn Vfs, path: &str) -> Vec<u8> {
    let mut out = Vec::new();
    v.open_read(&p(path))
        .unwrap()
        .read_to_end(&mut out)
        .unwrap();
    out
}

fn mklink_junction(link: &std::path::Path, target: &std::path::Path) -> bool {
    Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .output()
        .is_ok_and(|o| o.status.success())
}

#[test]
fn basic_operations_on_ntfs() {
    let dir = tempfile::tempdir().unwrap();
    let v = backend(dir.path());
    v.create_dir(&p("d")).unwrap();
    write_atomic(&v, &p("d/a.txt"), b"hello").unwrap();
    let e = v.stat(&p("d/a.txt")).unwrap().unwrap();
    assert_eq!((e.kind, e.size), (Kind::File, 5));
    assert!(v.stat(&p("d/none")).unwrap().is_none());
    let mut list = Vec::new();
    v.list(&p("d"), &mut list).unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].name.to_string_lossy(), "a.txt");
    assert_eq!(get(&v, "d/a.txt"), b"hello");
    let t = 132_223_104_000_000_000i64;
    v.set_times(&p("d/a.txt"), Some(t), Some(t)).unwrap();
    v.set_times(&p("d"), Some(t + 20_000_000), None).unwrap();
    let e = v.stat(&p("d/a.txt")).unwrap().unwrap();
    assert_eq!((e.mtime, e.ctime), (t, t));
    assert_eq!(v.stat(&p("d")).unwrap().unwrap().mtime, t + 20_000_000);
    v.delete(&p("d"), DeleteMode::Permanent).unwrap();
    assert!(v.stat(&p("d")).unwrap().is_none());
}

/// Backlog E5-S2.2: a path above MAX_PATH works through the `\\?\` form.
#[test]
fn long_paths() {
    let dir = tempfile::tempdir().unwrap();
    let v = backend(dir.path());
    let mut rel = String::new();
    for _ in 0..4 {
        rel.push_str(&format!("{}/", "d".repeat(100)));
        v.create_dir(&p(rel.trim_end_matches('/'))).unwrap();
    }
    let file = format!("{rel}file.txt");
    assert!(dir.path().to_str().unwrap().len() + file.len() > 400);
    write_atomic(&v, &p(&file), b"deep").unwrap();
    assert_eq!(get(&v, &file), b"deep");
    let mut list = Vec::new();
    v.list(&p(rel.trim_end_matches('/')), &mut list).unwrap();
    assert_eq!(list.len(), 1);
    v.delete(&p(&"d".repeat(100)), DeleteMode::Permanent)
        .unwrap();
}

/// Backlog E5-S3.4: names with trailing dots or spaces and device names are listed unchanged
/// and can be copied and deleted.
#[test]
fn odd_names_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let v = backend(dir.path());
    for name in ["foo.", "bar ", "aux.c", "nul.txt", "COM1"] {
        write_atomic(&v, &p("tmp"), name.as_bytes()).unwrap();
        v.rename(&p("tmp"), &p(name), false)
            .unwrap_or_else(|e| panic!("{name}: {e}"));
    }
    let mut list = Vec::new();
    v.list(&VPath::root(), &mut list).unwrap();
    let mut names: Vec<String> = list.iter().map(|e| e.name.to_string_lossy()).collect();
    names.sort();
    assert_eq!(
        names,
        ["COM1", "aux.c", "bar ", "foo.", "nul.txt"],
        "listed exactly as stored"
    );
    copy_between(
        &v,
        &p("foo."),
        &v,
        &p("foo2."),
        &CopyOptions::default(),
        &|_, _| {},
        &CancelToken::new(),
    )
    .unwrap();
    assert_eq!(get(&v, "foo2."), b"foo.");
    for n in ["foo.", "bar ", "aux.c", "nul.txt", "COM1", "foo2."] {
        v.delete(&p(n), DeleteMode::Permanent)
            .unwrap_or_else(|e| panic!("{n}: {e}"));
    }
    assert!(!std::fs::read_dir(dir.path()).unwrap().any(|e| e.is_ok()));
}

/// Backlog E5-S3.2 and E5-S7.1: a junction is a link, listed as such, and deleting it (or a
/// folder that contains it) never touches the target.
#[test]
fn junctions_are_links_and_are_never_traversed_by_delete() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("target");
    std::fs::create_dir(&target).unwrap();
    std::fs::write(target.join("precious.txt"), b"keep").unwrap();
    let outer = dir.path().join("outer");
    std::fs::create_dir(&outer).unwrap();
    if !mklink_junction(&outer.join("lnk"), &target) {
        eprintln!("SKIPPED: could not create a junction here");
        return;
    }
    let v = backend(dir.path());
    let mut list = Vec::new();
    v.list(&p("outer"), &mut list).unwrap();
    assert_eq!(list[0].kind, Kind::Link, "a junction is a link (D16)");
    assert_eq!(v.stat(&p("outer/lnk")).unwrap().unwrap().kind, Kind::Link);
    // deleting the folder that holds the junction removes the junction only
    v.delete(&p("outer"), DeleteMode::Permanent).unwrap();
    assert!(!outer.exists());
    assert_eq!(
        std::fs::read(target.join("precious.txt")).unwrap(),
        b"keep",
        "the target's contents are intact"
    );
    // deleting the junction directly, too
    assert!(mklink_junction(&dir.path().join("lnk2"), &target));
    v.delete(&p("lnk2"), DeleteMode::Permanent).unwrap();
    assert!(target.join("precious.txt").exists());
}

#[test]
fn symlinks_are_links_when_the_privilege_exists() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("real.txt"), b"x").unwrap();
    if std::os::windows::fs::symlink_file(dir.path().join("real.txt"), dir.path().join("sl.txt"))
        .is_err()
    {
        eprintln!("SKIPPED: creating symlinks needs developer mode or elevation");
        return;
    }
    let v = backend(dir.path());
    assert_eq!(v.stat(&p("sl.txt")).unwrap().unwrap().kind, Kind::Link);
    v.delete(&p("sl.txt"), DeleteMode::Permanent).unwrap();
    assert!(dir.path().join("real.txt").exists());
}

/// Backlog E5-S6.3 (fix B5): overwrite of a read-only + hidden destination.
#[test]
fn read_only_and_hidden_destinations() {
    let dir = tempfile::tempdir().unwrap();
    let v = backend(dir.path());
    write_atomic(&v, &p("src"), b"new").unwrap();
    write_atomic(&v, &p("dst"), b"old").unwrap();
    v.set_attrs(&p("dst"), attr::READONLY | attr::HIDDEN)
        .unwrap();
    let cancel = CancelToken::new();
    let plain = CopyOptions {
        overwrite: true,
        ..CopyOptions::default()
    };
    let e = copy_between(&v, &p("src"), &v, &p("dst"), &plain, &|_, _| {}, &cancel).unwrap_err();
    assert_eq!(e.kind(), ErrorKind::AccessDenied, "{e}");
    assert_eq!(get(&v, "dst"), b"old", "destination untouched");
    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.file_name())
        .collect();
    assert!(
        leftovers
            .iter()
            .all(|n| !n.to_string_lossy().ends_with(".dctmp~")),
        "{leftovers:?}"
    );
    let retry = CopyOptions {
        overwrite_readonly: true,
        ..plain
    };
    copy_between(&v, &p("src"), &v, &p("dst"), &retry, &|_, _| {}, &cancel).unwrap();
    assert_eq!(get(&v, "dst"), b"new");
    // permanent delete clears READONLY first
    v.set_attrs(&p("dst"), attr::READONLY).unwrap();
    v.delete(&p("dst"), DeleteMode::Permanent).unwrap();
}

/// Backlog E5-S6.1/S6.2: CopyFileExW copy keeps mtime, reports progress, and a cancel leaves the
/// destination untouched without a temp file.
#[test]
fn native_copy_progress_and_cancel() {
    let dir = tempfile::tempdir().unwrap();
    let v = backend(dir.path());
    let big = vec![9u8; 24 * 1024 * 1024];
    write_atomic(&v, &p("big"), &big).unwrap();
    let t = 132_223_104_000_000_000i64;
    v.set_times(&p("big"), Some(t), None).unwrap();
    let o = CopyOptions {
        overwrite: true,
        verify: true,
        ..CopyOptions::default()
    };
    let calls = std::sync::atomic::AtomicU32::new(0);
    copy_between(
        &v,
        &p("big"),
        &v,
        &p("big2"),
        &CopyOptions {
            overwrite: false,
            ..o
        },
        &|_, _| {
            calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        },
        &CancelToken::new(),
    )
    .unwrap();
    assert!(
        calls.load(std::sync::atomic::Ordering::Relaxed) > 0,
        "progress routine called"
    );
    assert_eq!(v.stat(&p("big2")).unwrap().unwrap().mtime, t, "mtime kept");
    assert_eq!(get(&v, "big2").len(), big.len());

    write_atomic(&v, &p("keep"), b"old destination").unwrap();
    let cancel = CancelToken::new();
    let c2 = cancel.clone();
    let e = copy_between(
        &v,
        &p("big"),
        &v,
        &p("keep"),
        &o,
        &move |d, t| {
            if d * 2 > t {
                c2.cancel()
            }
        },
        &cancel,
    )
    .unwrap_err();
    assert!(e.is_cancelled(), "{e}");
    assert_eq!(get(&v, "keep"), b"old destination");
    assert!(
        std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .all(|e| !e.file_name().to_string_lossy().ends_with(".dctmp~"))
    );
}

/// Backlog E5-S5: the same file through two paths has equal IDs; nested, same and disjoint
/// roots are detected, including through a junction alias.
#[test]
fn file_ids_and_root_overlap() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("a/b")).unwrap();
    std::fs::create_dir(dir.path().join("other")).unwrap();
    std::fs::write(dir.path().join("a/f"), b"x").unwrap();
    let whole = backend(dir.path());
    let id1 = whole.file_id(&p("a/f")).unwrap().unwrap();
    let inner = backend(&dir.path().join("a"));
    assert_eq!(
        inner.file_id(&p("f")).unwrap().unwrap(),
        id1,
        "same file through two roots"
    );

    let rel = |l: &VPath, r: &VPath| roots_relation(&whole, l, &whole, r).unwrap().unwrap();
    assert_eq!(rel(&p("a"), &p("a")), RootsRelation::Same);
    assert_eq!(rel(&p("a/b"), &p("a")), RootsRelation::LeftInsideRight);
    assert_eq!(rel(&p("a"), &p("a/b")), RootsRelation::RightInsideLeft);
    assert_eq!(rel(&p("a"), &p("other")), RootsRelation::Disjoint);

    // an alias through a junction is the same folder
    if mklink_junction(&dir.path().join("alias"), &dir.path().join("a")) {
        assert_eq!(
            rel(&p("alias"), &p("a")),
            RootsRelation::Same,
            "junction alias"
        );
        assert_eq!(rel(&p("alias/b"), &p("a")), RootsRelation::LeftInsideRight);
        whole.delete(&p("alias"), DeleteMode::Permanent).unwrap();
    }
}

#[test]
fn case_probing_and_caps() {
    let dir = tempfile::tempdir().unwrap();
    let v = backend(dir.path());
    assert!(
        !v.caps().case_sensitive,
        "NTFS volumes are case-insensitive by default"
    );
    assert!(!v.dir_case_sensitive(&VPath::root()).unwrap());
    assert!(v.caps().native_copy && v.caps().windows_names);
}

#[test]
fn missing_things_report_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let v = backend(dir.path());
    assert!(v.stat(&p("nope/deeper")).unwrap().is_none());
    assert_eq!(
        v.list(&p("nope"), &mut Vec::new()).unwrap_err().kind(),
        ErrorKind::NotFound
    );
    assert_eq!(
        v.delete(&p("nope"), DeleteMode::Permanent)
            .unwrap_err()
            .kind(),
        ErrorKind::NotFound
    );
    assert!(v.open_read(&p("nope")).is_err());
}

/// Review blocker: a native copy between two different roots must write into the *destination*
/// root, not into the source tree.
#[test]
fn copy_between_two_different_roots() {
    let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (left, right) = (backend(a.path()), backend(b.path()));
    assert_ne!(left.storage_id(), right.storage_id());
    write_atomic(&left, &p("x"), b"payload").unwrap();
    copy_between(
        &left,
        &p("x"),
        &right,
        &p("x"),
        &CopyOptions::default(),
        &|_, _| {},
        &CancelToken::new(),
    )
    .unwrap();
    assert_eq!(
        std::fs::read(b.path().join("x")).unwrap(),
        b"payload",
        "written into the right root"
    );
    assert_eq!(
        std::fs::read(a.path().join("x")).unwrap(),
        b"payload",
        "left root untouched"
    );
    assert_eq!(
        std::fs::read_dir(a.path()).unwrap().count(),
        1,
        "no stray temp file in the source tree"
    );
    // the same root gives the same storage id (native fast path)
    assert_eq!(backend(a.path()).storage_id(), left.storage_id());
}

/// Review blocker: two backends rooted in one tree are related through their OS ancestors.
#[test]
fn roots_relation_between_two_backends() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("a/b")).unwrap();
    std::fs::create_dir(dir.path().join("c")).unwrap();
    let rel = |l: &std::path::Path, r: &std::path::Path| {
        let (lb, rb) = (backend(l), backend(r));
        roots_relation(&lb, &VPath::root(), &rb, &VPath::root())
            .unwrap()
            .unwrap()
    };
    let a = dir.path().join("a");
    assert_eq!(rel(&a.join("b"), &a), RootsRelation::LeftInsideRight);
    assert_eq!(rel(&a, &a.join("b")), RootsRelation::RightInsideLeft);
    assert_eq!(rel(&a, &a), RootsRelation::Same);
    assert_eq!(rel(&a, &dir.path().join("c")), RootsRelation::Disjoint);
}

/// Backlog E5-S7.3, decision D37: a path the shell cannot take is `Unsupported`, never a silent
/// permanent delete. These cases never reach the Recycle Bin, so they always run.
#[test]
fn recycle_refuses_what_the_shell_cannot_recycle() {
    let dir = tempfile::tempdir().unwrap();
    let v = backend(dir.path());
    // a path beyond MAX_PATH
    let mut rel = String::new();
    for _ in 0..3 {
        rel.push_str(&format!("{}/", "d".repeat(100)));
        v.create_dir(&p(rel.trim_end_matches('/'))).unwrap();
    }
    let file = format!("{rel}file.txt");
    write_atomic(&v, &p(&file), b"deep").unwrap();
    let e = v.delete(&p(&file), DeleteMode::RecycleBin).unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Unsupported);
    assert_eq!(get(&v, &file), b"deep", "the file is untouched");
    // a missing item and the root
    assert_eq!(
        v.delete(&p("nope"), DeleteMode::RecycleBin)
            .unwrap_err()
            .kind(),
        ErrorKind::NotFound
    );
    assert!(v.delete(&VPath::root(), DeleteMode::RecycleBin).is_err());
    // a junction is never sent to the bin (rule 5)
    std::fs::create_dir(dir.path().join("target")).unwrap();
    let st = Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(dir.path().join("jn"))
        .arg(dir.path().join("target"))
        .output()
        .unwrap();
    assert!(st.status.success());
    let e = v.delete(&p("jn"), DeleteMode::RecycleBin).unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Unsupported);
    assert!(v.stat(&p("jn")).unwrap().is_some());
}

fn recycle_bin_items() -> i64 {
    let out = Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "(New-Object -ComObject Shell.Application).NameSpace(10).Items().Count",
        ])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .unwrap_or(-1)
}

/// Backlog E5-S7.2 (opt-in: it puts a temp file into the user's real Recycle Bin). Run with
/// `DS_TEST_RECYCLE=1 cargo test -p core-vfs --test win32 recycle_bin_round_trip`.
#[test]
fn recycle_bin_round_trip() {
    if std::env::var_os("DS_TEST_RECYCLE").is_none() {
        eprintln!("skipped: set DS_TEST_RECYCLE=1 to use the real Recycle Bin");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let v = backend(dir.path());
    v.create_dir(&p("sub")).unwrap();
    write_atomic(&v, &p("sub/ds-recycle-test.txt"), b"bye").unwrap();
    let before = recycle_bin_items();
    v.delete(&p("sub"), DeleteMode::RecycleBin).unwrap();
    assert!(v.stat(&p("sub")).unwrap().is_none());
    assert_eq!(
        recycle_bin_items(),
        before + 1,
        "the folder is in the Recycle Bin"
    );
}

/// Backlog E5-S6.5: copy speed against robocopy and the `NO_BUFFERING` threshold. Run with
/// `cargo test -p core-vfs --release --test win32 copy_vs_robocopy -- --ignored --nocapture`.
#[test]
#[ignore = "benchmark: writes about 3 GiB to the temp folder"]
fn copy_vs_robocopy() {
    use std::time::Instant;
    let dir = tempfile::tempdir().unwrap();
    let v = backend(dir.path());
    let best = |f: &mut dyn FnMut()| -> f64 {
        (0..3)
            .map(|_| {
                let t = Instant::now();
                f();
                t.elapsed().as_secs_f64()
            })
            .fold(f64::MAX, f64::min)
    };
    let best_one = |f: &mut dyn FnMut()| -> f64 {
        let t = Instant::now();
        f();
        t.elapsed().as_secs_f64()
    };
    for mib in [4usize, 64, 1024] {
        let name = format!("src{mib}.bin");
        let mut data = vec![0u8; mib << 20];
        for (i, b) in data.iter_mut().enumerate() {
            *b = (i.wrapping_mul(2654435761) >> 13) as u8;
        }
        std::fs::write(dir.path().join(&name), &data).unwrap();
        // the gate compares the copy engine with robocopy, which does not flush to the device; the
        // default `durable: true` (decision D65) is timed and printed separately below
        let opts = CopyOptions {
            overwrite: true,
            safe_copy: true,
            durable: false,
            ..CopyOptions::default()
        };
        let run = |threshold: u64| {
            core_vfs::set_no_buffering_threshold(threshold);
            best(&mut || {
                v.copy_file(
                    &p(&name),
                    &p("out.bin"),
                    &opts,
                    &|_, _| {},
                    &CancelToken::new(),
                )
                .unwrap();
            })
        };
        let buffered = run(u64::MAX);
        let unbuffered = run(0);
        core_vfs::set_no_buffering_threshold(u64::MAX);
        let robo = best(&mut || {
            // robocopy skips a file that is already up to date, so start from nothing each run
            let _ = std::fs::remove_dir_all(dir.path().join("rbdir"));
            Command::new("robocopy")
                .arg(dir.path())
                .arg(dir.path().join("rbdir"))
                .arg(&name)
                .args(["/NJH", "/NJS", "/NDL", "/NFL", "/NP", "/NS", "/NC"])
                .output()
                .unwrap();
        });
        let mbps = |s: f64| (mib as f64) / s;
        println!(
            "{mib:>5} MiB: buffered {:>7.0} MiB/s  no-buffering {:>7.0} MiB/s  robocopy {:>7.0} MiB/s  (ours/robocopy: buffered {:.2}, no-buffering {:.2})",
            mbps(buffered),
            mbps(unbuffered),
            mbps(robo),
            buffered / robo,
            unbuffered / robo
        );
        if mib == 1024 {
            // gate of `cargo xtask bench` (backlog E11-S2): the shipped setting is "buffered"
            // (decision D38); budget = within 5% of robocopy, as a time ratio. The table above
            // runs each tool three times in a row, which drifts with the state of the disk; the
            // gate interleaves the two tools and takes the median of nine of each (a minimum chases outliers: the best-of-N ratio ranged 0.7 to 1.1 between identical runs).
            core_vfs::set_no_buffering_threshold(u64::MAX);
            let (mut ours, mut theirs) = (Vec::new(), Vec::new());
            let mut overwrite = Vec::new();
            for _ in 0..9 {
                // like with like: robocopy starts from nothing, so does ours; overwriting an
                // existing file (replace-existing frees the old one) is timed separately
                let _ = std::fs::remove_file(dir.path().join("out.bin"));
                ours.push(best_one(&mut || {
                    v.copy_file(
                        &p(&name),
                        &p("out.bin"),
                        &opts,
                        &|_, _| {},
                        &CancelToken::new(),
                    )
                    .unwrap();
                }));
                overwrite.push(best_one(&mut || {
                    v.copy_file(
                        &p(&name),
                        &p("out.bin"),
                        &opts,
                        &|_, _| {},
                        &CancelToken::new(),
                    )
                    .unwrap();
                }));
                theirs.push(best_one(&mut || {
                    let _ = std::fs::remove_dir_all(dir.path().join("rbdir"));
                    Command::new("robocopy")
                        .arg(dir.path())
                        .arg(dir.path().join("rbdir"))
                        .arg(&name)
                        .args(["/NJH", "/NJS", "/NDL", "/NFL", "/NP", "/NS", "/NC"])
                        .output()
                        .unwrap();
                }));
            }
            let median = |v: &mut Vec<f64>| {
                v.sort_by(f64::total_cmp);
                v[v.len() / 2]
            };
            let (ours, theirs, overwrite) = (
                median(&mut ours),
                median(&mut theirs),
                median(&mut overwrite),
            );
            println!(
                "  1 GiB, medians of 9: ours onto a fresh name {ours:.3} s, ours replacing an existing file {overwrite:.3} s, robocopy {theirs:.3} s"
            );
            let ratio = ours / theirs;
            println!(
                "BUDGET copy_1gib_vs_robocopy {ratio:.2} 1.05 {}",
                if ratio <= 1.05 { "PASS" } else { "FAIL" }
            );
            // after the gate, so its flush cannot disturb the measurement
            let durable = CopyOptions {
                durable: true,
                ..opts
            };
            let _ = std::fs::remove_file(dir.path().join("out.bin"));
            let t = std::time::Instant::now();
            v.copy_file(
                &p(&name),
                &p("out.bin"),
                &durable,
                &|_, _| {},
                &CancelToken::new(),
            )
            .unwrap();
            eprintln!(
                "durable copy (flushed to the device before the rename, the default): {:.2} s",
                t.elapsed().as_secs_f64()
            );
            let _ = std::fs::remove_file(dir.path().join("out.bin"));
        }
    }
}

/// Backlog E5-S4.1: the per-directory case-sensitivity probe sees a directory that was switched
/// to case-sensitive with `fsutil file setCaseSensitiveInfo` (skipped when the tool or the
/// privilege is missing), and two names that differ only in case coexist there.
#[test]
fn per_directory_case_sensitivity_is_probed() {
    let dir = tempfile::tempdir().unwrap();
    let v = backend(dir.path());
    std::fs::create_dir(dir.path().join("cs")).unwrap();
    let out = Command::new("fsutil")
        .args(["file", "setCaseSensitiveInfo"])
        .arg(dir.path().join("cs"))
        .arg("enable")
        .output();
    match out {
        Ok(o) if o.status.success() => {}
        _ => {
            eprintln!(
                "skipped: fsutil setCaseSensitiveInfo is not available or needs more privilege"
            );
            return;
        }
    }
    assert!(
        v.dir_case_sensitive(&p("cs")).unwrap(),
        "the switched directory is case-sensitive"
    );
    assert!(
        !v.dir_case_sensitive(&VPath::root()).unwrap(),
        "its parent is not"
    );
    write_atomic(&v, &p("cs/Name.txt"), b"1").unwrap();
    write_atomic(&v, &p("cs/name.txt"), b"2").unwrap();
    let mut list = Vec::new();
    v.list(&p("cs"), &mut list).unwrap();
    assert_eq!(list.len(), 2, "both spellings exist");
    assert_eq!(get(&v, "cs/Name.txt"), b"1");
    assert_eq!(get(&v, "cs/name.txt"), b"2");
}

/// Decision D37 (reviewer finding): the shell re-parses a plain path, so a name it would read
/// differently is refused, and nothing it could hit by accident is touched.
#[test]
fn recycle_refuses_names_the_shell_would_misread() {
    let dir = tempfile::tempdir().unwrap();
    let v = backend(dir.path());
    // `foo.` and its sibling `foo`: the shell strips the trailing dot and would take `foo`
    v.create_dir(&p("foo.")).unwrap();
    v.create_dir(&p("foo")).unwrap();
    v.create_dir(&p("bar ")).unwrap();
    for name in ["foo.", "bar "] {
        let e = v.delete(&p(name), DeleteMode::RecycleBin).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Unsupported, "{name:?}");
        assert!(v.stat(&p(name)).unwrap().is_some(), "{name:?} untouched");
    }
    assert!(
        v.stat(&p("foo")).unwrap().is_some(),
        "the sibling is untouched"
    );
    // a wildcard is never a file name and never expands
    v.create_dir(&p("w")).unwrap();
    write_atomic(&v, &p("w/a1"), b"1").unwrap();
    write_atomic(&v, &p("w/a2"), b"2").unwrap();
    assert!(v.stat(&p("w/a*")).unwrap().is_none());
    assert_eq!(
        v.delete(&p("w/a*"), DeleteMode::RecycleBin)
            .unwrap_err()
            .kind(),
        ErrorKind::NotFound
    );
    assert_eq!(get(&v, "w/a1"), b"1");
    assert_eq!(get(&v, "w/a2"), b"2");
    // device names are refused too
    v.create_dir(&p("aux.c")).unwrap();
    assert_eq!(
        v.delete(&p("aux.c"), DeleteMode::RecycleBin)
            .unwrap_err()
            .kind(),
        ErrorKind::Unsupported
    );
}

/// Decision D28 on the native path: a source that changes while `CopyFileExW` runs fails the
/// copy with `ChangedSinceCompare`; the destination and the temp file are left clean.
#[test]
fn native_copy_detects_a_source_that_changes_during_the_copy() {
    let dir = tempfile::tempdir().unwrap();
    let v = backend(dir.path());
    write_atomic(&v, &p("src.bin"), &vec![7u8; 48 * 1024 * 1024]).unwrap();
    let touched = std::sync::atomic::AtomicBool::new(false);
    let changed = std::sync::atomic::AtomicBool::new(false);
    let e = copy_between(
        &v,
        &p("src.bin"),
        &v,
        &p("dst.bin"),
        &CopyOptions::default(),
        // attribute access does not conflict with the copy's share mode; no panic here, the
        // callback runs inside a Win32 routine
        &|_, _| {
            if !touched.swap(true, std::sync::atomic::Ordering::Relaxed) {
                let ok = v
                    .set_times(&p("src.bin"), Some(140_000_000_000_000_000), None)
                    .is_ok();
                changed.store(ok, std::sync::atomic::Ordering::Relaxed);
            }
        },
        &CancelToken::new(),
    )
    .unwrap_err();
    assert!(
        changed.load(std::sync::atomic::Ordering::Relaxed),
        "the source was touched"
    );
    assert_eq!(e.kind(), ErrorKind::ChangedSinceCompare);
    assert!(v.stat(&p("dst.bin")).unwrap().is_none(), "no destination");
    let mut list = Vec::new();
    v.list(&VPath::root(), &mut list).unwrap();
    assert_eq!(list.len(), 1, "no temp file left: {list:?}");
}

/// Decision D54: `caps().seek_penalty` agrees with what the OS reports for the physical disk
/// behind each local drive (skipped for a drive PowerShell cannot classify).
#[test]
fn seek_penalty_matches_the_physical_disk_type() {
    let mut checked = 0;
    for letter in 'C'..='H' {
        let root = format!("{letter}:\\");
        if !std::path::Path::new(&root).exists() {
            continue;
        }
        let cmd = format!(
            "(Get-Partition -DriveLetter {letter} | Get-Disk | Get-PhysicalDisk).MediaType"
        );
        let Ok(out) = Command::new("powershell")
            .args(["-NoProfile", "-Command", &cmd])
            .output()
        else {
            continue;
        };
        let kind = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let expected = match kind.as_str() {
            "HDD" => true,
            "SSD" => false,
            _ => continue, // unspecified, a virtual disk, a removable drive
        };
        let v = LocalWin32::new(core_vfs::VfsRoot::parse_windows(&root).unwrap());
        assert_eq!(
            v.caps().seek_penalty,
            expected,
            "drive {letter}: is a {kind}"
        );
        checked += 1;
    }
    eprintln!("checked {checked} drive(s)");
}

/// Backlog E11.5-S9.3: the status bar's free space.
#[test]
fn volume_space_reports_free_and_total_bytes() {
    let d = tempfile::tempdir().unwrap();
    let (free, total) = core_vfs::volume_space(d.path()).unwrap();
    assert!(
        total > 0 && free > 0 && free <= total,
        "free {free} of {total}"
    );
    assert!(core_vfs::volume_space(&d.path().join("no").join("such")).is_err());
}
