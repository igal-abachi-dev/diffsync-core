//! Execution behaviour (backlog E8-S4, S5, S6, S9.1; plan.md §8.3, §9.1-§9.4).

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use common::*;
use core_base::ErrorKind;
use core_base::ticks::TICKS_PER_SECOND as S;
use core_folder::{FolderRules, Status, Tree};
use core_sync::{Answer, ExecOverrides, JobEvent, Preset, QuestionKind, start_sync};
use core_vfs::{MemVfs, Op, Vfs};

fn mirror_tree() -> (MemVfs, MemVfs) {
    let (l, r) = (MemVfs::new(), MemVfs::new());
    l.add_dir("dir", T0 + 5 * S);
    l.add_file("keep.txt", b"keep", T0)
        .add_file("new.txt", b"brand new", T0)
        .add_file("changed.txt", b"left version", T0 + 50 * S)
        .add_file("dir/a.txt", b"aaa", T0)
        .add_file("dir/sub/b.txt", b"bb", T0);
    r.add_file("keep.txt", b"keep", T0)
        .add_file("changed.txt", b"old right", T0)
        .add_file("extra.txt", b"gone soon", T0)
        .add_file("olddir/x.txt", b"1", T0)
        .add_file("olddir/deep/y.txt", b"2", T0);
    (l, r)
}

#[test]
fn mirror_to_right_makes_the_right_side_equal_and_the_tree_follows() {
    let (l, r) = mirror_tree();
    let (lc, rc) = (l.clone(), r.clone());
    let c = shared(compare(arc(l), arc(r), FolderRules::default()));
    let plan = plan_for(&c.read(), Preset::MirrorToRight);
    let res = execute(&plan, &c, ExecOverrides::default());
    assert_eq!(
        (res.failed, res.skipped, res.aborted, res.cancelled),
        (0, 0, false, false),
        "{res:?}"
    );
    assert!(res.done >= 9);
    for p in [
        "keep.txt",
        "new.txt",
        "changed.txt",
        "dir/a.txt",
        "dir/sub/b.txt",
    ] {
        assert_eq!(rc.read(p), lc.read(p), "{p}");
    }
    assert!(rc.read("extra.txt").is_none());
    assert!(
        !rc.paths().iter().any(|p| p.starts_with("olddir")),
        "{:?}",
        rc.paths()
    );
    assert!(temp_files(&rc).is_empty());
    // the compare tree was refreshed after each action: nothing differs any more
    let g = c.read();
    let counters = g.tree.node(Tree::ROOT).counters;
    assert_eq!(
        (
            counters.diff,
            counters.left_newer,
            counters.right_newer,
            counters.left_orphan,
            counters.right_orphan,
            counters.error
        ),
        (0, 0, 0, 0, 0, 0),
        "{counters:?}"
    );
    assert_eq!(status(&g, "changed.txt"), Status::Same);
    assert_eq!(res.bytes_copied, plan.summary.bytes_to_right);
    // folder times are restored after the contents (decision of §8.3 step 5)
    assert_eq!(
        rc.stat(&core_vfs::VPath::from_parts(["dir"]))
            .unwrap()
            .unwrap()
            .mtime,
        T0 + 5 * S
    );
}

#[test]
fn update_both_copies_each_way_and_leaves_prompts_alone() {
    let (l, r) = (MemVfs::new(), MemVfs::new());
    l.add_file("lnew.txt", b"new on left", T0 + 9 * S)
        .add_file("conflict.txt", b"L", T0);
    r.add_file("lnew.txt", b"old", T0)
        .add_file("ronly.txt", b"right only", T0)
        .add_file("conflict.txt", b"RR", T0);
    let (lc, rc) = (l.clone(), r.clone());
    let c = shared(compare(arc(l), arc(r), FolderRules::default()));
    let plan = plan_for(&c.read(), Preset::UpdateBoth);
    let res = execute(&plan, &c, ExecOverrides::default());
    assert_eq!(res.failed + res.skipped, 0);
    assert_eq!(rc.read("lnew.txt").unwrap(), b"new on left");
    assert_eq!(lc.read("ronly.txt").unwrap(), b"right only");
    assert_eq!(
        (
            lc.read("conflict.txt").unwrap(),
            rc.read("conflict.txt").unwrap()
        ),
        (b"L".to_vec(), b"RR".to_vec())
    );
}

/// §9.3: the type conflict, executed.
#[test]
fn mirror_replaces_a_folder_by_a_file_and_back() {
    let (l, r) = (MemVfs::new(), MemVfs::new());
    l.add_file("x", b"i am a file", T0)
        .add_file("y/inner.txt", b"folder on left", T0);
    r.add_file("x/inner.txt", b"1", T0)
        .add_file("x/deeper/z.txt", b"2", T0)
        .add_file("y", b"file on right", T0);
    let (lc, rc) = (l.clone(), r.clone());
    let c = shared(compare(arc(l), arc(r), FolderRules::default()));
    let plan = plan_for(&c.read(), Preset::MirrorToRight);
    let res = execute(&plan, &c, ExecOverrides::default());
    assert_eq!((res.failed, res.skipped), (0, 0), "{res:?}");
    assert_eq!(rc.read("x").unwrap(), b"i am a file");
    assert_eq!(rc.read("y/inner.txt").unwrap(), b"folder on left");
    assert_eq!(rc.paths().len(), lc.paths().len(), "{:?}", rc.paths());
}

/// §9.4 golden: Mirror with `*.log` excluded keeps `a/x.log` and folder `a`.
#[test]
fn filtered_folder_delete_keeps_what_was_never_compared() {
    let (l, r) = (MemVfs::new(), MemVfs::new());
    l.add_file("same.txt", b"s", T0);
    r.add_file("same.txt", b"s", T0)
        .add_file("a/y.txt", b"delete me", T0)
        .add_file("a/x.log", b"never compared", T0)
        .add_file("b/z.txt", b"all of b goes", T0);
    let rc = r.clone();
    let rules = FolderRules {
        exclude_files: vec!["*.log".into()],
        ..FolderRules::default()
    };
    let c = shared(compare(arc(l), arc(r), rules));
    let plan = plan_for(&c.read(), Preset::MirrorToRight);
    assert!(plan.filters_active);
    let res = execute(&plan, &c, ExecOverrides::default());
    assert_eq!((res.failed, res.skipped), (0, 0), "{res:?}");
    assert_eq!(rc.read("a/x.log").unwrap(), b"never compared");
    assert!(rc.read("a/y.txt").is_none(), "the compared file is deleted");
    assert!(rc.paths().contains(&"a".to_string()), "folder a stays");
    assert!(
        !rc.paths().iter().any(|p| p.starts_with('b')),
        "folder b is empty and goes"
    );
    assert_eq!(res.kept, 1, "one folder reported as kept");
}

/// Without filters a folder that gained an item after the compare is kept too (§9.1): nothing the
/// user did not see is deleted.
#[test]
fn a_folder_that_gained_a_file_after_the_compare_is_kept() {
    let (l, r) = (MemVfs::new(), MemVfs::new());
    r.add_file("d/old.txt", b"1", T0);
    let rc = r.clone();
    let c = shared(compare(arc(l), arc(r), FolderRules::default()));
    let plan = plan_for(&c.read(), Preset::MirrorToRight);
    rc.add_file("d/surprise.txt", b"new", T0);
    let res = execute(&plan, &c, ExecOverrides::default());
    assert_eq!(res.kept, 1);
    assert_eq!(rc.read("d/surprise.txt").unwrap(), b"new");
    assert!(rc.read("d/old.txt").is_none());
}

/// §9.1: a destination that changed after the compare stops the action with a question.
#[test]
fn destination_changed_since_the_compare_asks_and_skip_leaves_it_untouched() {
    let (l, r) = (MemVfs::new(), MemVfs::new());
    l.add_file("f.txt", b"left", T0 + 10 * S);
    r.add_file("f.txt", b"right", T0);
    let rc = r.clone();
    let c = shared(compare(arc(l), arc(r), FolderRules::default()));
    let plan = plan_for(&c.read(), Preset::UpdateRight);
    // someone edits the destination between compare and execute
    rc.add_file("f.txt", b"edited meanwhile, longer", T0 + 3 * S);
    let job = start_sync(&plan, &c, ExecOverrides::default(), None).unwrap();
    let q = wait_question(&job);
    assert_eq!(q.kind, QuestionKind::ChangedSinceCompare);
    assert!(q.message.contains("destination changed"), "{}", q.message);
    job.answer(q.id, Answer::Skip).unwrap();
    let res = job.wait();
    assert_eq!((res.skipped, res.done), (1, 0));
    assert_eq!(rc.read("f.txt").unwrap(), b"edited meanwhile, longer");
}

#[test]
fn retry_after_the_destination_is_restored_goes_through() {
    let (l, r) = (MemVfs::new(), MemVfs::new());
    l.add_file("f.txt", b"left", T0 + 10 * S);
    r.add_file("f.txt", b"right", T0);
    let rc = r.clone();
    let c = shared(compare(arc(l), arc(r), FolderRules::default()));
    let plan = plan_for(&c.read(), Preset::UpdateRight);
    rc.add_file("f.txt", b"changed", T0 + 3 * S);
    let job = start_sync(&plan, &c, ExecOverrides::default(), None).unwrap();
    let q = wait_question(&job);
    rc.add_file("f.txt", b"right", T0); // the user puts it back
    job.answer(q.id, Answer::Retry).unwrap();
    let res = job.wait();
    assert_eq!((res.done, res.skipped), (1, 0), "{res:?}");
    assert_eq!(rc.read("f.txt").unwrap(), b"left");
}

#[test]
fn a_vanished_source_and_an_appeared_destination_are_questions_too() {
    let (l, r) = (MemVfs::new(), MemVfs::new());
    l.add_file("gone.txt", b"1", T0)
        .add_file("blocked.txt", b"2", T0);
    let (lc, rc) = (l.clone(), r.clone());
    let c = shared(compare(arc(l), arc(r), FolderRules::default()));
    let plan = plan_for(&c.read(), Preset::UpdateRight);
    core_vfs::Vfs::delete(
        &lc,
        &core_vfs::VPath::from_parts(["gone.txt"]),
        core_vfs::DeleteMode::Permanent,
    )
    .unwrap();
    rc.add_file("blocked.txt", b"someone was faster", T0);
    let job = start_sync(&plan, &c, ExecOverrides::default(), None).unwrap();
    let res = run_with_answers(&job, Answer::Skip);
    assert_eq!((res.skipped, res.done), (2, 0), "{res:?}");
    assert_eq!(rc.read("blocked.txt").unwrap(), b"someone was faster");
    assert!(rc.read("gone.txt").is_none());
}

#[test]
fn skip_all_answers_the_rest_without_asking() {
    let (l, r) = (MemVfs::new(), MemVfs::new());
    for i in 0..5 {
        l.add_file(&format!("f{i}"), b"new", T0);
        r.add_file(&format!("f{i}"), b"old!", T0 + S * 100);
    }
    let rc = r.clone();
    let c = shared(compare(arc(l), arc(r), FolderRules::default()));
    let plan = plan_for(&c.read(), Preset::MirrorToRight);
    for i in 0..5 {
        rc.add_file(&format!("f{i}"), b"changed behind our back", T0 + S * 7);
    }
    let asked = Arc::new(AtomicU32::new(0));
    let a = asked.clone();
    let sink: core_sync::JobSink = Arc::new(move |e: &JobEvent| {
        if matches!(e, JobEvent::Question(_)) {
            a.fetch_add(1, Ordering::Relaxed);
        }
    });
    let job = start_sync(&plan, &c, ExecOverrides::default(), Some(sink)).unwrap();
    let q = wait_question(&job);
    job.answer(q.id, Answer::SkipAll).unwrap();
    let res = job.wait();
    assert_eq!((res.skipped, res.failed), (1, 4), "{res:?}");
    assert_eq!(
        asked.load(Ordering::Relaxed),
        1,
        "only the first failure asked"
    );
}

#[test]
fn abort_stops_the_job_and_the_log_is_complete() {
    let (l, r) = (MemVfs::new(), MemVfs::new());
    l.add_file("a.txt", b"1", T0)
        .add_file("b.txt", b"2", T0)
        .add_file("c.txt", b"3", T0);
    r.add_file("b.txt", b"old", T0 + 100 * S);
    let rc = r.clone();
    let c = shared(compare(arc(l), arc(r), FolderRules::default()));
    let plan = plan_for(&c.read(), Preset::MirrorToRight);
    rc.add_file("b.txt", b"edited", T0 + 3 * S);
    let log_dir = tempfile::tempdir().unwrap();
    let opts = ExecOverrides {
        log_dir: Some(log_dir.path().to_path_buf()),
        job_name: Some("abort-test".into()),
        ..ExecOverrides::default()
    };
    let job = start_sync(&plan, &c, opts, None).unwrap();
    let q = wait_question(&job);
    job.answer(q.id, Answer::Abort).unwrap();
    let res = job.wait();
    assert!(res.aborted);
    let files: Vec<_> = std::fs::read_dir(log_dir.path()).unwrap().collect();
    assert_eq!(files.len(), 1);
    let text = std::fs::read_to_string(files[0].as_ref().unwrap().path()).unwrap();
    let lines: Vec<serde_json::Value> = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.first().unwrap()["event"], "start");
    let end = lines.last().unwrap();
    assert_eq!(
        (end["event"].as_str(), end["aborted"].as_bool()),
        (Some("end"), Some(true))
    );
    let aborted = lines
        .iter()
        .find(|l| l["outcome"] == "aborted")
        .expect("the aborted action is logged");
    assert!(aborted["error"].as_str().unwrap().contains("changed"));
    assert!(
        lines
            .iter()
            .all(|l| l["time"].as_str().is_some_and(|t| t.ends_with('Z')))
    );
}

#[test]
fn the_log_has_one_line_per_action() {
    let (l, r) = mirror_tree();
    let c = shared(compare(arc(l), arc(r), FolderRules::default()));
    let plan = plan_for(&c.read(), Preset::MirrorToRight);
    let dir = tempfile::tempdir().unwrap();
    let opts = ExecOverrides {
        log_dir: Some(dir.path().to_path_buf()),
        ..ExecOverrides::default()
    };
    let res = execute(&plan, &c, opts);
    let path = std::fs::read_dir(dir.path())
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let text = std::fs::read_to_string(path).unwrap();
    let actions = text
        .lines()
        .filter(|l| l.contains("\"event\":\"action\""))
        .count();
    assert_eq!(actions as u32, res.done + res.kept);
    assert!(text.contains("\"bytes\":9"), "new.txt is 9 bytes");
    assert!(text.lines().last().unwrap().contains("\"event\":\"end\""));
}

#[test]
fn answering_without_a_question_is_an_error() {
    let (l, r) = (MemVfs::new(), MemVfs::new());
    l.add_file("a", b"1", T0);
    let c = shared(compare(arc(l), arc(r), FolderRules::default()));
    let plan = plan_for(&c.read(), Preset::UpdateRight);
    let job = start_sync(&plan, &c, ExecOverrides::default(), None).unwrap();
    job.wait();
    assert_eq!(
        job.answer(1, Answer::Retry).unwrap_err().kind(),
        ErrorKind::NotFound
    );
    assert!(job.is_finished());
    assert_eq!(job.wait().done, 1, "wait can be called again");
}

#[test]
fn pause_holds_the_job_and_cancel_stops_it_without_leftovers() {
    let (l, r) = (MemVfs::new(), MemVfs::new());
    for i in 0..30 {
        l.add_file(&format!("f{i:02}"), &vec![7u8; 4096], T0);
    }
    let rf = fault(r.clone());
    rf.delay_each_op(Duration::from_millis(15));
    let c = shared(compare(arc(l), rf.clone(), FolderRules::default()));
    let plan = plan_for(&c.read(), Preset::UpdateRight);
    let job = start_sync(&plan, &c, ExecOverrides::default(), None).unwrap();
    job.pause();
    std::thread::sleep(Duration::from_millis(100));
    let held = job.progress();
    std::thread::sleep(Duration::from_millis(200));
    assert!(job.progress().paused);
    assert_eq!(
        job.progress().steps_done,
        held.steps_done,
        "no step runs while paused"
    );
    job.resume();
    std::thread::sleep(Duration::from_millis(100));
    job.cancel();
    let res = job.wait();
    assert!(res.cancelled && !res.aborted);
    assert!(res.done < 30, "stopped early: {res:?}");
    assert!(
        temp_files(&r).is_empty(),
        "no temp file is left: {:?}",
        r.paths()
    );
    // every copied file is complete
    for p in r.paths() {
        if let Some(data) = r.read(&p) {
            assert_eq!(data.len(), 4096, "{p}");
        }
    }
}

/// Backlog E8-S9.1: a failure at every write step leaves no half-written file and no temp file.
#[test]
fn a_failure_at_every_write_step_leaves_old_or_new_content_only() {
    for op in [
        Op::CreateTemp,
        Op::Rename,
        Op::CreateDir,
        Op::Delete,
        Op::SetTimes,
        Op::CopyFile,
    ] {
        for nth in 1..=6u64 {
            let (l, r) = (MemVfs::new(), MemVfs::new());
            l.add_file("d/a.txt", b"left a", T0 + 10 * S)
                .add_file("d/b.txt", b"left b", T0)
                .add_file("new/c.txt", b"left c", T0);
            r.add_file("d/a.txt", b"old a", T0)
                .add_file("old.txt", b"old", T0);
            let rf = fault(r.clone());
            rf.fail_nth(op, nth, ErrorKind::Io);
            let c = shared(compare(arc(l), rf.clone(), FolderRules::default()));
            let plan = plan_for(&c.read(), Preset::MirrorToRight);
            let job = start_sync(&plan, &c, ExecOverrides::default(), None).unwrap();
            let res = run_with_answers(&job, Answer::Skip);
            assert!(!res.aborted && !res.cancelled, "{op:?} #{nth}: {res:?}");
            assert!(temp_files(&r).is_empty(), "{op:?} #{nth}: {:?}", r.paths());
            for (p, old, new) in [
                ("d/a.txt", &b"old a"[..], &b"left a"[..]),
                ("d/b.txt", b"", b"left b"),
                ("new/c.txt", b"", b"left c"),
            ] {
                if let Some(d) = r.read(p) {
                    assert!(d == old || d == new, "{op:?} #{nth}: {p} holds {d:?}");
                }
            }
        }
    }
}

/// A failed copy asks the question and Retry runs it again.
#[test]
fn a_failed_copy_can_be_retried() {
    let (l, r) = (MemVfs::new(), MemVfs::new());
    l.add_file("f.txt", b"data", T0);
    let rf = fault(r.clone());
    rf.fail_nth(Op::CreateTemp, 1, ErrorKind::SharingViolation);
    let c = shared(compare(arc(l), rf.clone(), FolderRules::default()));
    let plan = plan_for(&c.read(), Preset::UpdateRight);
    let job = start_sync(&plan, &c, ExecOverrides::default(), None).unwrap();
    let q = wait_question(&job);
    assert_eq!(q.kind, QuestionKind::Failed);
    job.answer(q.id, Answer::Retry).unwrap();
    let res = job.wait();
    assert_eq!((res.done, res.failed, res.skipped), (1, 0, 0), "{res:?}");
    assert_eq!(r.read("f.txt").unwrap(), b"data");
}

/// Cancel at every phase boundary: whatever was done is consistent.
#[test]
fn cancel_at_every_step_is_clean() {
    let steps_total = {
        let (l, r) = mirror_tree();
        let c = shared(compare(arc(l), arc(r), FolderRules::default()));
        let plan = plan_for(&c.read(), Preset::MirrorToRight);
        core_sync::steps_from_plan(&plan, &c.read()).len()
    };
    for stop_after in 0..steps_total {
        let (l, r) = mirror_tree();
        let rc = r.clone();
        let c = shared(compare(arc(l), arc(r), FolderRules::default()));
        let plan = plan_for(&c.read(), Preset::MirrorToRight);
        let job = start_sync(&plan, &c, ExecOverrides::default(), None).unwrap();
        // pause, wait for `stop_after` steps, cancel
        job.pause();
        job.resume();
        while job.progress().steps_done < stop_after as u32 && !job.is_finished() {
            std::thread::sleep(Duration::from_millis(1));
        }
        job.cancel();
        let res = job.wait();
        assert!(res.failed == 0 && !res.aborted);
        assert!(
            temp_files(&rc).is_empty(),
            "stop {stop_after}: {:?}",
            rc.paths()
        );
    }
}
