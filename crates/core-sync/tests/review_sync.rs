//! Regression tests for the sync problems an external review reproduced (decisions D64–D68).

mod common;

use common::*;
use core_base::ticks::TICKS_PER_SECOND as S;
use core_folder::FolderRules;
use core_sync::{ExecOverrides, Preset, SyncRules, build_plan, start_sync};
use core_vfs::MemVfs;

/// D67: with content compare on (the default content kind ignores white space), a Python file that
/// was only re-indented and a YAML file whose nesting changed are real changes. A Mirror must copy
/// them; it planned nothing and left the old program on the right.
#[test]
fn mirror_with_content_compare_copies_whitespace_only_changes() {
    let (l, r) = (MemVfs::new(), MemVfs::new());
    let new_py = b"def f():\n    if x:\n        return 1\n    return 2\n";
    let old_py = b"def f():\n    if x:\n        return 1\n        return 2\n";
    l.add_file("app.py", new_py, T0 + 100 * S);
    r.add_file("app.py", old_py, T0);
    let new_yaml = b"a:\n  b: 1\nc: 2\n";
    let old_yaml = b"a:\n  b: 1\n  c: 2\n";
    l.add_file("cfg.yaml", new_yaml, T0 + 100 * S);
    r.add_file("cfg.yaml", old_yaml, T0);
    let rules = FolderRules {
        compare_content: true,
        ..FolderRules::default()
    };
    let c = compare(arc(l), arc(r.clone()), rules);
    let plan = build_plan(&c, &SyncRules::from_preset(Preset::MirrorToRight), None).unwrap();
    assert_eq!(plan.actions.len(), 2, "both files must be planned");
    let sc = shared(c);
    let res = start_sync(&plan, &sc, ExecOverrides::default(), None)
        .unwrap()
        .wait();
    assert_eq!(res.failed, 0, "{res:?}");
    assert_eq!(r.read("app.py").unwrap(), new_py.to_vec());
    assert_eq!(r.read("cfg.yaml").unwrap(), new_yaml.to_vec());
}

/// D68: nested roots on a backend that cannot give parent folder identities (the portable
/// backend on Unix, before the fix): a mirror between `a` and `a/b` deleted `important.docx`.
#[cfg(unix)]
#[test]
fn nested_roots_are_refused_on_the_portable_backend() {
    use std::sync::Arc;
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a");
    std::fs::create_dir_all(a.join("b")).unwrap();
    std::fs::write(a.join("important.docx"), b"left only").unwrap();
    std::fs::write(a.join("b/g.txt"), b"inside b").unwrap();
    let l: Arc<dyn core_vfs::Vfs> = Arc::new(core_vfs::LocalStd::new(&a));
    let r: Arc<dyn core_vfs::Vfs> = Arc::new(core_vfs::LocalStd::new(&a.join("b")));
    let c = compare(l, r, FolderRules::default());
    assert!(c.sync_blocked, "warnings: {:?}", c.warnings);
    assert!(build_plan(&c, &SyncRules::from_preset(Preset::MirrorToLeft), None).is_err());
    assert!(a.join("important.docx").exists());
}

/// D68: a volume without file IDs (FAT, exFAT, some shares) cannot prove that one folder is not
/// inside the other, so sync is blocked until the caller says it knows better.
#[test]
fn roots_that_cannot_be_checked_block_sync_unless_allowed() {
    let make = || {
        let (l, r) = (
            MemVfs::new().without_file_ids(),
            MemVfs::new().without_file_ids(),
        );
        l.add_file("a.txt", b"1", T0);
        r.add_file("b.txt", b"2", T0);
        (l, r)
    };
    let (l, r) = make();
    let c = compare(arc(l), arc(r), FolderRules::default());
    assert!(c.sync_blocked, "{:?}", c.warnings);
    assert!(build_plan(&c, &SyncRules::from_preset(Preset::MirrorToRight), None).is_err());
    let (l, r) = make();
    let rules = FolderRules {
        allow_unchecked_roots: true,
        ..FolderRules::default()
    };
    let c = compare(arc(l), arc(r), rules);
    assert!(!c.sync_blocked);
    assert!(build_plan(&c, &SyncRules::from_preset(Preset::MirrorToRight), None).is_ok());
}

/// D66: an audit log that was asked for and cannot be created stops the job before it starts.
#[test]
fn a_log_that_cannot_be_created_stops_the_job_before_any_step() {
    let (l, r) = (MemVfs::new(), MemVfs::new());
    l.add_file("a.txt", b"1", T0);
    let rc = r.clone();
    let c = compare(arc(l), arc(r), FolderRules::default());
    let plan = build_plan(&c, &SyncRules::from_preset(Preset::MirrorToRight), None).unwrap();
    let sc = shared(c);
    let dir = tempfile::tempdir().unwrap();
    // a log folder under a file cannot be created
    let blocker = dir.path().join("file");
    std::fs::write(&blocker, "x").unwrap();
    let bad = ExecOverrides {
        log_dir: Some(blocker.join("logs")),
        ..ExecOverrides::default()
    };
    let res = start_sync(&plan, &sc, bad.clone(), None).unwrap().wait();
    assert!(res.aborted, "{res:?}");
    assert_eq!((res.done, rc.read("a.txt")), (0, None));
    // the caller may choose to run without the guarantee
    let lax = ExecOverrides {
        log_required: Some(false),
        ..bad
    };
    let res = start_sync(&plan, &sc, lax, None).unwrap().wait();
    assert!(!res.aborted && res.done == 1, "{res:?}");
    assert_eq!(rc.read("a.txt").unwrap(), b"1");
}
