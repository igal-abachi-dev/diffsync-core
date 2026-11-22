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
