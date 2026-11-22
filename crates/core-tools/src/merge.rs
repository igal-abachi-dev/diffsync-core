//! Three-way merge: **a stub for now** (backlog E11.5-S4.3; the engine is E11.5-S14.4).
//!
//! What it does decide is what any merge tool must: when nothing needs a decision. If both sides
//! made the same change, or only one side changed, the result is known. Everything else is reported
//! as a conflict that the interactive merge has to resolve, so a source-control client that calls
//! the merge tool gets an honest "not merged" answer instead of a wrong file.

use std::path::Path;

use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "camelCase")]
pub enum MergeOutcome {
    /// Merged without conflicts.
    Clean { text: String, reason: &'static str },
    /// Both sides changed the base differently: needs the interactive merge.
    Conflict { reason: &'static str },
}

/// Merges three texts. The cases that need no line-level merge are decided; the rest is a conflict.
pub fn merge3(base: &str, local: &str, remote: &str) -> MergeOutcome {
    if local == remote {
        return MergeOutcome::Clean {
            text: local.to_string(),
            reason: "both sides are the same",
        };
    }
    if local == base {
        return MergeOutcome::Clean {
            text: remote.to_string(),
            reason: "only the remote side changed",
        };
    }
    if remote == base {
        return MergeOutcome::Clean {
            text: local.to_string(),
            reason: "only the local side changed",
        };
    }
    MergeOutcome::Conflict {
        reason: "both sides changed the file; three-way merge is not implemented in the core yet and needs the interactive merge",
    }
}

/// What `merge_files` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeFilesResult {
    /// 0: the merged file was written. 1: not merged (conflict or not implemented). 2: trouble.
    pub exit_code: i32,
    pub message: String,
}

/// The merge-tool contract of `git mergetool` and SourceTree: write the merged file to `output` and
/// exit 0, or exit non-zero and leave it alone. Without a base there is no common ancestor, so only
/// identical files merge.
pub fn merge_files(
    local: &Path,
    remote: &Path,
    base: Option<&Path>,
    output: &Path,
) -> MergeFilesResult {
    let read = |p: &Path| std::fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()));
    let (l, r) = match (read(local), read(remote)) {
        (Ok(l), Ok(r)) => (l, r),
        (Err(e), _) | (_, Err(e)) => {
            return MergeFilesResult {
                exit_code: 2,
                message: e,
            };
        }
    };
    let b = match base.map(read) {
        Some(Ok(b)) => b,
        Some(Err(e)) => {
            return MergeFilesResult {
                exit_code: 2,
                message: e,
            };
        }
        // no common ancestor: treat the local side as the base, so only a remote-only change merges
        None => l.clone(),
    };
    match merge3(&b, &l, &r) {
        MergeOutcome::Clean { text, reason } => match std::fs::write(output, text) {
            Ok(()) => MergeFilesResult {
                exit_code: 0,
                message: reason.to_string(),
            },
            Err(e) => MergeFilesResult {
                exit_code: 2,
                message: format!("{}: {e}", output.display()),
            },
        },
        MergeOutcome::Conflict { reason } => MergeFilesResult {
            exit_code: 1,
            message: reason.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trivial_cases_are_decided() {
        assert!(
            matches!(merge3("a\n", "b\n", "b\n"), MergeOutcome::Clean { ref text, .. } if text == "b\n")
        );
        assert!(
            matches!(merge3("a\n", "a\n", "c\n"), MergeOutcome::Clean { ref text, .. } if text == "c\n")
        );
        assert!(
            matches!(merge3("a\n", "b\n", "a\n"), MergeOutcome::Clean { ref text, .. } if text == "b\n")
        );
        assert!(matches!(
            merge3("a\n", "b\n", "c\n"),
            MergeOutcome::Conflict { .. }
        ));
    }

    #[test]
    fn the_merge_tool_contract_writes_only_a_clean_result() {
        let d = tempfile::tempdir().unwrap();
        let w = |n: &str, t: &str| {
            let p = d.path().join(n);
            std::fs::write(&p, t).unwrap();
            p
        };
        let (base, local, remote, out) = (
            w("base", "1\n"),
            w("local", "1\n"),
            w("remote", "2\n"),
            d.path().join("out"),
        );
        let r = merge_files(&local, &remote, Some(&base), &out);
        assert_eq!(r.exit_code, 0, "{}", r.message);
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "2\n");
        let conflicting = w("local2", "3\n");
        let out2 = d.path().join("out2");
        let r = merge_files(&conflicting, &remote, Some(&base), &out2);
        assert_eq!(r.exit_code, 1);
        assert!(!out2.exists(), "a conflict leaves the output alone");
        assert_eq!(
            merge_files(&d.path().join("missing"), &remote, None, &out2).exit_code,
            2
        );
    }
}
