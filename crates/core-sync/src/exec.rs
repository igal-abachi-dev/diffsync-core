//! The executor (plan.md §8.3, §9.1-§9.4; backlog E8-S3, E8-S4, E8-S5): steps in the safe order,
//! one worker per job with pause and cancel, re-validation before every step, the question
//! handshake (Retry / Skip / SkipAll / Abort) and the execution log.

use std::collections::HashSet;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use core_base::{CancelToken, Error, ErrorKind, PauseGate, Result, Ticks};
use core_folder::{FolderCompare, NodeId};
use core_vfs::{CopyOptions, DeleteMode, Kind, VPath, Vfs, copy_between};
use parking_lot::{Condvar, Mutex, RwLock};

use crate::log::JobLog;
use crate::plan::{ActionKind, Fingerprint, Plan};

/// A place in a file system.
#[derive(Clone)]
pub struct Loc {
    pub vfs: Arc<dyn Vfs>,
    pub path: VPath,
}

impl std::fmt::Debug for Loc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Loc({})", self.path.display())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepKind {
    CreateDir,
    CopyFile,
    /// Rename inside one file system (move within a volume).
    MoveFile,
    Delete,
    /// Sets a created folder's modification time after its contents are written.
    SetFolderTime,
}

/// The phases of §8.3, in execution order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Phase {
    /// Deletes that free a name needed by a type-conflict copy (§9.3).
    FreeNames,
    CreateDirs,
    Copies,
    Deletes,
    FolderTimes,
}

#[derive(Debug, Clone)]
pub struct Step {
    /// Unique inside a job; `requires` of another step refers to it.
    pub id: u32,
    pub kind: StepKind,
    pub phase: Phase,
    pub node: Option<NodeId>,
    pub src: Option<Loc>,
    /// The item written, created, moved to or deleted.
    pub dst: Loc,
    pub expect_src: Option<Fingerprint>,
    pub expect_dst: Option<Fingerprint>,
    pub bytes: u64,
    pub only_if_empty: bool,
    pub filters_active: bool,
    pub mtime: Option<Ticks>,
    /// The step that must have completed first. For a delete (the source of a cross-volume
    /// move) it is the copy of that item (decision: D43); for a folder-time step it is the
    /// creation of the folder, which must have been done by this job.
    pub requires: Option<u32>,
    /// A delete that must only run when this copy exists (the destination of a cross-volume
    /// move): `(where, fingerprint to match on kind and size)`.
    pub guard: Option<(Loc, Fingerprint)>,
    /// The name used in the log and in messages.
    pub label: &'static str,
}

/// How copies and deletes are done. For a sync, the flags come from the plan's rules
/// ([`ExecOptions::from_rules`]); only the log and a few overrides come from the caller.
#[derive(Debug, Clone)]
pub struct ExecOptions {
    pub overwrite_readonly: bool,
    pub safe_copy: bool,
    pub copy_ctime: bool,
    pub verify: bool,
    /// Flush every copy to the storage device before it replaces its destination (decision D65).
    pub durable_copy: bool,
    /// A log that was asked for (`log_dir`) must work: when it cannot be created the job does not
    /// start, and when a line cannot be written the job stops after the current step (decision
    /// D66). Without this an audit trail could silently be missing for a destructive job.
    pub log_required: bool,
    /// Tries again after a transient copy failure (a locked file, a dropped share): robocopy's `/R`.
    pub copy_retries: u32,
    /// Milliseconds between the tries (robocopy's `/W`).
    pub copy_retry_wait_ms: u32,
    pub use_recycle_bin: bool,
    /// Folder for the JSON-lines execution log; `None` = no log.
    pub log_dir: Option<PathBuf>,
    /// File-name prefix of the log.
    pub job_name: String,
}

impl Default for ExecOptions {
    fn default() -> Self {
        ExecOptions {
            overwrite_readonly: true,
            safe_copy: true,
            copy_ctime: false,
            verify: false,
            durable_copy: true,
            log_required: true,
            copy_retries: 2,
            copy_retry_wait_ms: 1000,
            use_recycle_bin: false,
            log_dir: None,
            job_name: "job".into(),
        }
    }
}

impl ExecOptions {
    pub fn from_rules(r: &crate::SyncRules) -> Self {
        ExecOptions {
            overwrite_readonly: r.overwrite_readonly,
            safe_copy: r.safe_copy,
            copy_ctime: r.copy_creation_time,
            verify: r.verify,
            use_recycle_bin: r.use_recycle_bin,
            ..Self::default()
        }
    }
}

/// What a caller may set when it starts a sync: where the log goes and, when it wants to,
/// stricter or different behaviour than the plan's rules. The rules decide everything else, so a
/// preview and its execution cannot disagree (decision: D46).
#[derive(Debug, Clone, Default)]
pub struct ExecOverrides {
    pub log_dir: Option<PathBuf>,
    pub job_name: Option<String>,
    pub verify: Option<bool>,
    pub use_recycle_bin: Option<bool>,
    pub durable_copy: Option<bool>,
    pub log_required: Option<bool>,
    pub copy_retries: Option<u32>,
    pub copy_retry_wait_ms: Option<u32>,
}

impl ExecOverrides {
    fn apply(self, rules: &crate::SyncRules) -> ExecOptions {
        let mut o = ExecOptions::from_rules(rules);
        o.log_dir = self.log_dir;
        o.job_name = self.job_name.unwrap_or_else(|| "sync".into());
        if let Some(v) = self.verify {
            o.verify = v;
        }
        if let Some(v) = self.use_recycle_bin {
            o.use_recycle_bin = v;
        }
        if let Some(v) = self.durable_copy {
            o.durable_copy = v;
        }
        if let Some(v) = self.log_required {
            o.log_required = v;
        }
        if let Some(v) = self.copy_retries {
            o.copy_retries = v;
        }
        if let Some(v) = self.copy_retry_wait_ms {
            o.copy_retry_wait_ms = v;
        }
        o
    }
}

/// Turns a plan into steps in execution order (plan.md §8.3).
pub fn steps_from_plan(plan: &Plan, compare: &FolderCompare) -> Vec<Step> {
    let mut steps: Vec<Step> = Vec::with_capacity(plan.actions.len());
    let mut time_steps = Vec::new();
    let mut next_id = 0u32;
    let mut take_id = || {
        next_id += 1;
        next_id
    };
    for a in &plan.actions {
        let to_left = a.kind.target_is_left();
        let dst = Loc {
            vfs: compare.vfs(to_left).clone(),
            path: a.dst.clone(),
        };
        let src = a.src.as_ref().map(|p| Loc {
            vfs: compare.vfs(!to_left).clone(),
            path: p.clone(),
        });
        // decision: §8.3: free names, create folders, copy, delete, set folder times
        let (kind, phase) = match a.kind {
            ActionKind::CreateFolderLeft | ActionKind::CreateFolderRight => {
                (StepKind::CreateDir, Phase::CreateDirs)
            }
            ActionKind::CopyToLeft | ActionKind::CopyToRight => (StepKind::CopyFile, Phase::Copies),
            ActionKind::DeleteLeft | ActionKind::DeleteRight => (
                StepKind::Delete,
                if a.frees_name {
                    Phase::FreeNames
                } else {
                    Phase::Deletes
                },
            ),
        };
        let id = take_id();
        if a.kind.is_create_folder() {
            if let Some(m) = a.folder_mtime {
                time_steps.push(Step {
                    id: take_id(),
                    kind: StepKind::SetFolderTime,
                    phase: Phase::FolderTimes,
                    node: None,
                    src: None,
                    dst: dst.clone(),
                    expect_src: None,
                    expect_dst: None,
                    bytes: 0,
                    only_if_empty: false,
                    filters_active: false,
                    mtime: Some(m),
                    requires: Some(id),
                    guard: None,
                    label: "set-folder-time",
                });
            }
        }
        steps.push(Step {
            id,
            kind,
            phase,
            node: Some(a.node),
            src,
            dst,
            expect_src: a.expect_src,
            expect_dst: a.expect_dst,
            bytes: a.bytes,
            only_if_empty: a.only_if_empty,
            filters_active: plan.filters_active,
            mtime: None,
            requires: None,
            guard: None,
            label: a.kind.name(),
        });
    }
    // folder times last, deepest folders first
    time_steps.reverse();
    steps.extend(time_steps);
    // a stable sort keeps the plan's own order (pre-order creates, post-order deletes) in a phase
    steps.sort_by_key(|s| s.phase);
    steps
}

// ---------------------------------------------------------------- questions and answers

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuestionKind {
    /// The source or destination is not what the compare saw (§9.1).
    ChangedSinceCompare,
    /// The action failed.
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    pub id: u64,
    pub kind: QuestionKind,
    pub step: usize,
    pub path: String,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    /// Try the action again (it is validated against the compare again).
    Retry,
    Skip,
    SkipAll,
    Abort,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JobProgress {
    pub steps_total: u32,
    pub steps_done: u32,
    pub bytes_total: u64,
    pub bytes_done: u64,
    pub current: String,
    pub paused: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepFailure {
    pub path: String,
    pub message: String,
}

/// The outcome of a job (plan.md §8.3).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JobResult {
    pub done: u32,
    pub skipped: u32,
    pub failed: u32,
    /// Folder deletes that left the folder in place because something was still in it (§9.4).
    pub kept: u32,
    pub bytes_copied: u64,
    pub errors: Vec<StepFailure>,
    pub aborted: bool,
    pub cancelled: bool,
}

#[derive(Debug, Clone)]
pub enum JobEvent {
    Progress(JobProgress),
    Question(Question),
    Finished,
}

pub type JobSink = Arc<dyn Fn(&JobEvent) + Send + Sync>;

#[derive(Debug, Default)]
struct Inner {
    question: Option<Question>,
    answer: Option<Answer>,
    progress: JobProgress,
    next_question: u64,
}

struct Shared {
    cancel: CancelToken,
    pause: PauseGate,
    inner: Mutex<Inner>,
    cv: Condvar,
    sink: Option<JobSink>,
    /// The result, once the worker is done; `wait` blocks on `done_cv` until it is there.
    done: Mutex<Option<JobResult>>,
    done_cv: Condvar,
    finished: AtomicBool,
}

enum Asked {
    Answer(Answer),
    Cancelled,
}

impl Shared {
    fn emit(&self, e: &JobEvent) {
        if let Some(s) = &self.sink {
            s(e);
        }
    }

    /// Publishes the result and wakes every waiter; the one place a job ends.
    fn finish(&self, result: JobResult) {
        *self.done.lock() = Some(result);
        self.finished.store(true, Ordering::SeqCst);
        self.done_cv.notify_all();
        self.emit(&JobEvent::Finished);
    }

    /// Posts a question and waits on the condvar for the answer. No callback is on the stack
    /// while waiting (plan.md §8.3). A cancel is reported as such, not as an abort.
    fn ask(&self, kind: QuestionKind, step: usize, path: String, message: String) -> Asked {
        let q = {
            let mut g = self.inner.lock();
            g.next_question += 1;
            let q = Question {
                id: g.next_question,
                kind,
                step,
                path,
                message,
            };
            g.question = Some(q.clone());
            g.answer = None;
            q
        };
        self.emit(&JobEvent::Question(q));
        let mut g = self.inner.lock();
        loop {
            if let Some(a) = g.answer.take() {
                g.question = None;
                return Asked::Answer(a);
            }
            if self.cancel.is_cancelled() {
                g.question = None;
                return Asked::Cancelled;
            }
            self.cv.wait_for(&mut g, Duration::from_millis(25));
        }
    }
}

impl std::fmt::Debug for Shared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Shared")
    }
}

/// A running (or finished) job. Every method is safe to call from any thread at any time; only
/// `wait` blocks.
#[derive(Debug)]
pub struct Job {
    shared: Arc<Shared>,
}

/// Called after every successful step (the sync starter refreshes the compare tree here). The
/// token is the job's: a cancel reaches the callback's own work.
pub type AfterStep = Box<dyn FnMut(&Step, &CancelToken) + Send>;

impl Job {
    /// Starts the worker thread.
    pub fn spawn(
        steps: Vec<Step>,
        opts: ExecOptions,
        after: AfterStep,
        sink: Option<JobSink>,
    ) -> Job {
        let shared = Arc::new(Shared {
            cancel: CancelToken::new(),
            pause: PauseGate::new(),
            inner: Mutex::new(Inner {
                progress: JobProgress {
                    steps_total: steps.len() as u32,
                    bytes_total: steps
                        .iter()
                        .filter(|s| s.kind == StepKind::CopyFile)
                        .map(|s| s.bytes)
                        .sum(),
                    ..JobProgress::default()
                },
                ..Inner::default()
            }),
            cv: Condvar::new(),
            sink,
            done: Mutex::new(None),
            done_cv: Condvar::new(),
            finished: AtomicBool::new(false),
        });
        let sh = shared.clone();
        let spawned = std::thread::Builder::new()
            .name("ds-job".into())
            .spawn(move || {
                let r = catch_unwind(AssertUnwindSafe(|| run_job(&steps, &opts, after, &sh)));
                let result = r.unwrap_or_else(|_| JobResult {
                    aborted: true,
                    errors: vec![StepFailure {
                        path: String::new(),
                        message: "the job thread panicked".into(),
                    }],
                    ..JobResult::default()
                });
                sh.finish(result);
            });
        if let Err(e) = spawned {
            // a job that cannot start has failed; it must not look like "nothing to do"
            shared.finish(JobResult {
                aborted: true,
                errors: vec![StepFailure {
                    path: String::new(),
                    message: format!("cannot start the job thread: {e}"),
                }],
                ..JobResult::default()
            });
        }
        Job { shared }
    }

    pub fn progress(&self) -> JobProgress {
        let mut p = self.shared.inner.lock().progress.clone();
        p.paused = self.shared.pause.is_paused();
        p
    }

    /// The open question, if the worker is waiting for an answer.
    pub fn question(&self) -> Option<Question> {
        self.shared.inner.lock().question.clone()
    }

    /// Answers the question with this id; `NotFound` when it is no longer (or not yet) the open
    /// one, so a late click never lands on the next question.
    pub fn answer(&self, question_id: u64, a: Answer) -> Result<()> {
        let mut g = self.shared.inner.lock();
        match &g.question {
            None => Err(Error::new(ErrorKind::NotFound, "no question is open")),
            // a second answer to the same question must not replace the first
            Some(_) if g.answer.is_some() => Err(Error::new(
                ErrorKind::NotFound,
                "that question was answered already",
            )),
            Some(q) if q.id != question_id => Err(Error::new(
                ErrorKind::NotFound,
                "that question is no longer open",
            )),
            Some(_) => {
                g.answer = Some(a);
                self.shared.cv.notify_all();
                Ok(())
            }
        }
    }

    pub fn pause(&self) {
        self.shared.pause.pause();
    }

    pub fn resume(&self) {
        self.shared.pause.resume();
    }

    /// Stops the job as soon as the current action allows; nothing half-written is left.
    pub fn cancel(&self) {
        self.shared.cancel.cancel();
        self.shared.pause.resume();
        self.shared.cv.notify_all();
    }

    /// Never blocks.
    pub fn is_finished(&self) -> bool {
        self.shared.finished.load(Ordering::SeqCst)
    }

    /// Waits for the job and returns its result (again on later calls, from any thread).
    pub fn wait(&self) -> JobResult {
        let mut g = self.shared.done.lock();
        loop {
            if let Some(r) = g.as_ref() {
                return r.clone();
            }
            self.shared.done_cv.wait(&mut g);
        }
    }
}

// ---------------------------------------------------------------- the worker

#[derive(Debug)]
enum Outcome {
    /// Done by this step (a folder: created by it).
    Done,
    /// A folder that was already there: as good as created, but not by this job.
    Existing,
    /// A folder delete that left the folder (reason for the log).
    Kept(&'static str),
}

#[derive(Debug)]
struct StepError {
    changed: bool,
    cancelled: bool,
    message: String,
}

impl StepError {
    fn from(e: &Error) -> Self {
        StepError {
            changed: e.kind() == ErrorKind::ChangedSinceCompare,
            cancelled: e.is_cancelled(),
            message: e.to_string(),
        }
    }

    fn changed(message: impl Into<String>) -> Self {
        StepError {
            changed: true,
            cancelled: false,
            message: message.into(),
        }
    }

    fn failed(message: impl Into<String>) -> Self {
        StepError {
            changed: false,
            cancelled: false,
            message: message.into(),
        }
    }
}

/// Re-stats source and destination and compares them with the fingerprints (plan.md §9.1).
/// decision: D53, plan §9.5: every folder above the item must still be a plain folder. A folder
/// that was replaced by a junction or symlink after the compare would make the step read, write
/// or delete somewhere else even though the item itself still has the expected fingerprint.
/// (The roots themselves may be links: that is the user's choice, and they are not checked.)
fn check_parents(loc: &Loc, what: &str) -> std::result::Result<(), StepError> {
    let mut cur = loc.path.parent();
    while let Some(dir) = cur {
        if dir.components().is_empty() {
            break; // the root
        }
        match loc.vfs.stat(&dir).map_err(|e| StepError::from(&e))? {
            Some(e) if e.kind != Kind::Dir => {
                return Err(StepError::changed(format!(
                    "a parent folder of the {what} is no longer a plain folder (a link?): {}",
                    dir.display()
                )));
            }
            // a missing folder is the operation's own business (it fails or has nothing to do)
            _ => {}
        }
        cur = dir.parent();
    }
    Ok(())
}

fn validate(step: &Step) -> std::result::Result<Option<Outcome>, StepError> {
    let fail = |e: &Error| StepError::from(e);
    if let Some(src) = &step.src {
        check_parents(src, "source")?;
    }
    check_parents(&step.dst, "destination")?;
    // decision: §9.1: never act on something that is not what the compare showed
    if let (Some(fp), Some(src)) = (&step.expect_src, &step.src) {
        match src.vfs.stat(&src.path).map_err(|e| fail(&e))? {
            None => {
                return Err(StepError::changed(format!(
                    "the source vanished: {}",
                    src.path.display()
                )));
            }
            Some(e) if Fingerprint::of_entry(&e) != *fp => {
                return Err(StepError::changed(format!(
                    "the source changed since the compare: {}",
                    src.path.display()
                )));
            }
            Some(_) => {}
        }
    }
    let now = step.dst.vfs.stat(&step.dst.path).map_err(|e| fail(&e))?;
    match (&step.expect_dst, now) {
        // a delete of something already gone has reached its goal
        (Some(_), None) if step.kind == StepKind::Delete => return Ok(Some(Outcome::Done)),
        (Some(_), None) => {
            return Err(StepError::changed(format!(
                "the destination vanished: {}",
                step.dst.path.display()
            )));
        }
        (Some(fp), Some(e)) if Fingerprint::of_entry(&e) != *fp => {
            return Err(StepError::changed(format!(
                "the destination changed since the compare: {}",
                step.dst.path.display()
            )));
        }
        // a folder that appeared meanwhile is as good as the one we wanted to create
        (None, Some(e)) if step.kind == StepKind::CreateDir && e.kind == Kind::Dir => {
            return Ok(Some(Outcome::Existing));
        }
        (None, Some(_))
            if step.kind != StepKind::SetFolderTime && step.kind != StepKind::Delete =>
        {
            return Err(StepError::changed(format!(
                "the destination appeared after the compare: {}",
                step.dst.path.display()
            )));
        }
        _ => {}
    }
    Ok(None)
}

fn perform(
    step: &Step,
    opts: &ExecOptions,
    shared: &Shared,
    base_bytes: u64,
) -> std::result::Result<Outcome, StepError> {
    let fail = |e: &Error| StepError::from(e);
    if let Some(done) = validate(step)? {
        return Ok(done);
    }
    shared.cancel.check().map_err(|e| fail(&e))?;
    if let Some((copy, fp)) = &step.guard {
        // decision: D43: the source of a move goes only after its copy is verified to exist
        let ok = copy
            .vfs
            .stat(&copy.path)
            .map_err(|e| fail(&e))?
            .is_some_and(|e| e.kind == fp.kind && (e.kind != Kind::File || e.size == fp.size));
        if !ok {
            return Err(StepError::failed(format!(
                "the copy is missing at {}; the source is kept",
                copy.path.display()
            )));
        }
    }
    match step.kind {
        StepKind::CreateDir => {
            step.dst
                .vfs
                .create_dir(&step.dst.path)
                .map_err(|e| fail(&e))?;
        }
        StepKind::CopyFile => {
            let Some(src) = &step.src else {
                return Err(StepError::from(&Error::internal(
                    "a copy step without a source",
                )));
            };
            let o = CopyOptions {
                overwrite: step.expect_dst.is_some(),
                overwrite_readonly: opts.overwrite_readonly,
                safe_copy: opts.safe_copy,
                copy_ctime: opts.copy_ctime,
                verify: opts.verify,
                durable: opts.durable_copy,
                retries: opts.copy_retries,
                retry_wait_ms: opts.copy_retry_wait_ms,
            };
            let prog = |done: u64, _total: u64| {
                shared.inner.lock().progress.bytes_done = base_bytes + done;
            };
            copy_between(
                &*src.vfs,
                &src.path,
                &*step.dst.vfs,
                &step.dst.path,
                &o,
                &prog,
                &shared.cancel,
            )
            .map_err(|e| fail(&e))?;
        }
        StepKind::MoveFile => {
            let Some(src) = &step.src else {
                return Err(StepError::from(&Error::internal(
                    "a move step without a source",
                )));
            };
            step.dst
                .vfs
                .rename(&src.path, &step.dst.path, false)
                .map_err(|e| fail(&e))?;
        }
        StepKind::Delete => {
            if step.only_if_empty && !opts.use_recycle_bin {
                // decision: D64 (was D42): the folder is removed by a call that refuses a folder
                // with anything in it, so a file created a moment ago is never deleted with it
                let removed = step
                    .dst
                    .vfs
                    .remove_dir_if_empty(&step.dst.path)
                    .map_err(|e| fail(&e))?;
                return Ok(if removed {
                    Outcome::Done
                } else {
                    Outcome::Kept(if step.filters_active {
                        "kept (contains excluded items)"
                    } else {
                        "kept (contains items added after the compare)"
                    })
                });
            }
            if step.only_if_empty {
                // to the Recycle Bin: the folder and anything in it can be restored, so a
                // look-then-delete is acceptable here
                let mut left = Vec::new();
                step.dst
                    .vfs
                    .list(&step.dst.path, &mut left)
                    .map_err(|e| fail(&e))?;
                if !left.is_empty() {
                    return Ok(Outcome::Kept(if step.filters_active {
                        "kept (contains excluded items)"
                    } else {
                        "kept (contains items added after the compare)"
                    }));
                }
            }
            let mode = if opts.use_recycle_bin {
                DeleteMode::RecycleBin
            } else {
                DeleteMode::Permanent
            };
            step.dst
                .vfs
                .delete(&step.dst.path, mode)
                .map_err(|e| fail(&e))?;
        }
        StepKind::SetFolderTime => {
            if let Some(m) = step.mtime {
                // best effort: a folder whose time cannot be set is still correct
                let _ = step.dst.vfs.set_times(&step.dst.path, Some(m), None);
            }
        }
    }
    Ok(Outcome::Done)
}

fn run_job(
    steps: &[Step],
    opts: &ExecOptions,
    mut after: AfterStep,
    shared: &Arc<Shared>,
) -> JobResult {
    let mut result = JobResult::default();
    let mut log = match opts
        .log_dir
        .as_ref()
        .map(|d| JobLog::create(d, &opts.job_name, steps.len()))
    {
        Some(Ok(l)) => Some(l),
        Some(Err(e)) => {
            // an audit log that was asked for and cannot be written is reported, not dropped
            // (decision D66: and with `log_required` the job does not start)
            if opts.log_required {
                result.aborted = true;
            }
            result.errors.push(StepFailure {
                path: opts
                    .log_dir
                    .as_ref()
                    .map(|d| d.display().to_string())
                    .unwrap_or_default(),
                message: format!("the execution log could not be created: {e}"),
            });
            None
        }
        None => None,
    };
    if result.aborted {
        return result; // the log is required and could not be created: nothing has been done
    }
    let mut skip_all = false;
    let mut base_bytes = 0u64;
    // steps that completed, and folders this job really created
    let mut completed: HashSet<u32> = HashSet::new();
    let mut created: HashSet<u32> = HashSet::new();
    'steps: for (i, step) in steps.iter().enumerate() {
        // a step that depends on another one runs only when that one completed
        if let Some(r) = step.requires {
            let ok = if step.kind == StepKind::SetFolderTime {
                created.contains(&r)
            } else {
                completed.contains(&r)
            };
            if !ok {
                if step.kind == StepKind::SetFolderTime {
                    continue; // a folder this job did not create keeps its own time
                }
                result.skipped += 1;
                let msg = "the copy of this item did not complete; the source is kept";
                result.errors.push(StepFailure {
                    path: step.dst.path.display(),
                    message: msg.into(),
                });
                if let Some(l) = &mut log {
                    l.action(step, "skipped", Some(msg), 0);
                }
                continue;
            }
        }
        loop {
            if shared.cancel.is_cancelled() {
                result.cancelled = true;
                break 'steps;
            }
            if shared.pause.wait_or_cancel(&shared.cancel).is_err() {
                result.cancelled = true;
                break 'steps;
            }
            {
                let mut g = shared.inner.lock();
                g.progress.current = step.dst.path.display();
            }
            match perform(step, opts, shared, base_bytes) {
                Ok(outcome) => {
                    let (word, note) = match outcome {
                        Outcome::Done => ("done", None),
                        Outcome::Existing => ("done", Some("the folder already existed")),
                        Outcome::Kept(r) => ("kept", Some(r)),
                    };
                    if matches!(outcome, Outcome::Kept(_)) {
                        result.kept += 1;
                    } else {
                        result.done += 1;
                        completed.insert(step.id);
                        if matches!(outcome, Outcome::Done) && step.kind == StepKind::CreateDir {
                            created.insert(step.id);
                        }
                    }
                    if step.kind == StepKind::CopyFile {
                        result.bytes_copied += step.bytes;
                        base_bytes += step.bytes;
                    }
                    if let Some(l) = &mut log {
                        l.action(step, word, note, step.bytes);
                    }
                    after(step, &shared.cancel);
                    break;
                }
                Err(e) if e.cancelled => {
                    result.cancelled = true;
                    if let Some(l) = &mut log {
                        l.action(step, "cancelled", None, 0);
                    }
                    break 'steps;
                }
                Err(e) => {
                    let path = step.dst.path.display();
                    if skip_all {
                        result.failed += 1;
                        result.errors.push(StepFailure {
                            path,
                            message: e.message.clone(),
                        });
                        if let Some(l) = &mut log {
                            l.action(step, "failed", Some(&e.message), 0);
                        }
                        break;
                    }
                    let kind = if e.changed {
                        QuestionKind::ChangedSinceCompare
                    } else {
                        QuestionKind::Failed
                    };
                    let asked = shared.ask(kind, i, path.clone(), e.message.clone());
                    let Asked::Answer(answer) = asked else {
                        result.cancelled = true;
                        if let Some(l) = &mut log {
                            l.action(step, "cancelled", Some(&e.message), 0);
                        }
                        break 'steps;
                    };
                    match answer {
                        Answer::Retry => {} // the loop validates and tries again
                        Answer::Skip => {
                            result.skipped += 1;
                            if let Some(l) = &mut log {
                                l.action(step, "skipped", Some(&e.message), 0);
                            }
                            break;
                        }
                        Answer::SkipAll => {
                            skip_all = true;
                            result.skipped += 1;
                            if let Some(l) = &mut log {
                                l.action(step, "skipped", Some(&e.message), 0);
                            }
                            break;
                        }
                        Answer::Abort => {
                            result.aborted = true;
                            result.errors.push(StepFailure {
                                path,
                                message: e.message.clone(),
                            });
                            if let Some(l) = &mut log {
                                l.action(step, "aborted", Some(&e.message), 0);
                            }
                            break 'steps;
                        }
                    }
                }
            }
        }
        // decision D66: no step runs after the audit trail broke
        if opts.log_required {
            if let Some(why) = log.as_ref().and_then(JobLog::write_error) {
                result.aborted = true;
                result.errors.push(StepFailure {
                    path: opts
                        .log_dir
                        .as_ref()
                        .map(|d| d.display().to_string())
                        .unwrap_or_default(),
                    message: format!(
                        "the execution log can no longer be written ({why}); the job was stopped"
                    ),
                });
                break 'steps;
            }
        }
        let snapshot = {
            let mut g = shared.inner.lock();
            g.progress.steps_done = (i + 1) as u32;
            g.progress.clone()
        };
        shared.emit(&JobEvent::Progress(snapshot));
    }
    if let Some(l) = &mut log {
        l.finish(&result);
    }
    result
}

// ---------------------------------------------------------------- starting a sync

/// A compare shared between the UI (reads) and the executor (refreshes after each action).
///
/// Readers take `read()` once per call and never wait on a job while holding it: the executor
/// takes the write lock briefly after each action, and a second `read()` on the same thread
/// can deadlock behind a waiting writer (parking_lot locks are write-preferring).
pub type SharedCompare = Arc<RwLock<FolderCompare>>;

/// Called with the node after the tree was refreshed for a finished action.
pub type Refreshed = Arc<dyn Fn(NodeId) + Send + Sync>;

/// Starts executing a plan. The plan is only read: it is immutable once execution starts.
/// Copy and delete behaviour (`safe_copy`, Recycle Bin, read-only handling) comes from the
/// plan's own rules (decision: D46); `overrides` sets the log and may tighten them.
pub fn start_sync(
    plan: &Plan,
    compare: &SharedCompare,
    overrides: ExecOverrides,
    sink: Option<JobSink>,
) -> Result<Job> {
    start_sync_with(plan, compare, overrides, sink, None)
}

/// Like [`start_sync`], and `refreshed` is called after each node's refresh (the C ABI bumps
/// its tree revision there).
pub fn start_sync_with(
    plan: &Plan,
    compare: &SharedCompare,
    overrides: ExecOverrides,
    sink: Option<JobSink>,
    refreshed: Option<Refreshed>,
) -> Result<Job> {
    {
        let g = compare.read();
        // decision: §9.7: nested roots never run a sync, even with a plan built elsewhere
        if g.sync_blocked {
            return Err(Error::invalid_argument(
                "one folder is inside the other: sync is blocked",
            ));
        }
        // decision: D46: a plan belongs to the compare it was built from
        if plan.compare_id != g.id {
            return Err(Error::invalid_argument(
                "the plan was built from another compare",
            ));
        }
    }
    let steps = steps_from_plan(plan, &compare.read());
    let opts = overrides.apply(&plan.rules);
    let c = compare.clone();
    let after: AfterStep = Box::new(move |s: &Step, cancel: &CancelToken| {
        let Some(n) = s.node else { return };
        // the tree follows reality after every action (plan.md §8.3): stat under a short write
        // lock, content compares (if the rules ask for them) without any lock and with the
        // job's own cancel token
        // stat without any lock, apply under a short write lock
        let probe = c.read().probe_batch(&[n]);
        if let Ok(probes) = probe.run(cancel) {
            c.write().apply_probes(probes);
        }
        let batch = c.write().take_content_batch();
        if let Some(batch) = batch {
            match batch.run(cancel, &|_| {}) {
                Ok(results) => c.write().apply_content_results(results),
                Err(_) => c.write().requeue(batch),
            }
        }
        if let Some(r) = &refreshed {
            r(n);
        }
    });
    Ok(Job::spawn(steps, opts, after, sink))
}
