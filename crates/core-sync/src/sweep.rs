//! Leftover temp-file sweep (plan.md §9.2, backlog E8-S8): `*.dctmp~` files left by a crash are
//! found, reported, and deleted only on request.

use core_base::{CancelToken, Result};
use core_vfs::{Kind, VPath, Vfs, is_temp_name};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// Every temp file found, with its size.
    pub found: Vec<(VPath, u64)>,
    pub deleted: u32,
    /// Files that could not be deleted: path and message.
    pub errors: Vec<(VPath, String)>,
}

/// Walks `root` (links are never entered) and lists the leftover temp files. With
/// `delete = true` they are removed too.
pub fn maintenance_sweep(
    vfs: &dyn Vfs,
    root: &VPath,
    delete: bool,
    cancel: &CancelToken,
) -> Result<SweepReport> {
    let mut report = SweepReport::default();
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        cancel.check()?;
        let mut entries = Vec::new();
        if let Err(e) = vfs.list(&dir, &mut entries) {
            if dir == *root {
                return Err(e);
            }
            continue; // an unreadable folder is skipped, not fatal
        }
        for e in entries {
            let path = dir.join(e.name.clone());
            match e.kind {
                Kind::Dir => stack.push(path),
                Kind::File if is_temp_name(&e.name) => {
                    report.found.push((path.clone(), e.size));
                    if delete {
                        match vfs.delete(&path, core_vfs::DeleteMode::Permanent) {
                            Ok(()) => report.deleted += 1,
                            Err(err) => report.errors.push((path, err.to_string())),
                        }
                    }
                }
                _ => {}
            }
        }
    }
    report.found.sort();
    Ok(report)
}
