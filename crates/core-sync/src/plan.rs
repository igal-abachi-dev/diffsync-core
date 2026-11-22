//! The sync plan: what would be done, decided before anything is written (plan.md §8.2, §9;
//! backlog E8-S2). Prompts are data, every action carries fingerprints for the re-validation
//! of §9.1, destination names are checked up front (§9.8), links are skipped (§9.5/D16) and
//! overlapping roots refuse planning (§9.7).

use std::collections::{HashMap, HashSet};

use core_base::{Error, Result, Ticks};
use core_folder::{FolderCompare, Node, NodeId, SideInfo, Tree, flag};
use core_vfs::fold::fold_name;
use core_vfs::{Kind, NameCheck, VPath, case_collisions, validate_name};

use crate::rules::{Op, SyncRules};

/// What an item looked like at compare time (plan.md §9.1): kind, and for files size and mtime.
/// Folders compare by kind only, because their own time moves with every change inside.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fingerprint {
    pub kind: Kind,
    pub size: u64,
    pub mtime: Ticks,
}

impl Fingerprint {
    pub fn of(side: &SideInfo) -> Fingerprint {
        let file = side.kind == Kind::File;
        Fingerprint {
            kind: side.kind,
            size: if file { side.size } else { 0 },
            mtime: if file { side.mtime } else { 0 },
        }
    }

    pub fn of_entry(e: &core_vfs::Entry) -> Fingerprint {
        let file = e.kind == Kind::File;
        Fingerprint {
            kind: e.kind,
            size: if file { e.size } else { 0 },
            mtime: if file { e.mtime } else { 0 },
        }
    }
}

/// The numbers are part of the C ABI (`DsPlanAction.kind`) and never change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ActionKind {
    CreateFolderLeft = 0,
    CreateFolderRight = 1,
    CopyToLeft = 2,
    CopyToRight = 3,
    DeleteLeft = 4,
    DeleteRight = 5,
}

impl ActionKind {
    /// The side that is written or deleted: `true` = left.
    pub fn target_is_left(self) -> bool {
        matches!(
            self,
            Self::CreateFolderLeft | Self::CopyToLeft | Self::DeleteLeft
        )
    }

    pub fn is_delete(self) -> bool {
        matches!(self, Self::DeleteLeft | Self::DeleteRight)
    }

    pub fn is_create_folder(self) -> bool {
        matches!(self, Self::CreateFolderLeft | Self::CreateFolderRight)
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::CreateFolderLeft => "create-folder-left",
            Self::CreateFolderRight => "create-folder-right",
            Self::CopyToLeft => "copy-to-left",
            Self::CopyToRight => "copy-to-right",
            Self::DeleteLeft => "delete-left",
            Self::DeleteRight => "delete-right",
        }
    }
}

/// One planned action. Paths are absolute inside their file system (root joined).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Action {
    pub kind: ActionKind,
    pub node: NodeId,
    pub bytes: u64,
    pub is_folder: bool,
    /// Where the data comes from (copies and folder creations).
    pub src: Option<VPath>,
    /// The item written, created or deleted.
    pub dst: VPath,
    pub expect_src: Option<Fingerprint>,
    /// `None` = the destination must not exist (decision: §9.1).
    pub expect_dst: Option<Fingerprint>,
    /// A folder delete that removes the folder only when nothing is left in it (§9.4).
    pub only_if_empty: bool,
    /// A delete that frees a name for a type-conflict copy: runs first (§9.3).
    pub frees_name: bool,
    /// For created folders: the time to set after the contents are written.
    pub folder_mtime: Option<Ticks>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Warning,
    Error,
}

/// A problem found while planning: the item gets no action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanIssue {
    pub node: NodeId,
    pub severity: Severity,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Summary {
    pub copies_to_left: u32,
    pub bytes_to_left: u64,
    pub copies_to_right: u32,
    pub bytes_to_right: u64,
    pub folders_created: u32,
    pub deletes_left: u32,
    pub deletes_right: u32,
    /// Items whose choice is still open (prompts).
    pub conflicts: u32,
    /// Items that cannot be planned (invalid names, blocked names).
    pub errors: u32,
    pub skipped_links: u32,
}

#[derive(Debug, Clone)]
pub struct Plan {
    pub actions: Vec<Action>,
    pub unresolved_prompts: Vec<NodeId>,
    pub issues: Vec<PlanIssue>,
    pub skipped_links: Vec<NodeId>,
    pub summary: Summary,
    pub rules: SyncRules,
    /// True when a filter or hidden/system skip was active at compare time (§9.4).
    pub filters_active: bool,
    /// The compare this plan was built from (decision: D46).
    pub compare_id: u64,
    overrides: HashMap<NodeId, Op>,
    selection: Option<Vec<NodeId>>,
}

impl Plan {
    /// Chooses what to do with a prompted (or any) node, then plans again. The plan is
    /// immutable once an executor owns it, so this is a preview-time call.
    pub fn set_op(&mut self, compare: &FolderCompare, node: NodeId, op: Op) -> Result<()> {
        let n = compare.tree.node(node);
        let needs_left = matches!(op, Op::CopyToRight | Op::DeleteLeft);
        let needs_right = matches!(op, Op::CopyToLeft | Op::DeleteRight);
        if (needs_left && !n.left.present) || (needs_right && !n.right.present) {
            return Err(Error::invalid_argument(
                "the operation needs the item on a side where it does not exist",
            ));
        }
        let mut overrides = self.overrides.clone();
        overrides.insert(node, op);
        let sel = self.selection.clone();
        // the old plan stays when the rebuild fails
        *self = build(compare, &self.rules.clone(), sel.as_deref(), overrides)?;
        Ok(())
    }

    /// The preview as text, one action per line (backs `--dry-run`, plan.md §9.6).
    pub fn describe(&self, compare: &FolderCompare) -> Vec<String> {
        let mut out: Vec<String> = self
            .actions
            .iter()
            .map(|a| {
                let item = |p: &VPath| p.display();
                match (&a.src, a.kind.is_delete()) {
                    (Some(s), false) => format!(
                        "{} {} -> {} ({} bytes)",
                        a.kind.name(),
                        item(s),
                        item(&a.dst),
                        a.bytes
                    ),
                    _ => format!("{} {}", a.kind.name(), item(&a.dst)),
                }
            })
            .collect();
        for &n in &self.unresolved_prompts {
            out.push(format!("prompt {} (leave)", compare.tree.path(n).display()));
        }
        for i in &self.issues {
            let level = if i.severity == Severity::Error {
                "error"
            } else {
                "warning"
            };
            out.push(format!(
                "{level} {}: {}",
                compare.tree.path(i.node).display(),
                i.message
            ));
        }
        for &n in &self.skipped_links {
            out.push(format!("skipped (link) {}", compare.tree.path(n).display()));
        }
        out
    }
}

/// Builds the plan for a finished compare. `selection` limits it to those nodes and everything
/// below them; folders that must exist for a selected item are created as well.
pub fn build_plan(
    compare: &FolderCompare,
    rules: &SyncRules,
    selection: Option<&[NodeId]>,
) -> Result<Plan> {
    build(compare, rules, selection, HashMap::new())
}

fn build(
    compare: &FolderCompare,
    rules: &SyncRules,
    selection: Option<&[NodeId]>,
    overrides: HashMap<NodeId, Op>,
) -> Result<Plan> {
    // decision: §9.7: nested roots compare with a warning, but nothing may be written
    if compare.sync_blocked {
        return Err(Error::invalid_argument(
            "one folder is inside the other: sync is blocked",
        ));
    }
    let mut b = Builder {
        c: compare,
        rules,
        overrides: &overrides,
        selected: selection.map(|s| s.iter().copied().collect()),
        created: HashSet::new(),
        plan: Plan {
            actions: Vec::new(),
            unresolved_prompts: Vec::new(),
            issues: Vec::new(),
            skipped_links: Vec::new(),
            summary: Summary::default(),
            rules: rules.clone(),
            filters_active: compare.filters_active(),
            compare_id: compare.id,
            overrides: HashMap::new(),
            selection: selection.map(<[NodeId]>::to_vec),
        },
    };
    b.walk();
    let mut plan = b.plan;
    plan.overrides = overrides;
    summarise(&mut plan);
    Ok(plan)
}

struct Builder<'a> {
    c: &'a FolderCompare,
    rules: &'a SyncRules,
    overrides: &'a HashMap<NodeId, Op>,
    selected: Option<HashSet<NodeId>>,
    /// Folders for which a create action exists already.
    created: HashSet<NodeId>,
    plan: Plan,
}

impl Builder<'_> {
    fn node(&self, id: NodeId) -> &Node {
        self.c.tree.node(id)
    }

    fn decide(&self, id: NodeId) -> Op {
        self.overrides
            .get(&id)
            .copied()
            .unwrap_or_else(|| self.rules.op_for(self.node(id).status))
    }

    fn in_selection(&self, id: NodeId) -> bool {
        let Some(sel) = &self.selected else {
            return true;
        };
        let mut cur = id;
        while cur != core_folder::tree::NONE && cur != Tree::ROOT {
            if sel.contains(&cur) {
                return true;
            }
            cur = self.node(cur).parent;
        }
        false
    }

    fn issue(&mut self, node: NodeId, severity: Severity, message: impl Into<String>) {
        self.plan.issues.push(PlanIssue {
            node,
            severity,
            message: message.into(),
        });
    }

    /// Pre-order walk. A folder on both sides recurses; an orphan folder takes its own decision
    /// for the whole subtree.
    fn walk(&mut self) {
        let mut stack: Vec<NodeId> = vec![Tree::ROOT];
        while let Some(id) = stack.pop() {
            let kids: Vec<NodeId> = self.node(id).children().collect();
            let blocked = self.check_children(&kids);
            let mut next = Vec::new();
            for &k in &kids {
                let n = self.node(k);
                if n.has(flag::GONE) {
                    continue;
                }
                if n.has(flag::LINK) {
                    // decision: D16: links are never copied or deleted by sync
                    self.plan.skipped_links.push(k);
                    continue;
                }
                let both = n.left.present && n.right.present;
                if both && n.is_dir() {
                    next.push(k); // recurse; the folder itself needs nothing
                    continue;
                }
                if blocked.contains(&k) {
                    continue;
                }
                self.plan_item(k, &mut next);
            }
            // keep tree order: the first child is handled first
            stack.extend(next.into_iter().rev());
        }
    }

    /// Name checks for the items that would be created in a folder (§9.8): invalid names are
    /// errors, device names a warning, and case collisions on a case-insensitive target error.
    fn check_children(&mut self, kids: &[NodeId]) -> HashSet<NodeId> {
        let mut blocked = HashSet::new();
        for to_left in [true, false] {
            let caps = self.c.vfs(to_left).caps();
            let mut new_nodes: Vec<(NodeId, String)> = Vec::new();
            let mut existing: Vec<String> = Vec::new();
            for &k in kids {
                let n = self.node(k);
                let (here, other) = if to_left {
                    (&n.left, &n.right)
                } else {
                    (&n.right, &n.left)
                };
                // an item that this plan deletes from the target frees its name (§9.3): it must
                // not collide with the new item that takes its place
                let freed = self.in_selection(k)
                    && self.decide(k)
                        == if to_left {
                            Op::DeleteLeft
                        } else {
                            Op::DeleteRight
                        };
                if here.present && !n.has(flag::GONE) && !freed {
                    existing.push(
                        self.c
                            .tree
                            .name(k, to_left)
                            .map(|x| x.to_string_lossy())
                            .unwrap_or_default(),
                    );
                } else if other.present
                    && !n.has(flag::LINK)
                    && self.in_selection(k)
                    && self.decide(k)
                        == if to_left {
                            Op::CopyToLeft
                        } else {
                            Op::CopyToRight
                        }
                {
                    let name = self
                        .c
                        .tree
                        .name(k, !to_left)
                        .map(|x| x.to_string_lossy())
                        .unwrap_or_default();
                    new_nodes.push((k, name));
                }
            }
            for (k, name) in &new_nodes {
                match validate_name(&caps, name) {
                    NameCheck::Ok => {}
                    NameCheck::ReservedName => {
                        self.issue(*k, Severity::Warning, format!("`{name}` is a device name on the target (written through the long-path form)"));
                    }
                    other => {
                        self.issue(
                            *k,
                            Severity::Error,
                            format!("`{name}` cannot be created on the target: {other:?}"),
                        );
                        blocked.insert(*k);
                    }
                }
            }
            if !caps.case_sensitive && !new_nodes.is_empty() {
                let all: Vec<&str> = existing
                    .iter()
                    .map(String::as_str)
                    .chain(new_nodes.iter().map(|(_, n)| n.as_str()))
                    .collect();
                for group in case_collisions(&all, false) {
                    for i in group {
                        if let Some(idx) = i.checked_sub(existing.len()) {
                            let k = new_nodes[idx].0;
                            if blocked.insert(k) {
                                self.issue(k, Severity::Error, "the name collides with another name that differs only in case on the target");
                            }
                        }
                    }
                }
            }
        }
        blocked
    }

    fn plan_item(&mut self, id: NodeId, next: &mut Vec<NodeId>) {
        let op = self.decide(id);
        let n = self.node(id).clone();
        let is_dir = n.is_dir();
        match op {
            Op::Leave => {}
            Op::Prompt => {
                if self.in_selection(id) {
                    self.plan.unresolved_prompts.push(id);
                }
            }
            Op::CopyToLeft | Op::CopyToRight => {
                let to_left = op == Op::CopyToLeft;
                if self.in_selection(id) {
                    if is_dir {
                        self.ensure_folder(id, to_left);
                    } else {
                        self.plan_copy(id, to_left);
                    }
                }
                if is_dir {
                    next.push(id); // children of a created folder are orphans too
                }
            }
            Op::DeleteLeft | Op::DeleteRight => {
                if self.in_selection(id) {
                    self.plan_delete(id, op == Op::DeleteLeft);
                }
            }
        }
    }

    /// Creates the folder on the target (and the missing ancestors above it, top down).
    fn ensure_folder(&mut self, id: NodeId, to_left: bool) {
        if self.created.contains(&id) {
            return;
        }
        let mut chain = vec![id];
        let mut cur = self.node(id).parent;
        while cur != Tree::ROOT && cur != core_folder::tree::NONE {
            let p = self.node(cur);
            let present = if to_left {
                p.left.present
            } else {
                p.right.present
            };
            if present || self.created.contains(&cur) {
                break;
            }
            chain.push(cur);
            cur = p.parent;
        }
        for &f in chain.iter().rev() {
            self.created.insert(f);
            let n = self.node(f);
            let src_side = if to_left { &n.right } else { &n.left };
            let (src_fp, mtime) = (Fingerprint::of(src_side), src_side.mtime);
            let Some(src) = self.c.side_vpath(f, !to_left) else {
                continue;
            };
            let dst = self.c.lenient_vpath(f, to_left);
            self.plan.actions.push(Action {
                kind: if to_left {
                    ActionKind::CreateFolderLeft
                } else {
                    ActionKind::CreateFolderRight
                },
                node: f,
                bytes: 0,
                is_folder: true,
                src: Some(src),
                dst,
                expect_src: Some(src_fp),
                expect_dst: None,
                only_if_empty: false,
                frees_name: false,
                folder_mtime: Some(mtime),
            });
        }
    }

    fn plan_copy(&mut self, id: NodeId, to_left: bool) {
        let n = self.node(id).clone();
        let (src_side, dst_side) = if to_left {
            (n.right, n.left)
        } else {
            (n.left, n.right)
        };
        if !src_side.present {
            return;
        }
        // the folder that receives the file may itself be new
        let parent = n.parent;
        if parent != Tree::ROOT {
            let p = self.node(parent);
            let present = if to_left {
                p.left.present
            } else {
                p.right.present
            };
            if !present {
                self.ensure_folder(parent, to_left);
            }
        }
        // a type conflict: the copy only works when the opposite item is deleted first (§9.3)
        if n.has(flag::TYPE_CONFLICT) {
            if let Some(other) = self.counterpart(id) {
                let want = if to_left {
                    Op::DeleteLeft
                } else {
                    Op::DeleteRight
                };
                if self.decide(other) != want || !self.in_selection(other) {
                    self.issue(id, Severity::Error, "a folder or file of the same name on the target blocks this copy; delete it first");
                    return;
                }
            }
        }
        let Some(src) = self.c.side_vpath(id, !to_left) else {
            return;
        };
        self.plan.actions.push(Action {
            kind: if to_left {
                ActionKind::CopyToLeft
            } else {
                ActionKind::CopyToRight
            },
            node: id,
            bytes: src_side.size,
            is_folder: false,
            src: Some(src),
            dst: self.c.lenient_vpath(id, to_left),
            expect_src: Some(Fingerprint::of(&src_side)),
            expect_dst: dst_side.present.then(|| Fingerprint::of(&dst_side)),
            only_if_empty: false,
            frees_name: false,
            folder_mtime: None,
        });
    }

    /// The sibling that is the other half of a type conflict with `id`.
    fn counterpart(&self, id: NodeId) -> Option<NodeId> {
        let n = self.node(id);
        let key = |x: NodeId| fold_name(self.c.tree.display_name(x).units());
        let me = key(id);
        self.node(n.parent)
            .children()
            .find(|&o| o != id && self.node(o).has(flag::TYPE_CONFLICT) && key(o) == me)
    }

    /// Deletes an item; a folder is emptied item by item (post-order) and removed only when
    /// nothing is left in it, so nothing unseen or filtered out is ever deleted (§9.1, §9.4).
    fn plan_delete(&mut self, id: NodeId, from_left: bool) {
        let mut order = Vec::new();
        let mut stack = vec![(id, false)];
        while let Some((x, visited)) = stack.pop() {
            let n = self.node(x);
            if n.has(flag::GONE) {
                continue;
            }
            if n.is_dir() && !visited {
                stack.push((x, true));
                stack.extend(n.children().map(|c| (c, false)));
            } else {
                order.push(x);
            }
        }
        for x in order {
            let n = self.node(x).clone();
            let side = if from_left { n.left } else { n.right };
            if !side.present || n.has(flag::LINK) {
                if n.has(flag::LINK) {
                    self.plan.skipped_links.push(x);
                }
                continue;
            }
            let frees = self.node(id).has(flag::TYPE_CONFLICT);
            self.plan.actions.push(Action {
                kind: if from_left {
                    ActionKind::DeleteLeft
                } else {
                    ActionKind::DeleteRight
                },
                node: x,
                bytes: if side.kind == Kind::File {
                    side.size
                } else {
                    0
                },
                is_folder: side.kind == Kind::Dir,
                src: None,
                dst: self.c.lenient_vpath(x, from_left),
                expect_src: None,
                expect_dst: Some(Fingerprint::of(&side)),
                only_if_empty: side.kind == Kind::Dir,
                frees_name: frees,
                folder_mtime: None,
            });
        }
    }
}

fn summarise(plan: &mut Plan) {
    let mut s = Summary::default();
    for a in &plan.actions {
        match a.kind {
            ActionKind::CopyToLeft => {
                s.copies_to_left += 1;
                s.bytes_to_left += a.bytes;
            }
            ActionKind::CopyToRight => {
                s.copies_to_right += 1;
                s.bytes_to_right += a.bytes;
            }
            ActionKind::CreateFolderLeft | ActionKind::CreateFolderRight => s.folders_created += 1,
            ActionKind::DeleteLeft => s.deletes_left += 1,
            ActionKind::DeleteRight => s.deletes_right += 1,
        }
    }
    s.conflicts = plan.unresolved_prompts.len() as u32;
    s.errors = plan
        .issues
        .iter()
        .filter(|i| i.severity == Severity::Error)
        .count() as u32;
    s.skipped_links = plan.skipped_links.len() as u32;
    plan.summary = s;
}
