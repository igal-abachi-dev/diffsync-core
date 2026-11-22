//! The folder compare engine (plan.md §7.3, backlog E7-S3..S7, S9, S10): scan, name alignment
//! (D6), filters, classification, the content phase, refresh, and the root-overlap check (§9.7).

use std::collections::{HashMap, VecDeque};
use std::io::Read;
use std::sync::Arc;

use core_base::crc32::FileCrc;
use core_base::{CancelToken, Error, ErrorKind, Progress, Result, Stage};
use core_diff::session::{Verdict, compare_verdict};
use core_glob::{MatchOptions, RuleSet};
use core_text::{DecodeOptions, TextDocument};
use core_vfs::fold::fold_name;
use core_vfs::identity::{RootsRelation, roots_relation};
use core_vfs::{Entry, Kind, Name, VPath, Vfs, attr};
use rayon::prelude::*;
use unicode_normalization::UnicodeNormalization;

use crate::rules::{ContentKind, FolderRules};
use crate::tree::{Counters, NONE, Node, NodeId, SideInfo, Status, Tree, flag};

const BLOCK: usize = 1 << 20;

static NEXT_COMPARE_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

type Listing = Option<std::result::Result<Option<Vec<Entry>>, Error>>;

/// Rules-based compare decodes at most this many bytes per file (decision D35); a larger file
/// that differs byte-wise is `Different`.
pub const TEXT_COMPARE_CAP: u64 = 32 << 20;

/// The outcome of a content comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentResult {
    Same,
    /// Only unimportant text differs (rules-based).
    Unimportant,
    Different,
}

/// A running or finished folder compare.
pub struct FolderCompare {
    pub tree: Tree,
    pub rules: FolderRules,
    left: Arc<dyn Vfs>,
    right: Arc<dyn Vfs>,
    left_root: VPath,
    right_root: VPath,
    /// Warnings (nested roots, unknown fields...).
    pub warnings: Vec<String>,
    /// Nested roots: sync and file operations must refuse (§9.7).
    pub sync_blocked: bool,
    /// Unique per compare in this process: a sync plan is tied to the compare it came from.
    pub id: u64,
    /// Error messages of nodes in `Status::Error`.
    pub errors: HashMap<NodeId, String>,
    /// Files whose content compare is queued.
    pub pending: Vec<NodeId>,
    /// `exclude_files`: tested for files only (decision D32).
    ignore_files: RuleSet,
    /// `exclude_folders`: tested for folders only (decision D32).
    ignore_folders: RuleSet,
    include: RuleSet,
}

/// How the two roots relate (spec §9.7). `Same` is an error at start.
pub fn check_roots(
    left: &dyn Vfs,
    lp: &VPath,
    right: &dyn Vfs,
    rp: &VPath,
    allow_unchecked: bool,
) -> Result<(Option<RootsRelation>, Vec<String>, bool)> {
    let rel = roots_relation(left, lp, right, rp)?;
    let mut warnings = Vec::new();
    let mut blocked = false;
    match rel {
        Some(RootsRelation::Same) => {
            return Err(
                Error::invalid_argument("both sides are the same folder").with_path(lp.display())
            );
        }
        Some(RootsRelation::LeftInsideRight | RootsRelation::RightInsideLeft) => {
            warnings.push(
                "one folder is inside the other: sync and file operations are blocked".to_string(),
            );
            blocked = true;
        }
        Some(RootsRelation::Disjoint) => {}
        None => {
            // the backend has no file IDs, so nesting cannot be ruled out (plan.md §9.7). Decision
            // D68: that blocks sync and file operations, as a nested pair does, unless the caller
            // has said it knows better (`allow_unchecked_roots`): a mirror between a folder and
            // one inside it deletes files that were just compared
            if allow_unchecked {
                warnings.push(
                    "could not check whether the folders overlap (no file IDs); sync is allowed because allowUncheckedRoots is set".to_string(),
                );
            } else {
                warnings.push(
                    "could not check whether the folders overlap (no file IDs): sync and file operations are blocked (set allowUncheckedRoots to allow them)".to_string(),
                );
                blocked = true;
            }
        }
    }
    Ok((rel, warnings, blocked))
}

fn join(root: &VPath, rel: &VPath) -> VPath {
    rel.components()
        .iter()
        .fold(root.clone(), |p, n| p.join(n.clone()))
}

fn side_info(e: &Entry, name_off: u32, name_len: u16) -> SideInfo {
    SideInfo {
        size: e.size,
        mtime: e.mtime,
        attrs: e.attrs,
        name_off,
        name_len,
        kind: e.kind,
        present: true,
    }
}

impl FolderCompare {
    /// Marks an item (and everything below it) as ignored, or takes the mark off (backlog
    /// E11.5-S14.5). Ignored items are hidden by views that do not ask for them and left alone by
    /// a sync plan; the root cannot be ignored.
    pub fn set_ignored(&mut self, id: crate::NodeId, on: bool) -> core_base::Result<()> {
        if id == 0 || id as usize >= self.tree.len() {
            return Err(core_base::Error::invalid_argument(
                "that item cannot be ignored",
            ));
        }
        let n = self.tree.node_mut(id);
        if on {
            n.flags |= crate::tree::flag::IGNORED;
        } else {
            n.flags &= !crate::tree::flag::IGNORED;
        }
        Ok(())
    }

    /// The root folder of one side.
    pub fn root_path(&self, left_side: bool) -> &VPath {
        if left_side {
            &self.left_root
        } else {
            &self.right_root
        }
    }
}

impl std::fmt::Debug for FolderCompare {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FolderCompare")
            .field("nodes", &self.tree.len())
            .field("warnings", &self.warnings)
            .finish_non_exhaustive()
    }
}

impl FolderCompare {
    /// Scans both roots and (when the rules ask for it) compares content.
    pub fn compare(
        left: Arc<dyn Vfs>,
        left_root: &VPath,
        right: Arc<dyn Vfs>,
        right_root: &VPath,
        rules: FolderRules,
        cancel: &CancelToken,
    ) -> Result<FolderCompare> {
        Self::compare_with_progress(left, left_root, right, right_root, rules, cancel, &|_| {})
    }

    /// Like [`FolderCompare::compare`], reporting progress: `Scanning` (folders done / known) and
    /// `Comparing` (files done / queued). The callback may run on worker threads.
    pub fn compare_with_progress(
        left: Arc<dyn Vfs>,
        left_root: &VPath,
        right: Arc<dyn Vfs>,
        right_root: &VPath,
        rules: FolderRules,
        cancel: &CancelToken,
        progress: &(dyn Fn(Progress) + Sync),
    ) -> Result<FolderCompare> {
        let (_, warnings, sync_blocked) = check_roots(
            &*left,
            left_root,
            &*right,
            right_root,
            rules.allow_unchecked_roots,
        )?;
        for (v, r) in [(&left, left_root), (&right, right_root)] {
            match v.stat(r)? {
                Some(e) if e.kind == Kind::Dir => {}
                Some(_) => {
                    return Err(Error::invalid_argument("a compare root must be a folder")
                        .with_path(r.display()));
                }
                None => {
                    return Err(
                        Error::new(ErrorKind::NotFound, "the compare root does not exist")
                            .with_path(r.display()),
                    );
                }
            }
        }
        let mo = MatchOptions {
            case_sensitive: rules.case_sensitive_names,
            windows_paths: true,
            allow_negation: true,
        };
        // decision: D32: two rule sets; folder lines are dir-only (a trailing `/` is added)
        let folder_lines: Vec<String> = rules
            .exclude_folders
            .iter()
            .map(|l| {
                if l.ends_with('/') || l.starts_with('!') {
                    l.clone()
                } else {
                    format!("{l}/")
                }
            })
            .collect();
        let bad = |e: core_glob::PatternError| Error::invalid_argument(e.to_string());
        let ignore_folders = RuleSet::parse(&folder_lines, &mo).map_err(bad)?;
        let ignore_files = RuleSet::parse(&rules.exclude_files, &mo).map_err(bad)?;
        let include = RuleSet::parse(
            &rules.include_files,
            &MatchOptions {
                allow_negation: false,
                ..mo
            },
        )
        .map_err(|e| Error::invalid_argument(e.to_string()))?;
        let mut fc = FolderCompare {
            id: NEXT_COMPARE_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            tree: Tree::new(),
            left,
            right,
            left_root: left_root.clone(),
            right_root: right_root.clone(),
            warnings,
            sync_blocked,
            errors: HashMap::new(),
            pending: Vec::new(),
            ignore_files,
            ignore_folders,
            include,
            rules,
        };
        fc.scan(cancel, progress)?;
        if fc.rules.compare_content {
            fc.run_content(cancel, progress)?;
        }
        progress(Progress::new(Stage::Done, 1, 1));
        Ok(fc)
    }

    /// The absolute path (root joined) of a node on one side, using that side's own names;
    /// `None` when the node or an ancestor has no name there.
    pub fn side_vpath(&self, id: NodeId, left_side: bool) -> Option<VPath> {
        let rel = self.tree.side_path(id, left_side)?;
        Some(join(
            if left_side {
                &self.left_root
            } else {
                &self.right_root
            },
            &rel,
        ))
    }

    /// The absolute path where a node is, or would be created, on one side: components
    /// without a name on that side take the other side's name.
    pub fn lenient_vpath(&self, id: NodeId, left_side: bool) -> VPath {
        join(
            if left_side {
                &self.left_root
            } else {
                &self.right_root
            },
            &self.tree.side_path_lenient(id, left_side),
        )
    }

    /// The file system of one side.
    pub fn vfs(&self, left_side: bool) -> &Arc<dyn Vfs> {
        if left_side { &self.left } else { &self.right }
    }

    fn fold_key(&self, name: &Name) -> Vec<u16> {
        let mut units: Vec<u16> = name.units().to_vec();
        // ASCII is always NFC, so the normaliser only runs for names that need it
        if self.rules.normalized_alignment && units.iter().any(|&u| u >= 0x80) {
            // NFC first (D6), then per-unit uppercase; lone surrogates cannot be normalised
            if let Ok(s) = String::from_utf16(&units) {
                units = s.nfc().collect::<String>().encode_utf16().collect();
            }
        }
        if !self.rules.case_sensitive_names {
            units = fold_name(&units);
        }
        units
    }

    fn key(&self, e: &Entry) -> (bool, Vec<u16>) {
        let is_dir = e.kind == Kind::Dir;
        let mut units = self.fold_key(&e.name);
        if self.rules.align_ignore_extensions && !is_dir {
            // decision: D34: a leading dot is part of the name (`.gitignore` keeps its name)
            if let Some(dot) = units.iter().rposition(|&u| u == u16::from(b'.')) {
                if dot > 0 {
                    units.truncate(dot);
                }
            }
        }
        (is_dir, units)
    }

    /// decision: D18: hidden and system items are shown unless the rules skip them.
    /// True when any filter or hidden/system skip is active: items outside the compare may
    /// exist on disk, so a folder delete must not take them along (plan.md §9.4).
    pub fn filters_active(&self) -> bool {
        !self.ignore_files.is_empty()
            || !self.ignore_folders.is_empty()
            || !self.include.is_empty()
            || !self.rules.include_hidden
            || !self.rules.include_system
    }

    fn hidden_skipped(&self, e: &Entry) -> bool {
        (!self.rules.include_hidden && e.attrs & attr::HIDDEN != 0)
            || (!self.rules.include_system && e.attrs & attr::SYSTEM != 0)
    }

    /// Does a filter exclude this entry (given the names of its parent folder)? decision D32:
    /// `exclude_files` applies to files, `exclude_folders` to folders, `include_files` to files.
    fn excluded(&self, parent_names: &[Name], e: &Entry) -> bool {
        let is_dir = e.kind == Kind::Dir;
        let set = if is_dir {
            &self.ignore_folders
        } else {
            &self.ignore_files
        };
        if set.is_empty() && (is_dir || self.include.is_empty()) {
            return false;
        }
        let mut names: Vec<&[u16]> = parent_names.iter().map(Name::units).collect();
        names.push(e.name.units());
        // the folders above passed their own rules already, so only the leaf is decided here
        if !set.is_empty() && set.decide_leaf(&names, is_dir).is_excluded() {
            return true;
        }
        !is_dir
            && !self.include.is_empty()
            && !self.include.decide_names(&names, false).is_excluded()
    }

    /// `Ok(None)`: the folder vanished between listing its parent and opening it.
    fn list_side(
        &self,
        id: NodeId,
        left_side: bool,
    ) -> std::result::Result<Option<Vec<Entry>>, Error> {
        let Some(path) = self.side_vpath(id, left_side) else {
            return Ok(Some(Vec::new()));
        };
        let mut out = Vec::new();
        match self.vfs(left_side).list(&path, &mut out) {
            Ok(()) => Ok(Some(out)),
            Err(e) if e.kind() == ErrorKind::NotFound && id != Tree::ROOT => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn scan(&mut self, cancel: &CancelToken, progress: &(dyn Fn(Progress) + Sync)) -> Result<()> {
        let mut queue = VecDeque::from([Tree::ROOT]);
        let (mut done, mut known) = (0u64, 1u64);
        while let Some(id) = queue.pop_front() {
            cancel.check()?;
            done += 1;
            progress(Progress::new(Stage::Scanning, done, known));
            let node = self.tree.node(id).clone();
            let parent_names: Vec<Name> = self.tree.path(id).components().to_vec();
            // both sides are listed at the same time (plan.md §7.3 step 1)
            let (l, r) = rayon::join(
                || {
                    (node.left.present && node.left.kind == Kind::Dir)
                        .then(|| self.list_side(id, true))
                },
                || {
                    (node.right.present && node.right.kind == Kind::Dir)
                        .then(|| self.list_side(id, false))
                },
            );
            let fail = |res: &Listing| res.as_ref().and_then(|x| x.as_ref().err()).cloned();
            if let Some(e) = fail(&l).or_else(|| fail(&r)) {
                if id == Tree::ROOT {
                    return Err(e); // an unreadable root fails the session
                }
                self.tree.set_status(id, Status::Error);
                self.errors.insert(id, e.to_string());
                continue;
            }
            // a side that vanished mid-scan stops being present: the folder becomes an orphan
            // (E7-S3.2), and what is left on the other side is listed as orphans below it
            let (mut lv, mut rv) = (Vec::new(), Vec::new());
            let mut vanished = [false, false];
            for (i, (res, out)) in [(l, &mut lv), (r, &mut rv)].into_iter().enumerate() {
                match res.and_then(Result::ok) {
                    Some(Some(list)) => *out = list,
                    Some(None) => vanished[i] = true,
                    None => {}
                }
            }
            if vanished[0] || vanished[1] {
                let n = self.tree.node_mut(id);
                if vanished[0] {
                    n.left.present = false;
                }
                if vanished[1] {
                    n.right.present = false;
                }
                let st = match (n.left.present, n.right.present) {
                    (true, false) => Status::LeftOrphan,
                    (false, true) => Status::RightOrphan,
                    _ => Status::Error,
                };
                self.tree.set_status(id, st);
            }
            let lists = (lv, rv);
            let kids = self.pair_children(&parent_names, lists);
            if kids.is_empty() {
                continue;
            }
            let first = self.tree.add_children(id, kids);
            let n = self.tree.node(id).child_count;
            for k in first..first + n {
                let kn = self.tree.node(k);
                let is_link = kn.has(flag::LINK);
                if !is_link && kn.is_dir() && kn.status != Status::Error {
                    queue.push_back(k);
                    known += 1;
                }
                if kn.has(flag::PENDING_CONTENT) {
                    self.pending.push(k);
                }
            }
        }
        self.tree.shrink();
        Ok(())
    }

    /// Pairs the entries of the two sides by key, applies the filters, and builds the child
    /// nodes in key order (folders first).
    fn pair_children(
        &mut self,
        parent_names: &[Name],
        (left, right): (Vec<Entry>, Vec<Entry>),
    ) -> Vec<Node> {
        struct Pair {
            key: (bool, Vec<u16>),
            l: Option<Entry>,
            r: Option<Entry>,
            dup: bool,
        }
        let mut by_key: HashMap<(bool, Vec<u16>), VecDeque<usize>> = HashMap::new();
        let mut left_count: HashMap<(bool, Vec<u16>), usize> = HashMap::new();
        for (i, e) in right.iter().enumerate() {
            by_key.entry(self.key(e)).or_default().push_back(i);
        }
        let mut dup_keys: std::collections::HashSet<(bool, Vec<u16>)> = by_key
            .iter()
            .filter(|(_, q)| q.len() > 1)
            .map(|(k, _)| k.clone())
            .collect();
        for e in &left {
            let k = self.key(e);
            let c = left_count.entry(k.clone()).or_default();
            *c += 1;
            if *c > 1 {
                dup_keys.insert(k);
            }
        }
        let mut right_used = vec![false; right.len()];
        let mut pairs: Vec<Pair> = Vec::new();
        let mut right_opt: Vec<Option<Entry>> = right.into_iter().map(Some).collect();
        // duplicates on one side are paired in order and flagged (spec FOLDER-3)
        for e in left {
            let key = self.key(&e);
            let dup = dup_keys.contains(&key);
            match by_key.get_mut(&key).and_then(VecDeque::pop_front) {
                Some(j) => {
                    right_used[j] = true;
                    pairs.push(Pair {
                        key,
                        l: Some(e),
                        r: right_opt[j].take(),
                        dup,
                    });
                }
                None => pairs.push(Pair {
                    key,
                    l: Some(e),
                    r: None,
                    dup,
                }),
            }
        }
        for (j, used) in right_used.iter().enumerate() {
            if !used {
                if let Some(e) = right_opt[j].take() {
                    let key = self.key(&e);
                    let dup = dup_keys.contains(&key);
                    pairs.push(Pair {
                        key,
                        l: None,
                        r: Some(e),
                        dup,
                    });
                }
            }
        }
        // filters decide per pair: excluded when either side's name matches
        pairs.retain(|p| {
            // decision: D18: hidden/system is decided per pair too, so attribute skew between
            // the sides cannot turn the visible twin into an orphan
            let hidden = |e: &Option<Entry>| e.as_ref().is_some_and(|e| self.hidden_skipped(e));
            if hidden(&p.l) || hidden(&p.r) {
                return false;
            }
            !p.l.as_ref().is_some_and(|e| self.excluded(parent_names, e))
                && !p.r.as_ref().is_some_and(|e| self.excluded(parent_names, e))
        });
        // folders first, then by folded name; left before right for equal keys
        pairs.sort_by(|a, b| (!a.key.0, &a.key.1).cmp(&(!b.key.0, &b.key.1)));

        // type conflicts: an orphan file on one side and an orphan folder of the same name on
        // the other (plan.md §7.3 step 3; decision D34: same-side pairs are not conflicts)
        let one_sided = |p: &Pair| p.l.is_some() != p.r.is_some();
        let mut orphan_dirs: HashMap<&[u16], Vec<(usize, bool)>> = HashMap::new();
        for (i, p) in pairs.iter().enumerate() {
            if one_sided(p) && p.key.0 {
                orphan_dirs
                    .entry(&p.key.1)
                    .or_default()
                    .push((i, p.l.is_some()));
            }
        }
        let mut conflicts = vec![false; pairs.len()];
        for (i, p) in pairs.iter().enumerate() {
            if one_sided(p) && !p.key.0 {
                if let Some(ds) = orphan_dirs.get(p.key.1.as_slice()) {
                    for &(d, dir_on_left) in ds {
                        if dir_on_left != p.l.is_some() {
                            conflicts[i] = true;
                            conflicts[d] = true;
                        }
                    }
                }
            }
        }

        let mut kids = Vec::with_capacity(pairs.len());
        for (i, p) in pairs.into_iter().enumerate() {
            let mut node = Node {
                parent: NONE,
                first_child: NONE,
                child_count: 0,
                depth: 0,
                status: Status::Same,
                flags: 0,
                left: SideInfo::ABSENT,
                right: SideInfo::ABSENT,
                counters: Counters::default(),
            };
            if let Some(e) = &p.l {
                let (off, len) = self.tree.push_name(&e.name);
                node.left = side_info(e, off, len);
            }
            if let Some(e) = &p.r {
                // identical names (the usual case) are stored once
                let (off, len) = match &p.l {
                    Some(l) if l.name == e.name => (node.left.name_off, node.left.name_len),
                    _ => self.tree.push_name(&e.name),
                };
                node.right = side_info(e, off, len);
            }
            if p.dup {
                node.flags |= flag::DUPLICATE_KEY;
            }
            if conflicts[i] {
                node.flags |= flag::TYPE_CONFLICT;
            }
            if [&p.l, &p.r]
                .iter()
                .any(|e| e.as_ref().is_some_and(|e| e.kind == Kind::Link))
            {
                node.flags |= flag::LINK;
            }
            if [&p.l, &p.r]
                .iter()
                .any(|e| e.as_ref().is_some_and(Entry::is_offline))
            {
                node.flags |= flag::OFFLINE;
            }
            self.classify(&mut node);
            kids.push(node);
        }
        kids
    }

    /// The status of two files known to differ: no newer side when the times are equal.
    fn diff_status(&self, l: &SideInfo, r: &SideInfo) -> Status {
        if self.rules.times_equal(l.mtime, r.mtime) {
            Status::Different
        } else if l.mtime > r.mtime {
            Status::LeftNewer
        } else {
            Status::RightNewer
        }
    }

    /// The status of a paired file from size, time and attributes (`ClassifyFile`, quick test).
    fn quick(&self, l: &SideInfo, r: &SideInfo) -> (bool, Status) {
        let rules = &self.rules;
        let times_same = !rules.compare_timestamp || rules.times_equal(l.mtime, r.mtime);
        let mask = attr::READONLY | attr::HIDDEN | attr::SYSTEM | attr::ARCHIVE;
        let same = (!rules.compare_size || l.size == r.size)
            && times_same
            && (!rules.compare_attributes || l.attrs & mask == r.attrs & mask);
        if same {
            return (true, Status::Same);
        }
        let newer = self.diff_status(l, r);
        (false, newer)
    }

    /// Sets the status of a freshly built node, and queues content compare when needed.
    fn classify(&self, n: &mut Node) {
        n.status = match (n.left.present, n.right.present) {
            (true, false) => Status::LeftOrphan,
            (false, true) => Status::RightOrphan,
            (false, false) => Status::Error,
            (true, true) => {
                let (l, r) = (n.left, n.right);
                if l.kind == Kind::Dir && r.kind == Kind::Dir {
                    Status::Same
                } else if l.kind == Kind::Link || r.kind == Kind::Link {
                    // decision: D31: links are compared by presence only, never followed; a link
                    // paired with a non-link differs
                    if l.kind == r.kind {
                        Status::Same
                    } else {
                        Status::Different
                    }
                } else {
                    let (quick_same, quick) = self.quick(&l, &r);
                    if self.rules.compare_content {
                        let offline =
                            n.flags & flag::OFFLINE != 0 && !self.rules.compare_offline_files; // D17
                        if offline || (self.rules.content_skip_same && quick_same) {
                            quick
                        } else if l.size != r.size
                            && self.rules.content_kind != ContentKind::RulesBased
                        {
                            // spec: FOLDER-2: sizes differ, so different without reading
                            n.flags |= flag::CONTENT_COMPARED;
                            self.diff_status(&l, &r)
                        } else {
                            n.flags |= flag::PENDING_CONTENT;
                            quick
                        }
                    } else {
                        quick
                    }
                }
            }
        };
    }

    /// Runs the queued content compares on a pool limited to `content_workers` threads.
    pub fn run_content(
        &mut self,
        cancel: &CancelToken,
        progress: &(dyn Fn(Progress) + Sync),
    ) -> Result<()> {
        let Some(batch) = self.take_content_batch() else {
            return Ok(()); // no pool for nothing: refresh calls this after every sync action
        };
        match batch.run(cancel, progress) {
            Ok(results) => {
                self.apply_content_results(results);
                Ok(())
            }
            Err(e) => {
                self.requeue(batch);
                Err(e)
            }
        }
    }

    /// Takes the queued content compares as a batch that can run **without** the compare
    /// (no lock needed), then be applied with [`FolderCompare::apply_content_results`].
    pub fn take_content_batch(&mut self) -> Option<ContentBatch> {
        let ids = std::mem::take(&mut self.pending);
        if ids.is_empty() {
            return None;
        }
        let jobs: Vec<(NodeId, VPath, VPath)> = ids
            .iter()
            .filter_map(|&id| Some((id, self.side_vpath(id, true)?, self.side_vpath(id, false)?)))
            .collect();
        Some(ContentBatch {
            ids,
            jobs,
            left: self.left.clone(),
            right: self.right.clone(),
            kind: self.rules.content_kind,
            text: self.rules.text.clone(),
            workers: self.rules.content_workers.max(1),
        })
    }

    /// Puts a batch that did not run (cancelled) back in the queue.
    pub fn requeue(&mut self, batch: ContentBatch) {
        self.pending = batch.ids;
    }

    /// Applies the results of a [`ContentBatch`].
    pub fn apply_content_results(&mut self, results: Vec<(NodeId, Result<ContentResult>)>) {
        for (id, res) in results {
            self.apply_content(id, res);
        }
    }

    /// Queues the paired files among `ids` for content compare (the rules may have it off).
    pub fn queue_content(&mut self, ids: &[NodeId]) {
        for &id in ids {
            let n = self.tree.node(id);
            if n.left.present
                && n.right.present
                && n.left.kind == Kind::File
                && n.right.kind == Kind::File
                // decision: D17: reading a cloud placeholder downloads it
                && (!n.has(flag::OFFLINE) || self.rules.compare_offline_files)
            {
                self.tree.node_mut(id).flags |= flag::PENDING_CONTENT;
                self.pending.push(id);
            }
        }
    }

    fn apply_content(&mut self, id: NodeId, res: Result<ContentResult>) {
        let (l, r) = {
            let n = self.tree.node(id);
            (n.left, n.right)
        };
        let (quick_same, quick) = self.quick(&l, &r);
        let status = match res {
            Err(e) => {
                self.errors.insert(id, e.to_string());
                Status::Error
            }
            Ok(c) => {
                let diff = self.diff_status(&l, &r);
                // spec: FOLDER-2: the content result overrides the quick result, or confirms it
                // when the quick test said "same"
                if self.rules.content_overrides || quick_same {
                    match c {
                        ContentResult::Same => Status::Same,
                        ContentResult::Unimportant => Status::UnimportantDiff,
                        ContentResult::Different => diff,
                    }
                } else {
                    quick
                }
            }
        };
        let n = self.tree.node_mut(id);
        n.flags = (n.flags & !flag::PENDING_CONTENT) | flag::CONTENT_COMPARED;
        self.tree.set_status(id, status);
    }

    /// Runs content compare for these nodes even when the rules have it off.
    pub fn compare_content_now(&mut self, ids: &[NodeId], cancel: &CancelToken) -> Result<()> {
        self.queue_content(ids);
        self.run_content(cancel, &|_| {})
    }

    /// Re-stats both sides of the nodes and reclassifies them (ancestors' counters follow).
    ///
    /// Limits: a node whose kind changes (file to folder) or a folder that becomes one-sided is
    /// reclassified but its children are not rescanned; the caller re-runs the compare for that.
    pub fn refresh(&mut self, ids: &[NodeId], cancel: &CancelToken) -> Result<()> {
        self.refresh_stat(ids, cancel)?;
        self.run_content(cancel, &|_| {})
    }

    /// The stat-and-classify half of [`FolderCompare::refresh`]: content compares that are
    /// needed stay queued (see [`FolderCompare::take_content_batch`]).
    pub fn refresh_stat(&mut self, ids: &[NodeId], cancel: &CancelToken) -> Result<()> {
        let probes = self.probe_batch(ids).run(cancel)?;
        self.apply_probes(probes);
        Ok(())
    }

    /// The paths and file systems of a refresh, detached from the compare: the stat calls of
    /// [`ProbeBatch::run`] need no lock, and [`FolderCompare::apply_probes`] is short.
    pub fn probe_batch(&self, ids: &[NodeId]) -> ProbeBatch {
        ProbeBatch {
            jobs: ids
                .iter()
                .map(|&id| {
                    (
                        id,
                        self.lenient_vpath(id, true),
                        self.lenient_vpath(id, false),
                    )
                })
                .collect(),
            left: self.left.clone(),
            right: self.right.clone(),
        }
    }

    /// Applies what a [`ProbeBatch`] saw: reclassifies the nodes (ancestors' counters follow).
    pub fn apply_probes(&mut self, probes: Vec<Probe>) {
        for p in probes {
            let id = p.id;
            let mut node = self.tree.node(id).clone();
            for left_side in [true, false] {
                let existing = if left_side { node.left } else { node.right };
                let other = if left_side { node.right } else { node.left };
                let seen = if left_side { &p.left } else { &p.right };
                match seen {
                    // decision: D47: an entry found under the other side's name is adopted only
                    // when it is the same kind of item; a file `x` is not the folder `x` that
                    // was just deleted (type conflicts)
                    Some(e) if !existing.present && other.present && e.kind != other.kind => {
                        if left_side {
                            node.left.present = false
                        } else {
                            node.right.present = false
                        }
                    }
                    Some(e) => {
                        let (off, len) = if existing.present {
                            (existing.name_off, existing.name_len)
                        } else {
                            self.tree.push_name(&e.name)
                        };
                        let info = side_info(e, off, len);
                        if left_side {
                            node.left = info
                        } else {
                            node.right = info
                        }
                    }
                    None => {
                        if left_side {
                            node.left.present = false
                        } else {
                            node.right.present = false
                        }
                    }
                }
            }
            node.flags &= !(flag::PENDING_CONTENT | flag::CONTENT_COMPARED);
            self.classify(&mut node);
            if !node.left.present && !node.right.present {
                // deleted on both sides: the node stays in the arena, flagged and neutral
                node.flags |= flag::GONE;
                node.status = Status::Same;
            } else {
                node.flags &= !flag::GONE;
            }
            let (status, flags, l, r) = (node.status, node.flags, node.left, node.right);
            let n = self.tree.node_mut(id);
            n.left = l;
            n.right = r;
            n.flags = flags;
            self.tree.set_status(id, status);
            if flags & flag::PENDING_CONTENT != 0 {
                self.pending.push(id);
            }
        }
    }
}

/// What a refresh saw for one node: the stat result of each side (`None` = not there).
#[derive(Debug, Clone)]
pub struct Probe {
    id: NodeId,
    left: Option<Entry>,
    right: Option<Entry>,
}

/// The stat calls of a refresh, detached from the compare so they run without any lock.
pub struct ProbeBatch {
    jobs: Vec<(NodeId, VPath, VPath)>,
    left: Arc<dyn Vfs>,
    right: Arc<dyn Vfs>,
}

impl std::fmt::Debug for ProbeBatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProbeBatch")
            .field("nodes", &self.jobs.len())
            .finish_non_exhaustive()
    }
}

impl ProbeBatch {
    pub fn run(&self, cancel: &CancelToken) -> Result<Vec<Probe>> {
        let mut out = Vec::with_capacity(self.jobs.len());
        for (id, lp, rp) in &self.jobs {
            cancel.check()?;
            out.push(Probe {
                id: *id,
                left: self.left.stat(lp)?,
                right: self.right.stat(rp)?,
            });
        }
        Ok(out)
    }
}

/// Queued content compares, detached from the compare so they run without holding any lock.
pub struct ContentBatch {
    ids: Vec<NodeId>,
    jobs: Vec<(NodeId, VPath, VPath)>,
    left: Arc<dyn Vfs>,
    right: Arc<dyn Vfs>,
    kind: ContentKind,
    text: core_diff::TextOptions,
    workers: usize,
}

impl std::fmt::Debug for ContentBatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContentBatch")
            .field("files", &self.jobs.len())
            .finish_non_exhaustive()
    }
}

impl ContentBatch {
    /// The nodes in the batch.
    pub fn nodes(&self) -> &[NodeId] {
        &self.ids
    }

    /// Runs the compares on a pool of `content_workers` threads (decision: D30).
    pub fn run(
        &self,
        cancel: &CancelToken,
        progress: &(dyn Fn(Progress) + Sync),
    ) -> Result<Vec<(NodeId, Result<ContentResult>)>> {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(self.workers)
            .build()
            .map_err(|e| Error::internal(format!("content pool: {e}")))?;
        let total = self.jobs.len() as u64;
        let done = std::sync::atomic::AtomicU64::new(0);
        let results: Vec<(NodeId, Result<ContentResult>)> = pool.install(|| {
            self.jobs
                .par_iter()
                .map(|(id, lp, rp)| {
                    let r = content_compare(
                        &*self.left,
                        lp,
                        &*self.right,
                        rp,
                        self.kind,
                        &self.text,
                        cancel,
                    );
                    let n = done.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                    progress(Progress::new(Stage::Comparing, n, total));
                    (*id, r)
                })
                .collect()
        });
        cancel.check()?;
        Ok(results)
    }
}

/// Compares the content of two files (spec FOLDER-2, decision D11: 1 MiB blocks). A file that
/// changes during the compare gives `Error("changed during compare")`.
pub fn content_compare(
    lv: &dyn Vfs,
    lp: &VPath,
    rv: &dyn Vfs,
    rp: &VPath,
    kind: ContentKind,
    text: &core_diff::TextOptions,
    cancel: &CancelToken,
) -> Result<ContentResult> {
    let before = (lv.stat(lp)?, rv.stat(rp)?);
    let (Some(bl), Some(br)) = before else {
        return Err(Error::new(
            ErrorKind::ChangedSinceCompare,
            "changed during compare: a file vanished",
        ));
    };
    let result = match kind {
        ContentKind::Crc32 => {
            // set when one side failed: the other side stops reading instead of hashing a
            // multi-GB file whose result nobody will use
            let stop = std::sync::atomic::AtomicBool::new(false);
            let crc = |v: &dyn Vfs, p: &VPath| -> Result<u32> {
                let one = || -> Result<u32> {
                    let mut r = v.open_read(p)?;
                    let (mut buf, mut c) = (vec![0u8; BLOCK], FileCrc::new());
                    loop {
                        cancel.check()?;
                        if stop.load(std::sync::atomic::Ordering::Relaxed) {
                            return Err(Error::new(ErrorKind::Cancelled, "the other side failed"));
                        }
                        let n = std::io::Read::read(&mut r, &mut buf)
                            .map_err(|e| Error::from_io(&e, p.display()))?;
                        if n == 0 {
                            break;
                        }
                        c.update(&buf[..n]);
                    }
                    Ok(c.finalize())
                };
                let res = one();
                if res.is_err() {
                    stop.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                res
            };
            // perf: big files hash both sides at once, so one pair keeps the disk busy (§12.2)
            let (cl, cr) = if overlap_reads(lv, rv, bl.size.min(br.size)) {
                std::thread::scope(|s| {
                    let right = s.spawn(|| crc(rv, rp));
                    (
                        crc(lv, lp),
                        right.join().unwrap_or_else(|_| {
                            Err(Error::new(ErrorKind::Internal, "crc worker panicked"))
                        }),
                    )
                })
            } else {
                (crc(lv, lp), crc(rv, rp))
            };
            // both failed: the "other side failed" stop error belongs to the side that failed second
            let (cl, cr) = match (cl, cr) {
                (Ok(a), Ok(b)) => (a, b),
                (Err(e), Ok(_)) | (Ok(_), Err(e)) => return Err(e),
                (Err(a), Err(b)) => {
                    return Err(if a.is_cancelled() && !cancel.is_cancelled() {
                        b
                    } else {
                        a
                    });
                }
            };
            if cl == cr {
                ContentResult::Same
            } else {
                ContentResult::Different
            }
        }
        ContentKind::Binary => bytes_equal(lv, lp, rv, rp, bl.size.min(br.size), cancel)?,
        ContentKind::RulesBased => {
            // decision: D35: stream first (early stop, cancellable); only differing files that are
            // small enough are decoded, so a large or binary file never fills memory
            match bytes_equal(lv, lp, rv, rp, bl.size.min(br.size), cancel)? {
                ContentResult::Same => ContentResult::Same,
                _ if bl.size > TEXT_COMPARE_CAP || br.size > TEXT_COMPARE_CAP => {
                    ContentResult::Different
                }
                _ => {
                    let (a, b) = (read_capped(lv, lp, cancel)?, read_capped(rv, rp, cancel)?);
                    let (da, db) = (
                        TextDocument::from_bytes(a, &DecodeOptions::default())?,
                        TextDocument::from_bytes(b, &DecodeOptions::default())?,
                    );
                    if da.is_binary() || db.is_binary() {
                        ContentResult::Different
                    } else {
                        match compare_verdict(&da, &db, text, cancel)? {
                            Verdict::Same => ContentResult::Same,
                            Verdict::UnimportantOnly => ContentResult::Unimportant,
                            Verdict::Different => ContentResult::Different,
                        }
                    }
                }
            }
        }
    };
    let after = (lv.stat(lp)?, rv.stat(rp)?);
    match after {
        (Some(al), Some(ar))
            if (al.size, al.mtime) == (bl.size, bl.mtime)
                && (ar.size, ar.mtime) == (br.size, br.mtime) =>
        {
            Ok(result)
        }
        _ => Err(Error::new(
            ErrorKind::ChangedSinceCompare,
            "changed during compare",
        )),
    }
}

/// Files at least this big may be read on both sides at the same time.
const OVERLAP_MIN: u64 = 8 << 20;

/// decision: D54: the two sides of a big file are read at the same time, except when both are on
/// rotating disks: there the head would travel between the two files and the compare ran a quarter
/// slower than reading them one after the other (measured on a SATA HDD: 0.75 of the serial speed).
/// On an SSD, a network share or two different kinds of storage the overlap is a gain.
fn overlap_reads(lv: &dyn Vfs, rv: &dyn Vfs, smaller: u64) -> bool {
    smaller >= OVERLAP_MIN && !(lv.caps().seek_penalty && rv.caps().seek_penalty)
}

/// The slice size of the one-after-the-other compare: normally 1 MiB. On two rotating disks the
/// compare alternates between the two files, and every switch costs a seek, so it alternates in big
/// slices (32 MiB, about a quarter of a second of transfer): measured on a SATA HDD, 1 MiB slices
/// ran at 0.85 of the serial read speed (decision: D54).
fn serial_block(lv: &dyn Vfs, rv: &dyn Vfs, smaller: u64) -> usize {
    if smaller >= OVERLAP_MIN && lv.caps().seek_penalty && rv.caps().seek_penalty {
        (smaller.min(ROTATING_BLOCK as u64) as usize).max(BLOCK)
    } else {
        BLOCK
    }
}

/// See [`serial_block`].
const ROTATING_BLOCK: usize = 32 << 20;

fn bytes_equal(
    lv: &dyn Vfs,
    lp: &VPath,
    rv: &dyn Vfs,
    rp: &VPath,
    smaller: u64,
    cancel: &CancelToken,
) -> Result<ContentResult> {
    let (mut a, mut b) = (lv.open_read(lp)?, rv.open_read(rp)?);
    if overlap_reads(lv, rv, smaller) {
        return bytes_equal_overlapped(&mut *a, lp, b, rp, cancel);
    }
    let block = serial_block(lv, rv, smaller);
    let (mut ba, mut bb) = (vec![0u8; block], vec![0u8; block]);
    loop {
        cancel.check()?;
        let na = read_full(&mut *a, &mut ba).map_err(|e| Error::from_io(&e, lp.display()))?;
        let nb = read_full(&mut *b, &mut bb).map_err(|e| Error::from_io(&e, rp.display()))?;
        if na != nb || ba[..na] != bb[..nb] {
            return Ok(ContentResult::Different); // stop at the first difference
        }
        if na == 0 {
            return Ok(ContentResult::Same);
        }
    }
}

/// perf: the right side is read ahead on a helper thread (3 blocks in flight, buffers are
/// recycled), so both disks, or both halves of one disk's queue, work at once (plan §12.2).
fn bytes_equal_overlapped(
    a: &mut dyn Read,
    lp: &VPath,
    mut b: Box<dyn core_vfs::ReadSeek>,
    rp: &VPath,
    cancel: &CancelToken,
) -> Result<ContentResult> {
    use std::sync::mpsc::{channel, sync_channel};
    std::thread::scope(|s| {
        let (tx, rx) = sync_channel::<std::io::Result<Vec<u8>>>(3);
        let (back_tx, back_rx) = channel::<Vec<u8>>();
        let helper = s.spawn(move || {
            loop {
                if cancel.is_cancelled() {
                    break;
                }
                let mut buf = back_rx.try_recv().unwrap_or_default();
                buf.resize(BLOCK, 0);
                match read_full(&mut *b, &mut buf) {
                    Ok(n) => {
                        buf.truncate(n);
                        if tx.send(Ok(buf)).is_err() || n == 0 {
                            break;
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e));
                        break;
                    }
                }
            }
        });
        let result = (|| {
            let mut ba = vec![0u8; BLOCK];
            loop {
                cancel.check()?;
                let na = read_full(a, &mut ba).map_err(|e| Error::from_io(&e, lp.display()))?;
                let Ok(next) = rx.recv() else {
                    cancel.check()?; // the helper stopped because of a cancel
                    // otherwise it panicked: reported by the join below
                    return Err(Error::new(ErrorKind::Internal, "read-ahead stopped early"));
                };
                let bb = next.map_err(|e| Error::from_io(&e, rp.display()))?;
                if na != bb.len() || ba[..na] != bb[..] {
                    return Ok(ContentResult::Different); // stop at the first difference
                }
                if na == 0 {
                    return Ok(ContentResult::Same);
                }
                let _ = back_tx.send(bb);
            }
        })();
        // both ends go before the join, so a helper blocked on `send` ends; a panic in the helper
        // is an error of this one file, never a panic of the whole compare (reviewer, E11-S6)
        drop(rx);
        if helper.join().is_err() {
            return Err(Error::new(
                ErrorKind::Internal,
                "read-ahead worker panicked",
            ));
        }
        result
    })
}

fn read_full(r: &mut dyn Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}

/// Reads a whole (capped) file in 1 MiB blocks, checking for cancel between blocks.
fn read_capped(v: &dyn Vfs, p: &VPath, cancel: &CancelToken) -> Result<Vec<u8>> {
    let mut r = v.open_read(p)?;
    let (mut out, mut buf) = (Vec::new(), vec![0u8; BLOCK]);
    loop {
        cancel.check()?;
        let n = read_full(&mut *r, &mut buf).map_err(|e| Error::from_io(&e, p.display()))?;
        out.extend_from_slice(&buf[..n]);
        if n < BLOCK {
            return Ok(out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_vfs::MemVfs;

    /// decision: D54: big files are read on both sides at once, unless both sides are rotating
    /// disks (the head would travel between the files).
    #[test]
    fn overlap_policy_follows_the_seek_penalty() {
        let (ssd, hdd) = (MemVfs::new(), MemVfs::new().rotating_disk());
        let big = OVERLAP_MIN;
        assert!(overlap_reads(&ssd, &ssd, big), "two SSDs overlap");
        assert!(
            overlap_reads(&ssd, &hdd, big),
            "an SSD with a disk: different devices"
        );
        assert!(overlap_reads(&hdd, &ssd, big));
        assert!(
            !overlap_reads(&hdd, &hdd, big),
            "two rotating disks take turns"
        );
        assert!(
            !overlap_reads(&ssd, &ssd, big - 1),
            "small files never overlap"
        );
    }
}
