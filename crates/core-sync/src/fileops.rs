//! File operations (plan.md §8.4, backlog E8-S7): `copy_items`, `move_items` and `delete_items`
//! run as jobs with the same progress, cancel, questions, safety rules and log as sync.

use std::collections::HashMap;

use core_base::{Error, Result};
use core_vfs::fold::fold_name;
use core_vfs::identity::{RootsRelation, roots_relation};
use core_vfs::{Entry, Kind, NameCheck, VPath, case_collisions, validate_name};

use crate::exec::{AfterStep, ExecOptions, Job, JobSink, Loc, Phase, Step, StepKind};
use crate::plan::Fingerprint;

/// Options of a file operation.
#[derive(Debug, Clone, Default)]
pub struct FileOpOptions {
    /// Replace files that already exist at the destination (they are re-validated first).
    pub overwrite: bool,
    pub exec: ExecOptions,
}

fn stat_item(l: &Loc) -> Result<Entry> {
    core_vfs::vfs::stat_required(&*l.vfs, &l.path)
}

fn step(kind: StepKind, phase: Phase, src: Option<Loc>, dst: Loc, label: &'static str) -> Step {
    Step {
        id: 0,
        requires: None,
        kind,
        phase,
        node: None,
        src,
        dst,
        expect_src: None,
        expect_dst: None,
        bytes: 0,
        only_if_empty: false,
        filters_active: false,
        mtime: None,
        guard: None,
        label,
    }
}

/// Lists `dir` into entries, sorted by name, links included (never entered).
fn children(l: &Loc) -> Result<Vec<Entry>> {
    let mut out = Vec::new();
    l.vfs.list(&l.path, &mut out)?;
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// Is `inner` the same place as `outer`, or inside it? (Plan.md §9.7, decision D48.) Decided by
/// file IDs where the backend has them, so `subst` drives, mapped drives and junctions that
/// alias one folder are caught; otherwise by path on one file system, case-folded when the
/// target ignores case.
fn is_within(outer: &Loc, inner: &Loc) -> Result<bool> {
    if let Some(rel) = roots_relation(&*outer.vfs, &outer.path, &*inner.vfs, &inner.path)? {
        return Ok(matches!(
            rel,
            RootsRelation::Same | RootsRelation::RightInsideLeft
        ));
    }
    if outer.vfs.storage_id() != inner.vfs.storage_id() {
        return Ok(false);
    }
    let fold = !outer.vfs.caps().case_sensitive;
    let key = |n: &core_vfs::Name| {
        if fold {
            fold_name(n.units())
        } else {
            n.units().to_vec()
        }
    };
    let (o, i) = (outer.path.components(), inner.path.components());
    Ok(i.len() >= o.len() && o.iter().zip(i).all(|(a, b)| key(a) == key(b)))
}

/// Numbers the steps, ties each folder-time step to its folder creation, and each guarded
/// delete (the source of a cross-volume move) to the copy of that item (decision: D43).
fn finalize(steps: &mut [Step]) {
    for (i, s) in steps.iter_mut().enumerate() {
        s.id = i as u32 + 1;
    }
    let creators: HashMap<VPath, u32> = steps
        .iter()
        .filter(|s| matches!(s.phase, Phase::CreateDirs | Phase::Copies))
        .map(|s| (s.dst.path.clone(), s.id))
        .collect();
    for s in steps.iter_mut() {
        if s.kind == StepKind::SetFolderTime {
            s.requires = creators.get(&s.dst.path).copied();
        } else if let Some((g, _)) = &s.guard {
            s.requires = creators.get(&g.path).copied();
        }
    }
}

/// Steps that copy `item` (a file, or a folder with everything in it) to `dst_dir/<name>`.
/// Links are skipped (decision: D16). Name problems and existing destinations are errors found
/// before anything is written (§9.8).
fn copy_steps(item: &Loc, dst_dir: &Loc, o: &FileOpOptions, out: &mut Vec<Step>) -> Result<()> {
    let root = stat_item(item)?;
    let name = item
        .path
        .file_name()
        .cloned()
        .ok_or_else(|| Error::invalid_argument("the root cannot be copied"))?;
    if root.kind == Kind::Link {
        return Ok(());
    }
    // a folder copied into itself would never end (§9.7)
    if root.kind == Kind::Dir && is_within(item, dst_dir)? {
        return Err(
            Error::invalid_argument("the destination is inside the source folder")
                .with_path(dst_dir.path.display()),
        );
    }
    let mut stack = vec![(item.clone(), dst_dir.path.join(name), root)];
    while let Some((src, dst_path, entry)) = stack.pop() {
        let dst = Loc {
            vfs: dst_dir.vfs.clone(),
            path: dst_path.clone(),
        };
        let leaf = dst_path
            .file_name()
            .map(core_vfs::Name::to_string_lossy)
            .unwrap_or_default();
        let check = validate_name(&dst.vfs.caps(), &leaf);
        if !matches!(check, NameCheck::Ok | NameCheck::ReservedName) {
            return Err(Error::new(
                core_base::ErrorKind::InvalidName,
                format!("`{leaf}` cannot be created on the target: {check:?}"),
            )
            .with_path(dst_path.display()));
        }
        let existing = dst.vfs.stat(&dst.path)?;
        match entry.kind {
            Kind::Dir => {
                match existing {
                    None => {
                        let mut s = step(
                            StepKind::CreateDir,
                            Phase::CreateDirs,
                            Some(src.clone()),
                            dst.clone(),
                            "create-folder",
                        );
                        s.expect_src = Some(Fingerprint::of_entry(&entry));
                        out.push(s);
                        let mut t = step(
                            StepKind::SetFolderTime,
                            Phase::FolderTimes,
                            None,
                            dst.clone(),
                            "set-folder-time",
                        );
                        t.mtime = Some(entry.mtime);
                        out.push(t);
                    }
                    Some(e) if e.kind == Kind::Dir => {} // merge into the existing folder
                    Some(_) => {
                        return Err(Error::new(
                            core_base::ErrorKind::AlreadyExists,
                            "a file of that name is in the way",
                        )
                        .with_path(dst_path.display()));
                    }
                }
                let kids = children(&src)?;
                let names: Vec<String> = kids.iter().map(|k| k.name.to_string_lossy()).collect();
                let refs: Vec<&str> = names.iter().map(String::as_str).collect();
                if !dst.vfs.caps().case_sensitive && !case_collisions(&refs, false).is_empty() {
                    return Err(Error::new(
                        core_base::ErrorKind::InvalidName,
                        "two names differ only in case and the target ignores case",
                    )
                    .with_path(src.path.display()));
                }
                for k in kids.into_iter().rev() {
                    if k.kind == Kind::Link {
                        continue;
                    }
                    let child_src = Loc {
                        vfs: src.vfs.clone(),
                        path: src.path.join(k.name.clone()),
                    };
                    stack.push((child_src, dst_path.join(k.name.clone()), k));
                }
            }
            Kind::File => {
                let mut s = step(
                    StepKind::CopyFile,
                    Phase::Copies,
                    Some(src.clone()),
                    dst,
                    "copy",
                );
                s.expect_src = Some(Fingerprint::of_entry(&entry));
                s.bytes = entry.size;
                // a file copied onto itself (same file ID) is refused (§9.7)
                if existing.is_some()
                    && roots_relation(&*src.vfs, &src.path, &*s.dst.vfs, &s.dst.path)?
                        == Some(RootsRelation::Same)
                {
                    return Err(
                        Error::invalid_argument("the destination is the source file")
                            .with_path(dst_path.display()),
                    );
                }
                match existing {
                    None => {}
                    Some(e) if e.kind == Kind::File && o.overwrite => {
                        s.expect_dst = Some(Fingerprint::of_entry(&e))
                    }
                    Some(_) => {
                        return Err(Error::new(
                            core_base::ErrorKind::AlreadyExists,
                            "the destination exists",
                        )
                        .with_path(dst_path.display()));
                    }
                }
                out.push(s);
            }
            Kind::Link => {}
        }
    }
    Ok(())
}

/// Delete steps for `item`, post-order; folders go only when empty, so nothing that was not
/// listed here is deleted (plan.md §9.1).
fn delete_steps(item: &Loc, out: &mut Vec<Step>) -> Result<()> {
    let root = stat_item(item)?;
    let mut stack = vec![(item.clone(), root, false)];
    while let Some((loc, entry, visited)) = stack.pop() {
        match entry.kind {
            Kind::Dir if !visited => {
                stack.push((loc.clone(), entry, true));
                // reversed, so the stack pops them in name order
                for k in children(&loc)?.into_iter().rev() {
                    let child = Loc {
                        vfs: loc.vfs.clone(),
                        path: loc.path.join(k.name.clone()),
                    };
                    stack.push((child, k, false));
                }
            }
            _ => {
                let mut s = step(StepKind::Delete, Phase::Deletes, None, loc, "delete");
                s.expect_dst = Some(Fingerprint::of_entry(&entry));
                s.bytes = if entry.kind == Kind::File {
                    entry.size
                } else {
                    0
                };
                s.only_if_empty = entry.kind == Kind::Dir;
                out.push(s);
            }
        }
    }
    Ok(())
}

/// The steps of `copy_items` (exposed for previews and `--dry-run`).
pub fn plan_copy_items(items: &[Loc], dst_dir: &Loc, o: &FileOpOptions) -> Result<Vec<Step>> {
    let mut steps = Vec::new();
    for i in items {
        copy_steps(i, dst_dir, o, &mut steps)?;
    }
    finalize(&mut steps);
    steps.sort_by_key(|s| s.phase);
    Ok(steps)
}

/// The steps of `move_items`: a rename inside one volume, copy then delete across volumes.
pub fn plan_move_items(items: &[Loc], dst_dir: &Loc, o: &FileOpOptions) -> Result<Vec<Step>> {
    let mut steps = Vec::new();
    for i in items {
        let name = i
            .path
            .file_name()
            .cloned()
            .ok_or_else(|| Error::invalid_argument("the root cannot be moved"))?;
        let dst = Loc {
            vfs: dst_dir.vfs.clone(),
            path: dst_dir.path.join(name),
        };
        let root = stat_item(i)?;
        if i.vfs.storage_id() == dst_dir.vfs.storage_id() && i.vfs.caps().atomic_rename {
            if root.kind == Kind::Dir && is_within(i, dst_dir)? {
                return Err(
                    Error::invalid_argument("the destination is inside the source folder")
                        .with_path(dst_dir.path.display()),
                );
            }
            if dst.vfs.stat(&dst.path)?.is_some() {
                return Err(Error::new(
                    core_base::ErrorKind::AlreadyExists,
                    "the destination exists",
                )
                .with_path(dst.path.display()));
            }
            let mut s = step(
                StepKind::MoveFile,
                Phase::Copies,
                Some(i.clone()),
                dst,
                "move",
            );
            s.expect_src = Some(Fingerprint::of_entry(&root));
            s.bytes = root.size;
            steps.push(s);
        } else {
            copy_steps(i, dst_dir, o, &mut steps)?;
            let mut deletes = Vec::new();
            delete_steps(i, &mut deletes)?;
            // each delete waits for its copy (decision: D43)
            let copy_root = dst_dir
                .path
                .join(i.path.file_name().cloned().unwrap_or_default());
            for mut d in deletes {
                let rel = d.dst.path.components().iter().skip(i.path.len());
                let copy_path = rel.fold(copy_root.clone(), |p, n| p.join(n.clone()));
                if let Some(fp) = d.expect_dst {
                    d.guard = Some((
                        Loc {
                            vfs: dst_dir.vfs.clone(),
                            path: copy_path,
                        },
                        fp,
                    ));
                }
                steps.push(d);
            }
        }
    }
    finalize(&mut steps);
    steps.sort_by_key(|s| s.phase);
    Ok(steps)
}

/// The steps of `delete_items`.
pub fn plan_delete_items(items: &[Loc]) -> Result<Vec<Step>> {
    let mut steps = Vec::new();
    for i in items {
        if i.path.is_root() {
            return Err(Error::invalid_argument("the root cannot be deleted"));
        }
        delete_steps(i, &mut steps)?;
    }
    finalize(&mut steps);
    Ok(steps)
}

fn start(steps: Vec<Step>, mut o: FileOpOptions, label: &str, sink: Option<JobSink>) -> Job {
    if o.exec.job_name == "job" {
        o.exec.job_name = label.into();
    }
    let after: AfterStep = Box::new(|_, _| {});
    Job::spawn(steps, o.exec, after, sink)
}

/// Copies files and folders into `dst_dir` as a job.
pub fn copy_items(
    items: &[Loc],
    dst_dir: &Loc,
    o: FileOpOptions,
    sink: Option<JobSink>,
) -> Result<Job> {
    let steps = plan_copy_items(items, dst_dir, &o)?;
    Ok(start(steps, o, "copy", sink))
}

/// Moves files and folders into `dst_dir` as a job.
pub fn move_items(
    items: &[Loc],
    dst_dir: &Loc,
    o: FileOpOptions,
    sink: Option<JobSink>,
) -> Result<Job> {
    let steps = plan_move_items(items, dst_dir, &o)?;
    Ok(start(steps, o, "move", sink))
}

/// Deletes files and folders as a job (to the Recycle Bin when the options say so).
pub fn delete_items(items: &[Loc], o: FileOpOptions, sink: Option<JobSink>) -> Result<Job> {
    let steps = plan_delete_items(items)?;
    Ok(start(steps, o, "delete", sink))
}

/// A `Loc` for a path in a file system.
pub fn loc(vfs: &std::sync::Arc<dyn core_vfs::Vfs>, path: VPath) -> Loc {
    Loc {
        vfs: vfs.clone(),
        path,
    }
}
