//! Expand/collapse splicing equals a full rebuild (backlog E7-S11, reviewer finding).

use core_folder::tree::{Counters, NONE, Node, SideInfo, Status, Tree};
use core_folder::{FolderView, ViewFilter};
use proptest::prelude::*;

const STATUSES: [Status; 4] = [
    Status::Same,
    Status::Different,
    Status::LeftOrphan,
    Status::LeftNewer,
];

fn node(status: Status, dir: bool) -> Node {
    let side = SideInfo {
        present: true,
        kind: if dir {
            core_vfs::Kind::Dir
        } else {
            core_vfs::Kind::File
        },
        ..SideInfo::ABSENT
    };
    Node {
        parent: NONE,
        first_child: NONE,
        child_count: 0,
        depth: 0,
        status,
        flags: 0,
        left: side,
        right: side,
        counters: Counters::default(),
    }
}

fn build(shape: &[usize], statuses: &[usize]) -> Tree {
    let mut t = Tree::new();
    let mut queue = std::collections::VecDeque::from([Tree::ROOT]);
    let mut k = 0usize;
    for &kids in shape {
        let Some(parent) = queue.pop_front() else {
            break;
        };
        if kids == 0 {
            continue;
        }
        let nodes: Vec<Node> = (0..kids)
            .map(|_| {
                k += 1;
                node(STATUSES[statuses[k % statuses.len()]], k % 3 != 0)
            })
            .collect();
        let first = t.add_children(parent, nodes);
        queue.extend(first..first + kids as u32);
    }
    t
}

proptest! {
    #[test]
    fn splice_equals_rebuild(
        shape in proptest::collection::vec(0usize..5, 1..30),
        statuses in proptest::collection::vec(0usize..4, 50),
        filter in 0usize..3,
        ops in proptest::collection::vec((any::<bool>(), 0usize..64), 0..40),
    ) {
        let t = build(&shape, &statuses);
        let mut v = FolderView::new();
        v.set_filter(&t, [ViewFilter::All, ViewFilter::Differences, ViewFilter::Orphans][filter]);
        for (expand, row) in ops {
            if v.is_empty() {
                break;
            }
            let row = row % v.len();
            if expand {
                v.expand_row(&t, row);
            } else {
                v.collapse_row(&t, row);
            }
            let mut fresh = v.clone();
            fresh.rebuild(&t);
            prop_assert_eq!(v.get_rows(0, usize::MAX), fresh.get_rows(0, usize::MAX));
        }
        // reveal and row_of agree
        for id in 1..t.len() as u32 {
            if let Some(row) = v.reveal(&t, id) {
                prop_assert_eq!(v.get_rows(row, 1), &[id][..]);
            }
        }
    }
}
