//! Pinning tests for the plan-reviewer findings on core-sync (backlog E8-S9.3).

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use core_base::ErrorKind;
use core_base::ticks::TICKS_PER_SECOND as S;
use core_folder::{FolderRules, Status, Tree};
use core_sync::{
    Answer, ExecOverrides, FileOpOptions, Loc, Preset, QuestionKind, SyncRules, build_plan,
    move_items, plan_copy_items, plan_move_items, start_sync,
};
use core_vfs::{Kind, MemVfs, Op, VPath, Vfs};

fn p(s: &str) -> VPath {
    VPath::from_parts(s.split('/').filter(|x| !x.is_empty()))
}

fn at(v: &Arc<dyn Vfs>, path: &str) -> Loc {
    Loc {
        vfs: v.clone(),
        path: p(path),
    }
}

/// Finding 1 and 4: the §9.3 type-conflict flow on a case-insensitive Windows-like target, in
/// both directions, ends with a tree that has nothing left to do.
#[test]
fn type_conflicts_work_on_a_case_insensitive_target_and_leave_a_clean_tree() {
    // file on the left, folder on the right
    let (l, r) = (
        MemVfs::new().case_insensitive().windows_names(),
        MemVfs::new().case_insensitive().windows_names(),
    );
    l.add_file("x", b"file", T0);
    r.add_file("x/inner.txt", b"1", T0);
    let rc = r.clone();
    let c = shared(compare(arc(l), arc(r), FolderRules::default()));
    let plan = plan_for(&c.read(), Preset::MirrorToRight);
    assert_eq!(plan.summary.errors, 0, "{:?}", plan.issues);
    let res = execute(&plan, &c, ExecOverrides::default());
    assert_eq!((res.failed, res.skipped), (0, 0), "{res:?}");
    assert_eq!(rc.read("x").unwrap(), b"file");
    let g = c.read();
    let n = root_status(&g);
    assert_eq!(
        (
            n.diff,
            n.left_orphan,
            n.right_orphan,
            n.left_newer,
            n.right_newer,
            n.error
        ),
        (0, 0, 0, 0, 0, 0),
        "the tree has no phantom nodes: {n:?}"
    );
    drop(g);

    // folder on the left, file on the right
    let (l, r) = (
        MemVfs::new().case_insensitive().windows_names(),
        MemVfs::new().case_insensitive().windows_names(),
    );
    l.add_file("y/inner.txt", b"left folder", T0);
    r.add_file("y", b"right file", T0);
    let rc = r.clone();
    let c = shared(compare(arc(l), arc(r), FolderRules::default()));
    let plan = plan_for(&c.read(), Preset::MirrorToRight);
    assert_eq!(plan.summary.errors, 0, "{:?}", plan.issues);
    let res = execute(&plan, &c, ExecOverrides::default());
    assert_eq!((res.failed, res.skipped), (0, 0), "{res:?}");
    assert_eq!(rc.read("y/inner.txt").unwrap(), b"left folder");
    let n = root_status(&c.read());
    assert_eq!(
        (n.left_orphan, n.right_orphan, n.diff, n.error),
        (0, 0, 0, 0),
        "{n:?}"
    );
}

/// Finding 3 (D43): a move with `overwrite`, an existing destination of the same size, and a
/// failing copy must not delete the source.
#[test]
fn a_cross_volume_move_over_an_equal_size_file_never_deletes_a_source_whose_copy_failed() {
    let src = MemVfs::new();
    src.add_file("f.txt", b"AAAA", T0);
    let dst = MemVfs::new();
    dst.add_file("f.txt", b"BBBB", T0 - 100 * S);
    let df = fault(dst.clone());
    df.fail_nth(Op::CreateTemp, 1, ErrorKind::SharingViolation);
    let (sv, dv): (Arc<dyn Vfs>, Arc<dyn Vfs>) = (arc(src.clone()), df);
    // a one-shot sharing violation would be retried silently (D57): this test is about a copy that
    // does fail, so the executor's own retries are off
    let o = FileOpOptions {
        overwrite: true,
        exec: core_sync::ExecOptions {
            copy_retries: 0,
            ..core_sync::ExecOptions::default()
        },
    };
    let job = move_items(&[at(&sv, "f.txt")], &at(&dv, ""), o, None).unwrap();
    let res = run_with_answers(&job, Answer::Skip);
    assert_eq!(
        src.read("f.txt").unwrap(),
        b"AAAA",
        "the source survives: {res:?}"
    );
    assert_eq!(dst.read("f.txt").unwrap(), b"BBBB");
    assert!(res.skipped >= 1);
}

/// Finding 2 (§9.7, D48): a folder moved or copied into itself is refused, also when the path
/// differs only in case on a case-insensitive file system.
#[test]
fn file_operations_refuse_to_go_into_themselves() {
    let m = MemVfs::new().case_insensitive();
    m.add_file("proj/a.txt", b"1", T0).add_dir("proj/sub", 0);
    let v = arc(m.clone());
    let o = FileOpOptions::default();
    for dst in ["proj/sub", "PROJ/SUB", "proj"] {
        let e = plan_move_items(&[at(&v, "proj")], &at(&v, dst), &o).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::InvalidArgument, "move into {dst}");
        let e = plan_copy_items(&[at(&v, "proj")], &at(&v, dst), &o).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::InvalidArgument, "copy into {dst}");
    }
    // a file copied onto itself with overwrite
    let ov = FileOpOptions {
        overwrite: true,
        ..FileOpOptions::default()
    };
    let e = plan_copy_items(&[at(&v, "proj/a.txt")], &at(&v, "proj"), &ov).unwrap_err();
    assert_eq!(e.kind(), ErrorKind::InvalidArgument);
}

/// Finding 17 (D46): a plan belongs to the compare it was built from.
#[test]
fn a_plan_cannot_run_against_another_compare() {
    let build = || {
        let (l, r) = (MemVfs::new(), MemVfs::new());
        l.add_file("f", b"1", T0);
        shared(compare(arc(l), arc(r), FolderRules::default()))
    };
    let (a, b) = (build(), build());
    let plan = plan_for(&a.read(), Preset::UpdateRight);
    let e = start_sync(&plan, &b, ExecOverrides::default(), None).unwrap_err();
    assert_eq!(e.kind(), ErrorKind::InvalidArgument);
    assert!(start_sync(&plan, &a, ExecOverrides::default(), None).is_ok());
}

/// Finding 7 (D46): the rules of the plan reach the executor: with the Recycle Bin on, deletes
/// ask for it (MemVfs has none, so the action fails and the user is asked).
#[test]
fn the_plans_rules_decide_how_deletes_are_done() {
    let (l, r) = (MemVfs::new(), MemVfs::new());
    l.add_file("keep", b"1", T0);
    r.add_file("keep", b"1", T0).add_file("old.txt", b"x", T0);
    let rc = r.clone();
    let c = shared(compare(arc(l), arc(r), FolderRules::default()));
    let rules = SyncRules {
        use_recycle_bin: true,
        ..SyncRules::from_preset(Preset::MirrorToRight)
    };
    let plan = build_plan(&c.read(), &rules, None).unwrap();
    let job = start_sync(&plan, &c, ExecOverrides::default(), None).unwrap();
    let q = wait_question(&job);
    assert_eq!(q.kind, QuestionKind::Failed);
    assert!(
        q.message.to_lowercase().contains("recycle"),
        "{}",
        q.message
    );
    job.answer(q.id, Answer::Skip).unwrap();
    job.wait();
    assert!(rc.read("old.txt").is_some(), "no silent permanent delete");
}

/// Finding 6: `is_finished` and `progress` never block while another thread is in `wait`, and
/// every waiter gets the result.
#[test]
fn status_calls_do_not_block_behind_wait() {
    let (l, r) = (MemVfs::new(), MemVfs::new());
    for i in 0..20 {
        l.add_file(&format!("f{i}"), b"x", T0);
    }
    let rf = fault(r);
    rf.delay_each_op(Duration::from_millis(15));
    let c = shared(compare(arc(l), rf, FolderRules::default()));
    let plan = plan_for(&c.read(), Preset::UpdateRight);
    let job = Arc::new(start_sync(&plan, &c, ExecOverrides::default(), None).unwrap());
    let (j1, j2) = (job.clone(), job.clone());
    let w1 = std::thread::spawn(move || j1.wait());
    let w2 = std::thread::spawn(move || j2.wait());
    let started = Instant::now();
    let mut worst = Duration::ZERO;
    while !job.is_finished() {
        let t = Instant::now();
        let _ = job.progress();
        let _ = job.question();
        worst = worst.max(t.elapsed());
        std::thread::sleep(Duration::from_millis(2));
        assert!(started.elapsed() < Duration::from_secs(30));
    }
    assert!(
        worst < Duration::from_millis(100),
        "a status call blocked for {worst:?}"
    );
    let (a, b) = (w1.join().unwrap(), w2.join().unwrap());
    assert_eq!(a, b);
    assert_eq!(a.done, 20);
}

/// Findings 10 and 11: a cancel while a question is open is a cancel, not an abort; a late
/// answer for an old question is refused.
#[test]
fn cancel_during_a_question_and_stale_answers() {
    let (l, r) = (MemVfs::new(), MemVfs::new());
    l.add_file("a", b"1", T0).add_file("b", b"2", T0);
    let rc = r.clone();
    let c = shared(compare(arc(l), arc(r), FolderRules::default()));
    let plan = plan_for(&c.read(), Preset::UpdateRight);
    rc.add_file("a", b"someone was first", T0);
    rc.add_file("b", b"also them", T0);
    let job = start_sync(&plan, &c, ExecOverrides::default(), None).unwrap();
    let q1 = wait_question(&job);
    job.answer(q1.id, Answer::Skip).unwrap();
    // the next question has another id; the old id is refused
    let t = Instant::now();
    let q2 = loop {
        if let Some(q) = job.question() {
            if q.id != q1.id {
                break q;
            }
        }
        assert!(t.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(2));
    };
    assert_eq!(
        job.answer(q1.id, Answer::Abort).unwrap_err().kind(),
        ErrorKind::NotFound
    );
    assert!(
        job.question().is_some(),
        "the stale answer did not take the question"
    );
    assert_ne!(q1.id, q2.id);
    job.cancel();
    let res = job.wait();
    assert!(res.cancelled && !res.aborted, "{res:?}");
    assert!(res.errors.is_empty());
}

/// Finding 9: a folder time is set only on folders this job created.
#[test]
fn folder_time_is_not_set_on_a_folder_the_job_did_not_create() {
    let (l, r) = (MemVfs::new(), MemVfs::new());
    l.add_dir("d", T0 + 40 * S);
    l.add_file("d/f.txt", b"1", T0);
    let rc = r.clone();
    let c = shared(compare(arc(l), arc(r), FolderRules::default()));
    let plan = plan_for(&c.read(), Preset::UpdateRight);
    // someone creates the folder before the job does
    rc.add_dir("d", T0 + 3 * S);
    let res = execute(&plan, &c, ExecOverrides::default());
    assert_eq!(res.failed + res.skipped, 0, "{res:?}");
    assert_eq!(rc.read("d/f.txt").unwrap(), b"1");
    assert_eq!(
        rc.stat(&p("d")).unwrap().unwrap().mtime,
        T0 + 3 * S,
        "the existing folder keeps its own time"
    );
}

/// Finding 13: a log that was asked for and cannot be written is reported.
#[test]
fn an_unwritable_log_folder_is_reported_in_the_result() {
    let (l, r) = (MemVfs::new(), MemVfs::new());
    l.add_file("f", b"1", T0);
    let c = shared(compare(arc(l), arc(r), FolderRules::default()));
    let plan = plan_for(&c.read(), Preset::UpdateRight);
    let dir = tempfile::tempdir().unwrap();
    let not_a_dir = dir.path().join("file.txt");
    std::fs::write(&not_a_dir, b"x").unwrap();
    let res = execute(
        &plan,
        &c,
        ExecOverrides {
            log_dir: Some(not_a_dir),
            // decision D66: by default a log that cannot be created stops the job; this test is
            // about the caller that chose to run without the guarantee
            log_required: Some(false),
            ..ExecOverrides::default()
        },
    );
    assert_eq!(res.done, 1, "the sync itself still runs");
    assert!(
        res.errors
            .iter()
            .any(|e| e.message.contains("log could not be created")),
        "{res:?}"
    );
}

/// A link inside an orphan folder: it is never touched, and the folder stays.
#[test]
fn a_link_inside_an_orphan_folder_is_kept_with_its_folder() {
    let (l, r) = (MemVfs::new(), MemVfs::new());
    l.add_file("same", b"1", T0);
    r.add_file("same", b"1", T0)
        .add_file("orphan/file.txt", b"x", T0)
        .add_link("orphan/lnk", Kind::Dir);
    let rc = r.clone();
    let c = shared(compare(arc(l), arc(r), FolderRules::default()));
    let plan = plan_for(&c.read(), Preset::MirrorToRight);
    assert_eq!(plan.summary.skipped_links, 1);
    let res = execute(&plan, &c, ExecOverrides::default());
    assert_eq!(res.kept, 1);
    assert!(rc.read("orphan/file.txt").is_none());
    assert!(
        rc.paths().iter().any(|x| x == "orphan/lnk"),
        "{:?}",
        rc.paths()
    );
}

/// §9.1 for deletes: an item that changed after the compare is asked about and kept.
#[test]
fn a_file_changed_after_the_compare_is_not_deleted() {
    let (l, r) = (MemVfs::new(), MemVfs::new());
    l.add_file("same", b"1", T0);
    r.add_file("same", b"1", T0)
        .add_file("old.txt", b"seen by the user", T0);
    let rc = r.clone();
    let c = shared(compare(arc(l), arc(r), FolderRules::default()));
    let plan = plan_for(&c.read(), Preset::MirrorToRight);
    rc.add_file(
        "old.txt",
        b"edited afterwards, so not what the user saw",
        T0 + 9 * S,
    );
    let job = start_sync(&plan, &c, ExecOverrides::default(), None).unwrap();
    let q = wait_question(&job);
    assert_eq!(q.kind, QuestionKind::ChangedSinceCompare);
    job.answer(q.id, Answer::Skip).unwrap();
    let res = job.wait();
    assert_eq!(res.skipped, 1);
    assert!(rc.read("old.txt").is_some());
    let _ = (Status::Same, Tree::ROOT);
}

/// Reviewer finding (E11-S6): logs of jobs that start in the same second, with the same name, in
/// one folder (even from two processes) never overwrite each other.
#[test]
fn logs_with_the_same_name_in_one_second_are_separate_files() {
    let dir = tempfile::tempdir().unwrap();
    let mut logs = Vec::new();
    for _ in 0..20 {
        logs.push(core_sync::log::JobLog::create(dir.path(), "same", 1).unwrap());
    }
    let n = std::fs::read_dir(dir.path()).unwrap().count();
    assert_eq!(n, 20, "every job has its own log file");
}
