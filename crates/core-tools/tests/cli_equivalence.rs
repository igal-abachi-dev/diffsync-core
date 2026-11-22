//! Whole-path check (backlog E11.5-S6): a `diff` or `git diff` command line goes through
//! `parse_diff_args` and `run`, and the text equals what the real program prints (a program that is
//! not installed skips its cases). Time stamps are the one thing that is not compared: GNU prints
//! local time, the core prints UTC.

use std::path::Path;
use std::process::{Command, Stdio};

use core_tools::{Dialect, RunOptions, parse_diff_args, run};

fn has(tool: &str) -> bool {
    Command::new(tool)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn strip_times(s: &str) -> String {
    s.replace("\r\n", "\n")
        .lines()
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

fn write(dir: &Path, name: &str, data: &[u8]) -> String {
    let p = dir.join(name);
    std::fs::write(&p, data).unwrap();
    p.to_str().unwrap().replace('\\', "/")
}

/// (left, right, flags): pairs where the minimal script is unique, so GNU and the core agree.
fn cases() -> Vec<(&'static str, &'static str, Vec<&'static str>)> {
    let base = "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\n";
    vec![
        (
            base,
            "one\ntwo\nthree\nfour\nFIVE\nsix\nseven\neight\nnine\nten\n",
            vec!["-u"],
        ),
        (
            base,
            "one\ntwo\nthree\nfour\nFIVE\nsix\nseven\neight\nnine\nten\n",
            vec!["-U1"],
        ),
        (
            base,
            "one\ntwo\nthree\nfour\nFIVE\nsix\nseven\neight\nnine\nten\n",
            vec!["-U0"],
        ),
        (
            base,
            "one\ntwo\nthree\nfour\nFIVE\nsix\nseven\neight\nnine\nten\n",
            vec!["-c"],
        ),
        (
            base,
            "one\ntwo\nthree\nfour\nFIVE\nsix\nseven\neight\nnine\nten\n",
            vec![],
        ),
        (
            base,
            "one\nTWO\nthree\nfour\nfive\nsix\nseven\neight\nNINE\nten\n",
            vec!["-u"],
        ),
        (
            base,
            "ONE\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nTEN\n",
            vec!["-u", "-i"],
        ),
        ("a  b\nc\nd\n", "a b\nc\nD\n", vec!["-u", "-b"]),
        ("a  b\nc\nd\n", "ab\nc\nD\n", vec!["-u", "-w"]),
        ("a\nb\n", "a\n\nb\nc\n", vec!["-u", "-B"]),
        ("a\nb", "a\nb\n", vec!["-u"]),
        ("a\r\nb\r\n", "a\nb\n", vec!["-u"]),
        ("a\r\nb\r\n", "a\nB\n", vec!["-u", "--strip-trailing-cr"]),
        ("", "new\nfile\n", vec!["-u"]),
        ("gone\n", "", vec!["-u"]),
    ]
}

fn argv(flags: &[&str], l: &str, r: &str) -> Vec<String> {
    flags
        .iter()
        .map(|s| (*s).to_string())
        .chain([l.to_string(), r.to_string()])
        .collect()
}

#[test]
fn gnu_command_lines_print_what_diff_prints() {
    if !has("diff") {
        eprintln!("skipped: GNU diff is not installed");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    for (i, (left, right, flags)) in cases().into_iter().enumerate() {
        let (l, r) = (
            write(dir.path(), &format!("l{i}.txt"), left.as_bytes()),
            write(dir.path(), &format!("r{i}.txt"), right.as_bytes()),
        );
        let args = argv(&flags, &l, &r);
        let req = parse_diff_args(&args, Dialect::Gnu).unwrap();
        let ours = run(&req, &RunOptions::default());
        let gnu = Command::new("diff").args(&args).output().unwrap();
        let gnu_text = String::from_utf8_lossy(&gnu.stdout).into_owned();
        assert_eq!(
            strip_times(&ours.stdout),
            strip_times(&gnu_text),
            "case {i} {flags:?}\n--- ours ---\n{}\n--- gnu ---\n{gnu_text}",
            ours.stdout
        );
        assert_eq!(
            ours.exit_code,
            gnu.status.code().unwrap(),
            "exit code of case {i} {flags:?}"
        );
    }
}

#[test]
fn git_command_lines_print_what_git_prints() {
    if !has("git") {
        eprintln!("skipped: git is not installed");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    for (i, (left, right, flags)) in cases().into_iter().enumerate() {
        // git has no -c/-w-style GNU flags for every case: use its own spellings
        let gflags: Vec<&str> = match flags.as_slice() {
            ["-u"] => vec![],
            ["-U1"] => vec!["-U1"],
            ["-U0"] => vec!["-U0"],
            ["-u", "-b"] => vec!["-b"],
            ["-u", "-w"] => vec!["-w"],
            ["-u", "-B"] => vec!["--ignore-blank-lines"],
            ["-u", "--strip-trailing-cr"] => vec!["--ignore-cr-at-eol"],
            _ => continue,
        };
        let (l, r) = (
            write(dir.path(), &format!("gl{i}.txt"), left.as_bytes()),
            write(dir.path(), &format!("gr{i}.txt"), right.as_bytes()),
        );
        let mut args = vec!["--no-index".to_string()];
        args.extend(gflags.iter().map(|s| (*s).to_string()));
        args.extend([l.clone(), r.clone()]);
        let req = parse_diff_args(&args, Dialect::Git).unwrap();
        let ours = run(&req, &RunOptions::default());
        let git = Command::new("git")
            .args([
                "-c",
                "core.autocrlf=false",
                "diff",
                "--no-color",
                "--no-indent-heuristic",
                "--minimal",
            ])
            .args(&args)
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&git.stdout).replace("\r\n", "\n");
        // git's `index ...` and mode lines depend on blob ids; the rest has to be equal
        let keep = |s: &str| {
            s.lines()
                .filter(|l| {
                    !l.starts_with("index ")
                        && !l.starts_with("new file mode")
                        && !l.starts_with("deleted file mode")
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        // the empty-file cases print `--- /dev/null` in git: keep the comparison on the hunks and headers
        assert_eq!(
            keep(&ours.stdout),
            keep(&text),
            "git case {i} {flags:?}\n--- ours ---\n{}\n--- git ---\n{text}",
            ours.stdout
        );
    }
}
