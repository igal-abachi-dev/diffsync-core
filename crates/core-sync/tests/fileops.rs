//! File operations and the temp-file sweep (backlog E8-S7, S8; plan.md §8.4, §9.2).

mod common;

use std::sync::Arc;

use common::*;
use core_base::{CancelToken, ErrorKind};
use core_sync::{
    Answer, FileOpOptions, Loc, QuestionKind, StepKind, copy_items, delete_items,
    maintenance_sweep, move_items, plan_copy_items, plan_delete_items, plan_move_items,
};
use core_vfs::{Kind, MemVfs, Op, VPath, Vfs, temp_sibling_name};

fn p(s: &str) -> VPath {
    VPath::from_parts(s.split('/').filter(|x| !x.is_empty()))
}

fn at(v: &Arc<dyn Vfs>, path: &str) -> Loc {
    Loc {
        vfs: v.clone(),
        path: p(path),
    }
}

fn tree() -> MemVfs {
    let m = MemVfs::new();
    m.add_dir("src/dir", T0 + 3 * S);
    m.add_file("src/a.txt", b"aaa", T0)
        .add_file("src/dir/b.txt", b"bb", T0)
        .add_file("src/dir/deep/c.txt", b"c", T0)
        .add_dir("dst", 0);
    m
}

use core_base::ticks::TICKS_PER_SECOND as S;

#[test]
fn copy_items_copies_a_file_and_a_whole_folder() {
    let m = tree();
    let v = arc(m.clone());
    let job = copy_items(
        &[at(&v, "src/a.txt"), at(&v, "src/dir")],
        &at(&v, "dst"),
        FileOpOptions::default(),
        None,
    )
    .unwrap();
    let res = job.wait();
    assert_eq!((res.failed, res.skipped), (0, 0), "{res:?}");
    assert_eq!(m.read("dst/a.txt").unwrap(), b"aaa");
    assert_eq!(m.read("dst/dir/b.txt").unwrap(), b"bb");
    assert_eq!(m.read("dst/dir/deep/c.txt").unwrap(), b"c");
    assert!(m.read("src/a.txt").is_some(), "copy leaves the source");
    assert_eq!(
        m.stat(&p("dst/dir")).unwrap().unwrap().mtime,
        T0 + 3 * S,
        "folder time restored"
    );
    assert_eq!(res.bytes_copied, 6);
}

#[test]
fn copy_refuses_existing_destinations_unless_overwrite_and_revalidates() {
    let m = tree();
    m.add_file("dst/a.txt", b"old", T0);
    let v = arc(m.clone());
    let e = plan_copy_items(
        &[at(&v, "src/a.txt")],
        &at(&v, "dst"),
        &FileOpOptions::default(),
    )
    .unwrap_err();
    assert_eq!(e.kind(), ErrorKind::AlreadyExists);
    let o = FileOpOptions {
        overwrite: true,
        ..FileOpOptions::default()
    };
    let steps = plan_copy_items(&[at(&v, "src/a.txt")], &at(&v, "dst"), &o).unwrap();
    assert!(steps[0].expect_dst.is_some());
    // the destination changes after planning: the job asks (§9.1)
    let job = copy_items(&[at(&v, "src/a.txt")], &at(&v, "dst"), o, None).unwrap();
    // (the job may already have run; plan again and change before starting)
    job.wait();
    m.add_file("dst/a.txt", b"edited later", T0 + 5 * S);
    let steps = plan_copy_items(
        &[at(&v, "src/a.txt")],
        &at(&v, "dst"),
        &FileOpOptions {
            overwrite: true,
            ..FileOpOptions::default()
        },
    )
    .unwrap();
    m.add_file("dst/a.txt", b"edited yet again, different size", T0 + 9 * S);
    let job = core_sync::exec::Job::spawn(
        steps,
        core_sync::ExecOptions::default(),
        Box::new(|_, _| {}),
        None,
    );
    let q = wait_question(&job);
    assert_eq!(q.kind, QuestionKind::ChangedSinceCompare);
    job.answer(q.id, Answer::Skip).unwrap();
    assert_eq!(job.wait().skipped, 1);
    assert_eq!(
        m.read("dst/a.txt").unwrap(),
        b"edited yet again, different size"
    );
}

#[test]
fn copying_a_folder_into_itself_and_bad_names_are_refused_before_anything_is_written() {
    let m = tree();
    let v = arc(m.clone());
    let e = plan_copy_items(
        &[at(&v, "src")],
        &at(&v, "src/dir"),
        &FileOpOptions::default(),
    )
    .unwrap_err();
    assert_eq!(e.kind(), ErrorKind::InvalidArgument);
    let w = MemVfs::new().windows_names();
    let wv = arc(w.clone());
    m.add_file("src/q?.txt", b"1", T0);
    let e = plan_copy_items(
        &[at(&v, "src/q?.txt")],
        &at(&wv, ""),
        &FileOpOptions::default(),
    )
    .unwrap_err();
    assert_eq!(e.kind(), ErrorKind::InvalidName);
    assert!(w.paths().is_empty());
}

#[test]
fn move_within_a_volume_is_a_rename_and_across_volumes_copies_then_deletes() {
    let m = tree();
    let v = arc(m.clone());
    let steps = plan_move_items(
        &[at(&v, "src/dir")],
        &at(&v, "dst"),
        &FileOpOptions::default(),
    )
    .unwrap();
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].kind, StepKind::MoveFile);
    let res = move_items(
        &[at(&v, "src/dir")],
        &at(&v, "dst"),
        FileOpOptions::default(),
        None,
    )
    .unwrap()
    .wait();
    assert_eq!(res.failed, 0);
    assert!(m.read("dst/dir/deep/c.txt").is_some() && m.read("src/dir/b.txt").is_none());

    let other = MemVfs::new();
    let ov = arc(other.clone());
    let res = move_items(
        &[at(&v, "src/a.txt")],
        &at(&ov, ""),
        FileOpOptions::default(),
        None,
    )
    .unwrap()
    .wait();
    assert_eq!((res.failed, res.skipped), (0, 0), "{res:?}");
    assert_eq!(other.read("a.txt").unwrap(), b"aaa");
    assert!(
        m.read("src/a.txt").is_none(),
        "the source is deleted only after the copy"
    );
}

/// A failed copy during a move never deletes the source.
#[test]
fn a_failed_copy_in_a_move_keeps_the_source() {
    let m = tree();
    let v = arc(m.clone());
    let other = MemVfs::new();
    let of = fault(other.clone());
    of.fail_nth(Op::CreateTemp, 1, ErrorKind::Io);
    let ov: Arc<dyn Vfs> = of;
    let job = move_items(
        &[at(&v, "src/a.txt")],
        &at(&ov, ""),
        FileOpOptions::default(),
        None,
    )
    .unwrap();
    let res = run_with_answers(&job, Answer::Skip);
    // the copy failed and was skipped; the delete waits for its copy, so it fails too
    assert_eq!(res.skipped, 2, "{res:?}");
    assert_eq!(
        m.read("src/a.txt").unwrap(),
        b"aaa",
        "the source is still there"
    );
}

#[test]
fn delete_items_deletes_post_order_and_stays_inside_what_it_listed() {
    let m = tree();
    let v = arc(m.clone());
    let steps = plan_delete_items(&[at(&v, "src/dir")]).unwrap();
    let order: Vec<String> = steps.iter().map(|s| s.dst.path.display()).collect();
    assert_eq!(
        order,
        [
            "src/dir/b.txt",
            "src/dir/deep/c.txt",
            "src/dir/deep",
            "src/dir"
        ]
    );
    // something appears in the folder after planning: the folder is kept, not wiped
    m.add_file("src/dir/deep/late.txt", b"new", T0);
    let job = core_sync::exec::Job::spawn(
        steps,
        core_sync::ExecOptions::default(),
        Box::new(|_, _| {}),
        None,
    );
    let res = job.wait();
    assert_eq!(res.kept, 2, "deep and dir keep standing: {res:?}");
    assert_eq!(m.read("src/dir/deep/late.txt").unwrap(), b"new");
    assert!(m.read("src/dir/b.txt").is_none());
    let res = delete_items(&[at(&v, "src/a.txt")], FileOpOptions::default(), None)
        .unwrap()
        .wait();
    assert_eq!(res.done, 1);
    assert_eq!(
        plan_delete_items(&[at(&v, "")]).unwrap_err().kind(),
        ErrorKind::InvalidArgument
    );
}

#[test]
fn links_are_not_copied_or_followed_by_file_operations() {
    let m = tree();
    m.add_link("src/lnk", Kind::Dir);
    let v = arc(m.clone());
    let steps =
        plan_copy_items(&[at(&v, "src")], &at(&v, "dst"), &FileOpOptions::default()).unwrap();
    assert!(steps.iter().all(|s| !s.dst.path.display().contains("lnk")));
}

#[test]
fn sweep_finds_leftover_temp_files_and_deletes_only_on_request() {
    let m = MemVfs::new();
    let tmp = |dir: &str, name: &str| {
        let n = temp_sibling_name(&core_vfs::Name::new(name));
        format!("{dir}{}", n.to_string_lossy())
    };
    m.add_file(&tmp("", "a.txt"), b"1234", 0)
        .add_file(&tmp("sub/", "b.txt"), b"12", 0)
        .add_file("sub/real.txt", b"keep", 0);
    let v: Arc<dyn Vfs> = Arc::new(m.clone());
    let r = maintenance_sweep(&*v, &VPath::root(), false, &CancelToken::new()).unwrap();
    assert_eq!(r.found.len(), 2);
    assert_eq!(r.found.iter().map(|f| f.1).sum::<u64>(), 6);
    assert_eq!(r.deleted, 0);
    assert_eq!(
        temp_files(&m).len(),
        2,
        "a report-only sweep deletes nothing"
    );
    let r = maintenance_sweep(&*v, &VPath::root(), true, &CancelToken::new()).unwrap();
    assert_eq!((r.found.len(), r.deleted), (2, 2));
    assert!(temp_files(&m).is_empty());
    assert!(m.read("sub/real.txt").is_some());
    let cancelled = CancelToken::new();
    cancelled.cancel();
    assert!(
        maintenance_sweep(&*v, &VPath::root(), false, &cancelled)
            .unwrap_err()
            .is_cancelled()
    );
}

#[test]
fn rename_gives_a_file_or_a_folder_another_name_and_never_replaces() {
    use core_sync::{plan_rename_item, rename_item};
    let m = tree();
    let v = arc(m.clone());
    let job = rename_item(
        &at(&v, "src/a.txt"),
        "renamed.txt",
        FileOpOptions::default(),
        None,
    )
    .unwrap();
    let r = job.wait();
    assert_eq!((r.failed, r.done), (0, 1), "{r:?}");
    assert!(m.read("src/a.txt").is_none());
    assert_eq!(m.read("src/renamed.txt").unwrap(), b"aaa");
    // a folder keeps what is inside it
    rename_item(&at(&v, "src/dir"), "folder", FileOpOptions::default(), None)
        .unwrap()
        .wait();
    assert_eq!(m.read("src/folder/b.txt").unwrap(), b"bb");
    assert_eq!(m.read("src/folder/deep/c.txt").unwrap(), b"c");
    // a name that exists, the same name, a path, and invalid names are refused before anything runs
    m.add_file("src/other.txt", b"o", T0);
    for (item, name, kind) in [
        ("src/renamed.txt", "other.txt", ErrorKind::AlreadyExists),
        ("src/renamed.txt", "renamed.txt", ErrorKind::InvalidArgument),
        ("src/renamed.txt", "a/b.txt", ErrorKind::InvalidName),
        ("src/renamed.txt", "", ErrorKind::InvalidName),
        ("src/renamed.txt", "..", ErrorKind::InvalidName),
    ] {
        let e = plan_rename_item(&at(&v, item), name).unwrap_err();
        assert_eq!(e.kind(), kind, "{name}: {e}");
    }
    assert!(plan_rename_item(&at(&v, "src/missing.txt"), "x").is_err());
    assert!(plan_rename_item(&at(&v, ""), "x").is_err());
    // a case-only change is a rename on a case-insensitive file system, a sibling clash is not
    let ci = arc(MemVfs::new().case_insensitive());
    let _ = ci;
    let m2 = MemVfs::new().case_insensitive();
    m2.add_file("File.txt", b"1", T0)
        .add_file("other.txt", b"2", T0);
    let v2 = arc(m2.clone());
    assert!(plan_rename_item(&at(&v2, "File.txt"), "FILE.TXT").is_ok());
    let e = plan_rename_item(&at(&v2, "File.txt"), "OTHER.txt").unwrap_err();
    assert_eq!(e.kind(), ErrorKind::AlreadyExists);
}

/// Decision D64: a move to another volume copies what it can, and a link was never copied, so it
/// is never deleted: it stays, and its folder stays with it. A link moved on its own is refused.
#[test]
fn a_cross_volume_move_never_deletes_a_link_it_did_not_copy() {
    let (a, b) = (MemVfs::new(), MemVfs::new());
    a.add_file("src/file.txt", b"data", T0)
        .add_link("src/lnk", Kind::File)
        .add_dir("dst0", 0);
    b.add_dir("dst", 0).add_link("dst/src", Kind::File); // an unrelated link of the same name
    let (va, vb) = (arc(a.clone()), arc(b.clone()));
    let steps = core_sync::plan_move_items(
        &[at(&va, "src")],
        &at(&vb, "dst"),
        &FileOpOptions::default(),
    );
    // the destination name is taken by a link: refused before anything happens
    assert!(steps.is_err());
    vb.delete(&p("dst/src"), core_vfs::DeleteMode::Permanent)
        .unwrap();
    let steps = core_sync::plan_move_items(
        &[at(&va, "src")],
        &at(&vb, "dst"),
        &FileOpOptions::default(),
    )
    .unwrap();
    assert!(
        steps
            .iter()
            .filter(|s| s.kind == StepKind::Delete)
            .all(|s| !s.dst.path.display().contains("lnk")),
        "a delete step for the link: {:?}",
        steps
            .iter()
            .map(|s| s.dst.path.display())
            .collect::<Vec<_>>()
    );
    let res = core_sync::move_items(
        &[at(&va, "src")],
        &at(&vb, "dst"),
        FileOpOptions::default(),
        None,
    )
    .unwrap()
    .wait();
    assert_eq!(res.failed, 0, "{res:?}");
    assert_eq!(b.read("dst/src/file.txt").unwrap(), b"data");
    assert!(a.read("src/file.txt").is_none(), "the copied file is moved");
    assert!(
        a.paths().iter().any(|p| p.contains("lnk")),
        "the link stays: {:?}",
        a.paths()
    );
    // a link on its own cannot cross a volume
    let e = core_sync::plan_move_items(
        &[at(&va, "src/lnk")],
        &at(&vb, "dst"),
        &FileOpOptions::default(),
    )
    .unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Unsupported);
}

/// A case-only rename on a case-insensitive file system really runs (it goes through a temporary
/// name; "the destination is absent" cannot hold when the new name is the same item).
#[test]
fn a_case_only_rename_is_executed() {
    use core_sync::rename_item;
    let m = MemVfs::new().case_insensitive();
    m.add_file("File.txt", b"1", T0);
    let v = arc(m.clone());
    let res = rename_item(
        &at(&v, "File.txt"),
        "FILE.TXT",
        FileOpOptions::default(),
        None,
    )
    .unwrap()
    .wait();
    assert_eq!((res.failed, res.done), (0, 2), "{res:?}");
    assert_eq!(m.paths(), vec!["FILE.TXT".to_string()]);
    assert_eq!(m.read("file.txt").unwrap(), b"1");
}
