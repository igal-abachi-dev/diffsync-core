//! Folder diff reports (backlog E11.5-S2): golden outputs on `MemVfs`, and a comparison with the
//! real GNU diff and git on folders on disk (skipped for a tool that is not installed).

use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;

use core_base::CancelToken;
use core_base::ticks::TICKS_PER_SECOND as S;
use core_folder::udiff::{
    DiffStyle, Entry, FolderDiffOptions, PatchFormat, Report, diff_folders, render,
};
use core_vfs::{MemVfs, VPath, Vfs};

const T0: i64 = 133_000_000_000_000_000; // some time in 2022; only the header format matters

fn arc(m: MemVfs) -> Arc<dyn Vfs> {
    Arc::new(m)
}

fn run(l: MemVfs, r: MemVfs, o: &FolderDiffOptions) -> core_folder::udiff::FolderDiff {
    diff_folders(
        arc(l),
        &VPath::root(),
        arc(r),
        &VPath::root(),
        o,
        &CancelToken::new(),
    )
    .unwrap()
}

fn trees() -> (MemVfs, MemVfs) {
    let (l, r) = (MemVfs::new(), MemVfs::new());
    l.add_file("a.txt", b"1\n2\n3\n", T0)
        .add_file("same.txt", b"same\n", T0)
        .add_file("sub/x.txt", b"old\n", T0)
        .add_file("only_left.txt", b"left\n", T0)
        .add_file("gone/deep.txt", b"deep\n", T0);
    r.add_file("a.txt", b"1\nTWO\n3\n", T0 + 5 * S)
        .add_file("same.txt", b"same\n", T0 + 9 * S) // the time stamp differs, the content does not
        .add_file("sub/x.txt", b"new\n", T0)
        .add_file("only_right.txt", b"right\n", T0);
    (l, r)
}

#[test]
fn gnu_style_reports_changes_and_orphans_in_walk_order() {
    let (l, r) = trees();
    let o = FolderDiffOptions {
        left_name: "d1".into(),
        right_name: "d2".into(),
        cmdline_flags: "-ru".into(),
        ..FolderDiffOptions::default()
    };
    let d = run(l, r, &o);
    assert_eq!(
        d.same_files, 1,
        "same content with another time stamp is the same"
    );
    let text = render(&d, &o);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0], "diff -ru d1/a.txt d2/a.txt");
    assert!(
        lines[1].starts_with("--- d1/a.txt\t") && lines[1].ends_with(" +0000"),
        "{}",
        lines[1]
    );
    assert!(lines[2].starts_with("+++ d2/a.txt\t"));
    assert_eq!(&lines[3..8], ["@@ -1,3 +1,3 @@", " 1", "-2", "+TWO", " 3"]);
    // gone/ is a folder on the left only, then only_left.txt, only_right.txt, then sub/x.txt
    let rest: Vec<&str> = lines[8..]
        .iter()
        .copied()
        .filter(|l| l.starts_with("Only in") || l.starts_with("diff "))
        .collect();
    assert_eq!(
        rest,
        [
            "Only in d1: gone",
            "Only in d1: only_left.txt",
            "Only in d2: only_right.txt",
            "diff -ru d1/sub/x.txt d2/sub/x.txt"
        ]
    );
    assert_eq!(d.exit_code(), 1);
}

#[test]
fn equal_folders_give_nothing_and_exit_zero() {
    let (l, r) = (MemVfs::new(), MemVfs::new());
    l.add_file("x/y.txt", b"same\n", T0);
    r.add_file("x/y.txt", b"same\n", T0 + S);
    let o = FolderDiffOptions::default();
    let d = run(l, r, &o);
    assert!(d.entries.is_empty());
    assert_eq!(render(&d, &o), "");
    assert_eq!(d.exit_code(), 0);
}

#[test]
fn git_style_makes_orphans_new_and_deleted_files() {
    let (l, r) = trees();
    let o = FolderDiffOptions {
        style: DiffStyle::Git,
        new_file: true,
        left_name: "d1".into(),
        right_name: "d2".into(),
        ..FolderDiffOptions::default()
    };
    let d = run(l, r, &o);
    let text = render(&d, &o);
    assert!(
        text.contains(
            "diff --git a/d1/a.txt b/d2/a.txt\n--- a/d1/a.txt\n+++ b/d2/a.txt\n@@ -1,3 +1,3 @@\n"
        ),
        "{text}"
    );
    assert!(text.contains("diff --git a/d1/only_right.txt b/d2/only_right.txt\nnew file mode 100644\n--- /dev/null\n+++ b/d2/only_right.txt\n@@ -0,0 +1 @@\n+right\n"), "{text}");
    assert!(text.contains("diff --git a/d1/gone/deep.txt b/d2/gone/deep.txt\ndeleted file mode 100644\n--- a/d1/gone/deep.txt\n+++ /dev/null\n@@ -1 +0,0 @@\n-deep\n"), "{text}");
    assert!(!text.contains("Only in"));
}

#[test]
fn the_report_family() {
    let (l, r) = trees();
    let mut o = FolderDiffOptions {
        new_file: true,
        style: DiffStyle::Git,
        ..FolderDiffOptions::default()
    };
    let d = run(l, r, &o);
    o.report = Report::NameStatus;
    assert_eq!(
        render(&d, &o),
        "M\ta.txt\nD\tgone/deep.txt\nD\tonly_left.txt\nA\tonly_right.txt\nM\tsub/x.txt\n"
    );
    o.report = Report::NameOnly;
    assert_eq!(
        render(&d, &o),
        "a.txt\ngone/deep.txt\nonly_left.txt\nonly_right.txt\nsub/x.txt\n"
    );
    o.report = Report::NumStat;
    assert_eq!(
        render(&d, &o),
        "1\t1\ta.txt\n0\t1\tgone/deep.txt\n0\t1\tonly_left.txt\n1\t0\tonly_right.txt\n1\t1\tsub/x.txt\n"
    );
    o.report = Report::ShortStat;
    assert_eq!(
        render(&d, &o),
        " 5 files changed, 3 insertions(+), 4 deletions(-)\n"
    );
    o.report = Report::Stat;
    assert_eq!(
        render(&d, &o),
        " a.txt          | 2 +-\n gone/deep.txt  | 1 -\n only_left.txt  | 1 -\n only_right.txt | 1 +\n sub/x.txt      | 2 +-\n 5 files changed, 3 insertions(+), 4 deletions(-)\n"
    );
}

#[test]
fn brief_and_non_recursive_modes() {
    let (l, r) = trees();
    let mut o = FolderDiffOptions {
        report: Report::Brief,
        left_name: "d1".into(),
        right_name: "d2".into(),
        ..FolderDiffOptions::default()
    };
    let d = run(l.clone(), r.clone(), &o);
    assert_eq!(
        render(&d, &o),
        "Files d1/a.txt and d2/a.txt differ\nOnly in d1: gone\nOnly in d1: only_left.txt\nOnly in d2: only_right.txt\nFiles d1/sub/x.txt and d2/sub/x.txt differ\n"
    );
    // without -r the common folder is not entered
    o.report = Report::Patch;
    o.recursive = false;
    let d = run(l, r, &o);
    let text = render(&d, &o);
    assert!(
        text.contains("Common subdirectories: d1/sub and d2/sub\n"),
        "{text}"
    );
    assert!(!text.contains("sub/x.txt and"), "{text}");
}

#[test]
fn binary_files_and_excluded_names() {
    let (l, r) = (MemVfs::new(), MemVfs::new());
    l.add_file("bin.dat", &[0, 1, 2, 3, 0, 9], T0)
        .add_file("package-lock.json", b"{}\n", T0);
    r.add_file("bin.dat", &[0, 1, 2, 4, 0, 9], T0).add_file(
        "package-lock.json",
        b"{\"a\":1}\n",
        T0,
    );
    let o = FolderDiffOptions {
        left_name: "d1".into(),
        right_name: "d2".into(),
        exclude_files: vec!["*package-lock.json".into()],
        ..FolderDiffOptions::default()
    };
    let d = run(l, r, &o);
    assert_eq!(
        render(&d, &o),
        "Binary files d1/bin.dat and d2/bin.dat differ\n"
    );
    assert!(d.entries.iter().all(|e| !matches!(e, Entry::Error { .. })));
}

#[test]
fn whitespace_options_make_files_equal() {
    let (l, r) = (MemVfs::new(), MemVfs::new());
    l.add_file("a.txt", b"x  y\nz\n", T0);
    r.add_file("a.txt", b"x y\nz\n", T0);
    let strict = FolderDiffOptions::default();
    assert_eq!(run(l.clone(), r.clone(), &strict).exit_code(), 1);
    let lax = FolderDiffOptions {
        text: core_diff::udiff::diff_text_options(
            core_diff::options::AlignmentName::Myers,
            core_diff::options::WhitespaceName::Changes,
            false,
        ),
        ..FolderDiffOptions::default()
    };
    assert_eq!(run(l, r, &lax).exit_code(), 0);
}

// ------------------------------------------------------------------ the real tools

fn has(tool: &str) -> bool {
    Command::new(tool)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn write(root: &Path, rel: &str, data: &[u8]) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, data).unwrap();
}

fn local(p: &Path) -> Arc<dyn Vfs> {
    #[cfg(windows)]
    {
        Arc::new(core_vfs::LocalWin32::new(
            core_vfs::VfsRoot::parse_windows(p.to_str().unwrap()).unwrap(),
        ))
    }
    #[cfg(not(windows))]
    {
        Arc::new(core_vfs::LocalStd::new(p))
    }
}

/// GNU prints local time stamps after a tab on the `---`/`+++` lines; ours are UTC. Everything else
/// has to be identical.
fn strip_times(s: &str) -> String {
    s.lines()
        .map(|l| {
            match (l.starts_with("--- ") || l.starts_with("+++ ") || l.starts_with("*** "))
                .then(|| l.find('\t'))
                .flatten()
            {
                Some(i) => &l[..i],
                None => l,
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn the_reports_equal_gnu_diff_and_git() {
    let (have_diff, have_git) = (has("diff"), has("git"));
    if !have_diff && !have_git {
        eprintln!("skipped: neither diff nor git is installed");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let (l, r) = (dir.path().join("d1"), dir.path().join("d2"));
    write(&l, "a.txt", b"1\n2\n3\n4\n5\n6\n7\n8\n9\n");
    write(&r, "a.txt", b"1\n2\n3\n4\nFIVE\n6\n7\n8\n9\n");
    write(&l, "same.txt", b"same\n");
    write(&r, "same.txt", b"same\n");
    write(&l, "sub/x.txt", b"old\nline\n");
    write(&r, "sub/x.txt", b"new\nline\nextra\n");
    write(&l, "sub/deeper/y.txt", b"y\n");
    write(&r, "sub/deeper/y.txt", b"y2\n");
    write(&l, "only_left.txt", b"left\n");
    write(&r, "only_right.txt", b"right\nright2");
    write(&l, "gone/deep.txt", b"deep\n");
    write(&r, "zeta/new.txt", b"n\n");
    write(&l, "crlf.txt", b"a\r\nb\r\n");
    write(&r, "crlf.txt", b"a\nb\n");
    let (ln, rn) = (
        l.to_str().unwrap().replace('\\', "/"),
        r.to_str().unwrap().replace('\\', "/"),
    );
    let cancel = CancelToken::new();
    let mut o = FolderDiffOptions {
        left_name: ln.clone(),
        right_name: rn.clone(),
        cmdline_flags: "-ru".into(),
        ..FolderDiffOptions::default()
    };
    let go = |o: &FolderDiffOptions| {
        diff_folders(
            local(&l),
            &VPath::root(),
            local(&r),
            &VPath::root(),
            o,
            &cancel,
        )
        .unwrap()
    };

    if have_diff {
        // diff -ru
        let ours = render(&go(&o), &o);
        let out = Command::new("diff")
            .args(["-ru", &ln, &rn])
            .output()
            .unwrap();
        let gnu = String::from_utf8_lossy(&out.stdout).into_owned();
        assert_eq!(
            strip_times(&ours),
            strip_times(&gnu),
            "diff -ru differs\n--- ours ---\n{ours}\n--- gnu ---\n{gnu}"
        );
        assert_eq!(go(&o).exit_code(), out.status.code().unwrap());
        // diff -rq
        let q = FolderDiffOptions {
            report: Report::Brief,
            ..o.clone()
        };
        let ours = render(&go(&q), &q);
        let out = Command::new("diff")
            .args(["-rq", &ln, &rn])
            .output()
            .unwrap();
        assert_eq!(
            ours,
            String::from_utf8_lossy(&out.stdout),
            "diff -rq differs"
        );
        // diff -ruN
        let n = FolderDiffOptions {
            new_file: true,
            cmdline_flags: "-ruN".into(),
            ..o.clone()
        };
        let ours = render(&go(&n), &n);
        let out = Command::new("diff")
            .args(["-ruN", &ln, &rn])
            .output()
            .unwrap();
        let gnu = String::from_utf8_lossy(&out.stdout).into_owned();
        // GNU prints the epoch for the time of an absent file; only the structure is compared
        assert_eq!(
            strip_times(&ours),
            strip_times(&gnu),
            "diff -ruN differs\n--- ours ---\n{ours}\n--- gnu ---\n{gnu}"
        );
        // diff -c and the normal format of a folder
        let c = FolderDiffOptions {
            format: PatchFormat::Context,
            cmdline_flags: "-rc".into(),
            ..o.clone()
        };
        let ours = render(&go(&c), &c);
        let out = Command::new("diff")
            .args(["-rc", &ln, &rn])
            .output()
            .unwrap();
        assert_eq!(
            strip_times(&ours),
            strip_times(&String::from_utf8_lossy(&out.stdout)),
            "diff -rc differs"
        );
    }
    if have_git {
        o.style = DiffStyle::Git;
        o.new_file = true;
        o.cmdline_flags = String::new();
        let git = |args: &[&str]| {
            let out = Command::new("git")
                .args([
                    "-c",
                    "core.autocrlf=false",
                    "diff",
                    "--no-index",
                    "--no-color",
                    "--no-indent-heuristic",
                    "--minimal",
                ])
                .args(args)
                .args([&ln, &rn])
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n")
        };
        // name-status, numstat and stat do not depend on blob ids
        let ns = FolderDiffOptions {
            report: Report::NameStatus,
            ..o.clone()
        };
        let ours = render(&go(&ns), &ns);
        // git prints the paths as given (`d1/x` with the full path of the roots): compare the letters and the tails
        let tail = |s: &str| {
            s.lines()
                .map(|l| {
                    let (a, b) = l.split_once('\t').unwrap();
                    format!(
                        "{a}\t{}",
                        b.rsplit_once("d1/")
                            .or(b.rsplit_once("d2/"))
                            .map_or(b, |(_, t)| t)
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        let gitns = git(&["--name-status"]);
        assert_eq!(
            tail(&ours),
            tail(&gitns),
            "name-status differs\n--- ours ---\n{ours}\n--- git ---\n{gitns}"
        );
        let nu = FolderDiffOptions {
            report: Report::NumStat,
            ..o.clone()
        };
        let (ours, gitnu) = (render(&go(&nu), &nu), git(&["--numstat"]));
        // git writes the paths in its rename notation (`{d1 => d2}/a.txt`) for two roots, so only
        // the counts are compared here; the names are covered by name-status above
        let numtail = |s: &str| {
            s.lines()
                .map(|l| l.split('\t').take(2).collect::<Vec<_>>().join("\t"))
                .collect::<Vec<_>>()
                .join("\n")
        };
        assert_eq!(
            numtail(&ours),
            numtail(&gitnu),
            "numstat differs\n--- ours ---\n{ours}\n--- git ---\n{gitnu}"
        );
    }
}
