//! Sync planning and execution, file operations and the execution log (plan.md §8, §9).
#![forbid(unsafe_code)]

pub mod exec;
pub mod fileops;
pub mod log;
pub mod plan;
pub mod rules;
pub mod sweep;

pub use exec::{
    Answer, ExecOptions, ExecOverrides, Job, JobEvent, JobProgress, JobResult, JobSink, Loc, Phase,
    Question, QuestionKind, Refreshed, SharedCompare, Step, StepFailure, StepKind, start_sync,
    start_sync_with, steps_from_plan,
};
pub use fileops::{
    FileOpOptions, copy_items, delete_items, move_items, plan_copy_items, plan_delete_items,
    plan_move_items,
};
pub use plan::{Action, ActionKind, Fingerprint, Plan, PlanIssue, Severity, Summary, build_plan};
pub use rules::{Op, PRESETS, Preset, SyncRules};
pub use sweep::{SweepReport, maintenance_sweep};
