//! Helpers shared by the sync tests.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use core_base::CancelToken;
use core_base::ticks::TICKS_PER_SECOND as S;
use core_folder::{FolderCompare, FolderRules, NodeId, Status, Tree};
use core_sync::{
    ActionKind, Answer, ExecOverrides, Job, JobResult, Plan, Preset, SharedCompare, SyncRules,
    build_plan, start_sync,
};
use core_vfs::{FaultVfs, MemVfs, VPath, Vfs};
use parking_lot::RwLock;

pub const T0: i64 = 1_000_000 * S;

pub fn arc(m: MemVfs) -> Arc<dyn Vfs> {
    Arc::new(m)
}

pub fn compare(l: Arc<dyn Vfs>, r: Arc<dyn Vfs>, rules: FolderRules) -> FolderCompare {
    FolderCompare::compare(
        l,
        &VPath::root(),
        r,
        &VPath::root(),
        rules,
        &CancelToken::new(),
    )
    .expect("compare")
}

pub fn shared(c: FolderCompare) -> SharedCompare {
    Arc::new(RwLock::new(c))
}

pub fn find(c: &FolderCompare, path: &str) -> NodeId {
    (1..c.tree.len() as NodeId)
        .find(|&i| c.tree.path(i).display() == path)
        .unwrap_or_else(|| panic!("no node {path}"))
}

pub fn status(c: &FolderCompare, path: &str) -> Status {
    c.tree.node(find(c, path)).status
}

pub fn rules(p: Preset) -> SyncRules {
    SyncRules::from_preset(p)
}

/// Action kinds by item path (files and folders), in plan order.
pub fn kinds(plan: &Plan, c: &FolderCompare) -> BTreeMap<String, ActionKind> {
    plan.actions
        .iter()
        .map(|a| (c.tree.path(a.node).display(), a.kind))
        .collect()
}

pub fn names(c: &FolderCompare, ids: &[NodeId]) -> Vec<String> {
    ids.iter().map(|&n| c.tree.path(n).display()).collect()
}

/// Waits (bounded) until the job has an open question.
pub fn wait_question(job: &Job) -> core_sync::Question {
    let t = Instant::now();
    loop {
        if let Some(q) = job.question() {
            return q;
        }
        assert!(
            t.elapsed() < Duration::from_secs(10),
            "no question appeared"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Runs a plan to the end, answering every question with `answer` from another thread.
pub fn run_with_answers(job: &Job, answer: Answer) -> JobResult {
    let done = std::sync::atomic::AtomicBool::new(false);
    std::thread::scope(|s| {
        let d = &done;
        s.spawn(move || {
            while !d.load(std::sync::atomic::Ordering::Relaxed) {
                if let Some(q) = job.question() {
                    let _ = job.answer(q.id, answer);
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        });
        let r = job.wait();
        done.store(true, std::sync::atomic::Ordering::Relaxed);
        r
    })
}

pub fn execute(plan: &Plan, c: &SharedCompare, opts: ExecOverrides) -> JobResult {
    let job = start_sync(plan, c, opts, None).expect("start");
    job.wait()
}

pub fn plan_for(c: &FolderCompare, p: Preset) -> Plan {
    build_plan(c, &rules(p), None).expect("plan")
}

pub fn read(v: &MemVfs, path: &str) -> Option<Vec<u8>> {
    v.read(path)
}

pub fn temp_files(v: &MemVfs) -> Vec<String> {
    v.paths()
        .into_iter()
        .filter(|p| p.contains(".dctmp~"))
        .collect()
}

pub fn fault(m: MemVfs) -> Arc<FaultVfs<MemVfs>> {
    Arc::new(FaultVfs::new(m))
}

pub fn root_status(c: &FolderCompare) -> core_folder::Counters {
    c.tree.node(Tree::ROOT).counters
}
