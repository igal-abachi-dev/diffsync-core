//! Pinning tests for the plan-reviewer findings on core-folder (backlog E7-S12.3).

use std::sync::Arc;

use core_base::ticks::TICKS_PER_SECOND as S;
use core_base::{CancelToken, ErrorKind};
use core_folder::{ContentKind, FolderCompare, FolderRules, NodeId, Status, Tree, flag};
use core_vfs::{FaultVfs, Kind, MemVfs, Op, VPath, Vfs, attr};

const T0: i64 = 1_000_000 * S;

fn run(l: &Arc<dyn Vfs>, r: &Arc<dyn Vfs>, rules: FolderRules) -> FolderCompare {
    FolderCompare::compare(
        l.clone(),
        &VPath::root(),
        r.clone(),
        &VPath::root(),
        rules,
        &CancelToken::new(),
    )
    .unwrap()
}

fn arc(m: MemVfs) -> Arc<dyn Vfs> {
    Arc::new(m)
}

fn find(fc: &FolderCompare, path: &str) -> NodeId {
    (1..fc.tree.len() as NodeId)
        .find(|&i| fc.tree.path(i).display() == path)
        .unwrap_or_else(|| panic!("no node {path}"))
}

fn status(fc: &FolderCompare, path: &str) -> Status {
    fc.tree.node(find(fc, path)).status
}

fn content_rules(kind: ContentKind) -> FolderRules {
    FolderRules {
        compare_content: true,
        content_kind: kind,
        ..FolderRules::default()
    }
}

/// Decision D18: hidden/system is decided per pair, so attribute skew never makes an orphan.
#[test]
fn hidden_on_one_side_only_does_not_create_an_orphan() {
    let (l, r) = (MemVfs::new(), MemVfs::new());
    l.add_file("h.txt", b"1", T0);
    r.add_file("h.txt", b"1", T0);
    l.set_raw_attrs("h.txt", attr::HIDDEN | attr::ARCHIVE);
    let rules = FolderRules {
        include_hidden: false,
        ..FolderRules::default()
    };
    let fc = run(&arc(l), &arc(r), rules);
    assert_eq!(
        fc.tree.node(Tree::ROOT).child_count,
        0,
        "the pair is skipped, not half of it"
    );
}

/// Decision D32: `exclude_files` never excludes a folder.
#[test]
fn exclude_files_does_not_prune_folders() {
    let (l, r) = (MemVfs::new(), MemVfs::new());
    for m in [&l, &r] {
        m.add_file("x.obj/inner.txt", b"1", T0)
            .add_file("y.obj", b"1", T0);
    }
    let rules = FolderRules {
        exclude_files: vec!["*.obj".into()],
        ..FolderRules::default()
    };
    let fc = run(&arc(l), &arc(r), rules);
    assert_eq!(
        status(&fc, "x.obj/inner.txt"),
        Status::Same,
        "the folder x.obj is still scanned"
    );
    assert!((1..fc.tree.len() as NodeId).all(|i| fc.tree.path(i).display() != "y.obj"));
}

/// Decision D32: `exclude_folders` never hides a file of that name.
#[test]
fn exclude_folders_does_not_hide_files_of_that_name() {
    let (l, r) = (MemVfs::new(), MemVfs::new());
    for m in [&l, &r] {
        m.add_file("bin", b"1", T0).add_file("src/bin/x", b"1", T0);
    }
    let rules = FolderRules {
        exclude_folders: vec!["bin".into()],
        ..FolderRules::default()
    };
    let fc = run(&arc(l), &arc(r), rules);
    assert_eq!(status(&fc, "bin"), Status::Same, "a file named bin stays");
    assert!((1..fc.tree.len() as NodeId).all(|i| fc.tree.path(i).display() != "src/bin"));
}

/// Decision D34: a type conflict needs the file and the folder on opposite sides.
#[test]
fn same_side_file_and_folder_are_not_a_type_conflict() {
    let l = MemVfs::new();
    l.add_file("x", b"1", T0).add_dir("X", T0);
    let fc = run(&arc(l), &arc(MemVfs::new()), FolderRules::default());
    let kids: Vec<_> = fc.tree.node(Tree::ROOT).children().collect();
    assert_eq!(kids.len(), 2);
    assert!(
        kids.iter()
            .all(|&k| !fc.tree.node(k).has(flag::TYPE_CONFLICT))
    );
}

/// Decision D34: `.gitignore` keeps its whole name when extensions are ignored.
#[test]
fn leading_dot_names_keep_their_name_when_extensions_are_ignored() {
    let case = |a: &str, b: &str| {
        let (l, r) = (MemVfs::new(), MemVfs::new());
        l.add_file(a, b"1", T0);
        r.add_file(b, b"1", T0);
        let rules = FolderRules {
            align_ignore_extensions: true,
            ..FolderRules::default()
        };
        run(&arc(l), &arc(r), rules)
            .tree
            .node(Tree::ROOT)
            .child_count
            == 1
    };
    assert!(case(".a.txt", ".a.bak"));
    assert!(!case(".gitignore", ".env"));
}

/// Decision D17: compare-now does not download cloud placeholders unless allowed.
#[test]
fn compare_now_skips_offline_files_unless_allowed() {
    let (l, r) = (MemVfs::new(), MemVfs::new());
    l.add_file("cloud", b"abc", T0);
    r.add_file("cloud", b"abd", T0);
    l.set_raw_attrs("cloud", attr::OFFLINE | attr::ARCHIVE);
    let fl = Arc::new(FaultVfs::new(l));
    let rules = FolderRules {
        content_kind: ContentKind::Binary,
        ..FolderRules::default()
    };
    let mut fc = FolderCompare::compare(
        fl.clone(),
        &VPath::root(),
        arc(r),
        &VPath::root(),
        rules,
        &CancelToken::new(),
    )
    .unwrap();
    let id = find(&fc, "cloud");
    fc.compare_content_now(&[id], &CancelToken::new()).unwrap();
    assert_eq!(fl.count(Op::OpenRead), 0, "nothing is downloaded");
    fc.rules.compare_offline_files = true;
    fc.compare_content_now(&[id], &CancelToken::new()).unwrap();
    assert_eq!(fl.count(Op::OpenRead), 1);
}

/// ClassifyFile parity: when the quick test ignores times, the newer side still comes from them.
#[test]
fn newer_side_follows_the_times_when_the_quick_test_ignores_them() {
    let base = FolderRules {
        compare_timestamp: false,
        content_overrides: false,
        ..content_rules(ContentKind::Binary)
    };
    let (l, r) = (MemVfs::new(), MemVfs::new());
    l.add_file("f", b"aaa", T0 + 100 * S);
    r.add_file("f", b"bbb", T0);
    assert_eq!(
        status(&run(&arc(l), &arc(r), base.clone()), "f"),
        Status::LeftNewer
    );
    // sizes differ with a non-rules kind: different without reading, still with the newer side
    let (l, r) = (MemVfs::new(), MemVfs::new());
    l.add_file("f", b"aaa", T0 + 100 * S);
    r.add_file("f", b"bbbb", T0);
    assert_eq!(status(&run(&arc(l), &arc(r), base), "f"), Status::LeftNewer);
}

/// Decision D31 and §9 rule 5 in the content phase: a paired link is never read.
#[test]
fn paired_links_are_never_opened_by_content_compare() {
    let (l, r) = (MemVfs::new(), MemVfs::new());
    l.add_link("lnk", Kind::File);
    r.add_link("lnk", Kind::File);
    let (fl, fr) = (Arc::new(FaultVfs::new(l)), Arc::new(FaultVfs::new(r)));
    let fc = FolderCompare::compare(
        fl.clone(),
        &VPath::root(),
        fr.clone(),
        &VPath::root(),
        content_rules(ContentKind::Binary),
        &CancelToken::new(),
    )
    .unwrap();
    assert_eq!(status(&fc, "lnk"), Status::Same);
    assert_eq!((fl.count(Op::OpenRead), fr.count(Op::OpenRead)), (0, 0));
}

/// Decision D35: a large file that differs is `Different` without being decoded.
#[test]
fn large_rules_based_files_are_not_decoded() {
    let big = vec![b'a'; (core_folder::compare::TEXT_COMPARE_CAP + 1) as usize];
    let mut big2 = big.clone();
    *big2.last_mut().unwrap() = b'b';
    let (l, r) = (MemVfs::new(), MemVfs::new());
    l.add_file("f.txt", &big, T0);
    r.add_file("f.txt", &big2, T0);
    let fc = run(&arc(l), &arc(r), content_rules(ContentKind::RulesBased));
    assert_eq!(status(&fc, "f.txt"), Status::Different);
}

#[test]
fn a_cancelled_content_phase_keeps_the_nodes_queued() {
    let (l, r) = (MemVfs::new(), MemVfs::new());
    l.add_file("f", b"abc", T0);
    r.add_file("f", b"abd", T0);
    let mut fc = run(&arc(l), &arc(r), FolderRules::default());
    let id = find(&fc, "f");
    let cancel = CancelToken::new();
    cancel.cancel();
    assert!(
        fc.compare_content_now(&[id], &cancel)
            .unwrap_err()
            .is_cancelled()
    );
    assert_eq!(fc.pending, [id]);
    assert!(fc.tree.node(id).has(flag::PENDING_CONTENT));
}

#[test]
fn memvfs_clones_are_the_same_folder_and_distinct_instances_are_not() {
    let m = MemVfs::new();
    m.add_file("a", b"1", T0);
    let e = FolderCompare::compare(
        arc(m.clone()),
        &VPath::root(),
        arc(m.clone()),
        &VPath::root(),
        FolderRules::default(),
        &CancelToken::new(),
    )
    .unwrap_err();
    assert_eq!(e.kind(), ErrorKind::InvalidArgument);
    assert!(
        run(&arc(m), &arc(MemVfs::new()), FolderRules::default())
            .warnings
            .is_empty()
    );
}

/// Plan §9.7: when overlap cannot be checked the user is told.
#[cfg(windows)]
#[test]
fn roots_without_file_ids_give_an_overlap_warning() {
    let (d1, d2) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let l: Arc<dyn Vfs> = Arc::new(core_vfs::LocalStd::new(d1.path()));
    let r: Arc<dyn Vfs> = Arc::new(core_vfs::LocalStd::new(d2.path()));
    let fc = run(&l, &r, FolderRules::default());
    assert_eq!(fc.warnings.len(), 1);
    assert!(fc.warnings[0].contains("could not check"));
}
