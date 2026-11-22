//! Folder compare and view exports (plan.md §10.2; backlog E9-S5). The scan and the content
//! compare run on core threads; every read function takes a short read lock and never waits for
//! them.
#![allow(unsafe_code)]

use std::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use core_base::{CancelToken, Stage};
use core_folder::{FolderCompare, FolderView, NodeId};
use core_vfs::Vfs;
use parking_lot::{Mutex, RwLock};

use crate::events::Bridge;
use crate::options;
use crate::registry::{self, Object};
use crate::status::{DS_E_NOT_FOUND, DsStatus, Fail, Res, ffi_entry};
use crate::types::*;
use crate::util::{finish, json_arg, os_path, out, slice, units, write_buffer};

pub type Shared = Arc<RwLock<FolderCompare>>;

#[derive(Debug, Clone)]
pub enum FolderState {
    Scanning,
    Done,
    /// The kind of the engine error (so a missing root stays `DS_E_NOT_FOUND`) and its message.
    Failed(core_base::ErrorKind, String),
    Cancelled,
}

pub struct FolderObj {
    pub bridge: Arc<Bridge>,
    /// Cancels the scan (`ds_folder_cancel`).
    cancel: CancelToken,
    /// Cancelled when the handle is closed: stops scan and background work.
    closed: CancelToken,
    state: Mutex<FolderState>,
    pub compare: Mutex<Option<Shared>>,
    stage: AtomicU32,
    done: AtomicU64,
    total: AtomicU64,
    /// Bumped whenever the tree changes; views rebuild when it moves.
    pub revision: Arc<AtomicU64>,
}

impl Drop for FolderObj {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.closed.cancel();
    }
}

impl std::fmt::Debug for FolderObj {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FolderObj")
    }
}

impl FolderObj {
    /// `ds_close`: stop all work behind the handle and detach the events (a scan or background
    /// job that is still running finishes silently).
    pub fn shutdown(&self) {
        self.cancel.cancel();
        self.closed.cancel();
        self.bridge.detach();
    }

    /// The finished compare, or `DS_E_BUSY` while it is still scanning.
    pub fn ready(&self) -> Res<Shared> {
        match &*self.state.lock() {
            FolderState::Done => {}
            FolderState::Scanning => return Err(Fail::busy("the folder compare is still running")),
            FolderState::Failed(kind, m) => {
                return Err(Fail::new(crate::status::status_of(*kind), m.clone()));
            }
            FolderState::Cancelled => {
                return Err(Fail::new(
                    crate::status::DS_E_CANCELLED,
                    "the folder compare was cancelled",
                ));
            }
        }
        self.compare
            .lock()
            .clone()
            .ok_or_else(|| Fail::busy("the folder compare is not ready"))
    }

    pub fn bump(&self) {
        self.revision.fetch_add(1, Ordering::SeqCst);
    }
}

/// A backend for a local root given as an OS path.
pub fn local_backend(path: &std::path::Path) -> Res<Arc<dyn Vfs>> {
    #[cfg(windows)]
    {
        let s = path
            .to_str()
            .ok_or_else(|| Fail::invalid("the path is not valid Unicode"))?;
        let root = core_vfs::VfsRoot::parse_windows(s)?;
        Ok(Arc::new(core_vfs::LocalWin32::new(root)))
    }
    #[cfg(not(windows))]
    {
        Ok(Arc::new(core_vfs::LocalStd::new(path)))
    }
}

/// Starts a folder compare of two local folders. Events: `DS_EV_PROGRESS` (`a` done, `b`
/// total), `DS_EV_STATE` (`a` = 1 done, 2 failed, 3 cancelled), `DS_EV_NODE` after changes.
///
/// # Safety
/// All pointers must be valid for the lengths given; `out_handle` for a write.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ds_folder_open(
    left_root: DsStr16,
    right_root: DsStr16,
    options_json: DsStr8,
    cb: DsEventFn,
    user: *mut c_void,
    out_handle: *mut DsHandle,
) -> DsStatus {
    ffi_entry!({
        // SAFETY: the ABI contract covers every pointer argument.
        let (l, r, json, outp) = unsafe {
            (
                units(left_root)?,
                units(right_root)?,
                json_arg(options_json)?,
                out(out_handle)?,
            )
        };
        let rules = options::folder_rules(json)?;
        let (lp, rp) = (os_path(l), os_path(r));
        let (lv, rv) = (local_backend(&lp)?, local_backend(&rp)?);
        let bridge = Bridge::new(cb, user);
        let obj = Arc::new(FolderObj {
            bridge: bridge.clone(),
            cancel: CancelToken::new(),
            closed: CancelToken::new(),
            state: Mutex::new(FolderState::Scanning),
            compare: Mutex::new(None),
            stage: AtomicU32::new(Stage::Scanning as u32),
            done: AtomicU64::new(0),
            total: AtomicU64::new(0),
            revision: Arc::new(AtomicU64::new(1)),
        });
        let worker = obj.clone();
        std::thread::Builder::new()
            .name("ds-folder".into())
            .spawn(move || run_folder(&worker, lv, rv, rules))
            .map_err(|e| {
                Fail::new(
                    crate::status::DS_E_INTERNAL,
                    format!("cannot start the worker: {e}"),
                )
            })?;
        let h = registry::insert(Object::Folder(obj));
        bridge.arm(h);
        *outp = h;
        Ok(())
    })
}

fn run_folder(
    obj: &Arc<FolderObj>,
    lv: Arc<dyn Vfs>,
    rv: Arc<dyn Vfs>,
    rules: core_folder::FolderRules,
) {
    let progress = |p: core_base::Progress| {
        obj.stage.store(p.stage as u32, Ordering::Relaxed);
        obj.done.store(p.done, Ordering::Relaxed);
        obj.total.store(p.total, Ordering::Relaxed);
        obj.bridge.emit(DS_EV_PROGRESS, p.done, p.total, false);
    };
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        FolderCompare::compare_with_progress(
            lv,
            &core_vfs::VPath::root(),
            rv,
            &core_vfs::VPath::root(),
            rules,
            &obj.cancel,
            &progress,
        )
    }));
    let (state, code) = match res {
        Ok(Ok(c)) => {
            *obj.compare.lock() = Some(Arc::new(RwLock::new(c)));
            obj.bump();
            (FolderState::Done, 1)
        }
        Ok(Err(e)) if e.is_cancelled() => (FolderState::Cancelled, 3),
        Ok(Err(e)) => (FolderState::Failed(e.kind(), e.to_string()), 2),
        Err(_) => (
            FolderState::Failed(
                core_base::ErrorKind::Internal,
                "internal error: the scan panicked".into(),
            ),
            2,
        ),
    };
    *obj.state.lock() = state;
    obj.bridge.emit(DS_EV_STATE, code, 0, true);
}

/// Progress, state and counters of a folder compare.
///
/// # Safety
/// `out_status` must be valid for a write.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ds_folder_status(
    h: DsHandle,
    out_status: *mut DsFolderStatus,
) -> DsStatus {
    ffi_entry!({
        let f = registry::folder(h)?;
        // SAFETY: the ABI contract covers the out pointer.
        let o = unsafe { out(out_status) }?;
        let mut s = DsFolderStatus {
            stage: f.stage.load(Ordering::Relaxed),
            progress_done: f.done.load(Ordering::Relaxed),
            progress_total: f.total.load(Ordering::Relaxed),
            revision: f.revision.load(Ordering::SeqCst),
            ..DsFolderStatus::default()
        };
        match &*f.state.lock() {
            FolderState::Scanning => s.state = 0,
            FolderState::Done => s.state = 1,
            FolderState::Failed(_, m) => {
                s.state = 2;
                crate::status::set_last_error(m);
            }
            FolderState::Cancelled => s.state = 3,
        }
        if s.state == 1 {
            if let Some(c) = f.compare.lock().clone() {
                let g = c.read();
                let n = g.tree.node(core_folder::Tree::ROOT).counters;
                s.node_count = g.tree.len() as u32;
                s.warning_count = g.warnings.len() as u32;
                s.sync_blocked = u8::from(g.sync_blocked);
                (s.diff, s.left_newer, s.right_newer) = (n.diff, n.left_newer, n.right_newer);
                (s.left_orphan, s.right_orphan, s.error, s.items) =
                    (n.left_orphan, n.right_orphan, n.error, n.items);
            }
        }
        *o = s;
        Ok(())
    })
}

fn side_info(s: &core_folder::SideInfo) -> DsSideInfo {
    DsSideInfo {
        size: s.size,
        mtime: s.mtime,
        ctime: 0,
        attrs: s.attrs,
        present: u8::from(s.present),
        kind: match s.kind {
            core_vfs::Kind::File => 0,
            core_vfs::Kind::Dir => 1,
            core_vfs::Kind::Link => 2,
        },
        reserved: 0,
    }
}

/// Fills a `DsDirRow` for a node. Counters of a folder are its own subtree's.
pub fn dir_row(g: &FolderCompare, id: NodeId) -> DsDirRow {
    let n = g.tree.node(id);
    let c = n.counters;
    DsDirRow {
        node: id,
        parent: n.parent,
        depth: n.depth,
        status: n.status as u8,
        flags: n.flags as u8,
        child_count: n.child_count,
        left: side_info(&n.left),
        right: side_info(&n.right),
        diff_count: c.diff + c.left_newer + c.right_newer + c.error,
        orphan_count: c.left_orphan + c.right_orphan,
    }
}

fn check_node(g: &FolderCompare, node: u32) -> Res {
    if (node as usize) < g.tree.len() {
        Ok(())
    } else {
        Err(Fail::new(DS_E_NOT_FOUND, "no such node"))
    }
}

/// The info of one node.
///
/// # Safety
/// `out_info` must be valid for a write.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ds_folder_node_info(
    h: DsHandle,
    node: u32,
    out_info: *mut DsDirRow,
) -> DsStatus {
    ffi_entry!({
        let c = registry::folder(h)?.ready()?;
        // SAFETY: the ABI contract covers the out pointer.
        let o = unsafe { out(out_info) }?;
        let g = c.read();
        check_node(&g, node)?;
        *o = dir_row(&g, node);
        Ok(())
    })
}

/// The name of a node as UTF-16: `side` 0 left, 1 right, 2 display (left, else right).
///
/// # Safety
/// `buf` must be valid for `cap` units (or null with `cap` 0); `len` for a write.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ds_folder_node_name(
    h: DsHandle,
    node: u32,
    side: u8,
    buf: *mut u16,
    cap: u32,
    len: *mut u32,
) -> DsStatus {
    ffi_entry!({
        let c = registry::folder(h)?.ready()?;
        let g = c.read();
        check_node(&g, node)?;
        let name = match side {
            0 => g.tree.name(node, true),
            1 => g.tree.name(node, false),
            2 => Some(g.tree.display_name(node)),
            _ => return Err(Fail::invalid("side must be 0, 1 or 2")),
        }
        .ok_or_else(|| Fail::new(DS_E_NOT_FOUND, "the node has no name on that side"))?;
        // SAFETY: the ABI contract covers `buf` and `len`.
        finish(unsafe { write_buffer(name.units(), buf, cap as usize, len) })
    })
}

/// A JSON report: roots' warnings and per-node errors.
///
/// # Safety
/// `out_buf` must be valid for a write; free the result with `ds_buf_free`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ds_folder_report(h: DsHandle, out_buf: *mut DsBuf) -> DsStatus {
    ffi_entry!({
        let c = registry::folder(h)?.ready()?;
        // SAFETY: the ABI contract covers the out pointer.
        let o = unsafe { out(out_buf) }?;
        let g = c.read();
        let errors: Vec<serde_json::Value> = g
            .errors
            .iter()
            .map(|(n, m)| serde_json::json!({"node": n, "path": g.tree.path(*n).display(), "message": m}))
            .collect();
        let v = serde_json::json!({"warnings": g.warnings, "syncBlocked": g.sync_blocked, "errors": errors});
        *o = crate::util::to_buf(v.to_string().into_bytes());
        Ok(())
    })
}

/// Spawns background work on a folder: `body` gets the shared compare; the tree revision is
/// bumped and a `DS_EV_NODE` event goes out when it is done.
/// Refuses node numbers that are not in the tree (nodes are never removed, so a number that is
/// valid now stays valid): the work runs on a worker thread, where an out-of-range index would
/// only be a swallowed panic and a batch that silently did nothing (found by the random-call
/// campaign, E11-S6).
pub fn check_nodes(f: &Arc<FolderObj>, ids: &[NodeId]) -> Res {
    let c = f.ready()?;
    let len = c.read().tree.len();
    match ids.iter().find(|&&i| i as usize >= len) {
        Some(bad) => Err(Fail::invalid(format!("node {bad} does not exist"))),
        None => Ok(()),
    }
}

fn spawn_work(
    f: &Arc<FolderObj>,
    name: &str,
    body: impl FnOnce(&Arc<FolderObj>, &Shared) -> u64 + Send + 'static,
) -> Res {
    let c = f.ready()?;
    let obj = f.clone();
    std::thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| body(&obj, &c)));
            obj.bump();
            let n = r.unwrap_or(0);
            obj.bridge
                .emit(DS_EV_NODE, n, obj.revision.load(Ordering::SeqCst), true);
        })
        .map_err(|e| {
            Fail::new(
                crate::status::DS_E_INTERNAL,
                format!("cannot start the worker: {e}"),
            )
        })?;
    Ok(())
}

/// Runs content compare for these nodes in the background (the folder rules may have it off).
///
/// # Safety
/// `nodes` must be valid for `n` entries.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ds_folder_compare_content(
    h: DsHandle,
    nodes: *const u32,
    n: u32,
) -> DsStatus {
    ffi_entry!({
        let f = registry::folder(h)?;
        // SAFETY: the ABI contract covers the array.
        let ids: Vec<NodeId> = unsafe { slice(nodes, n as usize) }?.to_vec();
        check_nodes(&f, &ids)?;
        spawn_work(&f, "ds-content", move |obj, c| {
            // queue under a short write lock, compare without any lock, apply under another
            let batch = {
                let mut g = c.write();
                g.queue_content(&ids);
                g.take_content_batch()
            };
            let Some(batch) = batch else { return 0 };
            let count = batch.nodes().len() as u64;
            match batch.run(&obj.closed, &|_| {}) {
                Ok(results) => c.write().apply_content_results(results),
                Err(_) => c.write().requeue(batch),
            }
            count
        })
    })
}

/// Re-stats these nodes in the background and reclassifies them.
///
/// # Safety
/// `nodes` must be valid for `n` entries.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ds_folder_refresh(h: DsHandle, nodes: *const u32, n: u32) -> DsStatus {
    ffi_entry!({
        let f = registry::folder(h)?;
        // SAFETY: the ABI contract covers the array.
        let ids: Vec<NodeId> = unsafe { slice(nodes, n as usize) }?.to_vec();
        check_nodes(&f, &ids)?;
        spawn_work(&f, "ds-refresh", move |obj, c| {
            // stat without any lock (a network share can be slow), apply under a short write lock
            let probe = c.read().probe_batch(&ids);
            match probe.run(&obj.closed) {
                Ok(probes) => c.write().apply_probes(probes),
                Err(_) => return 0,
            }
            let batch = c.write().take_content_batch();
            if let Some(batch) = batch {
                match batch.run(&obj.closed, &|_| {}) {
                    Ok(results) => c.write().apply_content_results(results),
                    Err(_) => c.write().requeue(batch),
                }
            }
            ids.len() as u64
        })
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn ds_folder_cancel(h: DsHandle) -> DsStatus {
    ffi_entry!({
        registry::folder(h)?.cancel.cancel();
        Ok(())
    })
}

// ---------------------------------------------------------------- views

pub struct ViewObj {
    pub folder: Arc<FolderObj>,
    view: Mutex<ViewState>,
}

struct ViewState {
    view: FolderView,
    seen: u64,
}

impl std::fmt::Debug for ViewObj {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ViewObj")
    }
}

impl ViewObj {
    /// Rebuilds the view when the tree changed since it last looked.
    fn current<R>(&self, body: impl FnOnce(&mut FolderView, &FolderCompare) -> R) -> Res<R> {
        let c = self.folder.ready()?;
        let g = c.read();
        let mut st = self.view.lock();
        let rev = self.folder.revision.load(Ordering::SeqCst);
        if st.seen != rev {
            st.view.rebuild(&g.tree);
            st.seen = rev;
        }
        Ok(body(&mut st.view, &g))
    }
}

fn apply_view_json(view: &mut FolderView, tree: &core_folder::Tree, json: &str) -> Res {
    let v = options::view_json(json)?;
    // every field is resolved before the first one is applied, so a bad value leaves the view as
    // it was (reviewer, E11-S6: a failed call used to be partly applied)
    let sort = v.sort.as_ref().map(options::sort_of).transpose()?;
    let filter = v.filter.as_deref().map(options::filter_of).transpose()?;
    if let Some(s) = sort {
        view.set_sort(tree, s);
    }
    if let Some(q) = &v.search {
        view.set_search(tree, q);
    }
    if let Some(f) = filter {
        view.set_filter(tree, f);
    }
    if let Some(on) = v.ignore_unimportant {
        view.set_ignore_unimportant(tree, on);
    }
    if let Some(on) = v.show_ignored {
        view.set_show_ignored(tree, on);
    }
    Ok(())
}

/// Marks items as ignored (hidden from views that do not show ignored items, left alone by a sync
/// plan, with everything below them), or takes the mark off. Views pick the change up on their
/// next call. The root cannot be ignored.
///
/// # Safety
/// `nodes` must be valid for `n` entries.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ds_folder_set_ignored(
    h: DsHandle,
    nodes: *const u32,
    n: u32,
    ignored: u8,
) -> DsStatus {
    ffi_entry!({
        let f = registry::folder(h)?;
        // SAFETY: the ABI contract covers the array.
        let ids: Vec<NodeId> = unsafe { slice(nodes, n as usize) }?.to_vec();
        check_nodes(&f, &ids)?;
        if ids.contains(&0) {
            return Err(Fail::invalid("the root cannot be ignored"));
        }
        let c = f.ready()?;
        {
            let mut g = c.write();
            for id in &ids {
                g.set_ignored(*id, ignored != 0)?;
            }
        }
        f.revision.fetch_add(1, Ordering::SeqCst);
        Ok(())
    })
}

/// Creates a flat view over a finished folder compare. The compare must be done (`DS_E_BUSY`
/// otherwise).
///
/// # Safety
/// `view_json` must be valid for its length; `out_handle` for a write.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ds_view_create(
    folder: DsHandle,
    view_json: DsStr8,
    out_handle: *mut DsHandle,
) -> DsStatus {
    ffi_entry!({
        let f = registry::folder(folder)?;
        // SAFETY: the ABI contract covers both pointers.
        let (json, outp) = unsafe { (json_arg(view_json)?, out(out_handle)?) };
        let c = f.ready()?;
        let mut view = FolderView::new();
        let seen;
        {
            let g = c.read();
            // the revision is read under the read lock, before the view is built: a change that
            // lands later makes the next call rebuild
            seen = f.revision.load(Ordering::SeqCst);
            view.rebuild(&g.tree);
            apply_view_json(&mut view, &g.tree, json)?;
        }
        *outp = registry::insert(Object::View(Arc::new(ViewObj {
            folder: f,
            view: Mutex::new(ViewState { view, seen }),
        })));
        Ok(())
    })
}

/// Changes a view's filter, search or sort (fields not given stay).
///
/// # Safety
/// `view_json` must be valid for its length.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ds_view_set(view: DsHandle, view_json: DsStr8) -> DsStatus {
    ffi_entry!({
        let v = registry::view(view)?;
        // SAFETY: the ABI contract covers the JSON view.
        let json = unsafe { json_arg(view_json) }?;
        v.current(|vw, g| apply_view_json(vw, &g.tree, json))??;
        Ok(())
    })
}

/// Expands or collapses a folder node, optionally with everything below it.
#[unsafe(no_mangle)]
pub extern "C" fn ds_view_expand(
    view: DsHandle,
    node: u32,
    expanded: u8,
    recursive: u8,
) -> DsStatus {
    ffi_entry!({
        let v = registry::view(view)?;
        v.current(|vw, g| {
            check_node(g, node)?;
            vw.set_node_expanded(&g.tree, node, expanded != 0, recursive != 0);
            Ok::<(), Fail>(())
        })??;
        Ok(())
    })
}

/// The number of visible rows and the view's generation (it changes whenever rows may have).
///
/// # Safety
/// `count` and `generation` must be valid for writes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ds_view_row_count(
    view: DsHandle,
    count: *mut u32,
    generation: *mut u64,
) -> DsStatus {
    ffi_entry!({
        let v = registry::view(view)?;
        // SAFETY: the ABI contract covers both pointers.
        let (c, g) = unsafe { (out(count)?, out(generation)?) };
        v.current(|vw, _| {
            *c = vw.len() as u32;
            *g = vw.generation();
        })?;
        Ok(())
    })
}

/// Copies visible rows `first..first + count` into `out_rows`.
///
/// # Safety
/// `out_rows` must be valid for `count` rows, `written` for a write.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ds_view_get_rows(
    view: DsHandle,
    first: u32,
    count: u32,
    out_rows: *mut DsDirRow,
    written: *mut u32,
) -> DsStatus {
    ffi_entry!({
        let v = registry::view(view)?;
        // SAFETY: the ABI contract covers the buffers.
        let (w, dst) = unsafe { (out(written)?, rows_mut(out_rows, count as usize)?) };
        v.current(|vw, g| {
            let ids = vw.get_rows(first as usize, count as usize);
            for (d, &id) in dst.iter_mut().zip(ids) {
                *d = dir_row(g, id);
            }
            *w = ids.len() as u32;
        })?;
        Ok(())
    })
}

/// # Safety
/// `ptr` must be valid for `len` writes.
unsafe fn rows_mut<'a>(ptr: *mut DsDirRow, len: usize) -> Res<&'a mut [DsDirRow]> {
    if len == 0 {
        return Ok(&mut []);
    }
    if ptr.is_null() {
        return Err(Fail::invalid("a null pointer with a non-zero length"));
    }
    // SAFETY: non-null; validity for `len` writes is the caller's contract.
    Ok(unsafe { std::slice::from_raw_parts_mut(ptr, len) })
}
