//! The shared VFS conformance suite (backlog E5-S10.2, S11): the same tests run for every
//! backend (`MemVfs`, `LocalStd` on a temp folder, `FaultVfs` passing through), plus the safety
//! tests of plan.md §9 with injected faults.

use std::io::{Read, Write};
use std::sync::Arc;

use core_base::{CancelToken, ErrorKind};
use core_vfs::{
    CopyOptions, DeleteMode, FaultVfs, Kind, LocalStd, MemVfs, Op, VPath, Vfs, attr, copy_between,
    copy_tree, is_temp_name, write_atomic,
};

fn p(s: &str) -> VPath {
    VPath::from_parts(s.split('/').filter(|x| !x.is_empty()))
}

fn put(v: &dyn Vfs, path: &str, data: &[u8]) {
    write_atomic(v, &p(path), data).unwrap();
}

fn get(v: &dyn Vfs, path: &str) -> Vec<u8> {
    let mut out = Vec::new();
    v.open_read(&p(path))
        .unwrap()
        .read_to_end(&mut out)
        .unwrap();
    out
}

fn names(v: &dyn Vfs, dir: &str) -> Vec<String> {
    let mut e = Vec::new();
    v.list(&p(dir), &mut e).unwrap();
    let mut n: Vec<String> = e.iter().map(|e| e.name.to_string_lossy()).collect();
    n.sort();
    n
}

fn no_temp_files(v: &dyn Vfs, dir: &str) -> bool {
    let mut e = Vec::new();
    v.list(&p(dir), &mut e).unwrap();
    !e.iter().any(|e| is_temp_name(&e.name))
}

/// Runs `f` once per backend.
fn each_backend(f: impl Fn(&str, &dyn Vfs)) {
    f("mem", &MemVfs::new());
    let dir = tempfile::tempdir().unwrap();
    f("local_std", &LocalStd::new(dir.path()));
    f("fault(mem)", &FaultVfs::new(MemVfs::new()));
    #[cfg(windows)]
    {
        let dir = tempfile::tempdir().unwrap();
        let root = core_vfs::VfsRoot::parse_windows(dir.path().to_str().unwrap()).unwrap();
        f("local_win32", &core_vfs::LocalWin32::new(root));
    }
}

#[test]
fn stat_list_create_and_missing() {
    each_backend(|name, v| {
        assert!(v.stat(&p("nope")).unwrap().is_none(), "{name}");
        v.create_dir(&p("d")).unwrap();
        v.create_dir(&p("d")).unwrap(); // idempotent
        put(v, "d/a.txt", b"hello");
        let e = v.stat(&p("d/a.txt")).unwrap().unwrap();
        assert_eq!((e.kind, e.size), (Kind::File, 5), "{name}");
        assert_eq!(v.stat(&p("d")).unwrap().unwrap().kind, Kind::Dir);
        assert_eq!(names(v, "d"), ["a.txt"], "{name}");
        assert_eq!(names(v, ""), ["d"], "{name}");
        assert_eq!(
            v.list(&p("missing"), &mut Vec::new()).unwrap_err().kind(),
            ErrorKind::NotFound,
            "{name}"
        );
        assert_eq!(get(v, "d/a.txt"), b"hello");
        assert!(v.open_read(&p("missing")).is_err());
    });
}

#[test]
fn rename_replace_and_delete() {
    each_backend(|name, v| {
        put(v, "a", b"1");
        put(v, "b", b"2");
        let e = v.rename(&p("a"), &p("b"), false).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::AlreadyExists, "{name}");
        v.rename(&p("a"), &p("b"), true).unwrap();
        assert_eq!(get(v, "b"), b"1");
        assert!(v.stat(&p("a")).unwrap().is_none());
        // recursive delete
        v.create_dir(&p("t")).unwrap();
        v.create_dir(&p("t/u")).unwrap();
        put(v, "t/u/f", b"x");
        v.delete(&p("t"), DeleteMode::Permanent).unwrap();
        assert!(v.stat(&p("t")).unwrap().is_none(), "{name}");
        assert_eq!(
            v.delete(&p("t"), DeleteMode::Permanent).unwrap_err().kind(),
            ErrorKind::NotFound,
            "{name}"
        );
        assert!(v.delete(&VPath::root(), DeleteMode::Permanent).is_err());
        // The Win32 backend really uses the Recycle Bin (decision D37), so it is exercised only
        // by its own opt-in test; every other backend has no bin.
        if name != "local_win32" {
            assert_eq!(
                v.delete(&p("b"), DeleteMode::RecycleBin)
                    .unwrap_err()
                    .kind(),
                ErrorKind::Unsupported,
                "{name}"
            );
        }
    });
}

#[test]
fn set_times_round_trips_on_files_and_folders() {
    each_backend(|name, v| {
        v.create_dir(&p("d")).unwrap();
        put(v, "d/f", b"x");
        // 2020-01-01 in ticks, on a 2 s boundary
        let t = 132_223_104_000_000_000i64;
        v.set_times(&p("d/f"), Some(t), None).unwrap();
        v.set_times(&p("d"), Some(t + 20_000_000), None).unwrap();
        assert_eq!(v.stat(&p("d/f")).unwrap().unwrap().mtime, t, "{name}");
        assert_eq!(
            v.stat(&p("d")).unwrap().unwrap().mtime,
            t + 20_000_000,
            "{name}"
        );
    });
}

#[test]
fn write_atomic_replaces_and_leaves_no_temp_file() {
    each_backend(|name, v| {
        put(v, "doc", b"old");
        put(v, "doc", b"new content");
        assert_eq!(get(v, "doc"), b"new content", "{name}");
        assert!(no_temp_files(v, ""), "{name}");
    });
}

#[test]
fn copy_keeps_content_and_mtime_and_respects_overwrite() {
    each_backend(|name, v| {
        put(v, "src", b"payload");
        let t = 132_223_104_000_000_000i64;
        v.set_times(&p("src"), Some(t), None).unwrap();
        let (cancel, o) = (CancelToken::new(), CopyOptions::default());
        copy_between(v, &p("src"), v, &p("dst"), &o, &|_, _| {}, &cancel).unwrap();
        assert_eq!(get(v, "dst"), b"payload", "{name}");
        assert_eq!(v.stat(&p("dst")).unwrap().unwrap().mtime, t, "{name}");
        let e = copy_between(v, &p("src"), v, &p("dst"), &o, &|_, _| {}, &cancel).unwrap_err();
        assert_eq!(
            e.kind(),
            ErrorKind::AlreadyExists,
            "{name}: no overwrite without the flag"
        );
        put(v, "src", b"changed");
        let ow = CopyOptions {
            overwrite: true,
            ..o
        };
        copy_between(v, &p("src"), v, &p("dst"), &ow, &|_, _| {}, &cancel).unwrap();
        assert_eq!(get(v, "dst"), b"changed", "{name}");
        assert!(no_temp_files(v, ""), "{name}");
        // links and folders are not copied as files
        v.create_dir(&p("dir")).unwrap();
        assert!(copy_between(v, &p("dir"), v, &p("x"), &o, &|_, _| {}, &cancel).is_err());
        assert!(copy_between(v, &p("missing"), v, &p("x"), &o, &|_, _| {}, &cancel).is_err());
    });
}

#[test]
fn a_large_copy_reports_progress_in_blocks() {
    each_backend(|name, v| {
        let big = vec![7u8; 3 * 1024 * 1024 + 11];
        put(v, "big", &big);
        let seen = std::sync::Mutex::new(Vec::new());
        copy_between(
            v,
            &p("big"),
            v,
            &p("big2"),
            &CopyOptions::default(),
            &|d, t| seen.lock().unwrap().push((d, t)),
            &CancelToken::new(),
        )
        .unwrap();
        let seen = seen.into_inner().unwrap();
        assert_eq!(seen.first(), Some(&(0, big.len() as u64)), "{name}");
        assert_eq!(
            seen.last(),
            Some(&(big.len() as u64, big.len() as u64)),
            "{name}"
        );
        assert!(
            seen.len() >= 4,
            "{name}: progress per 1 MiB block, got {}",
            seen.len()
        );
        assert_eq!(get(v, "big2"), big);
    });
}

#[test]
fn copy_tree_creates_folders_first_and_sets_folder_times_last() {
    each_backend(|name, v| {
        v.create_dir(&p("s")).unwrap();
        v.create_dir(&p("s/sub")).unwrap();
        put(v, "s/a", b"a");
        put(v, "s/sub/b", b"bb");
        let (t_dir, t_sub) = (132_223_104_000_000_000i64, 132_223_104_200_000_000i64);
        v.set_times(&p("s"), Some(t_dir), None).unwrap();
        v.set_times(&p("s/sub"), Some(t_sub), None).unwrap();
        let r = copy_tree(
            v,
            &p("s"),
            v,
            &p("d"),
            &CopyOptions::default(),
            &CancelToken::new(),
        )
        .unwrap();
        assert_eq!((r.files, r.dirs, r.bytes), (2, 2, 3), "{name}");
        assert_eq!(get(v, "d/sub/b"), b"bb");
        assert_eq!(
            v.stat(&p("d")).unwrap().unwrap().mtime,
            t_dir,
            "{name}: folder time set after its files"
        );
        assert_eq!(v.stat(&p("d/sub")).unwrap().unwrap().mtime, t_sub, "{name}");
    });
}

/// MemVfs -> local file system and back (backlog E5-S12.1).
#[test]
fn copy_between_different_backends() {
    let mem = MemVfs::new();
    let dir = tempfile::tempdir().unwrap();
    let local = LocalStd::new(dir.path());
    put(&mem, "m.txt", b"from memory");
    mem.set_times(&p("m.txt"), Some(132_223_104_000_000_000), None)
        .unwrap();
    let (cancel, o) = (CancelToken::new(), CopyOptions::default());
    copy_between(
        &mem,
        &p("m.txt"),
        &local,
        &p("l.txt"),
        &o,
        &|_, _| {},
        &cancel,
    )
    .unwrap();
    assert_eq!(
        std::fs::read(dir.path().join("l.txt")).unwrap(),
        b"from memory"
    );
    copy_between(
        &local,
        &p("l.txt"),
        &mem,
        &p("back.txt"),
        &o,
        &|_, _| {},
        &cancel,
    )
    .unwrap();
    assert_eq!(mem.read("back.txt").unwrap(), b"from memory");
}

// ---------------------------------------------------------------- safety (plan.md §9)

fn fault_with_dest() -> FaultVfs<MemVfs> {
    let mem = MemVfs::new();
    mem.add_file("src", &vec![1u8; 3 * 1024 * 1024], 100)
        .add_file("dst", b"OLD CONTENT", 50);
    FaultVfs::new(mem)
}

fn assert_untouched(v: &FaultVfs<MemVfs>) {
    assert_eq!(
        v.inner().read("dst").unwrap(),
        b"OLD CONTENT",
        "the destination must be byte-identical"
    );
    assert!(
        v.inner().paths().iter().all(|p| !p.ends_with(".dctmp~")),
        "temp file left behind: {:?}",
        v.inner().paths()
    );
}

#[test]
fn a_full_disk_midway_leaves_the_destination_untouched() {
    let v = fault_with_dest();
    v.fail_writes_after(1024 * 1024);
    let o = CopyOptions {
        overwrite: true,
        ..CopyOptions::default()
    };
    let e = copy_between(
        &v,
        &p("src"),
        &v,
        &p("dst"),
        &o,
        &|_, _| {},
        &CancelToken::new(),
    )
    .unwrap_err();
    assert!(
        matches!(e.kind(), ErrorKind::Io | ErrorKind::DiskFull),
        "{e}"
    );
    assert_untouched(&v);
}

#[test]
fn cancel_at_half_leaves_the_destination_untouched() {
    let v = fault_with_dest();
    let cancel = CancelToken::new();
    v.cancel_after_bytes(1_500_000, &cancel);
    let o = CopyOptions {
        overwrite: true,
        ..CopyOptions::default()
    };
    let e = copy_between(&v, &p("src"), &v, &p("dst"), &o, &|_, _| {}, &cancel).unwrap_err();
    assert!(e.is_cancelled(), "{e}");
    assert_untouched(&v);
}

#[test]
fn a_failure_at_the_final_rename_leaves_the_destination_untouched() {
    let v = fault_with_dest();
    v.fail_nth(Op::Rename, 1, ErrorKind::Io);
    let o = CopyOptions {
        overwrite: true,
        ..CopyOptions::default()
    };
    assert!(
        copy_between(
            &v,
            &p("src"),
            &v,
            &p("dst"),
            &o,
            &|_, _| {},
            &CancelToken::new()
        )
        .is_err()
    );
    assert_untouched(&v);
}

#[test]
fn a_failure_creating_the_temp_file_changes_nothing() {
    let v = fault_with_dest();
    v.fail_nth(Op::CreateTemp, 1, ErrorKind::AccessDenied);
    let o = CopyOptions {
        overwrite: true,
        ..CopyOptions::default()
    };
    let e = copy_between(
        &v,
        &p("src"),
        &v,
        &p("dst"),
        &o,
        &|_, _| {},
        &CancelToken::new(),
    )
    .unwrap_err();
    assert_eq!(e.kind(), ErrorKind::AccessDenied);
    assert_untouched(&v);
}

/// Fix B5: a read-only destination is overwritten only with `overwrite_readonly`, and the
/// attributes are restored when the retry fails too.
#[test]
fn read_only_destination_attribute_retry() {
    let mem = MemVfs::new();
    mem.add_file("src", b"new", 100)
        .add_file("dst", b"old", 50)
        .set_raw_attrs("dst", attr::READONLY | attr::HIDDEN);
    let (cancel, plain) = (
        CancelToken::new(),
        CopyOptions {
            overwrite: true,
            ..CopyOptions::default()
        },
    );
    let e = copy_between(
        &mem,
        &p("src"),
        &mem,
        &p("dst"),
        &plain,
        &|_, _| {},
        &cancel,
    )
    .unwrap_err();
    assert_eq!(e.kind(), ErrorKind::AccessDenied);
    assert_eq!(mem.read("dst").unwrap(), b"old");
    let retry = CopyOptions {
        overwrite_readonly: true,
        ..plain
    };
    copy_between(
        &mem,
        &p("src"),
        &mem,
        &p("dst"),
        &retry,
        &|_, _| {},
        &cancel,
    )
    .unwrap();
    assert_eq!(mem.read("dst").unwrap(), b"new");
    // retry that fails again: the attributes of the old destination come back
    let v = FaultVfs::new(MemVfs::new());
    v.inner()
        .add_file("src", b"new", 100)
        .add_file("dst", b"old", 50)
        .set_raw_attrs("dst", attr::READONLY | attr::HIDDEN);
    v.fail_nth(Op::Rename, 2, ErrorKind::Io); // first rename: AccessDenied (read-only); second: injected
    let e = copy_between(&v, &p("src"), &v, &p("dst"), &retry, &|_, _| {}, &cancel).unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Io);
    let attrs = v.inner().stat(&p("dst")).unwrap().unwrap().attrs;
    assert_eq!(
        attrs & (attr::READONLY | attr::HIDDEN),
        attr::READONLY | attr::HIDDEN,
        "attributes restored"
    );
    assert_eq!(v.inner().read("dst").unwrap(), b"old");
}

/// Fix B6: a corrupted copy fails verification and the destination stays as it was.
#[test]
fn verify_after_copy_catches_corruption() {
    // corrupt the temp file after it was written: the hook runs on the SetTimes call that
    // follows the streaming and flips a byte
    let v = fault_with_dest();
    let inner = v.inner().clone();
    v.on_nth(
        Op::SetTimes,
        1,
        Arc::new(move |tmp: &VPath| {
            let name = tmp.display();
            let mut w = inner.read(&name).unwrap();
            w[10] ^= 0xFF;
            inner.add_file(&name, &w, 0);
        }),
    );
    let o = CopyOptions {
        overwrite: true,
        verify: true,
        ..CopyOptions::default()
    };
    let e = copy_between(
        &v,
        &p("src"),
        &v,
        &p("dst"),
        &o,
        &|_, _| {},
        &CancelToken::new(),
    )
    .unwrap_err();
    assert!(e.to_string().contains("verification failed"), "{e}");
    assert_untouched(&v);
    // without verify the same corruption would go through unnoticed (shows the check matters)
}

#[test]
fn verify_passes_for_a_good_copy() {
    let v = fault_with_dest();
    let o = CopyOptions {
        overwrite: true,
        verify: true,
        ..CopyOptions::default()
    };
    copy_between(
        &v,
        &p("src"),
        &v,
        &p("dst"),
        &o,
        &|_, _| {},
        &CancelToken::new(),
    )
    .unwrap();
    assert_eq!(v.inner().read("dst").unwrap().len(), 3 * 1024 * 1024);
}

#[test]
fn a_destination_changed_between_plan_and_execute_is_visible_to_a_recheck() {
    // The sync engine re-stats before acting (plan.md §9.1); FaultVfs can change the file
    // behind its back between two stats.
    let v = FaultVfs::new(MemVfs::new());
    v.inner().add_file("f", b"v1", 10);
    let inner = v.inner().clone();
    v.on_nth(
        Op::Stat,
        2,
        Arc::new(move |_| {
            inner.add_file("f", b"changed by someone else", 99);
        }),
    );
    let planned = v.stat(&p("f")).unwrap().unwrap();
    let before_action = v.stat(&p("f")).unwrap().unwrap();
    assert_ne!(
        (planned.size, planned.mtime),
        (before_action.size, before_action.mtime)
    );
}

#[test]
fn deleting_a_link_never_touches_its_target() {
    let mem = MemVfs::new();
    mem.add_dir("target", 1)
        .add_file("target/keep.txt", b"keep", 1)
        .add_link("lnk", Kind::Dir);
    mem.delete(&p("lnk"), DeleteMode::Permanent).unwrap();
    assert_eq!(mem.read("target/keep.txt").unwrap(), b"keep");
    assert!(mem.stat(&p("lnk")).unwrap().is_none());
}

#[test]
fn links_are_not_copied_by_trees() {
    let mem = MemVfs::new();
    mem.add_dir("s", 1)
        .add_file("s/a", b"a", 1)
        .add_link("s/l", Kind::File);
    let r = copy_tree(
        &mem,
        &p("s"),
        &mem,
        &p("d"),
        &CopyOptions::default(),
        &CancelToken::new(),
    )
    .unwrap();
    assert_eq!((r.files, r.skipped_links.len()), (1, 1));
    assert!(mem.stat(&p("d/l")).unwrap().is_none());
}

#[test]
fn memvfs_rules_case_attrs_ids_and_recipes() {
    let m = MemVfs::new().case_insensitive();
    m.add_file("Readme.TXT", b"x", 5);
    assert!(
        m.stat(&p("README.txt")).unwrap().is_some(),
        "case-insensitive lookup"
    );
    assert!(!m.caps().case_sensitive && !m.dir_case_sensitive(&VPath::root()).unwrap());
    let ids = (
        m.file_id(&p("readme.txt")).unwrap().unwrap(),
        m.file_id(&p("README.TXT")).unwrap().unwrap(),
    );
    assert_eq!(ids.0, ids.1, "the same file has one id");
    let r = MemVfs::from_recipe(r#"{"caseSensitive":false,"entries":[
        {"path":"a/b.txt","data":"hi","mtime":7},{"path":"d","kind":"dir"},{"path":"a/big","size":4,"attrs":1}]}"#).unwrap();
    assert_eq!(r.read("a/b.txt").unwrap(), b"hi");
    assert_eq!(r.stat(&p("a/BIG")).unwrap().unwrap().size, 4);
    assert!(r.stat(&p("a/big")).unwrap().unwrap().is_readonly());
    assert!(MemVfs::from_recipe("nope").is_err());
}

#[test]
fn odd_names_round_trip_in_memvfs() {
    let m = MemVfs::new();
    for n in ["foo.", "foo ", "aux.c", "a:b", "A", "a"] {
        m.add_file(n, b"x", 1);
    }
    let mut e = Vec::new();
    m.list(&VPath::root(), &mut e).unwrap();
    let mut got: Vec<String> = e.iter().map(|e| e.name.to_string_lossy()).collect();
    got.sort();
    assert_eq!(
        got,
        ["A", "a", "a:b", "aux.c", "foo ", "foo."],
        "listed unchanged"
    );
}

#[test]
fn fault_counters_and_failing_the_nth_call() {
    let v = FaultVfs::new(MemVfs::new());
    v.fail_nth(Op::CreateDir, 2, ErrorKind::AccessDenied);
    v.create_dir(&p("a")).unwrap();
    assert_eq!(
        v.create_dir(&p("b")).unwrap_err().kind(),
        ErrorKind::AccessDenied
    );
    v.create_dir(&p("c")).unwrap();
    assert_eq!(v.count(Op::CreateDir), 3);
    let mut w = v.create_temp_sibling(&p("x")).unwrap().1;
    w.write_all(b"12345").unwrap();
    assert_eq!(v.total_bytes_written(), 5);
}

/// Review finding: copying a folder into itself must not recurse forever.
#[test]
fn copy_tree_refuses_a_destination_inside_the_source() {
    let mem = MemVfs::new();
    mem.add_dir("a", 1).add_file("a/f", b"x", 1);
    let e = copy_tree(
        &mem,
        &p("a"),
        &mem,
        &p("a/b"),
        &CopyOptions::default(),
        &CancelToken::new(),
    )
    .unwrap_err();
    assert_eq!(e.kind(), ErrorKind::InvalidArgument);
    assert!(
        mem.stat(&p("a/b")).unwrap().is_none(),
        "nothing was created"
    );
}

/// Review finding (plan.md §9.2): a source that changes while it is copied must not produce a
/// destination that looks up to date.
#[test]
fn a_source_modified_during_the_copy_is_detected() {
    let v = FaultVfs::new(MemVfs::new());
    v.inner()
        .add_file("src", b"original", 10)
        .add_file("dst", b"old", 5);
    let inner = v.inner().clone();
    // stat calls: 1 source, 2 destination, 3 re-check of the source after streaming
    v.on_nth(
        Op::Stat,
        3,
        Arc::new(move |_| {
            inner.add_file("src", b"original but longer now", 99);
        }),
    );
    let o = CopyOptions {
        overwrite: true,
        ..CopyOptions::default()
    };
    let e = copy_between(
        &v,
        &p("src"),
        &v,
        &p("dst"),
        &o,
        &|_, _| {},
        &CancelToken::new(),
    )
    .unwrap_err();
    assert_eq!(e.kind(), ErrorKind::ChangedSinceCompare);
    assert_eq!(v.inner().read("dst").unwrap(), b"old");
    assert!(v.inner().paths().iter().all(|p| !p.ends_with(".dctmp~")));
}

#[test]
fn local_std_deletes_links_without_touching_targets() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("target")).unwrap();
    std::fs::write(dir.path().join("target/keep"), b"k").unwrap();
    std::fs::create_dir(dir.path().join("outer")).unwrap();
    let link = dir.path().join("outer/lnk");
    #[cfg(unix)]
    let made = std::os::unix::fs::symlink(dir.path().join("target"), &link).is_ok();
    #[cfg(windows)]
    let made = std::os::windows::fs::symlink_dir(dir.path().join("target"), &link).is_ok();
    if !made {
        eprintln!("SKIPPED: cannot create symlinks here");
        return;
    }
    let v = LocalStd::new(dir.path());
    v.delete(&p("outer"), DeleteMode::Permanent).unwrap();
    assert!(
        dir.path().join("target/keep").exists(),
        "the link target must survive"
    );
    assert!(!dir.path().join("outer").exists());
}

/// Decision D16: a destination folder that is really a link is never entered; nothing is
/// written through it.
#[test]
fn copy_tree_refuses_a_destination_that_is_a_link() {
    let src = MemVfs::new();
    src.add_file("s/a.txt", b"x", 0);
    let dst = MemVfs::new();
    dst.add_link("d", Kind::Dir);
    let e = copy_tree(
        &src,
        &p("s"),
        &dst,
        &p("d"),
        &CopyOptions::default(),
        &CancelToken::new(),
    )
    .unwrap_err();
    assert_eq!(e.kind(), ErrorKind::AlreadyExists);
    assert_eq!(dst.paths(), ["d"], "nothing was written");
}

/// Decision D13: safe copy is the default.
#[test]
fn safe_copy_is_on_by_default() {
    let o = CopyOptions::default();
    assert!(o.safe_copy && !o.overwrite && !o.verify);
}
