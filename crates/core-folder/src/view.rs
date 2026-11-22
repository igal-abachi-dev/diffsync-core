//! Folder views: the flat projection the UI scrolls (plan.md §7.4, backlog E7-S11).
//!
//! A view owns no tree data; it holds a filter, a name search, a sort order, the set of
//! expanded folders and the resulting list of visible `NodeId`s. Several views can share one
//! compare. Every rebuild bumps `generation`, so the UI knows to re-pull rows.
//!
//! Scheduling a rebuild on the `cpu` pool is the job of the session layer (E9); the view itself
//! is a plain value, so a rebuild is one call on whichever thread the owner chooses.

use core_vfs::fold::fold_name;

use crate::tree::{NONE, Node, NodeId, Status, Tree};

/// Which items a view shows (a folder is shown when it or anything below it matches).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ViewFilter {
    #[default]
    All,
    /// Anything that is not `Same`.
    Differences,
    Same,
    Orphans,
    LeftNewer,
    RightNewer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortKey {
    #[default]
    Name,
    Size,
    Time,
    Status,
}

/// How children are ordered inside a folder. Folders always come first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SortOrder {
    pub key: SortKey,
    pub descending: bool,
}

#[derive(Debug, Clone, Default)]
pub struct FolderView {
    filter: ViewFilter,
    search: Vec<u16>,
    sort: SortOrder,
    expanded: Vec<u64>,
    /// Per node: does it or anything below it match the filter and search?
    shown: Vec<bool>,
    visible: Vec<NodeId>,
    generation: u64,
}

fn status_matches(f: ViewFilter, s: Status) -> bool {
    match f {
        ViewFilter::All => true,
        ViewFilter::Differences => s != Status::Same,
        ViewFilter::Same => s == Status::Same,
        ViewFilter::Orphans => matches!(s, Status::LeftOrphan | Status::RightOrphan),
        ViewFilter::LeftNewer => s == Status::LeftNewer,
        ViewFilter::RightNewer => s == Status::RightNewer,
    }
}

impl FolderView {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn len(&self) -> usize {
        self.visible.len()
    }

    pub fn is_empty(&self) -> bool {
        self.visible.is_empty()
    }

    /// The visible rows `first..first + count` (clamped). A slice: O(1).
    pub fn get_rows(&self, first: usize, count: usize) -> &[NodeId] {
        let a = first.min(self.visible.len());
        &self.visible[a..(a + count).min(self.visible.len())]
    }

    pub fn is_expanded(&self, id: NodeId) -> bool {
        self.expanded
            .get(id as usize / 64)
            .is_some_and(|w| w >> (id % 64) & 1 == 1)
    }

    fn set_expanded(&mut self, id: NodeId, on: bool) {
        let w = id as usize / 64;
        if self.expanded.len() <= w {
            self.expanded.resize(w + 1, 0);
        }
        if on {
            self.expanded[w] |= 1 << (id % 64);
        } else {
            self.expanded[w] &= !(1 << (id % 64));
        }
    }

    pub fn filter(&self) -> ViewFilter {
        self.filter
    }

    pub fn sort(&self) -> SortOrder {
        self.sort
    }

    /// Changes the filter and rebuilds.
    pub fn set_filter(&mut self, tree: &Tree, filter: ViewFilter) {
        self.filter = filter;
        self.rebuild(tree);
    }

    /// Changes the name search (case-insensitive substring, decision D6 folding) and rebuilds.
    pub fn set_search(&mut self, tree: &Tree, text: &str) {
        self.search = fold_name(&text.encode_utf16().collect::<Vec<_>>());
        self.rebuild(tree);
    }

    pub fn set_sort(&mut self, tree: &Tree, sort: SortOrder) {
        self.sort = sort;
        self.rebuild(tree);
    }

    fn name_matches(&self, tree: &Tree, id: NodeId) -> bool {
        if self.search.is_empty() {
            return true;
        }
        let name = fold_name(tree.display_name(id).units());
        name.windows(self.search.len())
            .any(|w| w == self.search.as_slice())
    }

    /// Recomputes which nodes are shown: own match, or any descendant matches. Children always
    /// have larger ids than their parent, so one reverse pass is enough.
    fn compute_shown(&mut self, tree: &Tree) {
        let n = tree.len();
        self.shown = vec![false; n];
        for id in (1..n).rev() {
            let node = tree.node(id as NodeId);
            let own = !node.has(crate::tree::flag::GONE)
                && status_matches(self.filter, node.status)
                && self.name_matches(tree, id as NodeId);
            if own || self.shown[id] {
                self.shown[id] = true;
                self.shown[node.parent as usize] = true;
            }
        }
    }

    fn sorted_children(&self, tree: &Tree, parent: &Node) -> Vec<NodeId> {
        let mut kids: Vec<NodeId> = parent
            .children()
            .filter(|&c| self.shown[c as usize])
            .collect();
        if self.sort == SortOrder::default() {
            return kids; // the tree already stores children folders-first by folded name
        }
        let (key, desc) = (self.sort.key, self.sort.descending);
        kids.sort_by(|&a, &b| {
            let (na, nb) = (tree.node(a), tree.node(b));
            // folders first, whatever the direction
            let dirs = nb.is_dir().cmp(&na.is_dir());
            let by = match key {
                SortKey::Name => fold_name(tree.display_name(a).units())
                    .cmp(&fold_name(tree.display_name(b).units())),
                SortKey::Size => size_of_node(na).cmp(&size_of_node(nb)),
                SortKey::Time => time_of_node(na).cmp(&time_of_node(nb)),
                SortKey::Status => (na.status as u8).cmp(&(nb.status as u8)),
            };
            dirs.then(if desc { by.reverse() } else { by })
                .then(a.cmp(&b))
        });
        kids
    }

    /// Rebuilds the visible list from the tree (depth first, expanded folders only), keeping
    /// the expanded set. Bumps the generation.
    pub fn rebuild(&mut self, tree: &Tree) {
        self.compute_shown(tree);
        self.visible.clear();
        // explicit stack, no recursion
        let mut stack: Vec<NodeId> = self
            .sorted_children(tree, tree.node(Tree::ROOT))
            .into_iter()
            .rev()
            .collect();
        while let Some(id) = stack.pop() {
            self.visible.push(id);
            let node = tree.node(id);
            if node.child_count > 0 && self.is_expanded(id) {
                stack.extend(self.sorted_children(tree, node).into_iter().rev());
            }
        }
        self.generation += 1;
    }

    /// The visible descendants of `id` (as they would appear when `id` is expanded).
    fn subtree_rows(&self, tree: &Tree, id: NodeId) -> Vec<NodeId> {
        let mut out = Vec::new();
        let mut stack: Vec<NodeId> = self
            .sorted_children(tree, tree.node(id))
            .into_iter()
            .rev()
            .collect();
        while let Some(c) = stack.pop() {
            out.push(c);
            let node = tree.node(c);
            if node.child_count > 0 && self.is_expanded(c) {
                stack.extend(self.sorted_children(tree, node).into_iter().rev());
            }
        }
        out
    }

    /// Expands the folder shown at `row`, splicing its rows in after it: O(subtree). Returns
    /// how many rows were inserted.
    pub fn expand_row(&mut self, tree: &Tree, row: usize) -> usize {
        let Some(&id) = self.visible.get(row) else {
            return 0;
        };
        if self.is_expanded(id) || tree.node(id).child_count == 0 {
            return 0;
        }
        self.set_expanded(id, true);
        let rows = self.subtree_rows(tree, id);
        let n = rows.len();
        let at = row + 1;
        self.visible.splice(at..at, rows);
        self.generation += 1;
        n
    }

    /// Collapses the folder shown at `row`, removing the rows below it. Returns how many.
    pub fn collapse_row(&mut self, tree: &Tree, row: usize) -> usize {
        let Some(&id) = self.visible.get(row) else {
            return 0;
        };
        if !self.is_expanded(id) {
            return 0;
        }
        self.set_expanded(id, false);
        let depth = tree.node(id).depth;
        let end = self.visible[row + 1..]
            .iter()
            .position(|&c| tree.node(c).depth <= depth)
            .map_or(self.visible.len(), |p| row + 1 + p);
        let removed = end - (row + 1);
        self.visible.drain(row + 1..end);
        self.generation += 1;
        removed
    }

    /// Expands every folder (used by "expand all").
    pub fn expand_all(&mut self, tree: &Tree) {
        for id in 1..tree.len() as NodeId {
            if tree.node(id).child_count > 0 {
                self.set_expanded(id, true);
            }
        }
        self.rebuild(tree);
    }

    /// Expands or collapses the folder `id` (and, with `recursive`, everything below it). A
    /// visible folder without `recursive` splices its rows in or out; anything else rebuilds.
    pub fn set_node_expanded(&mut self, tree: &Tree, id: NodeId, on: bool, recursive: bool) {
        if !recursive {
            if let Some(row) = self.row_of(id) {
                if on {
                    self.expand_row(tree, row);
                } else {
                    self.collapse_row(tree, row);
                }
                return;
            }
        }
        let mut stack = vec![id];
        while let Some(x) = stack.pop() {
            self.set_expanded(x, on);
            if recursive {
                stack.extend(
                    tree.node(x)
                        .children()
                        .filter(|&c| tree.node(c).child_count > 0),
                );
            }
        }
        self.rebuild(tree);
    }

    /// The row where `id` is shown, if visible.
    pub fn row_of(&self, id: NodeId) -> Option<usize> {
        self.visible.iter().position(|&v| v == id)
    }

    /// Makes `id` visible by expanding its ancestors; returns its row.
    pub fn reveal(&mut self, tree: &Tree, id: NodeId) -> Option<usize> {
        if !self.shown.get(id as usize).copied().unwrap_or(false) {
            return None;
        }
        let mut cur = tree.node(id).parent;
        while cur != NONE && cur != Tree::ROOT {
            self.set_expanded(cur, true);
            cur = tree.node(cur).parent;
        }
        self.rebuild(tree);
        self.row_of(id)
    }
}

fn size_of_node(n: &Node) -> u64 {
    n.left.size.max(n.right.size)
}

fn time_of_node(n: &Node) -> i64 {
    n.left.mtime.max(n.right.mtime)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::{Counters, SideInfo};
    use core_vfs::{Kind, Name};

    fn node(name: &str, dir: bool, status: Status, size: u64, t: &mut Tree) -> Node {
        let (off, len) = t.push_name(&Name::new(name));
        let side = SideInfo {
            present: true,
            kind: if dir { Kind::Dir } else { Kind::File },
            size,
            name_off: off,
            name_len: len,
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
            right: SideInfo::ABSENT,
            counters: Counters::default(),
        }
    }

    /// root: [d (dir): [x.txt, y.txt (Different)], a.txt, b.txt]
    fn sample() -> Tree {
        let mut t = Tree::new();
        let kids = vec![
            node("d", true, Status::Same, 0, &mut t),
            node("a.txt", false, Status::Same, 30, &mut t),
            node("b.txt", false, Status::LeftOrphan, 10, &mut t),
        ];
        t.add_children(Tree::ROOT, kids);
        let kids = vec![
            node("x.txt", false, Status::Same, 5, &mut t),
            node("y.txt", false, Status::Different, 50, &mut t),
        ];
        t.add_children(1, kids);
        t
    }

    fn names(v: &FolderView, t: &Tree) -> Vec<String> {
        v.get_rows(0, 100)
            .iter()
            .map(|&i| t.display_name(i).to_string_lossy())
            .collect()
    }

    #[test]
    fn collapsed_by_default_then_expand_and_collapse_splice() {
        let t = sample();
        let mut v = FolderView::new();
        v.rebuild(&t);
        assert_eq!(names(&v, &t), ["d", "a.txt", "b.txt"]);
        let g = v.generation();
        assert_eq!(v.expand_row(&t, 0), 2);
        assert_eq!(names(&v, &t), ["d", "x.txt", "y.txt", "a.txt", "b.txt"]);
        assert!(v.generation() > g);
        assert_eq!(v.expand_row(&t, 0), 0, "already expanded");
        assert_eq!(v.collapse_row(&t, 0), 2);
        assert_eq!(names(&v, &t), ["d", "a.txt", "b.txt"]);
        assert_eq!(v.expand_row(&t, 1), 0, "a file has nothing to expand");
    }

    #[test]
    fn filter_keeps_ancestors_of_matches() {
        let t = sample();
        let mut v = FolderView::new();
        v.expand_all(&t);
        v.set_filter(&t, ViewFilter::Differences);
        assert_eq!(names(&v, &t), ["d", "y.txt", "b.txt"]);
        v.set_filter(&t, ViewFilter::Orphans);
        assert_eq!(names(&v, &t), ["b.txt"]);
        v.set_filter(&t, ViewFilter::Same);
        assert_eq!(names(&v, &t), ["d", "x.txt", "a.txt"]);
        v.set_filter(&t, ViewFilter::All);
        assert_eq!(v.len(), 5);
    }

    #[test]
    fn search_is_case_insensitive_and_keeps_the_path_to_a_match() {
        let t = sample();
        let mut v = FolderView::new();
        v.expand_all(&t);
        v.set_search(&t, "Y.T");
        assert_eq!(names(&v, &t), ["d", "y.txt"]);
        v.set_search(&t, "");
        assert_eq!(v.len(), 5);
        v.set_search(&t, "zzz");
        assert!(v.is_empty());
    }

    #[test]
    fn sorting_keeps_folders_first_in_both_directions() {
        let t = sample();
        let mut v = FolderView::new();
        v.expand_all(&t);
        v.set_sort(
            &t,
            SortOrder {
                key: SortKey::Size,
                descending: false,
            },
        );
        assert_eq!(names(&v, &t), ["d", "x.txt", "y.txt", "b.txt", "a.txt"]);
        v.set_sort(
            &t,
            SortOrder {
                key: SortKey::Size,
                descending: true,
            },
        );
        assert_eq!(names(&v, &t), ["d", "y.txt", "x.txt", "a.txt", "b.txt"]);
        v.set_sort(
            &t,
            SortOrder {
                key: SortKey::Name,
                descending: true,
            },
        );
        assert_eq!(names(&v, &t), ["d", "y.txt", "x.txt", "b.txt", "a.txt"]);
        v.set_sort(
            &t,
            SortOrder {
                key: SortKey::Status,
                descending: false,
            },
        );
        assert_eq!(names(&v, &t)[0], "d");
    }

    #[test]
    fn two_views_share_one_tree_and_reveal_expands_ancestors() {
        let t = sample();
        let (mut a, mut b) = (FolderView::new(), FolderView::new());
        a.rebuild(&t);
        b.rebuild(&t);
        b.set_filter(&t, ViewFilter::Orphans);
        assert_eq!((a.len(), b.len()), (3, 1));
        let y = (0..t.len() as NodeId)
            .find(|&i| t.display_name(i).to_string_lossy() == "y.txt")
            .unwrap();
        assert_eq!(a.row_of(y), None);
        assert_eq!(a.reveal(&t, y), Some(2));
        assert_eq!(b.reveal(&t, y), None, "y.txt does not pass b's filter");
    }

    #[test]
    fn get_rows_clamps() {
        let t = sample();
        let mut v = FolderView::new();
        v.rebuild(&t);
        assert_eq!(v.get_rows(1, 100).len(), 2);
        assert!(v.get_rows(50, 3).is_empty());
    }
}
