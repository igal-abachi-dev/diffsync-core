//! Plan builder behaviour (backlog E8-S2, S3; plan.md §8.2, §9.3, §9.5, §9.7, §9.8).

mod common;

use common::*;
use core_base::ErrorKind;
use core_folder::{FolderRules, Status};
use core_sync::{ActionKind, Op, Phase, Preset, Severity, StepKind, build_plan, steps_from_plan};
use core_vfs::{Kind, MemVfs};

/// left/right trees with one item per interesting status.
fn mixed() -> (MemVfs, MemVfs) {
    let (l, r) = (MemVfs::new(), MemVfs::new());
    l.add_file("same.txt", b"abc", T0)
        .add_file("lnew.txt", b"abc", T0 + 100 * S)
        .add_file("rnew.txt", b"abc", T0)
        .add_file("diff.txt", b"abc", T0)
        .add_file("lonly.txt", b"abc", T0);
    r.add_file("same.txt", b"abc", T0)
        .add_file("lnew.txt", b"abcd", T0)
        .add_file("rnew.txt", b"abcd", T0 + 100 * S)
        .add_file("diff.txt", b"abcd", T0)
        .add_file("ronly.txt", b"abc", T0);
    (l, r)
}

use core_base::ticks::TICKS_PER_SECOND as S;

/// A preset, the actions it plans (item and kind) and the items left as prompts.
type Row<'a> = (Preset, &'a [(&'a str, ActionKind)], &'a [&'a str]);

#[test]
fn every_preset_plans_what_the_table_says_on_a_memvfs_tree() {
    use ActionKind::{CopyToLeft as CL, CopyToRight as CR, DeleteLeft as DL, DeleteRight as DR};
    let (l, r) = mixed();
    let c = compare(arc(l), arc(r), FolderRules::default());
    let want: [Row<'_>; 5] = [
        (
            Preset::UpdateRight,
            &[("lnew.txt", CR), ("lonly.txt", CR)],
            &["diff.txt"],
        ),
        (
            Preset::UpdateLeft,
            &[("rnew.txt", CL), ("ronly.txt", CL)],
            &["diff.txt"],
        ),
        (
            Preset::UpdateBoth,
            &[
                ("lnew.txt", CR),
                ("rnew.txt", CL),
                ("lonly.txt", CR),
                ("ronly.txt", CL),
            ],
            &["diff.txt"],
        ),
        (
            Preset::MirrorToRight,
            &[
                ("lnew.txt", CR),
                ("rnew.txt", CR),
                ("diff.txt", CR),
                ("lonly.txt", CR),
                ("ronly.txt", DR),
            ],
            &[],
        ),
        (
            Preset::MirrorToLeft,
            &[
                ("lnew.txt", CL),
                ("rnew.txt", CL),
                ("diff.txt", CL),
                ("lonly.txt", DL),
                ("ronly.txt", CL),
            ],
            &[],
        ),
    ];
    for (preset, actions, prompts) in want {
        let plan = plan_for(&c, preset);
        let got = kinds(&plan, &c);
        let expect: std::collections::BTreeMap<String, ActionKind> = actions
            .iter()
            .map(|(n, k)| ((*n).to_string(), *k))
            .collect();
        assert_eq!(got, expect, "{preset:?}");
        assert_eq!(names(&c, &plan.unresolved_prompts), prompts, "{preset:?}");
    }
}

/// Decision D10: a Different file under an Update preset is a prompt, nothing happens until the
/// user picks a direction.
#[test]
fn prompts_are_data_and_set_op_resolves_them() {
    let (l, r) = mixed();
    let c = compare(arc(l), arc(r), FolderRules::default());
    let mut plan = plan_for(&c, Preset::UpdateBoth);
    assert_eq!(plan.summary.conflicts, 1);
    assert!(
        !kinds(&plan, &c).contains_key("diff.txt"),
        "Leave is preselected"
    );
    let id = find(&c, "diff.txt");
    plan.set_op(&c, id, Op::CopyToLeft).unwrap();
    assert_eq!(kinds(&plan, &c)["diff.txt"], ActionKind::CopyToLeft);
    assert_eq!(plan.summary.conflicts, 0);
    // an operation that needs a side the item is not on is refused
    let lonly = find(&c, "lonly.txt");
    assert_eq!(
        plan.set_op(&c, lonly, Op::DeleteRight).unwrap_err().kind(),
        ErrorKind::InvalidArgument
    );
    // overriding back to Leave removes the action again
    plan.set_op(&c, id, Op::Leave).unwrap();
    assert!(!kinds(&plan, &c).contains_key("diff.txt"));
}

#[test]
fn orphan_folder_is_created_first_then_its_children_and_its_time_is_restored() {
    let (l, r) = (MemVfs::new(), MemVfs::new());
    l.add_file("d/sub/a.txt", b"1", T0)
        .add_file("d/b.txt", b"22", T0);
    l.add_dir("d", T0 + 7 * S);
    r.add_file("keep.txt", b"k", T0);
    l.add_file("keep.txt", b"k", T0);
    let c = compare(arc(l), arc(r), FolderRules::default());
    let plan = plan_for(&c, Preset::UpdateRight);
    let order: Vec<(String, ActionKind)> = plan
        .actions
        .iter()
        .map(|a| (c.tree.path(a.node).display(), a.kind))
        .collect();
    assert_eq!(
        order[0],
        ("d".to_string(), ActionKind::CreateFolderRight),
        "{order:?}"
    );
    assert!(order.contains(&("d/sub".to_string(), ActionKind::CreateFolderRight)));
    let pos = |n: &str| order.iter().position(|(p, _)| p == n).unwrap();
    assert!(
        pos("d") < pos("d/sub") && pos("d/sub") < pos("d/sub/a.txt") && pos("d") < pos("d/b.txt")
    );
    assert_eq!(plan.summary.folders_created, 2);
    assert_eq!(
        (plan.summary.copies_to_right, plan.summary.bytes_to_right),
        (2, 3)
    );
    let steps = steps_from_plan(&plan, &c);
    let times: Vec<_> = steps
        .iter()
        .filter(|s| s.kind == StepKind::SetFolderTime)
        .collect();
    assert_eq!(times.len(), 2);
    assert_eq!(times.last().unwrap().phase, Phase::FolderTimes);
    assert!(
        steps
            .iter()
            .position(|s| s.kind == StepKind::SetFolderTime)
            .unwrap()
            > steps
                .iter()
                .rposition(|s| s.kind == StepKind::CopyFile)
                .unwrap()
    );
}

/// Backlog E8-S3.1: the phases are in order, creates in pre-order, deletes in post-order.
#[test]
fn steps_follow_the_phase_order() {
    let (l, r) = (MemVfs::new(), MemVfs::new());
    l.add_file("new/f.txt", b"1", T0);
    r.add_file("old/deep/g.txt", b"2", T0)
        .add_file("old/h.txt", b"3", T0);
    let c = compare(arc(l), arc(r), FolderRules::default());
    let plan = plan_for(&c, Preset::MirrorToRight);
    let steps = steps_from_plan(&plan, &c);
    let phases: Vec<Phase> = steps.iter().map(|s| s.phase).collect();
    let mut sorted = phases.clone();
    sorted.sort();
    assert_eq!(phases, sorted);
    let deletes: Vec<String> = steps
        .iter()
        .filter(|s| s.kind == StepKind::Delete)
        .map(|s| s.dst.path.display())
        .collect();
    let pos = |p: &str| deletes.iter().position(|d| d == p).unwrap();
    assert!(pos("old/deep/g.txt") < pos("old/deep"));
    assert!(
        pos("old/deep") < pos("old"),
        "children before parents: {deletes:?}"
    );
    assert!(pos("old/h.txt") < pos("old"));
    assert_eq!(steps[0].kind, StepKind::CreateDir);
}

/// §9.3: Mirror with file `x` against folder `x` plans the delete as a name-freeing step that
/// runs before the copy.
#[test]
fn type_conflict_delete_runs_before_the_copy() {
    let (l, r) = (MemVfs::new(), MemVfs::new());
    l.add_file("x", b"file", T0);
    r.add_file("x/inner.txt", b"1", T0);
    let c = compare(arc(l), arc(r), FolderRules::default());
    let plan = plan_for(&c, Preset::MirrorToRight);
    assert_eq!(plan.summary.errors, 0);
    let steps = steps_from_plan(&plan, &c);
    let first_copy = steps
        .iter()
        .position(|s| s.kind == StepKind::CopyFile)
        .unwrap();
    let last_free = steps
        .iter()
        .rposition(|s| s.phase == Phase::FreeNames)
        .unwrap();
    assert!(
        last_free < first_copy,
        "deletes that free the name come first"
    );
    assert!(
        steps.iter().filter(|s| s.phase == Phase::FreeNames).count() == 2,
        "inner file and the folder"
    );
}

/// Under an Update preset the folder is not deleted, so the copy cannot work: a plan error, not
/// a failure halfway.
#[test]
fn type_conflict_without_a_delete_is_a_plan_error() {
    let (l, r) = (MemVfs::new(), MemVfs::new());
    l.add_file("x", b"file", T0);
    r.add_file("x/inner.txt", b"1", T0);
    let c = compare(arc(l), arc(r), FolderRules::default());
    let plan = plan_for(&c, Preset::UpdateRight);
    assert!(
        !kinds(&plan, &c)
            .values()
            .any(|k| *k == ActionKind::CopyToRight)
    );
    assert_eq!(plan.summary.errors, 1);
    assert!(plan.issues[0].message.contains("blocks this copy"));
}

/// §9.5 / D16: links are listed as skipped and never copied or deleted.
#[test]
fn links_are_skipped_rows() {
    let (l, r) = (MemVfs::new(), MemVfs::new());
    l.add_link("lnk", Kind::File).add_file("f", b"1", T0);
    r.add_link("rlnk", Kind::Dir);
    let c = compare(arc(l), arc(r), FolderRules::default());
    let plan = plan_for(&c, Preset::MirrorToRight);
    assert_eq!(names(&c, &plan.skipped_links), ["lnk", "rlnk"]);
    assert_eq!(plan.summary.skipped_links, 2);
    assert_eq!(kinds(&plan, &c).len(), 1, "only the file is copied");
}

/// §9.8: invalid names are plan errors before anything is written; device names a warning;
/// names that collide by case on a case-insensitive target are errors.
#[test]
fn destination_names_are_validated_up_front() {
    let l = MemVfs::new();
    l.add_file("ok.txt", b"1", T0)
        .add_file("a:b", b"1", T0)
        .add_file("q?.txt", b"1", T0)
        .add_file("CON.txt", b"1", T0)
        .add_file("Dup.txt", b"1", T0)
        .add_file("dup.TXT", b"2", T0);
    let r = MemVfs::new().windows_names().case_insensitive();
    let c = compare(arc(l), arc(r), FolderRules::default());
    let plan = plan_for(&c, Preset::UpdateRight);
    let planned = kinds(&plan, &c);
    assert!(planned.contains_key("ok.txt") && planned.contains_key("CON.txt"));
    for bad in ["a:b", "q?.txt", "Dup.txt", "dup.TXT"] {
        assert!(!planned.contains_key(bad), "{bad} must not be planned");
    }
    let errors: Vec<_> = plan
        .issues
        .iter()
        .filter(|i| i.severity == Severity::Error)
        .collect();
    assert_eq!(errors.len(), 4, "{:?}", plan.issues);
    let warnings = plan
        .issues
        .iter()
        .filter(|i| i.severity == Severity::Warning)
        .count();
    assert_eq!(warnings, 1, "CON.txt");
    assert_eq!(plan.summary.errors, 4);
}

/// §9.7: nested roots compare with a warning but planning is refused.
#[test]
fn nested_roots_refuse_planning() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("a/b")).unwrap();
    let local = |p: &std::path::Path| -> std::sync::Arc<dyn core_vfs::Vfs> {
        #[cfg(windows)]
        {
            std::sync::Arc::new(core_vfs::LocalWin32::new(
                core_vfs::VfsRoot::parse_windows(p.to_str().unwrap()).unwrap(),
            ))
        }
        #[cfg(not(windows))]
        {
            std::sync::Arc::new(core_vfs::LocalStd::new(p))
        }
    };
    let c = compare(
        local(&dir.path().join("a")),
        local(&dir.path().join("a/b")),
        FolderRules::default(),
    );
    assert!(c.sync_blocked);
    let e = build_plan(&c, &rules(Preset::MirrorToRight), None).unwrap_err();
    assert_eq!(e.kind(), ErrorKind::InvalidArgument);
    let sc = shared(c);
    // a plan cannot be built, and starting a job on a blocked compare is refused as well
    let ok = {
        let (l, r) = (MemVfs::new(), MemVfs::new());
        l.add_file("f", b"1", T0);
        let c2 = compare(arc(l), arc(r), FolderRules::default());
        plan_for(&c2, Preset::UpdateRight)
    };
    assert!(core_sync::start_sync(&ok, &sc, core_sync::ExecOverrides::default(), None).is_err());
}

/// A selection limits the plan to the chosen nodes; the folders they need are created too.
#[test]
fn selection_limits_the_plan_and_creates_needed_folders() {
    let (l, r) = (MemVfs::new(), MemVfs::new());
    l.add_file("d/a.txt", b"1", T0)
        .add_file("d/b.txt", b"2", T0)
        .add_file("top.txt", b"3", T0);
    let c = compare(arc(l), arc(r), FolderRules::default());
    let sel = [find(&c, "d/a.txt")];
    let plan = build_plan(&c, &rules(Preset::UpdateRight), Some(&sel)).unwrap();
    let k = kinds(&plan, &c);
    assert_eq!(k.len(), 2, "{k:?}");
    assert_eq!(k["d"], ActionKind::CreateFolderRight);
    assert_eq!(k["d/a.txt"], ActionKind::CopyToRight);
}

#[test]
fn describe_lists_the_exact_actions_for_a_dry_run() {
    let (l, r) = mixed();
    let c = compare(arc(l), arc(r), FolderRules::default());
    let plan = plan_for(&c, Preset::UpdateBoth);
    let text = plan.describe(&c);
    assert!(
        text.iter()
            .any(|l| l.starts_with("copy-to-right lnew.txt -> lnew.txt")),
        "{text:?}"
    );
    assert!(text.iter().any(|l| l == "prompt diff.txt (leave)"));
    assert_eq!(status(&c, "same.txt"), Status::Same);
}

/// The summary of the mixed tree under Mirror to Right.
#[test]
fn summary_counts_and_bytes() {
    let (l, r) = mixed();
    let c = compare(arc(l), arc(r), FolderRules::default());
    let s = plan_for(&c, Preset::MirrorToRight).summary;
    assert_eq!((s.copies_to_right, s.bytes_to_right), (4, 12));
    assert_eq!(
        (s.copies_to_left, s.deletes_right, s.deletes_left),
        (0, 1, 0)
    );
    assert_eq!((s.conflicts, s.errors, s.folders_created), (0, 0, 0));
}
