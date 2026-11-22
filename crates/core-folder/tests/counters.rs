//! Incremental folder counters equal a full recompute (backlog E7-S8.1, plan.md §7.2).

use core_folder::tree::{Counters, NONE, Node, SideInfo, Status, Tree};
use proptest::prelude::*;

const STATUSES: [Status; 8] = [
    Status::Same,
    Status::UnimportantDiff,
    Status::Different,
    Status::LeftNewer,
    Status::RightNewer,
    Status::LeftOrphan,
    Status::RightOrphan,
    Status::Error,
];

fn leaf(status: Status) -> Node {
    Node {
        parent: NONE,
        first_child: NONE,
        child_count: 0,
        depth: 0,
        status,
        flags: 0,
        left: SideInfo {
            present: true,
            ..SideInfo::ABSENT
        },
        right: SideInfo {
            present: true,
            ..SideInfo::ABSENT
        },
        counters: Counters::default(),
    }
}

proptest! {
    /// Random trees (children created breadth first, all at once) and random status changes.
    #[test]
    fn incremental_equals_recompute(
        shape in proptest::collection::vec(0usize..6, 1..40),
        initial in proptest::collection::vec(0usize..8, 200),
        changes in proptest::collection::vec((0usize..1000, 0usize..8), 0..200),
    ) {
        let mut t = Tree::new();
        let mut next = 0usize;
        let mut queue = std::collections::VecDeque::from([Tree::ROOT]);
        for &kids in &shape {
            let Some(parent) = queue.pop_front() else { break };
            if kids == 0 { continue; }
            let nodes: Vec<Node> = (0..kids).map(|_| { next += 1; leaf(STATUSES[initial[next % initial.len()]]) }).collect();
            let first = t.add_children(parent, nodes);
            queue.extend(first..first + kids as u32);
        }
        for (i, s) in changes {
            if t.len() > 1 {
                t.set_status(1 + (i % (t.len() - 1)) as u32, STATUSES[s]);
            }
            let fresh = t.recompute_counters();
            for (id, want) in fresh.iter().enumerate() {
                prop_assert_eq!(*want, t.node(id as u32).counters, "node {}", id);
            }
        }
    }
}
