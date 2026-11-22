//! The handle registry (plan.md §10.1; backlog E9-S1.1): a `slotmap` with generation counters, so
//! a stale or double-closed handle is `DS_E_INVALID_HANDLE` and never a crash.

use std::sync::{Arc, LazyLock};

use parking_lot::Mutex;
use slotmap::{DefaultKey, Key, KeyData, SlotMap};

use crate::folder::{FolderObj, ViewObj};
use crate::status::{Fail, Res};
use crate::sync::{JobObj, PlanObj};
use crate::text::TextObj;
use crate::types::DsHandle;

#[derive(Clone, Debug)]
pub enum Object {
    Text(Arc<TextObj>),
    Folder(Arc<FolderObj>),
    View(Arc<ViewObj>),
    Plan(Arc<PlanObj>),
    Job(Arc<JobObj>),
}

impl Object {
    /// Cancels the work behind a handle and detaches its event bridge.
    fn shutdown(&self) {
        match self {
            Object::Text(t) => t.shutdown(),
            Object::Folder(f) => f.shutdown(),
            Object::Job(j) => j.shutdown(),
            Object::View(_) | Object::Plan(_) => {}
        }
    }
}

static REGISTRY: LazyLock<Mutex<SlotMap<DefaultKey, Object>>> =
    LazyLock::new(|| Mutex::new(SlotMap::new()));

pub fn insert(o: Object) -> DsHandle {
    REGISTRY.lock().insert(o).data().as_ffi()
}

fn lookup(h: DsHandle) -> Res<Object> {
    if h == 0 {
        return Err(Fail::handle());
    }
    REGISTRY
        .lock()
        .get(KeyData::from_ffi(h).into())
        .cloned()
        .ok_or_else(Fail::handle)
}

/// Removes a handle; any other call with it now fails with `DS_E_INVALID_HANDLE`. Work in
/// progress is stopped when the last reference goes away.
pub fn close(h: DsHandle) -> Res {
    if h == 0 {
        return Err(Fail::handle());
    }
    let removed = REGISTRY.lock().remove(KeyData::from_ffi(h).into());
    let Some(obj) = removed else {
        return Err(Fail::handle());
    };
    // outside the registry lock: stopping may wait for a running callback or worker. After this
    // returns no callback of the handle runs or starts (docs/abi.md, decision D51).
    obj.shutdown();
    drop(obj);
    Ok(())
}

pub fn text(h: DsHandle) -> Res<Arc<TextObj>> {
    match lookup(h)? {
        Object::Text(t) => Ok(t),
        _ => Err(Fail::handle()),
    }
}

pub fn folder(h: DsHandle) -> Res<Arc<FolderObj>> {
    match lookup(h)? {
        Object::Folder(t) => Ok(t),
        _ => Err(Fail::handle()),
    }
}

pub fn view(h: DsHandle) -> Res<Arc<ViewObj>> {
    match lookup(h)? {
        Object::View(t) => Ok(t),
        _ => Err(Fail::handle()),
    }
}

pub fn plan(h: DsHandle) -> Res<Arc<PlanObj>> {
    match lookup(h)? {
        Object::Plan(t) => Ok(t),
        _ => Err(Fail::handle()),
    }
}

pub fn job(h: DsHandle) -> Res<Arc<JobObj>> {
    match lookup(h)? {
        Object::Job(t) => Ok(t),
        _ => Err(Fail::handle()),
    }
}

/// Number of open handles (for tests and leak checks).
pub fn open_count() -> usize {
    REGISTRY.lock().len()
}
