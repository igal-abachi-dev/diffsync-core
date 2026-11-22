//! The arena tree (plan.md §7.2, backlog E7-S2, E7-S8): `Vec<Node>` with `u32` ids; each
//! folder's children are one contiguous range. Folder counters are updated **incrementally**
//! on every status change by walking up to the root.

use core_base::Ticks;
use core_vfs::{Kind, Name, VPath};

pub type NodeId = u32;
/// No node (parent of the root, "no children").
pub const NONE: NodeId = u32::MAX;

/// The result of comparing one item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Status {
    Same = 0,
    /// Differs only in unimportant ways (rules-based content compare).
    UnimportantDiff = 1,
    Different = 2,
    LeftNewer = 3,
    RightNewer = 4,
    LeftOrphan = 5,
    RightOrphan = 6,
    Error = 7,
}

/// Node flag bits.
pub mod flag {
    /// Content compare is queued.
    pub const PENDING_CONTENT: u16 = 1;
    pub const CONTENT_COMPARED: u16 = 2;
    /// A file on one side and a folder of the same name on the other (plan.md §9.3).
    pub const TYPE_CONFLICT: u16 = 4;
    /// Several names on one side mapped to this key.
    pub const DUPLICATE_KEY: u16 = 8;
    pub const OFFLINE: u16 = 16;
    pub const LINK: u16 = 32;
    pub const FILTERED_CHILDREN: u16 = 64;
    /// The item no longer exists on either side (after a sync or a refresh); the arena keeps
    /// the node so ids stay valid, and views skip it.
    pub const GONE: u16 = 128;
}

/// What one side knows about an item. 32 bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SideInfo {
    pub size: u64,
    pub mtime: Ticks,
    pub attrs: u32,
    pub name_off: u32,
    pub name_len: u16,
    pub kind: Kind,
    pub present: bool,
}

impl SideInfo {
    pub const ABSENT: SideInfo = SideInfo {
        size: 0,
        mtime: 0,
        attrs: 0,
        name_off: 0,
        name_len: 0,
        kind: Kind::File,
        present: false,
    };
}

/// Counts of the files and orphans below a folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Counters {
    /// Different or unimportantly different items.
    pub diff: u32,
    pub left_newer: u32,
    pub right_newer: u32,
    pub left_orphan: u32,
    pub right_orphan: u32,
    pub error: u32,
    /// All items below (files and folders).
    pub items: u32,
}

impl Counters {
    fn add(&mut self, o: &Counters, sign: i64) {
        let f = |a: &mut u32, b: u32| *a = (i64::from(*a) + sign * i64::from(b)) as u32;
        f(&mut self.diff, o.diff);
        f(&mut self.left_newer, o.left_newer);
        f(&mut self.right_newer, o.right_newer);
        f(&mut self.left_orphan, o.left_orphan);
        f(&mut self.right_orphan, o.right_orphan);
        f(&mut self.error, o.error);
        f(&mut self.items, o.items);
    }

    /// True when anything below differs.
    pub fn has_differences(&self) -> bool {
        self.diff
            + self.left_newer
            + self.right_newer
            + self.left_orphan
            + self.right_orphan
            + self.error
            > 0
    }
}

/// The contribution of one node with `status` to its ancestors' counters.
fn contribution(status: Status) -> Counters {
    let mut c = Counters {
        items: 1,
        ..Counters::default()
    };
    match status {
        Status::Same => {}
        Status::UnimportantDiff | Status::Different => c.diff = 1,
        Status::LeftNewer => c.left_newer = 1,
        Status::RightNewer => c.right_newer = 1,
        Status::LeftOrphan => c.left_orphan = 1,
        Status::RightOrphan => c.right_orphan = 1,
        Status::Error => c.error = 1,
    }
    c
}

/// One item of the compare (a file, folder or link present on either side).
#[derive(Debug, Clone)]
pub struct Node {
    pub parent: NodeId,
    pub first_child: NodeId,
    pub child_count: u32,
    pub depth: u16,
    pub status: Status,
    pub flags: u16,
    pub left: SideInfo,
    pub right: SideInfo,
    pub counters: Counters,
}

impl Node {
    pub fn is_dir(&self) -> bool {
        (self.left.present && self.left.kind == Kind::Dir)
            || (self.right.present && self.right.kind == Kind::Dir)
    }

    pub fn has(&self, f: u16) -> bool {
        self.flags & f != 0
    }

    /// The children as an id range.
    pub fn children(&self) -> std::ops::Range<NodeId> {
        if self.first_child == NONE {
            0..0
        } else {
            self.first_child..self.first_child + self.child_count
        }
    }
}

/// The arena: nodes plus one name arena per side (left and right names can differ in case or
/// normalisation).
#[derive(Debug, Default)]
pub struct Tree {
    pub nodes: Vec<Node>,
    /// One arena for both sides: a pair whose names are identical stores the name once.
    names: Vec<u16>,
}

impl Tree {
    /// A tree with a root node (the two compare roots).
    pub fn new() -> Tree {
        let mut t = Tree::default();
        t.nodes.push(Node {
            parent: NONE,
            first_child: NONE,
            child_count: 0,
            depth: 0,
            status: Status::Same,
            flags: 0,
            left: SideInfo {
                present: true,
                kind: Kind::Dir,
                ..SideInfo::ABSENT
            },
            right: SideInfo {
                present: true,
                kind: Kind::Dir,
                ..SideInfo::ABSENT
            },
            counters: Counters::default(),
        });
        t
    }

    pub const ROOT: NodeId = 0;

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id as usize]
    }

    pub fn node_mut(&mut self, id: NodeId) -> &mut Node {
        &mut self.nodes[id as usize]
    }

    /// Bytes held by the nodes and the name arenas (for the memory budget, backlog E7-S2).
    pub fn memory_bytes(&self) -> usize {
        self.nodes.capacity() * size_of::<Node>() + self.names.capacity() * 2
    }

    /// Stores a name in the arena and returns `(offset, len)`.
    pub fn push_name(&mut self, name: &Name) -> (u32, u16) {
        // NTFS names are at most 255 units; the casts below cannot truncate for real file systems
        debug_assert!(
            u16::try_from(name.len_units()).is_ok() && u32::try_from(self.names.len()).is_ok()
        );
        let off = self.names.len() as u32;
        self.names.extend_from_slice(name.units());
        (off, name.len_units() as u16)
    }

    /// Releases the growth slack once a scan is complete.
    pub fn shrink(&mut self) {
        self.nodes.shrink_to_fit();
        self.names.shrink_to_fit();
    }

    /// The name of a node on a side, if present there.
    pub fn name(&self, id: NodeId, left_side: bool) -> Option<Name> {
        let n = self.node(id);
        let s = if left_side { &n.left } else { &n.right };
        if !s.present {
            return None;
        }
        Some(Name::from_units(
            &self.names[s.name_off as usize..s.name_off as usize + s.name_len as usize],
        ))
    }

    /// The display name (left name when present, else the right one).
    pub fn display_name(&self, id: NodeId) -> Name {
        self.name(id, true)
            .or_else(|| self.name(id, false))
            .unwrap_or_default()
    }

    /// The root-relative path of a node (names of the left side, else the right side).
    pub fn path(&self, id: NodeId) -> VPath {
        let mut parts = Vec::new();
        let mut cur = id;
        while cur != Self::ROOT && cur != NONE {
            parts.push(self.display_name(cur));
            cur = self.node(cur).parent;
        }
        parts.reverse();
        parts.into_iter().fold(VPath::root(), |p, n| p.join(n))
    }

    /// The path of a node on one side (that side's own names).
    pub fn side_path(&self, id: NodeId, left_side: bool) -> Option<VPath> {
        let mut parts = Vec::new();
        let mut cur = id;
        while cur != Self::ROOT && cur != NONE {
            parts.push(self.name(cur, left_side)?);
            cur = self.node(cur).parent;
        }
        parts.reverse();
        Some(parts.into_iter().fold(VPath::root(), |p, n| p.join(n)))
    }

    /// Like [`Tree::side_path`], but a component that has no name on this side takes the
    /// name from the other side: where an item would be created or has just been created.
    pub fn side_path_lenient(&self, id: NodeId, left_side: bool) -> VPath {
        let mut parts = Vec::new();
        let mut cur = id;
        while cur != Self::ROOT && cur != NONE {
            parts.push(
                self.name(cur, left_side)
                    .or_else(|| self.name(cur, !left_side))
                    .unwrap_or_default(),
            );
            cur = self.node(cur).parent;
        }
        parts.reverse();
        parts.into_iter().fold(VPath::root(), |p, n| p.join(n))
    }

    /// Adds the children of `parent` as one contiguous range and returns the first id.
    pub fn add_children(&mut self, parent: NodeId, mut kids: Vec<Node>) -> NodeId {
        debug_assert_eq!(
            self.node(parent).child_count,
            0,
            "children are created all at once"
        );
        let first = self.nodes.len() as NodeId;
        let depth = self.node(parent).depth + 1;
        for k in &mut kids {
            k.parent = parent;
            k.depth = depth;
        }
        let count = kids.len() as u32;
        let contributions: Vec<Counters> = kids.iter().map(|k| contribution(k.status)).collect();
        self.nodes.extend(kids);
        let p = self.node_mut(parent);
        p.first_child = if count == 0 { NONE } else { first };
        p.child_count = count;
        // each child's contribution (and its own subtree, empty so far) goes up the chain
        let mut total = Counters::default();
        for c in &contributions {
            total.add(c, 1);
        }
        self.add_up(parent, &total, 1);
        first
    }

    fn add_up(&mut self, from: NodeId, delta: &Counters, sign: i64) {
        let mut cur = from;
        while cur != NONE {
            let n = self.node_mut(cur);
            n.counters.add(delta, sign);
            cur = n.parent;
        }
    }

    /// Changes a node's status and updates every ancestor's counters (incrementally).
    pub fn set_status(&mut self, id: NodeId, status: Status) {
        let old = self.node(id).status;
        if old == status {
            return;
        }
        let (o, n) = (contribution(old), contribution(status));
        // `items` does not change when only the status does
        let parent = self.node(id).parent;
        self.add_up(parent, &o, -1);
        self.add_up(parent, &n, 1);
        self.node_mut(id).status = status;
    }

    /// Recomputes every folder's counters from the statuses (used to check the incremental
    /// bookkeeping).
    pub fn recompute_counters(&self) -> Vec<Counters> {
        let mut out = vec![Counters::default(); self.nodes.len()];
        for id in (1..self.nodes.len()).rev() {
            let c = contribution(self.nodes[id].status);
            let mut sub = out[id];
            sub.add(&c, 1);
            let parent = self.nodes[id].parent as usize;
            out[parent].add(&sub, 1);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// Backlog E7-S2.1: well under the 160 bytes per node of the budget.
    #[test]
    fn node_layout_fits_the_budget() {
        assert_eq!(size_of::<SideInfo>(), 32);
        assert!(size_of::<Node>() <= 160, "{} bytes", size_of::<Node>());
    }

    #[test]
    fn children_are_contiguous_and_paths_are_built_from_names() {
        let mut t = Tree::new();
        let mut a = leaf(Status::Same);
        a.left.kind = Kind::Dir;
        a.right.kind = Kind::Dir;
        let (off, len) = t.push_name(&Name::new("dir"));
        a.left.name_off = off;
        a.left.name_len = len;
        let first = t.add_children(Tree::ROOT, vec![a, leaf(Status::LeftOrphan)]);
        assert_eq!((t.node(0).first_child, t.node(0).child_count), (first, 2));
        assert_eq!(t.node(0).children(), 1..3);
        let (off, len) = t.push_name(&Name::new("f.txt"));
        let mut f = leaf(Status::Different);
        f.left.name_off = off;
        f.left.name_len = len;
        f.right.present = false;
        let kid = t.add_children(1, vec![f]);
        assert_eq!(t.path(kid).display(), "dir/f.txt");
        assert_eq!(t.node(kid).depth, 2);
        assert_eq!(t.node(1).counters.diff, 1);
        assert_eq!(t.node(0).counters.items, 3);
        assert_eq!(t.node(0).counters.left_orphan, 1);
        assert!(t.node(0).counters.has_differences());
        assert_eq!(t.display_name(kid), Name::new("f.txt"));
        assert_eq!(t.side_path(kid, false), None, "no right-side name stored");
    }

    #[test]
    fn set_status_moves_counts_up_the_chain() {
        let mut t = Tree::new();
        t.add_children(Tree::ROOT, vec![leaf(Status::Same)]);
        assert!(!t.node(0).counters.has_differences());
        t.set_status(1, Status::LeftNewer);
        assert_eq!(t.node(0).counters.left_newer, 1);
        t.set_status(1, Status::Error);
        assert_eq!(
            (t.node(0).counters.left_newer, t.node(0).counters.error),
            (0, 1)
        );
        t.set_status(1, Status::Error); // no-op
        assert_eq!(t.node(0).counters.error, 1);
        assert_eq!(t.recompute_counters()[0], t.node(0).counters);
    }
}
