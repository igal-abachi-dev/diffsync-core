//! The view modes of the folder compare (backlog E11.5-S14.5): every filter of the reference
//! products, "ignore unimportant differences", and items the user marked as ignored.

use std::sync::Arc;

use core_base::CancelToken;
use core_base::ticks::TICKS_PER_SECOND as S;
use core_folder::{FolderCompare, FolderRules, FolderView, NodeId, Status, ViewFilter};
use core_vfs::{MemVfs, VPath, Vfs};

const T0: i64 = 1_000_000 * S;

fn compare() -> FolderCompare {
    let (l, r) = (MemVfs::new(), MemVfs::new());
    l.add_file("same.txt", b"abc", T0)
        .add_file("diff.txt", b"abc", T0)
        .add_file("lnew.txt", b"abc", T0 + 10 * S)
        .add_file("rnew.txt", b"abc", T0)
        .add_file("only_l.txt", b"x", T0)
        .add_dir("sub", T0)
        .add_file("sub/inner.txt", b"1", T0);
    r.add_file("same.txt", b"abc", T0)
        .add_file("diff.txt", b"abcd", T0)
        .add_file("lnew.txt", b"abcd", T0)
        .add_file("rnew.txt", b"abcd", T0 + 10 * S)
        .add_file("only_r.txt", b"x", T0)
        .add_dir("sub", T0)
        .add_file("sub/inner.txt", b"1", T0);
    let (l, r): (Arc<dyn Vfs>, Arc<dyn Vfs>) = (Arc::new(l), Arc::new(r));
    FolderCompare::compare(
        l,
        &VPath::root(),
        r,
        &VPath::root(),
        FolderRules::default(),
        &CancelToken::new(),
    )
    .unwrap()
}

fn names(fc: &FolderCompare, v: &FolderView) -> Vec<String> {
    let mut out: Vec<String> = v
        .get_rows(0, v.len())
        .iter()
        .filter(|&&id| !fc.tree.node(id).is_dir())
        .map(|&id| fc.tree.path(id).display())
        .collect();
    out.sort();
    out
}

fn shown(fc: &FolderCompare, f: ViewFilter) -> Vec<String> {
    let mut v = FolderView::new();
    v.rebuild(&fc.tree);
    v.expand_all(&fc.tree);
    v.set_filter(&fc.tree, f);
    v.expand_all(&fc.tree);
    names(fc, &v)
}

#[test]
fn every_filter_mode_shows_the_right_files() {
    let fc = compare();
    let all = [
        "diff.txt",
        "lnew.txt",
        "only_l.txt",
        "only_r.txt",
        "rnew.txt",
        "same.txt",
        "sub/inner.txt",
    ];
    assert_eq!(shown(&fc, ViewFilter::All), all);
    assert_eq!(
        shown(&fc, ViewFilter::Differences),
        [
            "diff.txt",
            "lnew.txt",
            "only_l.txt",
            "only_r.txt",
            "rnew.txt"
        ]
    );
    assert_eq!(shown(&fc, ViewFilter::Same), ["same.txt", "sub/inner.txt"]);
    assert_eq!(
        shown(&fc, ViewFilter::Orphans),
        ["only_l.txt", "only_r.txt"]
    );
    assert_eq!(
        shown(&fc, ViewFilter::NoOrphans),
        [
            "diff.txt",
            "lnew.txt",
            "rnew.txt",
            "same.txt",
            "sub/inner.txt"
        ]
    );
    assert_eq!(
        shown(&fc, ViewFilter::DifferencesNoOrphans),
        ["diff.txt", "lnew.txt", "rnew.txt"]
    );
    assert_eq!(shown(&fc, ViewFilter::LeftOrphans), ["only_l.txt"]);
    assert_eq!(shown(&fc, ViewFilter::RightOrphans), ["only_r.txt"]);
    assert_eq!(shown(&fc, ViewFilter::LeftNewer), ["lnew.txt"]);
    assert_eq!(shown(&fc, ViewFilter::RightNewer), ["rnew.txt"]);
    assert_eq!(
        shown(&fc, ViewFilter::LeftNewerOrphans),
        ["lnew.txt", "only_l.txt", "only_r.txt"]
    );
    assert_eq!(
        shown(&fc, ViewFilter::RightNewerOrphans),
        ["only_l.txt", "only_r.txt", "rnew.txt"]
    );
}

#[test]
fn unimportant_differences_can_count_as_equal() {
    let mut fc = compare();
    let id = (1..fc.tree.len() as NodeId)
        .find(|&i| fc.tree.path(i).display() == "diff.txt")
        .unwrap();
    fc.tree.node_mut(id).status = Status::UnimportantDiff;
    let mut v = FolderView::new();
    v.set_filter(&fc.tree, ViewFilter::Differences);
    assert!(names(&fc, &v).contains(&"diff.txt".to_string()));
    v.set_ignore_unimportant(&fc.tree, true);
    assert!(!names(&fc, &v).contains(&"diff.txt".to_string()));
    v.set_filter(&fc.tree, ViewFilter::Same);
    assert!(names(&fc, &v).contains(&"diff.txt".to_string()));
}

#[test]
fn an_ignored_item_and_what_is_below_it_are_hidden_until_asked_for() {
    let mut fc = compare();
    let find = |fc: &FolderCompare, p: &str| {
        (1..fc.tree.len() as NodeId)
            .find(|&i| fc.tree.path(i).display() == p)
            .unwrap()
    };
    let (sub, one) = (find(&fc, "sub"), find(&fc, "only_l.txt"));
    fc.set_ignored(sub, true).unwrap();
    fc.set_ignored(one, true).unwrap();
    assert!(fc.set_ignored(0, true).is_err());
    let mut v = FolderView::new();
    v.expand_all(&fc.tree);
    v.rebuild(&fc.tree);
    v.expand_all(&fc.tree);
    let n = names(&fc, &v);
    assert!(!n.contains(&"sub/inner.txt".to_string()) && !n.contains(&"only_l.txt".to_string()));
    assert!(v.get_rows(0, v.len()).iter().all(|&i| i != sub));
    v.set_show_ignored(&fc.tree, true);
    v.expand_all(&fc.tree);
    assert!(names(&fc, &v).contains(&"sub/inner.txt".to_string()));
    fc.set_ignored(sub, false).unwrap();
    v.set_show_ignored(&fc.tree, false);
    v.expand_all(&fc.tree);
    assert!(names(&fc, &v).contains(&"sub/inner.txt".to_string()));
}
