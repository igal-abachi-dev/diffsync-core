//! Sync plan, job and file-operation exports (plan.md §10.2; backlog E9-S6).
//!
//! The question flow works from any thread: `ds_job_answer` only sets the answer and wakes the
//! worker, so the host may answer from the UI thread while the event callback that announced
//! the question has long returned.
#![allow(unsafe_code)]

use std::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use core_folder::NodeId;
use core_sync::{
    Answer, Job, JobEvent, JobSink, Loc, Op, Plan, QuestionKind, SyncRules, build_plan, copy_items,
    delete_items, move_items, start_sync_with,
};
use core_vfs::{VPath, Vfs};
use parking_lot::Mutex;

use crate::events::Bridge;
use crate::options;
use crate::registry::{self, Object};
use crate::status::{DS_E_NOT_FOUND, DsStatus, Fail, Res, ffi_entry};
use crate::types::*;
use crate::util::{finish, json_arg, out, slice, to_buf, unit_list, units, write_buffer};

pub struct PlanObj {
    folder: Arc<crate::folder::FolderObj>,
    plan: Mutex<Plan>,
    /// Set by `ds_plan_execute`: the plan is immutable from then on (plan.md §8.2).
    executed: std::sync::atomic::AtomicBool,
}

impl std::fmt::Debug for PlanObj {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PlanObj")
    }
}

pub struct JobObj {
    job: Job,
    bridge: Arc<Bridge>,
}

impl JobObj {
    /// `ds_close`: cancel the job and detach its events.
    pub fn shutdown(&self) {
        self.job.cancel();
        self.bridge.detach();
    }
}

impl Drop for JobObj {
    fn drop(&mut self) {
        // closing the handle of a running job cancels it; nothing half-written is left
        self.job.cancel();
    }
}

impl std::fmt::Debug for JobObj {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("JobObj")
    }
}

/// Builds the plan of a finished folder compare. `nodes` limits it to a selection (n = 0 means
/// everything). Refused with `DS_E_INVALID_ARG` when the roots overlap (plan.md §9.7).
///
/// # Safety
/// `rules_json` and `nodes` must be valid for their lengths; `out_plan` for a write.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ds_sync_plan(
    folder: DsHandle,
    rules_json: DsStr8,
    nodes: *const u32,
    n: u32,
    out_plan: *mut DsHandle,
) -> DsStatus {
    ffi_entry!({
        let f = registry::folder(folder)?;
        // SAFETY: the ABI contract covers every pointer argument.
        let (json, sel, outp) = unsafe {
            (
                json_arg(rules_json)?,
                slice(nodes, n as usize)?,
                out(out_plan)?,
            )
        };
        let rules: SyncRules = options::sync_rules(json)?;
        crate::folder::check_nodes(&f, sel)?;
        let c = f.ready()?;
        let plan = {
            let g = c.read();
            build_plan(&g, &rules, (n > 0).then_some(sel))?
        };
        *outp = registry::insert(Object::Plan(Arc::new(PlanObj {
            folder: f,
            plan: Mutex::new(plan),
            executed: std::sync::atomic::AtomicBool::new(false),
        })));
        Ok(())
    })
}

/// The counts and bytes of a plan.
///
/// # Safety
/// `out_summary` must be valid for a write.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ds_plan_summary(
    plan: DsHandle,
    out_summary: *mut DsPlanSummary,
) -> DsStatus {
    ffi_entry!({
        let p = registry::plan(plan)?;
        // SAFETY: the ABI contract covers the out pointer.
        let o = unsafe { out(out_summary) }?;
        let g = p.plan.lock();
        let s = g.summary;
        *o = DsPlanSummary {
            copies_to_left: s.copies_to_left,
            copies_to_right: s.copies_to_right,
            folders_created: s.folders_created,
            deletes_left: s.deletes_left,
            deletes_right: s.deletes_right,
            conflicts: s.conflicts,
            errors: s.errors,
            skipped_links: s.skipped_links,
            bytes_to_left: s.bytes_to_left,
            bytes_to_right: s.bytes_to_right,
            action_count: g.actions.len() as u32,
            filters_active: u8::from(g.filters_active),
            pad: [0; 3],
        };
        Ok(())
    })
}

/// Copies planned actions `first..first + count`.
///
/// # Safety
/// `out_actions` must be valid for `count` entries, `written` for a write.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ds_plan_get_actions(
    plan: DsHandle,
    first: u32,
    count: u32,
    out_actions: *mut DsPlanAction,
    written: *mut u32,
) -> DsStatus {
    ffi_entry!({
        let p = registry::plan(plan)?;
        // SAFETY: the ABI contract covers the buffers.
        let (w, dst) = unsafe {
            (
                out(written)?,
                crate::sync::actions_mut(out_actions, count as usize)?,
            )
        };
        let g = p.plan.lock();
        let rows = g.actions.iter().skip(first as usize).take(count as usize);
        let mut n = 0;
        for (d, a) in dst.iter_mut().zip(rows) {
            *d = DsPlanAction {
                kind: a.kind as u8,
                is_folder: u8::from(a.is_folder),
                flags: u16::from(a.only_if_empty) | (u16::from(a.frees_name) << 1),
                node: a.node,
                bytes: a.bytes,
            };
            n += 1;
        }
        *w = n;
        Ok(())
    })
}

/// # Safety
/// `ptr` must be valid for `len` writes.
pub(crate) unsafe fn actions_mut<'a>(
    ptr: *mut DsPlanAction,
    len: usize,
) -> Res<&'a mut [DsPlanAction]> {
    if len == 0 {
        return Ok(&mut []);
    }
    if ptr.is_null() {
        return Err(Fail::invalid("a null pointer with a non-zero length"));
    }
    // SAFETY: non-null; validity for `len` writes is the caller's contract.
    Ok(unsafe { std::slice::from_raw_parts_mut(ptr, len) })
}

/// The nodes whose choice is still open (prompts), `first..first + count`.
///
/// # Safety
/// `out_nodes` must be valid for `count` entries, `written` for a write.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ds_plan_get_prompts(
    plan: DsHandle,
    first: u32,
    count: u32,
    out_nodes: *mut u32,
    written: *mut u32,
) -> DsStatus {
    ffi_entry!({
        let p = registry::plan(plan)?;
        // SAFETY: the ABI contract covers the written count.
        let w = unsafe { out(written) }?;
        let g = p.plan.lock();
        let all = &g.unresolved_prompts;
        let from = (first as usize).min(all.len());
        let part = &all[from..(from + count as usize).min(all.len())];
        if !part.is_empty() && out_nodes.is_null() {
            return Err(Fail::invalid("a null pointer with a non-zero length"));
        }
        for (i, &n) in part.iter().enumerate() {
            // SAFETY: `out_nodes` is valid for `count >= part.len()` writes.
            unsafe { *out_nodes.add(i) = n };
        }
        *w = part.len() as u32;
        Ok(())
    })
}

/// The plan as UTF-8 JSON: issues (errors and warnings), skipped links, prompts and the text
/// lines of the dry run. Free with `ds_buf_free`.
///
/// # Safety
/// `out_buf` must be valid for a write.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ds_plan_report(plan: DsHandle, out_buf: *mut DsBuf) -> DsStatus {
    ffi_entry!({
        let p = registry::plan(plan)?;
        // SAFETY: the ABI contract covers the out pointer.
        let o = unsafe { out(out_buf) }?;
        let c = p.folder.ready()?;
        let g = c.read();
        let pl = p.plan.lock();
        let issues: Vec<serde_json::Value> = pl
            .issues
            .iter()
            .map(|i| {
                serde_json::json!({
                    "node": i.node,
                    "path": g.tree.path(i.node).display(),
                    "severity": if i.severity == core_sync::Severity::Error { "error" } else { "warning" },
                    "message": i.message,
                })
            })
            .collect();
        let v = serde_json::json!({
            "issues": issues,
            "skippedLinks": pl.skipped_links,
            "prompts": pl.unresolved_prompts,
            "lines": pl.describe(&g),
        });
        *o = to_buf(v.to_string().into_bytes());
        Ok(())
    })
}

/// Finds the `*.dctmp~` temp files that a crash or a host that exited mid-copy left under `root`
/// (plan §9.2) and, with `delete != 0`, removes them. Links are never entered. The call is
/// synchronous and walks the whole tree: call it off the UI thread. The report is UTF-8 JSON:
/// `{"found":[{"path":..,"bytes":..}],"deleted":n,"errors":[{"path":..,"message":..}]}`.
///
/// # Safety
/// `root` must be valid for its length; `out_report` for a write (free it with `ds_buf_free`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ds_maintenance_sweep(
    root: DsStr16,
    delete: u8,
    out_report: *mut DsBuf,
) -> DsStatus {
    ffi_entry!({
        // SAFETY: the ABI contract covers every pointer argument.
        let (units, o) = unsafe { (units(root)?, out(out_report)?) };
        let path = crate::util::os_path(units);
        if !path.is_absolute() {
            return Err(Fail::invalid("root must be an absolute path"));
        }
        let vfs = crate::folder::local_backend(&path)?;
        let report = core_sync::maintenance_sweep(
            &*vfs,
            &VPath::root(),
            delete != 0,
            &core_base::CancelToken::new(),
        )?;
        let v = serde_json::json!({
            "found": report.found.iter().map(|(p, n)| serde_json::json!({"path": p.display(), "bytes": n})).collect::<Vec<_>>(),
            "deleted": report.deleted,
            "errors": report.errors.iter().map(|(p, m)| serde_json::json!({"path": p.display(), "message": m})).collect::<Vec<_>>(),
        });
        *o = to_buf(v.to_string().into_bytes());
        Ok(())
    })
}

/// Chooses what to do with a node: 0 leave, 1 prompt, 2 copy to left, 3 copy to right,
/// 4 delete left, 5 delete right. The plan is rebuilt; only valid before execution.
#[unsafe(no_mangle)]
pub extern "C" fn ds_plan_set_op(plan: DsHandle, node: u32, op: u8) -> DsStatus {
    ffi_entry!({
        let p = registry::plan(plan)?;
        let op = match op {
            0 => Op::Leave,
            1 => Op::Prompt,
            2 => Op::CopyToLeft,
            3 => Op::CopyToRight,
            4 => Op::DeleteLeft,
            5 => Op::DeleteRight,
            _ => return Err(Fail::invalid("unknown operation")),
        };
        let c = p.folder.ready()?;
        // lock order everywhere: compare, then plan
        let g = c.read();
        if (node as usize) >= g.tree.len() {
            return Err(Fail::new(DS_E_NOT_FOUND, "no such node"));
        }
        let mut pl = p.plan.lock();
        // checked under the plan lock: `ds_plan_execute` claims the plan under the same lock
        if p.executed.load(Ordering::SeqCst) {
            return Err(Fail::busy(
                "the plan is already executing; it can no longer be changed",
            ));
        }
        pl.set_op(&g, node as NodeId, op)?;
        Ok(())
    })
}

fn job_sink(bridge: &Arc<Bridge>) -> JobSink {
    let b = bridge.clone();
    Arc::new(move |e: &JobEvent| match e {
        JobEvent::Progress(p) => b.emit(
            DS_EV_PROGRESS,
            u64::from(p.steps_done),
            u64::from(p.steps_total),
            false,
        ),
        JobEvent::Question(q) => {
            let kind = u64::from(q.kind == QuestionKind::Failed);
            b.emit(DS_EV_QUESTION, q.id, kind, true);
        }
        JobEvent::Finished => b.emit(DS_EV_STATE, 2, 0, true),
    })
}

fn new_job(job: Job, bridge: &Arc<Bridge>) -> DsHandle {
    let h = registry::insert(Object::Job(Arc::new(JobObj {
        job,
        bridge: bridge.clone(),
    })));
    bridge.arm(h);
    h
}

/// Executes a plan as a job. Events: `DS_EV_PROGRESS` (steps done / total), `DS_EV_QUESTION`
/// (`a` = question id, `b` = 0 changed since compare / 1 failed), `DS_EV_STATE` (`a` = 2 when
/// finished). The plan stays readable but is not changed by the job.
///
/// # Safety
/// `exec_json` must be valid for its length; `out_job` for a write.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ds_plan_execute(
    plan: DsHandle,
    exec_json: DsStr8,
    cb: DsEventFn,
    user: *mut c_void,
    out_job: *mut DsHandle,
) -> DsStatus {
    ffi_entry!({
        let p = registry::plan(plan)?;
        // SAFETY: the ABI contract covers both pointers.
        let (json, outp) = unsafe { (json_arg(exec_json)?, out(out_job)?) };
        let opts = options::exec_overrides(json)?;
        let c = p.folder.ready()?;
        let bridge = Bridge::new(cb, user);
        let (folder, b) = (p.folder.clone(), bridge.clone());
        let refreshed: core_sync::Refreshed = Arc::new(move |n| {
            folder.bump();
            b.emit(
                DS_EV_NODE,
                u64::from(n),
                folder.revision.load(Ordering::SeqCst),
                false,
            );
        });
        // copy the plan out and release its lock first: the compare lock is always taken before
        // the plan lock, never after it
        // the claim and the copy happen under the plan lock, so `ds_plan_set_op` cannot change
        // the plan between them, and a second execute of the same plan is refused (reviewer, E11-S6)
        let snapshot = {
            let pl = p.plan.lock();
            if p.executed.swap(true, Ordering::SeqCst) {
                return Err(Fail::busy("the plan was already executed"));
            }
            pl.clone()
        };
        let job = match start_sync_with(
            &snapshot,
            &c,
            opts,
            Some(job_sink(&bridge)),
            Some(refreshed),
        ) {
            Ok(j) => j,
            Err(e) => {
                p.executed.store(false, Ordering::SeqCst); // nothing started: the plan stays usable
                return Err(e.into());
            }
        };
        *outp = new_job(job, &bridge);
        Ok(())
    })
}

// ---------------------------------------------------------------- file operations

fn backend_for(root: &str) -> Res<Arc<dyn Vfs>> {
    #[cfg(windows)]
    {
        Ok(Arc::new(core_vfs::LocalWin32::new(
            core_vfs::VfsRoot::parse_windows(root)?,
        )))
    }
    #[cfg(not(windows))]
    {
        Ok(Arc::new(core_vfs::LocalStd::new(std::path::Path::new(
            root,
        ))))
    }
}

/// A location for an absolute OS path: a backend rooted at the volume (or share), and the path
/// below it, so items on one volume share a file system and a move can be a rename.
pub fn loc_for(path: &[u16]) -> Res<Loc> {
    // decision: §9.1/AGENTS: never normalise a name; an unpaired surrogate cannot be written as
    // UTF-8 without becoming a different name, so the path is refused instead
    let text = String::from_utf16(path)
        .map_err(|_| Fail::invalid("the path contains an unpaired surrogate"))?;
    #[cfg(windows)]
    let parsed = core_vfs::VfsRoot::parse_windows(&text)?;
    #[cfg(not(windows))]
    let parsed = core_vfs::VfsRoot::parse_posix(&text)?;
    let vfs = backend_for(parsed.root_string())?;
    Ok(Loc {
        vfs,
        path: VPath::from_parts(parsed.parts().iter().map(String::as_str)),
    })
}

fn locs(list: DsStr16List) -> Res<Vec<Loc>> {
    // SAFETY: the ABI contract of the calling export covers the list.
    unsafe { unit_list(list) }?
        .iter()
        .map(|u| loc_for(u))
        .collect()
}

/// Copies items into a folder as a job (same events and questions as sync).
///
/// # Safety
/// All pointers must be valid for the lengths given; `out_job` for a write.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ds_fileops_copy(
    items: DsStr16List,
    dst: DsStr16,
    opts_json: DsStr8,
    cb: DsEventFn,
    user: *mut c_void,
    out_job: *mut DsHandle,
) -> DsStatus {
    ffi_entry!({
        // SAFETY: the ABI contract covers every pointer argument.
        let (d, json, outp) = unsafe { (units(dst)?, json_arg(opts_json)?, out(out_job)?) };
        let (o, items) = (options::fileop_options(json)?, locs(items)?);
        let bridge = Bridge::new(cb, user);
        let job = copy_items(&items, &loc_for(d)?, o, Some(job_sink(&bridge)))?;
        *outp = new_job(job, &bridge);
        Ok(())
    })
}

/// Moves items into a folder as a job: a rename inside one volume, copy then delete otherwise.
///
/// # Safety
/// All pointers must be valid for the lengths given; `out_job` for a write.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ds_fileops_move(
    items: DsStr16List,
    dst: DsStr16,
    opts_json: DsStr8,
    cb: DsEventFn,
    user: *mut c_void,
    out_job: *mut DsHandle,
) -> DsStatus {
    ffi_entry!({
        // SAFETY: the ABI contract covers every pointer argument.
        let (d, json, outp) = unsafe { (units(dst)?, json_arg(opts_json)?, out(out_job)?) };
        let (o, items) = (options::fileop_options(json)?, locs(items)?);
        let bridge = Bridge::new(cb, user);
        let job = move_items(&items, &loc_for(d)?, o, Some(job_sink(&bridge)))?;
        *outp = new_job(job, &bridge);
        Ok(())
    })
}

/// Deletes items as a job (to the Recycle Bin with `"useRecycleBin": true`).
///
/// # Safety
/// All pointers must be valid for the lengths given; `out_job` for a write.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ds_fileops_delete(
    items: DsStr16List,
    opts_json: DsStr8,
    cb: DsEventFn,
    user: *mut c_void,
    out_job: *mut DsHandle,
) -> DsStatus {
    ffi_entry!({
        // SAFETY: the ABI contract covers every pointer argument.
        let (json, outp) = unsafe { (json_arg(opts_json)?, out(out_job)?) };
        let (o, items) = (options::fileop_options(json)?, locs(items)?);
        let bridge = Bridge::new(cb, user);
        let job = delete_items(&items, o, Some(job_sink(&bridge)))?;
        *outp = new_job(job, &bridge);
        Ok(())
    })
}

// ---------------------------------------------------------------- jobs

/// Progress and, when finished, the counts of a job.
///
/// # Safety
/// `out_status` must be valid for a write.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ds_job_status(job: DsHandle, out_status: *mut DsJobStatus) -> DsStatus {
    ffi_entry!({
        let j = registry::job(job)?;
        // SAFETY: the ABI contract covers the out pointer.
        let o = unsafe { out(out_status) }?;
        let p = j.job.progress();
        let finished = j.job.is_finished();
        let mut s = DsJobStatus {
            state: if finished { 2 } else { u32::from(p.paused) },
            steps_total: p.steps_total,
            steps_done: p.steps_done,
            has_question: u8::from(j.job.question().is_some()),
            bytes_total: p.bytes_total,
            bytes_done: p.bytes_done,
            ..DsJobStatus::default()
        };
        if finished {
            let r = j.job.wait();
            (s.done, s.skipped, s.failed, s.kept) = (r.done, r.skipped, r.failed, r.kept);
            s.bytes_copied = r.bytes_copied;
            (s.aborted, s.cancelled) = (u8::from(r.aborted), u8::from(r.cancelled));
        }
        *o = s;
        Ok(())
    })
}

/// The open question, if any (`out_q.id == 0` when none) with its message as UTF-16 (two-call
/// pattern).
///
/// # Safety
/// `out_q` and `len` must be valid for writes; `msg` for `cap` units (or null with `cap` 0).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ds_job_get_question(
    job: DsHandle,
    out_q: *mut DsQuestion,
    msg: *mut u16,
    cap: u32,
    len: *mut u32,
) -> DsStatus {
    ffi_entry!({
        let j = registry::job(job)?;
        // SAFETY: the ABI contract covers the out pointers.
        let o = unsafe { out(out_q) }?;
        match j.job.question() {
            None => {
                *o = DsQuestion::default();
                // SAFETY: the ABI contract covers `len`.
                unsafe { *out(len)? = 0 };
                Ok(())
            }
            Some(q) => {
                *o = DsQuestion {
                    id: q.id,
                    kind: u32::from(q.kind == QuestionKind::Failed),
                    step: q.step as u32,
                };
                let text: Vec<u16> = q.message.encode_utf16().collect();
                // SAFETY: the ABI contract covers `msg` and `len`.
                finish(unsafe { write_buffer(&text, msg, cap as usize, len) })
            }
        }
    })
}

/// Answers the open question: 0 retry, 1 skip, 2 skip all, 3 abort. `question_id` must be the
/// open question's id, so a late answer never lands on the next question.
#[unsafe(no_mangle)]
pub extern "C" fn ds_job_answer(job: DsHandle, question_id: u64, answer: u8) -> DsStatus {
    ffi_entry!({
        let j = registry::job(job)?;
        let a = match answer {
            0 => Answer::Retry,
            1 => Answer::Skip,
            2 => Answer::SkipAll,
            3 => Answer::Abort,
            _ => return Err(Fail::invalid("unknown answer")),
        };
        // the job checks the id under its own lock, so a late answer cannot land on the next one
        Ok(j.job.answer(question_id, a)?)
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn ds_job_pause(job: DsHandle) -> DsStatus {
    ffi_entry!({
        registry::job(job)?.job.pause();
        Ok(())
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn ds_job_resume(job: DsHandle) -> DsStatus {
    ffi_entry!({
        registry::job(job)?.job.resume();
        Ok(())
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn ds_job_cancel(job: DsHandle) -> DsStatus {
    ffi_entry!({
        registry::job(job)?.job.cancel();
        Ok(())
    })
}

/// The result of a finished job as UTF-8 JSON (`DS_E_BUSY` while it runs). Free with
/// `ds_buf_free`.
///
/// # Safety
/// `out_buf` must be valid for a write.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ds_job_result(job: DsHandle, out_buf: *mut DsBuf) -> DsStatus {
    ffi_entry!({
        let j = registry::job(job)?;
        // SAFETY: the ABI contract covers the out pointer.
        let o = unsafe { out(out_buf) }?;
        if !j.job.is_finished() {
            return Err(Fail::busy("the job is still running"));
        }
        let r = j.job.wait();
        let errors: Vec<serde_json::Value> = r
            .errors
            .iter()
            .map(|e| serde_json::json!({"path": e.path, "message": e.message}))
            .collect();
        let v = serde_json::json!({
            "done": r.done, "skipped": r.skipped, "failed": r.failed, "kept": r.kept,
            "bytesCopied": r.bytes_copied, "aborted": r.aborted, "cancelled": r.cancelled,
            "errors": errors,
        });
        let _ = &j.bridge;
        *o = to_buf(v.to_string().into_bytes());
        Ok(())
    })
}
