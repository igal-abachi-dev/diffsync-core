//! Glob patterns, brace expansion and include/exclude rule sets: a Rust port of the Glob C#
//! library (plan.md §7.5, decision D15). It depends only on `core-base` and `core-vfs`.
#![forbid(unsafe_code)]

pub mod braces;
pub mod ignore;
pub mod matcher;
pub mod rules;
pub mod walker;

pub use braces::{MAX_EXPANSIONS, PatternError, expand_braces};
pub use ignore::{IgnoreList, LOCK_FILE_PATTERNS, MORE_LOCK_FILE_PATTERNS};
pub use matcher::Segment;
pub use rules::{Decision, MatchOptions, Pattern, Rule, RuleSet, RuleWarning};
pub use walker::{WalkHit, WalkOptions, WalkReport, walk};
