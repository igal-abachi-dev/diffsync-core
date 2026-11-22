//! Folder compare: aligned tree, rules, content compare and views (plan.md §7).
#![forbid(unsafe_code)]

pub mod compare;
pub mod rules;
pub mod tree;
pub mod udiff;
pub mod view;

pub use compare::{
    ContentBatch, ContentResult, FolderCompare, Probe, ProbeBatch, check_roots, content_compare,
};
pub use rules::{ContentKind, FolderRules};
pub use tree::{Counters, Node, NodeId, SideInfo, Status, Tree, flag};
pub use view::{FolderView, SortKey, SortOrder, ViewFilter};
