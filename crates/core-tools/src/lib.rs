//! Integration with other programs (backlog E11.5): how `diff` and `git diff` command lines are
//! understood, how source-control clients and Explorer start a compare tool, which files they ask
//! to skip, the "left side" chosen in Explorer, and the three-way merge entry point.
//!
//! Everything a command-line wrapper, an MCP server or a UI needs to behave like `diff -u`,
//! `diff -r` and `git diff --no-index` is here; the wrapper only passes arguments in and prints
//! what comes out.
#![forbid(unsafe_code)]

pub mod apply;
pub mod bindiff;
pub mod diffargs;
pub mod invoke;
pub mod jsonpatch;
pub mod mbox;
pub mod merge;
pub mod run;
pub mod selection;
pub mod shell;

pub use diffargs::{ArgError, Dialect, DiffRequest, Emit, parse_diff_args};
pub use invoke::{
    CompareInvocation, GitExternalDiff, Invocation, MergeInvocation, Parsed, parse_invocation,
};
pub use mbox::MailMeta;
pub use merge::{MergeFilesResult, MergeOutcome, merge_files, merge3};
pub use run::{DiffOutput, RunOptions, run, run_texts};
pub use selection::{Selection, SelectionStore};
