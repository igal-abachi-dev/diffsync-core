//! Folder compare behaviour on `MemVfs` (backlog E7-S3..S10, S12.1; plan.md §11.3 folder rows).

use std::sync::{Arc, Mutex};

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

fn mem() -> MemVfs {
    MemVfs::new()
}

fn arc(m: MemVfs) -> Arc<dyn Vfs> {
    Arc::new(m)
}

/// Finds a node by its `/`-separated display path.
fn find(fc: &FolderCompare, path: &str) -> NodeId {
    (1..fc.tree.len() as NodeId)
        .find(|&i| fc.tree.path(i).display() == path)
        .unwrap_or_else(|| panic!("no node {path}"))
}

fn status(fc: &FolderCompare, path: &str) -> Status {
    fc.tree.node(find(fc, path)).status
}

#[test]
fn basic_statuses_and_counters() {
    let (l, r) = (mem(), mem());
    l.add_file("same.txt", b"abc", T0)
        .add_file("diff.txt", b"abc", T0)
        .add_file("lnew.txt", b"abc", T0 + 10 * S)
        .add_file("only_l.txt", b"x", T0);
    r.add_file("same.txt", b"abc", T0)
        .add_file("diff.txt", b"abcd", T0)
        .add_file("lnew.txt", b"abcd", T0)
        .add_file("only_r.txt", b"x", T0);
    let fc = run(&arc(l), &arc(r), FolderRules::default());
    assert_eq!(status(&fc, "same.txt"), Status::Same);
    assert_eq!(
        status(&fc, "diff.txt"),
        Status::Different,
        "size differs, times equal → no newer side"
    );
    assert_eq!(status(&fc, "lnew.txt"), Status::LeftNewer);
    assert_eq!(status(&fc, "only_l.txt"), Status::LeftOrphan);
    assert_eq!(status(&fc, "only_r.txt"), Status::RightOrphan);
    let c = fc.tree.node(Tree::ROOT).counters;
    assert_eq!(
        (c.diff, c.left_newer, c.left_orphan, c.right_orphan, c.items),
        (1, 1, 1, 1, 5)
    );
    assert_eq!(fc.tree.recompute_counters()[0], c);
}

#[test]
fn nested_folders_and_orphan_folders_list_their_content() {
    let (l, r) = (mem(), mem());
    l.add_file("d/a.txt", b"1", T0)
        .add_file("d/e/b.txt", b"1", T0)
        .add_file("only/x.txt", b"1", T0);
    r.add_file("d/a.txt", b"1", T0).add_dir("d/e", T0);
    let fc = run(&arc(l), &arc(r), FolderRules::default());
    assert_eq!(status(&fc, "d/e/b.txt"), Status::LeftOrphan);
    assert_eq!(status(&fc, "only"), Status::LeftOrphan);
    assert_eq!(status(&fc, "only/x.txt"), Status::LeftOrphan);
    assert_eq!(fc.tree.node(find(&fc, "d")).counters.left_orphan, 1);
    assert_eq!(fc.tree.node(Tree::ROOT).counters.left_orphan, 3);
    // folders sort before files
    assert!(find(&fc, "d") < find(&fc, "only") || find(&fc, "only") < find(&fc, "d"));
    assert_eq!(fc.tree.node(find(&fc, "d")).depth, 1);
}

/// Golden rows of plan.md §11.3: name pairing (D6 + NFC).
#[test]
fn name_pairing_golden_rows() {
    let pair = |ln: &str, rn: &str, rules: FolderRules| -> bool {
        let (l, r) = (mem(), mem());
        l.add_file(ln, b"1", T0);
        r.add_file(rn, b"1", T0);
        let fc = run(&arc(l), &arc(r), rules);
        fc.tree.node(Tree::ROOT).child_count == 1
    };
    let d = FolderRules::default;
    assert!(pair("Readme.TXT", "README.txt", d()));
    // NFC / NFD: é composed vs decomposed
    assert!(pair("caf\u{e9}.txt", "cafe\u{301}.txt", d()));
    assert!(!pair(
        "caf\u{e9}.txt",
        "cafe\u{301}.txt",
        FolderRules {
            normalized_alignment: false,
            ..d()
        }
    ));
    // decision: D6: simple per-unit folding only
    assert!(
        !pair("stra\u{df}e", "STRASSE", d()),
        "ß and SS are not the same name"
    );
    assert!(
        !pair("\u{130}x", "ix", d()),
        "İ and i are not the same name"
    );
    assert!(pair("\u{e9}", "\u{c9}", d()));
    assert!(!pair(
        "a.txt",
        "A.txt",
        FolderRules {
            case_sensitive_names: true,
            ..d()
        }
    ));
    assert!(pair(
        "a.txt",
        "a.bak",
        FolderRules {
            align_ignore_extensions: true,
            ..d()
        }
    ));
    assert!(!pair("a.txt", "a.bak", d()));
}

#[test]
fn ignore_extensions_applies_to_files_only() {
    let (l, r) = (mem(), mem());
    l.add_dir("v1.0", T0);
    r.add_dir("v1.1", T0);
    let fc = run(
        &arc(l),
        &arc(r),
        FolderRules {
            align_ignore_extensions: true,
            ..FolderRules::default()
        },
    );
    assert_eq!(
        fc.tree.node(Tree::ROOT).child_count,
        2,
        "folder names keep their `extension`"
    );
}

#[test]
fn type_conflict_gives_two_flagged_orphans() {
    let (l, r) = (mem(), mem());
    l.add_file("x", b"1", T0);
    r.add_file("x/inner.txt", b"1", T0);
    let fc = run(&arc(l), &arc(r), FolderRules::default());
    let kids: Vec<_> = fc.tree.node(Tree::ROOT).children().collect();
    assert_eq!(kids.len(), 2);
    let (dir, file) = (kids[0], kids[1]);
    assert!(fc.tree.node(dir).is_dir() && !fc.tree.node(file).is_dir());
    assert_eq!(
        (fc.tree.node(file).status, fc.tree.node(dir).status),
        (Status::LeftOrphan, Status::RightOrphan)
    );
    assert!(
        fc.tree.node(file).has(flag::TYPE_CONFLICT) && fc.tree.node(dir).has(flag::TYPE_CONFLICT)
    );
    assert_eq!(status(&fc, "x/inner.txt"), Status::RightOrphan);
}

#[test]
fn duplicate_keys_pair_in_order_and_are_flagged() {
    let (l, r) = (mem(), mem());
    l.add_file("a.txt", b"1", T0).add_file("A.txt", b"2", T0);
    r.add_file("a.txt", b"1", T0);
    let fc = run(&arc(l), &arc(r), FolderRules::default());
    let kids: Vec<_> = fc.tree.node(Tree::ROOT).children().collect();
    assert_eq!(kids.len(), 2, "one pair plus one orphan");
    assert!(
        kids.iter()
            .all(|&k| fc.tree.node(k).has(flag::DUPLICATE_KEY))
    );
    assert_eq!(
        kids.iter()
            .filter(|&&k| fc.tree.node(k).status == Status::LeftOrphan)
            .count(),
        1
    );
}

#[test]
fn times_golden_rows_through_the_classifier() {
    let case = |dt: i64, rules: FolderRules| {
        let (l, r) = (mem(), mem());
        l.add_file("f", b"1", T0);
        r.add_file("f", b"1", T0 + dt);
        let fc = run(&arc(l), &arc(r), rules);
        status(&fc, "f")
    };
    let d = FolderRules::default;
    assert_eq!(case(19 * S / 10, d()), Status::Same);
    assert_eq!(case(2 * S, d()), Status::Same);
    assert_eq!(case(21 * S / 10, d()), Status::RightNewer);
    assert_eq!(case(3600 * S, d()), Status::RightNewer);
    assert_eq!(
        case(
            3600 * S,
            FolderRules {
                ignore_dst: true,
                ..d()
            }
        ),
        Status::Same
    );
    let tz = FolderRules {
        ignore_time_zone: true,
        ..d()
    };
    assert_eq!(case(19_800 * S, tz.clone()), Status::Same);
    assert_eq!(case(19_800 * S + 3 * S, tz), Status::RightNewer);
}

#[test]
fn attributes_are_compared_only_when_asked() {
    let build = || {
        let (l, r) = (mem(), mem());
        l.add_file("f", b"1", T0);
        r.add_file("f", b"1", T0);
        r.set_raw_attrs("f", attr::ARCHIVE | attr::READONLY);
        (arc(l), arc(r))
    };
    let (l, r) = build();
    assert_eq!(
        status(&run(&l, &r, FolderRules::default()), "f"),
        Status::Same
    );
    assert_eq!(
        status(
            &run(
                &l,
                &r,
                FolderRules {
                    compare_attributes: true,
                    ..FolderRules::default()
                }
            ),
            "f"
        ),
        Status::Different
    );
}

// ---------------------------------------------------------------- filters (E7-S5)

#[test]
fn excluded_folder_is_never_opened() {
    let (l, r) = (mem(), mem());
    for m in [&l, &r] {
        m.add_file("src/a.rs", b"1", T0)
            .add_file("node_modules/p/x.js", b"1", T0);
    }
    let (fl, fr) = (Arc::new(FaultVfs::new(l)), Arc::new(FaultVfs::new(r)));
    let rules = FolderRules {
        exclude_folders: vec!["node_modules".into()],
        ..FolderRules::default()
    };
    let fc = FolderCompare::compare(
        fl.clone(),
        &VPath::root(),
        fr.clone(),
        &VPath::root(),
        rules,
        &CancelToken::new(),
    )
    .unwrap();
    // root + src on each side; node_modules was never listed
    assert_eq!((fl.count(Op::List), fr.count(Op::List)), (2, 2));
    assert!(
        (1..fc.tree.len() as NodeId).all(|i| !fc.tree.path(i).display().contains("node_modules"))
    );
}

/// No false orphans: a pair is excluded when either side's name matches.
#[test]
fn filter_decides_per_pair() {
    let (l, r) = (mem(), mem());
    l.add_file("a.obj", b"1", T0).add_file("keep.txt", b"1", T0);
    r.add_file("A.OBJ", b"1", T0).add_file("keep.txt", b"1", T0);
    let rules = FolderRules {
        exclude_files: vec!["*.obj".into()],
        ..FolderRules::default()
    };
    let fc = run(&arc(l), &arc(r), rules);
    assert_eq!(fc.tree.node(Tree::ROOT).child_count, 1);
    assert_eq!(status(&fc, "keep.txt"), Status::Same);
}

#[test]
fn include_files_limits_files_but_not_folders() {
    let (l, r) = (mem(), mem());
    l.add_file("d/a.cs", b"1", T0).add_file("d/b.txt", b"1", T0);
    r.add_file("d/a.cs", b"1", T0);
    let rules = FolderRules {
        include_files: vec!["*.cs".into()],
        ..FolderRules::default()
    };
    let fc = run(&arc(l), &arc(r), rules);
    assert_eq!(status(&fc, "d/a.cs"), Status::Same);
    assert_eq!(fc.tree.node(find(&fc, "d")).child_count, 1);
}

#[test]
fn hidden_and_system_items_follow_d18() {
    let build = || {
        let (l, r) = (mem(), mem());
        l.add_file("h.txt", b"1", T0)
            .add_file("s.txt", b"1", T0)
            .add_file("v.txt", b"1", T0);
        l.set_raw_attrs("h.txt", attr::HIDDEN)
            .set_raw_attrs("s.txt", attr::SYSTEM);
        (arc(l), arc(r))
    };
    let (l, r) = build();
    assert_eq!(
        run(&l, &r, FolderRules::default())
            .tree
            .node(Tree::ROOT)
            .child_count,
        3,
        "shown by default"
    );
    let skip_h = FolderRules {
        include_hidden: false,
        ..FolderRules::default()
    };
    assert_eq!(run(&l, &r, skip_h).tree.node(Tree::ROOT).child_count, 2);
    let skip_s = FolderRules {
        include_system: false,
        ..FolderRules::default()
    };
    assert_eq!(run(&l, &r, skip_s).tree.node(Tree::ROOT).child_count, 2);
}

// ---------------------------------------------------------------- scan errors (E7-S3)

#[test]
fn unreadable_subfolder_is_an_error_node_and_the_scan_continues() {
    let (l, r) = (mem(), mem());
    for m in [&l, &r] {
        m.add_file("bad/x", b"1", T0).add_file("good/y", b"1", T0);
    }
    let fl = Arc::new(FaultVfs::new(l));
    // list #1 is the root; #2 is `bad` (folders sort alphabetically after folding)
    fl.fail_nth(Op::List, 2, ErrorKind::AccessDenied);
    let fc = FolderCompare::compare(
        fl,
        &VPath::root(),
        arc(r),
        &VPath::root(),
        FolderRules::default(),
        &CancelToken::new(),
    )
    .unwrap();
    assert_eq!(status(&fc, "bad"), Status::Error);
    assert!(fc.errors.contains_key(&find(&fc, "bad")));
    assert_eq!(status(&fc, "good/y"), Status::Same);
    assert_eq!(fc.tree.node(Tree::ROOT).counters.error, 1);
}

#[test]
fn unreadable_root_fails_the_session() {
    let (l, r) = (mem(), mem());
    l.add_file("a", b"1", T0);
    let fl = Arc::new(FaultVfs::new(l));
    fl.fail_nth(Op::List, 1, ErrorKind::AccessDenied);
    let e = FolderCompare::compare(
        fl,
        &VPath::root(),
        arc(r),
        &VPath::root(),
        FolderRules::default(),
        &CancelToken::new(),
    )
    .unwrap_err();
    assert_eq!(e.kind(), ErrorKind::AccessDenied);
}

#[test]
fn folder_that_vanishes_mid_scan_becomes_an_orphan_not_an_error() {
    let (l, r) = (mem(), mem());
    for m in [&l, &r] {
        m.add_file("d/x", b"1", T0);
    }
    let fl = Arc::new(FaultVfs::new(l));
    fl.fail_nth(Op::List, 2, ErrorKind::NotFound);
    let fc = FolderCompare::compare(
        fl,
        &VPath::root(),
        arc(r),
        &VPath::root(),
        FolderRules::default(),
        &CancelToken::new(),
    )
    .unwrap();
    assert_eq!(status(&fc, "d/x"), Status::RightOrphan);
    // the folder itself is an orphan too, not a "same" folder (sync must not copy into it)
    assert_eq!(status(&fc, "d"), Status::RightOrphan);
    assert!(!fc.tree.node(find(&fc, "d")).left.present);
    assert_eq!(fc.tree.node(Tree::ROOT).counters.error, 0);
    assert_eq!(
        fc.tree.recompute_counters()[0],
        fc.tree.node(Tree::ROOT).counters
    );
}

#[test]
fn missing_or_file_roots_are_rejected() {
    let (l, r) = (mem(), mem());
    l.add_file("f", b"1", T0);
    let go = |lp: &str| {
        FolderCompare::compare(
            arc_clone(&l),
            &VPath::from_parts([lp]),
            arc(r.clone()),
            &VPath::root(),
            FolderRules::default(),
            &CancelToken::new(),
        )
    };
    assert_eq!(go("f").unwrap_err().kind(), ErrorKind::InvalidArgument);
    assert_eq!(go("nope").unwrap_err().kind(), ErrorKind::NotFound);
}

fn arc_clone(m: &MemVfs) -> Arc<dyn Vfs> {
    Arc::new(m.clone())
}

#[test]
fn cancelled_scan_stops() {
    let (l, r) = (mem(), mem());
    l.add_file("a", b"1", T0);
    let cancel = CancelToken::new();
    cancel.cancel();
    let e = FolderCompare::compare(
        arc(l),
        &VPath::root(),
        arc(r),
        &VPath::root(),
        FolderRules::default(),
        &cancel,
    )
    .unwrap_err();
    assert!(e.is_cancelled());
}

#[test]
fn links_are_listed_but_never_descended() {
    let (l, r) = (mem(), mem());
    l.add_dir("real", T0)
        .add_file("real/x", b"1", T0)
        .add_link("lnk", Kind::Dir);
    r.add_dir("real", T0)
        .add_file("real/x", b"1", T0)
        .add_link("lnk", Kind::Dir);
    let fc = run(&arc(l), &arc(r), FolderRules::default());
    let n = fc.tree.node(find(&fc, "lnk"));
    assert!(n.has(flag::LINK));
    assert_eq!(n.child_count, 0);
}

// ---------------------------------------------------------------- content (E7-S7)

fn content_rules(kind: ContentKind) -> FolderRules {
    FolderRules {
        compare_content: true,
        content_kind: kind,
        ..FolderRules::default()
    }
}

#[test]
fn rules_based_crlf_vs_lf_is_unimportant() {
    let (l, r) = (mem(), mem());
    l.add_file("f.txt", b"a\r\nb\r\n", T0);
    r.add_file("f.txt", b"a\nb\n", T0 + 60 * S);
    let fc = run(&arc(l), &arc(r), content_rules(ContentKind::RulesBased));
    assert_eq!(status(&fc, "f.txt"), Status::UnimportantDiff);
    assert!(fc.tree.node(find(&fc, "f.txt")).has(flag::CONTENT_COMPARED));
    assert!(!fc.tree.node(find(&fc, "f.txt")).has(flag::PENDING_CONTENT));
    assert_eq!(fc.tree.node(Tree::ROOT).counters.diff, 1);
}

#[test]
fn content_overrides_decides_between_time_and_content() {
    let build = || {
        let (l, r) = (mem(), mem());
        l.add_file("f.txt", b"same\n", T0);
        r.add_file("f.txt", b"same\n", T0 + 3600 * S);
        (arc(l), arc(r))
    };
    let (l, r) = build();
    // same content, different time: override → Same
    assert_eq!(
        status(&run(&l, &r, content_rules(ContentKind::Binary)), "f.txt"),
        Status::Same
    );
    let no = FolderRules {
        content_overrides: false,
        ..content_rules(ContentKind::Binary)
    };
    assert_eq!(status(&run(&l, &r, no), "f.txt"), Status::RightNewer);
}

#[test]
fn different_content_with_equal_times_is_different_and_newer_side_otherwise() {
    let (l, r) = (mem(), mem());
    l.add_file("eq.txt", b"aaaa", T0)
        .add_file("new.txt", b"aaaa", T0 + 100 * S);
    r.add_file("eq.txt", b"bbbb", T0)
        .add_file("new.txt", b"bbbb", T0);
    let fc = run(&arc(l), &arc(r), content_rules(ContentKind::Crc32));
    assert_eq!(status(&fc, "eq.txt"), Status::Different);
    assert_eq!(status(&fc, "new.txt"), Status::LeftNewer);
}

#[test]
fn crc32_and_binary_agree_on_equal_and_different_content() {
    for kind in [ContentKind::Crc32, ContentKind::Binary] {
        let (l, r) = (mem(), mem());
        let big: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
        let mut big2 = big.clone();
        big2[2_900_000] ^= 1;
        l.add_file("same", &big, T0).add_file("diff", &big, T0);
        r.add_file("same", &big, T0 + 100 * S)
            .add_file("diff", &big2, T0);
        let fc = run(&arc(l), &arc(r), content_rules(kind));
        assert_eq!(status(&fc, "same"), Status::Same, "{kind:?}");
        assert_eq!(status(&fc, "diff"), Status::Different, "{kind:?}");
    }
}

/// Golden row: a size mismatch under Binary means different without reading.
#[test]
fn size_mismatch_under_binary_reads_nothing() {
    let (l, r) = (mem(), mem());
    l.add_file("f", b"abc", T0);
    r.add_file("f", b"abcd", T0);
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
    assert_eq!(status(&fc, "f"), Status::Different);
    assert_eq!((fl.count(Op::OpenRead), fr.count(Op::OpenRead)), (0, 0));
}

#[test]
fn skip_same_does_not_read_when_the_quick_test_says_same() {
    let (l, r) = (mem(), mem());
    l.add_file("f", b"abc", T0);
    r.add_file("f", b"abc", T0);
    let (fl, fr) = (Arc::new(FaultVfs::new(l)), Arc::new(FaultVfs::new(r)));
    let rules = FolderRules {
        content_skip_same: true,
        ..content_rules(ContentKind::Binary)
    };
    let fc = FolderCompare::compare(
        fl.clone(),
        &VPath::root(),
        fr.clone(),
        &VPath::root(),
        rules,
        &CancelToken::new(),
    )
    .unwrap();
    assert_eq!(status(&fc, "f"), Status::Same);
    assert_eq!(fl.count(Op::OpenRead), 0);
}

#[test]
fn offline_files_are_not_read_by_default_d17() {
    let (l, r) = (mem(), mem());
    l.add_file("cloud.txt", b"abc", T0);
    r.add_file("cloud.txt", b"abd", T0);
    l.set_raw_attrs("cloud.txt", attr::OFFLINE | attr::ARCHIVE);
    let (l, r) = (Arc::new(FaultVfs::new(l)), Arc::new(FaultVfs::new(r)));
    let fc = FolderCompare::compare(
        l.clone(),
        &VPath::root(),
        r.clone(),
        &VPath::root(),
        content_rules(ContentKind::Binary),
        &CancelToken::new(),
    )
    .unwrap();
    assert_eq!(l.count(Op::OpenRead), 0);
    assert!(fc.tree.node(find(&fc, "cloud.txt")).has(flag::OFFLINE));
    let rules = FolderRules {
        compare_offline_files: true,
        ..content_rules(ContentKind::Binary)
    };
    let fc = FolderCompare::compare(
        l.clone(),
        &VPath::root(),
        r.clone(),
        &VPath::root(),
        rules,
        &CancelToken::new(),
    )
    .unwrap();
    assert_eq!(status(&fc, "cloud.txt"), Status::Different);
    assert_eq!(l.count(Op::OpenRead), 1);
}

#[test]
fn a_file_that_changes_during_the_compare_is_an_error() {
    let (l, r) = (mem(), mem());
    l.add_file("f", b"abc", T0);
    r.add_file("f", b"abc", T0);
    let fl = Arc::new(FaultVfs::new(l.clone()));
    let l2 = l.clone();
    fl.on_nth(
        Op::OpenRead,
        1,
        Arc::new(move |_| {
            l2.add_file("f", b"abcdef", T0 + 5 * S);
        }),
    );
    let fc = FolderCompare::compare(
        fl,
        &VPath::root(),
        arc(r),
        &VPath::root(),
        content_rules(ContentKind::Binary),
        &CancelToken::new(),
    )
    .unwrap();
    assert_eq!(status(&fc, "f"), Status::Error);
    assert!(fc.errors[&find(&fc, "f")].contains("changed during compare"));
}

#[test]
fn content_worker_limit_is_respected() {
    let (l, r) = (mem(), mem());
    for i in 0..40 {
        l.add_file(&format!("f{i}"), b"abc", T0);
        r.add_file(&format!("f{i}"), b"abd", T0);
    }
    let active = Arc::new(Mutex::new((0usize, 0usize)));
    let fl = Arc::new(FaultVfs::new(l));
    let a = active.clone();
    for n in 1..=40 {
        let a = a.clone();
        fl.on_nth(
            Op::OpenRead,
            n,
            Arc::new(move |_| {
                let mut g = a.lock().unwrap();
                g.0 += 1;
                g.1 = g.1.max(g.0);
                drop(g);
                std::thread::sleep(std::time::Duration::from_millis(5));
                a.lock().unwrap().0 -= 1;
            }),
        );
    }
    let rules = FolderRules {
        content_workers: 2,
        ..content_rules(ContentKind::Binary)
    };
    let fc = FolderCompare::compare(
        fl,
        &VPath::root(),
        arc(r),
        &VPath::root(),
        rules,
        &CancelToken::new(),
    )
    .unwrap();
    assert_eq!(fc.tree.node(Tree::ROOT).counters.diff, 40);
    assert!(
        active.lock().unwrap().1 <= 2,
        "at most 2 concurrent content compares"
    );
}

// ---------------------------------------------------------------- refresh / compare now (E7-S9)

#[test]
fn refresh_reclassifies_a_node_and_its_ancestors() {
    let (l, r) = (mem(), mem());
    l.add_file("d/f", b"abc", T0);
    r.add_file("d/f", b"abc", T0);
    let (lc, rc) = (l.clone(), r.clone());
    let mut fc = run(&arc(l), &arc(r), FolderRules::default());
    let id = find(&fc, "d/f");
    assert_eq!(fc.tree.node(find(&fc, "d")).counters.diff, 0);
    // modify behind the compare's back
    rc.add_file("d/f", b"abcdef", T0 + 50 * S);
    fc.refresh(&[id], &CancelToken::new()).unwrap();
    assert_eq!(fc.tree.node(id).status, Status::RightNewer);
    assert_eq!(fc.tree.node(find(&fc, "d")).counters.right_newer, 1);
    assert_eq!(fc.tree.node(Tree::ROOT).counters.right_newer, 1);
    // deleting one side makes it an orphan
    lc.add_file("d/other", b"1", T0);
    let kept = fc.tree.node(id).left.size;
    assert_eq!(kept, 3);
    core_vfs::Vfs::delete(
        &lc,
        &VPath::from_parts(["d", "f"]),
        core_vfs::DeleteMode::Permanent,
    )
    .unwrap();
    fc.refresh(&[id], &CancelToken::new()).unwrap();
    assert_eq!(fc.tree.node(id).status, Status::RightOrphan);
    assert_eq!(
        fc.tree.recompute_counters()[0],
        fc.tree.node(Tree::ROOT).counters
    );
}

#[test]
fn compare_content_now_works_when_the_rules_have_it_off() {
    let (l, r) = (mem(), mem());
    l.add_file("f", b"a\r\n", T0);
    r.add_file("f", b"a\n", T0);
    let mut fc = run(&arc(l), &arc(r), FolderRules::default());
    let id = find(&fc, "f");
    assert_eq!(fc.tree.node(id).status, Status::Different, "size differs");
    fc.compare_content_now(&[id], &CancelToken::new()).unwrap();
    assert_eq!(fc.tree.node(id).status, Status::UnimportantDiff);
    assert!(fc.tree.node(id).has(flag::CONTENT_COMPARED));
}

// ---------------------------------------------------------------- roots (E7-S10)

#[test]
fn same_and_nested_roots_on_a_real_disk() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("a/b")).unwrap();
    std::fs::write(dir.path().join("a/f.txt"), b"1").unwrap();
    std::fs::write(dir.path().join("a/b/g.txt"), b"1").unwrap();
    let outer = local(dir.path());
    let a = local(&dir.path().join("a"));
    let ab = local(&dir.path().join("a/b"));
    let go = |l: &Arc<dyn Vfs>, lp: &VPath, r: &Arc<dyn Vfs>, rp: &VPath| {
        FolderCompare::compare(
            l.clone(),
            lp,
            r.clone(),
            rp,
            FolderRules::default(),
            &CancelToken::new(),
        )
    };
    let e = go(&a, &VPath::root(), &a, &VPath::root()).unwrap_err();
    assert_eq!(e.kind(), ErrorKind::InvalidArgument, "same folder");
    // the same folder through two different roots
    let e = go(&outer, &VPath::from_parts(["a"]), &a, &VPath::root()).unwrap_err();
    assert_eq!(e.kind(), ErrorKind::InvalidArgument, "alias of one folder");
    let fc = go(&a, &VPath::root(), &ab, &VPath::root()).unwrap();
    assert!(
        fc.sync_blocked,
        "nested roots: compare proceeds, sync is blocked"
    );
    assert_eq!(fc.warnings.len(), 1);
    let fc = go(
        &outer,
        &VPath::from_parts(["a", "b"]),
        &a,
        &VPath::from_parts(["b"]),
    );
    assert!(fc.is_err());
}

/// A backend that knows file IDs (LocalStd has none on Windows).
fn local(path: &std::path::Path) -> Arc<dyn Vfs> {
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

#[test]
fn disjoint_roots_are_not_blocked() {
    let (d1, d2) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (l, r) = (local(d1.path()), local(d2.path()));
    let fc = FolderCompare::compare(
        l,
        &VPath::root(),
        r,
        &VPath::root(),
        FolderRules::default(),
        &CancelToken::new(),
    )
    .unwrap();
    assert!(!fc.sync_blocked && fc.warnings.is_empty());
}

/// Decision D31: links are compared by presence only; a link against a non-link differs.
#[test]
fn link_against_file_differs_and_link_against_link_is_same() {
    let (l, r) = (mem(), mem());
    l.add_link("both", Kind::File).add_link("mixed", Kind::File);
    r.add_link("both", Kind::File).add_file("mixed", b"x", T0);
    let fc = run(&arc(l), &arc(r), FolderRules::default());
    assert_eq!(status(&fc, "both"), Status::Same);
    assert_eq!(status(&fc, "mixed"), Status::Different);
}

#[test]
fn progress_reports_scanning_then_comparing_then_done() {
    use core_base::Stage;
    let (l, r) = (mem(), mem());
    for m in [&l, &r] {
        m.add_file("a/x", b"abc", T0).add_file("b/y", b"abc", T0);
    }
    let seen = Mutex::new(Vec::new());
    FolderCompare::compare_with_progress(
        arc(l),
        &VPath::root(),
        arc(r),
        &VPath::root(),
        content_rules(ContentKind::Binary),
        &CancelToken::new(),
        &|p| seen.lock().unwrap().push(p),
    )
    .unwrap();
    let seen = seen.into_inner().unwrap();
    let scans: Vec<_> = seen.iter().filter(|p| p.stage == Stage::Scanning).collect();
    assert_eq!(scans.len(), 3, "root, a, b");
    assert_eq!((scans[2].done, scans[2].total), (3, 3));
    assert!(
        seen.iter()
            .any(|p| p.stage == Stage::Comparing && p.done == 2 && p.total == 2)
    );
    assert_eq!(seen.last().map(|p| p.stage), Some(Stage::Done));
}

/// Backlog E7-S3: both sides of a folder are listed at the same time.
#[test]
fn both_sides_are_listed_concurrently() {
    let (l, r) = (mem(), mem());
    l.add_file("f", b"1", T0);
    r.add_file("f", b"1", T0);
    let gate = Arc::new(std::sync::Barrier::new(2));
    let (fl, fr) = (Arc::new(FaultVfs::new(l)), Arc::new(FaultVfs::new(r)));
    for f in [&fl, &fr] {
        let g = gate.clone();
        f.on_nth(
            Op::List,
            1,
            Arc::new(move |_| {
                // both root listings must be in flight together, or this never returns
                g.wait();
            }),
        );
    }
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let d = done.clone();
    let h = std::thread::spawn(move || {
        FolderCompare::compare(
            fl,
            &VPath::root(),
            fr,
            &VPath::root(),
            FolderRules::default(),
            &CancelToken::new(),
        )
        .unwrap();
        d.store(true, std::sync::atomic::Ordering::SeqCst);
    });
    for _ in 0..200 {
        if done.load(std::sync::atomic::Ordering::SeqCst) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    assert!(
        done.load(std::sync::atomic::Ordering::SeqCst),
        "listings are sequential: the barrier was never released"
    );
    h.join().unwrap();
}

/// Backlog E7-S3.3: a cancel is noticed within 100 ms, even in the middle of a slow scan.
#[test]
fn cancel_during_a_slow_scan_returns_quickly() {
    let (l, r) = (mem(), mem());
    for i in 0..200 {
        l.add_file(&format!("d{i}/f"), b"1", T0);
        r.add_file(&format!("d{i}/f"), b"1", T0);
    }
    let (fl, fr) = (Arc::new(FaultVfs::new(l)), Arc::new(FaultVfs::new(r)));
    fl.delay_each_op(std::time::Duration::from_millis(10));
    fr.delay_each_op(std::time::Duration::from_millis(10));
    let cancel = CancelToken::new();
    let c = cancel.clone();
    let canceller = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(100));
        c.cancel();
        std::time::Instant::now()
    });
    let res = FolderCompare::compare(
        fl,
        &VPath::root(),
        fr,
        &VPath::root(),
        FolderRules::default(),
        &cancel,
    );
    let returned = std::time::Instant::now();
    let cancelled_at = canceller.join().unwrap();
    assert!(res.unwrap_err().is_cancelled());
    assert!(returned.duration_since(cancelled_at) < std::time::Duration::from_millis(100));
}

/// perf (plan §12.2): files of 8 MiB and more are read on both sides at once; the result must
/// be the same as the serial path: equal, a difference in the last block, a difference in the
/// first block (early stop), and a left file that is a prefix of the right one.
#[test]
fn big_files_use_the_overlapped_readers_with_the_same_results() {
    const N: usize = 9 << 20;
    let data: Vec<u8> = (0..N as u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8)
        .collect();
    let mut last = data.clone();
    last[N - 1] ^= 1;
    let mut first = data.clone();
    first[10] ^= 1;
    for kind in [
        ContentKind::Binary,
        ContentKind::Crc32,
        ContentKind::RulesBased,
    ] {
        let (l, r) = (mem(), mem());
        l.add_file("same", &data, T0)
            .add_file("last", &data, T0)
            .add_file("first", &data, T0)
            .add_file("len", &data, T0);
        r.add_file("same", &data, T0 + 5 * S)
            .add_file("last", &last, T0)
            .add_file("first", &first, T0)
            .add_file("len", &data[..N - 3], T0);
        let mut rules = content_rules(kind);
        rules.compare_size = false; // the size shortcut would hide the prefix case
        let fc = run(&arc(l), &arc(r), rules);
        assert_eq!(status(&fc, "same"), Status::Same, "{kind:?}");
        assert_eq!(status(&fc, "last"), Status::Different, "{kind:?}");
        assert_eq!(status(&fc, "first"), Status::Different, "{kind:?}");
        assert_eq!(status(&fc, "len"), Status::Different, "{kind:?}");
    }
}

#[test]
fn a_cancelled_overlapped_compare_stops_without_hanging() {
    const N: usize = 9 << 20;
    let (l, r) = (mem(), mem());
    l.add_file("f", &vec![7u8; N], T0);
    r.add_file("f", &vec![7u8; N], T0 + 5 * S);
    let mut rules = content_rules(ContentKind::Binary);
    rules.compare_content = true;
    let cancel = CancelToken::new();
    let mut fc = FolderCompare::compare(
        arc(l),
        &VPath::root(),
        arc(r),
        &VPath::root(),
        rules,
        &cancel,
    )
    .unwrap();
    cancel.cancel();
    let id = find(&fc, "f");
    let err = fc.compare_content_now(&[id], &cancel);
    assert!(
        err.is_err_and(|e| e.is_cancelled()),
        "a cancel ends the content compare"
    );
}

/// Reviewer finding (E11-S6): a read failure on either side of a big pair (the overlapped
/// readers) marks that file as an error and ends the compare cleanly, for both content kinds.
/// Each side has its own `FaultVfs`, so the failing open is the same one every run.
#[test]
fn a_read_failure_on_either_side_of_a_big_pair_is_an_error_for_that_file() {
    const N: usize = 9 << 20;
    let data = vec![3u8; N];
    for kind in [ContentKind::Binary, ContentKind::Crc32] {
        for failing_side in [0, 1] {
            let (l, r) = (mem(), mem());
            l.add_file("f", &data, T0);
            r.add_file("f", &data, T0);
            let (fl, fr) = (Arc::new(FaultVfs::new(l)), Arc::new(FaultVfs::new(r)));
            // one file only: content compares run in parallel, so with two files the first open of
            // the failing side would be either of them
            let bad = if failing_side == 0 { &fl } else { &fr };
            bad.fail_nth(Op::OpenRead, 1, ErrorKind::Io);
            let fc = FolderCompare::compare(
                fl.clone(),
                &VPath::root(),
                fr.clone(),
                &VPath::root(),
                content_rules(kind),
                &CancelToken::new(),
            )
            .unwrap();
            let what = format!("{kind:?}, side {failing_side}");
            assert_eq!(status(&fc, "f"), Status::Error, "{what}");
        }
    }
}

/// A cancel that arrives while big files are being compared ends the run without a hang, and the
/// result is either "cancelled" or a finished compare (the race can go both ways).
#[test]
fn a_cancel_in_the_middle_of_big_content_compares_does_not_hang() {
    const N: usize = 24 << 20;
    for kind in [ContentKind::Binary, ContentKind::Crc32] {
        let (l, r) = (mem(), mem());
        for i in 0..6 {
            let data = vec![i as u8; N];
            l.add_file(&format!("f{i}"), &data, T0);
            r.add_file(&format!("f{i}"), &data, T0);
        }
        let cancel = CancelToken::new();
        let c2 = cancel.clone();
        let stopper = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(3));
            c2.cancel();
        });
        let started = std::time::Instant::now();
        let res = FolderCompare::compare(
            arc(l),
            &VPath::root(),
            arc(r),
            &VPath::root(),
            content_rules(kind),
            &cancel,
        );
        stopper.join().unwrap();
        assert!(
            started.elapsed() < std::time::Duration::from_secs(20),
            "{kind:?}: hang"
        );
        if let Err(e) = res {
            assert!(e.is_cancelled(), "{kind:?}: {e:?}");
        }
    }
}
