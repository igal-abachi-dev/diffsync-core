//! Safety tests on real temp folders (backlog E8-S9; plan.md §9.2, §9.5): a junction is never
//! traversed, a sync on a real disk works end to end, and a process killed in the middle of a
//! copy leaves the old file or the new one, plus at most one `.dctmp~` (child-process test).

mod common;

use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use core_base::CancelToken;
use core_folder::{FolderCompare, FolderRules};
use core_sync::{ExecOverrides, Preset, build_plan, maintenance_sweep, start_sync};
use core_vfs::{VPath, Vfs};
use parking_lot::RwLock;

fn local(path: &Path) -> Arc<dyn Vfs> {
    #[cfg(windows)]
    {
        Arc::new(core_vfs::LocalWin32::new(
            core_vfs::VfsRoot::parse_windows(path.to_str().unwrap()).unwrap(),
        ))
    }
    #[cfg(not(windows))]
    {
        Arc::new(core_vfs::LocalStd::new(path))
    }
}

fn disk_compare(l: &Path, r: &Path, rules: FolderRules) -> FolderCompare {
    FolderCompare::compare(
        local(l),
        &VPath::root(),
        local(r),
        &VPath::root(),
        rules,
        &CancelToken::new(),
    )
    .expect("compare")
}

#[test]
fn a_real_disk_mirror_end_to_end() {
    let (l, r) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    std::fs::create_dir_all(l.path().join("d/sub")).unwrap();
    std::fs::write(l.path().join("d/sub/a.txt"), b"left a").unwrap();
    std::fs::write(l.path().join("b.txt"), b"left b").unwrap();
    std::fs::create_dir_all(r.path().join("stale/x")).unwrap();
    std::fs::write(r.path().join("stale/x/old.txt"), b"old").unwrap();
    std::fs::write(r.path().join("b.txt"), b"older b!").unwrap();
    // read-only destination file: the B5 retry clears the attribute
    let mut perm = std::fs::metadata(r.path().join("b.txt"))
        .unwrap()
        .permissions();
    perm.set_readonly(true);
    std::fs::set_permissions(r.path().join("b.txt"), perm).unwrap();
    let c = Arc::new(RwLock::new(disk_compare(
        l.path(),
        r.path(),
        FolderRules::default(),
    )));
    let plan = build_plan(
        &c.read(),
        &core_sync::SyncRules::from_preset(Preset::MirrorToRight),
        None,
    )
    .unwrap();
    let res = start_sync(&plan, &c, ExecOverrides::default(), None)
        .unwrap()
        .wait();
    assert_eq!((res.failed, res.skipped), (0, 0), "{res:?}");
    assert_eq!(
        std::fs::read(r.path().join("d/sub/a.txt")).unwrap(),
        b"left a"
    );
    assert_eq!(std::fs::read(r.path().join("b.txt")).unwrap(), b"left b");
    assert!(!r.path().join("stale").exists());
    // a second compare finds nothing to do
    let again = disk_compare(l.path(), r.path(), FolderRules::default());
    let plan = build_plan(
        &again,
        &core_sync::SyncRules::from_preset(Preset::MirrorToRight),
        None,
    )
    .unwrap();
    assert!(plan.actions.is_empty(), "{:?}", plan.describe(&again));
}

/// §9.5: Mirror deleting an orphan folder that holds a junction removes neither the junction nor
/// anything in its target.
#[cfg(windows)]
#[test]
fn a_junction_is_never_traversed_and_its_target_stays_intact() {
    let (l, r, outside) = (
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
    );
    std::fs::write(outside.path().join("precious.txt"), b"do not delete").unwrap();
    std::fs::create_dir_all(r.path().join("orphan")).unwrap();
    std::fs::write(r.path().join("orphan/file.txt"), b"x").unwrap();
    let st = Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        // mklink wants backslashes, so join one component at a time
        .arg(r.path().join("orphan").join("jn"))
        .arg(outside.path())
        .output()
        .unwrap();
    assert!(st.status.success());
    let c = Arc::new(RwLock::new(disk_compare(
        l.path(),
        r.path(),
        FolderRules::default(),
    )));
    let plan = build_plan(
        &c.read(),
        &core_sync::SyncRules::from_preset(Preset::MirrorToRight),
        None,
    )
    .unwrap();
    assert_eq!(plan.summary.skipped_links, 1);
    let res = start_sync(&plan, &c, ExecOverrides::default(), None)
        .unwrap()
        .wait();
    assert_eq!(res.failed, 0, "{res:?}");
    assert_eq!(
        std::fs::read(outside.path().join("precious.txt")).unwrap(),
        b"do not delete"
    );
    assert!(
        r.path().join("orphan/jn").exists(),
        "the junction itself stays (links are skipped)"
    );
    assert!(!r.path().join("orphan/file.txt").exists());
    assert_eq!(
        res.kept, 1,
        "the folder is kept because it still holds the junction"
    );
}

/// The child half of the crash test: copies a big file over an old one and is killed by the
/// parent while the copy runs. Not a test on its own.
#[test]
#[ignore = "child process of killed_mid_copy_leaves_the_old_or_the_new_file"]
fn crash_child_copy() {
    let (Some(l), Some(r)) = (
        std::env::var_os("DS_CRASH_LEFT"),
        std::env::var_os("DS_CRASH_RIGHT"),
    ) else {
        return;
    };
    let c = Arc::new(RwLock::new(disk_compare(
        Path::new(&l),
        Path::new(&r),
        FolderRules::default(),
    )));
    let plan = build_plan(
        &c.read(),
        &core_sync::SyncRules::from_preset(Preset::MirrorToRight),
        None,
    )
    .unwrap();
    let job = start_sync(&plan, &c, ExecOverrides::default(), None).unwrap();
    job.wait();
}

fn is_temp(p: &Path) -> bool {
    p.file_name()
        .is_some_and(|n| n.to_string_lossy().ends_with(".dctmp~"))
}

/// Backlog E8-S9.2 / plan.md §9.2: kill the process mid-copy; the destination holds the old
/// file or the new file in full, at most one temp file is left, and the sweep removes it.
#[test]
fn killed_mid_copy_leaves_the_old_or_the_new_file() {
    const OLD: usize = 16 << 20;
    const NEW: usize = 384 << 20;
    let (l, r) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let chunk = |b: u8, n: usize| vec![b; n];
    {
        use std::io::Write;
        let mut f = std::fs::File::create(l.path().join("big.bin")).unwrap();
        for _ in 0..NEW / (8 << 20) {
            f.write_all(&chunk(b'N', 8 << 20)).unwrap();
        }
    }
    std::fs::write(r.path().join("big.bin"), chunk(b'O', OLD)).unwrap();
    // the destination is older, so Mirror overwrites it
    let old_time = std::time::SystemTime::now() - Duration::from_secs(86_400);
    std::fs::File::options()
        .write(true)
        .open(r.path().join("big.bin"))
        .unwrap()
        .set_modified(old_time)
        .unwrap();

    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "crash_child_copy",
            "--exact",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("DS_CRASH_LEFT", l.path())
        .env("DS_CRASH_RIGHT", r.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    // wait for the temp file, then kill at once
    let t = Instant::now();
    let mut saw_temp = false;
    while t.elapsed() < Duration::from_secs(60) {
        if std::fs::read_dir(r.path())
            .unwrap()
            .flatten()
            .any(|e| is_temp(&e.path()))
        {
            saw_temp = true;
            break;
        }
        if child.try_wait().unwrap().is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    let _ = child.kill();
    let _ = child.wait();
    // the test proves nothing if the copy finished before the temp file was seen
    assert!(
        saw_temp,
        "the temp file never appeared: the copy ended before the kill"
    );

    // the destination is whole: the old content or the new content, never a mix
    let data = std::fs::read(r.path().join("big.bin")).unwrap();
    let all = |b: u8| data.iter().all(|&x| x == b);
    assert!(
        (data.len() == OLD && all(b'O')) || (data.len() == NEW && all(b'N')),
        "the destination is neither the old nor the new file ({} bytes)",
        data.len()
    );
    let temps: Vec<_> = std::fs::read_dir(r.path())
        .unwrap()
        .flatten()
        .filter(|e| is_temp(&e.path()))
        .collect();
    assert!(temps.len() <= 1, "at most one temp file: {temps:?}");
    // the sweep finds and removes it
    let v = local(r.path());
    let report = maintenance_sweep(&*v, &VPath::root(), false, &CancelToken::new()).unwrap();
    assert_eq!(report.found.len(), temps.len());
    let report = maintenance_sweep(&*v, &VPath::root(), true, &CancelToken::new()).unwrap();
    assert_eq!(report.deleted as usize, temps.len());
    assert!(
        std::fs::read_dir(r.path())
            .unwrap()
            .flatten()
            .all(|e| !is_temp(&e.path()))
    );
    // and a fresh run finishes the job
    let c = Arc::new(RwLock::new(disk_compare(
        l.path(),
        r.path(),
        FolderRules::default(),
    )));
    let plan = build_plan(
        &c.read(),
        &core_sync::SyncRules::from_preset(Preset::MirrorToRight),
        None,
    )
    .unwrap();
    let res = start_sync(&plan, &c, ExecOverrides::default(), None)
        .unwrap()
        .wait();
    assert_eq!(res.failed, 0);
    assert_eq!(
        std::fs::metadata(r.path().join("big.bin")).unwrap().len(),
        NEW as u64
    );
}

/// Reviewer finding (E11-S6), decision D53, plan §9.5: a folder that is swapped for a junction
/// between the compare and the execute is noticed for the steps below it. The junction points
/// at the old folder, so the item below it still has exactly the fingerprint the compare saw
/// (the check of the item alone passes); only the check of its parents catches the swap.
#[cfg(windows)]
#[test]
fn a_folder_swapped_for_a_junction_after_the_compare_stops_the_steps_below_it() {
    let (l, r, outside) = (
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
    );
    std::fs::create_dir_all(l.path().join("d")).unwrap();
    std::fs::create_dir_all(r.path().join("d")).unwrap();
    std::fs::write(l.path().join("d/f.txt"), b"new content, longer").unwrap();
    std::fs::write(r.path().join("d/f.txt"), b"old").unwrap();
    // the destination is older, so UpdateRight copies the left file over it
    let old = std::time::SystemTime::now() - Duration::from_secs(3600);
    std::fs::File::options()
        .write(true)
        .open(r.path().join("d/f.txt"))
        .unwrap()
        .set_modified(old)
        .unwrap();
    let c = Arc::new(RwLock::new(disk_compare(
        l.path(),
        r.path(),
        FolderRules::default(),
    )));
    let plan = build_plan(
        &c.read(),
        &core_sync::SyncRules::from_preset(Preset::UpdateRight),
        None,
    )
    .unwrap();
    assert!(!plan.actions.is_empty());
    // the swap: R/d moves away to `outside` and a junction named d points back at it
    std::fs::rename(r.path().join("d"), outside.path().join("d")).unwrap();
    let st = Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(r.path().join("d"))
        .arg(outside.path().join("d"))
        .output()
        .unwrap();
    assert!(st.status.success());
    let job = start_sync(&plan, &c, ExecOverrides::default(), None).unwrap();
    let q = common::wait_question(&job);
    assert_eq!(q.kind, core_sync::QuestionKind::ChangedSinceCompare);
    assert!(
        q.message.contains("no longer a plain folder"),
        "{}",
        q.message
    );
    job.answer(q.id, core_sync::Answer::Skip).unwrap();
    let res = job.wait();
    assert_eq!(res.done, 0, "{res:?}");
    assert_eq!(
        std::fs::read(outside.path().join("d/f.txt")).unwrap(),
        b"old",
        "nothing was written through the junction"
    );
}
