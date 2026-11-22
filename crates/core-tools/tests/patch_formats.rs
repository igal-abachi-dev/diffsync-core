//! The patch formats against the real tools (backlog E11.5-S16, decisions D58–D61):
//!
//! * what the core writes (unified, SEARCH/REPLACE, mailbox) applies back to the old text and gives
//!   the new text, for random file pairs;
//! * a mailbox written by the core is accepted by `git am`, and one written by `git format-patch`
//!   is read and applied by the core to the same result;
//! * the core places hunks like GNU `patch` does when the file moved or changed around them.
//!
//! The tests that need `git` or `patch` skip themselves (and say so) when the tool is missing.

use std::path::Path;
use std::process::{Command, Stdio};

use core_tools::apply::{ApplyRequest, apply};
use core_tools::{Dialect, RunOptions, parse_diff_args, run_texts};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

const POOL: &[&str] = &[
    "alpha",
    "beta",
    "gamma",
    "delta",
    "",
    "    indented",
    "}",
    "{",
    "return x;",
    "let y = 1;",
    "// note",
    "fn main() {",
];

/// `min..min+spread` lines; the last one ends in a newline unless a 1-in-`no_nl` chance hits
/// (`no_nl == 0`: always).
fn text(rng: &mut Rng, min: usize, spread: usize, no_nl: usize) -> String {
    let lines = min + rng.below(spread.max(1));
    let final_newline = no_nl == 0 || rng.below(no_nl) != 0;
    let mut v: Vec<String> = (0..lines)
        .map(|i| format!("{} {}", POOL[rng.below(POOL.len())], i % 7))
        .collect();
    // some lines repeat on purpose: that is where blocks and hunks must be exact
    if v.len() > 6 {
        let (a, b) = (rng.below(v.len()), rng.below(v.len()));
        let src = v[b].clone();
        v[a].clone_from(&src);
    }
    let mut s = v.join("\n");
    if final_newline && !s.is_empty() {
        s.push('\n');
    }
    s
}

fn mutate(rng: &mut Rng, old: &str) -> String {
    let mut lines: Vec<String> = old.lines().map(str::to_string).collect();
    for _ in 0..=rng.below(4) {
        match rng.below(3) {
            0 if !lines.is_empty() => {
                lines.remove(rng.below(lines.len()));
            }
            1 => {
                let at = rng.below(lines.len() + 1);
                lines.insert(at, format!("new {}", rng.below(5)));
            }
            _ if !lines.is_empty() => {
                let at = rng.below(lines.len());
                lines[at] = format!("{} changed", lines[at]);
            }
            _ => {}
        }
    }
    let mut s = lines.join("\n");
    if !s.is_empty() && old.ends_with('\n') {
        s.push('\n');
    }
    s
}

fn args(a: &[&str], d: Dialect) -> core_tools::DiffRequest {
    parse_diff_args(&a.iter().map(|s| (*s).to_string()).collect::<Vec<_>>(), d).unwrap()
}

fn apply_text(patch: &str, base: &str, fmt: &str) -> Result<String, String> {
    let rep = apply(&ApplyRequest {
        patch: patch.into(),
        format: fmt.into(),
        text: Some(base.into()),
        ..ApplyRequest::default()
    });
    match rep.files.first() {
        Some(f) if rep.ok => Ok(f.text.clone().unwrap_or_default()),
        Some(f) => Err(f.message.clone()),
        None => Err(rep.error.unwrap_or_default()),
    }
}

fn have(tool: &str, arg: &str) -> bool {
    let ok = Command::new(tool)
        .arg(arg)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !ok {
        eprintln!("skipped: `{tool}` is not available");
    }
    ok
}

#[test]
fn unified_search_replace_and_mailbox_round_trip_for_random_pairs() {
    let mut rng = Rng(0x00C0_FFEE_1234_5678);
    let ro = RunOptions::default();
    for case in 0..400 {
        let old = text(&mut rng, 0, 40, 4);
        let new = if rng.below(10) == 0 {
            text(&mut rng, 0, 30, 2)
        } else {
            mutate(&mut rng, &old)
        };
        if old == new {
            continue;
        }
        let ctx = ["-U0", "-U1", "-U3"][rng.below(3)];
        for (flags, fmt) in [
            (vec![ctx.to_string()], "unified"),
            (
                vec![ctx.to_string(), "--search-replace".into()],
                "searchReplace",
            ),
            (vec!["--format-patch".to_string()], "unified"),
        ] {
            let mut a: Vec<&str> = flags.iter().map(String::as_str).collect();
            a.extend(["a.txt", "b.txt"]);
            let out = run_texts(&args(&a, Dialect::Gnu), &old, &new, &ro);
            assert_eq!(out.exit_code, 1, "case {case}");
            let got = apply_text(&out.stdout, &old, fmt).unwrap_or_else(|e| {
                panic!("case {case} {flags:?}: {e}\n{}\nold={old:?}", out.stdout)
            });
            // SEARCH/REPLACE has no way to say "no newline at the end of the file"
            let same = if fmt == "searchReplace" {
                got.trim_end_matches('\n') == new.trim_end_matches('\n')
            } else {
                got == new
            };
            assert!(
                same,
                "case {case} {flags:?}\n{}\ngot={got:?}\nnew={new:?}",
                out.stdout
            );
            // and backwards, for the formats that can be reversed
            if fmt == "unified" {
                let rep = apply(&ApplyRequest {
                    patch: out.stdout.clone(),
                    text: Some(new.clone()),
                    reverse: true,
                    ..ApplyRequest::default()
                });
                assert_eq!(
                    rep.files[0].text.as_deref(),
                    Some(old.as_str()),
                    "case {case} reverse"
                );
            }
        }
    }
}

#[test]
fn a_mailbox_of_the_core_is_accepted_by_git_am() {
    if !have("git", "--version") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let git = |args: &[&str]| {
        let o = Command::new("git")
            .current_dir(&repo)
            .args(["-c", "user.name=T", "-c", "user.email=t@example.com"])
            .args(["-c", "core.autocrlf=false", "-c", "commit.gpgsign=false"])
            .args(args)
            .output()
            .unwrap();
        assert!(
            o.status.success(),
            "git {args:?}: {}{}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        );
    };
    git(&["init", "-q", "."]);
    let mut rng = Rng(0x1357_9BDF_2468_ACE0);
    let ro = RunOptions::default();
    for case in 0..12 {
        let old = text(&mut rng, 5, 30, 0);
        let new = mutate(&mut rng, &old);
        if old == new {
            continue;
        }
        std::fs::write(repo.join("f.txt"), &old).unwrap();
        git(&["add", "f.txt"]);
        git(&["commit", "-q", "--allow-empty", "-m", "base"]);
        let mut r = args(
            &[
                "--format-patch",
                "--subject=Change é",
                "--message=Why.",
                "f.txt",
                "f.txt",
            ],
            Dialect::Gnu,
        );
        r.left = "f.txt".into();
        r.right = "f.txt".into();
        let out = run_texts(&r, &old, &new, &ro);
        let mail = repo.join("p.mbox");
        std::fs::write(&mail, &out.stdout).unwrap();
        git(&["am", "-q", "p.mbox"]);
        assert_eq!(
            std::fs::read_to_string(repo.join("f.txt")).unwrap(),
            new,
            "case {case}\n{}",
            out.stdout
        );
        std::fs::remove_file(&mail).unwrap();
    }
}

#[test]
fn a_mailbox_of_git_is_read_and_applied_to_the_same_result() {
    if !have("git", "--version") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    let git = |args: &[&str]| -> String {
        let o = Command::new("git")
            .current_dir(repo)
            .args([
                "-c",
                "user.name=Tést Ünicode",
                "-c",
                "user.email=t@example.com",
            ])
            .args(["-c", "core.autocrlf=false", "-c", "commit.gpgsign=false"])
            .args(args)
            .output()
            .unwrap();
        assert!(
            o.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&o.stderr)
        );
        String::from_utf8_lossy(&o.stdout).into_owned()
    };
    git(&["init", "-q", "."]);
    let mut rng = Rng(0x0F0F_1234_ABCD_9876);
    let mut cur = text(&mut rng, 30, 1, 0);
    std::fs::write(repo.join("f.txt"), &cur).unwrap();
    git(&["add", "."]);
    git(&["commit", "-q", "-m", "base"]);
    for case in 0..10 {
        let next = mutate(&mut rng, &cur);
        if next == cur {
            continue;
        }
        std::fs::write(repo.join("f.txt"), &next).unwrap();
        git(&["commit", "-q", "-am", "Zmiana wielu rzeczy ąę"]);
        let mail = git(&["format-patch", "-1", "--stdout"]);
        let got = apply_text(&mail, &cur, "unified")
            .unwrap_or_else(|e| panic!("case {case}: {e}\n{mail}"));
        assert_eq!(got, next, "case {case}\n{mail}");
        let rep = apply(&ApplyRequest {
            patch: mail.clone(),
            text: Some(cur.clone()),
            ..ApplyRequest::default()
        });
        let m = rep.mail.expect("a mail header");
        assert_eq!(m.subject, "Zmiana wielu rzeczy ąę");
        assert!(
            m.from.starts_with("Tést Ünicode <t@example.com>"),
            "{}",
            m.from
        );
        cur = next;
    }
}

#[test]
fn hunks_are_placed_like_gnu_patch_places_them() {
    if !have("patch", "--version") || !have("diff", "--version") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let mut rng = Rng(0xDEAD_BEEF_0BAD_F00D);
    let mut compared = 0;
    for case in 0..150 {
        let base = text(&mut rng, 20, 30, 0);
        let edited = mutate(&mut rng, &base);
        if base == edited {
            continue;
        }
        // the target the patch is applied to has drifted: lines added above and below
        let target = format!("top {case}\nlines\n{base}bottom\n");
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        std::fs::write(&a, &base).unwrap();
        std::fs::write(&b, &edited).unwrap();
        let patch = Command::new("diff")
            .arg("-u")
            .arg(&a)
            .arg(&b)
            .output()
            .unwrap()
            .stdout;
        let patch = String::from_utf8_lossy(&patch).into_owned();
        let t = dir.path().join("t");
        std::fs::write(&t, &target).unwrap();
        let st = Command::new("patch")
            .args(["-s", "--no-backup-if-mismatch", "-F2"])
            .arg(&t)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .and_then(|mut c| {
                use std::io::Write as _;
                c.stdin.take().unwrap().write_all(patch.as_bytes())?;
                c.wait()
            })
            .unwrap();
        let theirs = std::fs::read_to_string(&t).unwrap();
        let ours = apply_text(&patch, &target, "unified");
        if st.success() {
            assert_eq!(ours.as_deref(), Ok(theirs.as_str()), "case {case}\n{patch}");
            compared += 1;
        } else {
            // patch rejected a hunk: so must we, and neither may write a wrong result
            assert!(
                ours.is_err(),
                "case {case}: patch refused, we applied\n{patch}"
            );
        }
    }
    assert!(compared > 100, "only {compared} cases compared");
}

#[test]
#[allow(clippy::items_after_statements)]
fn json_patch_round_trips_random_documents() {
    use serde_json::{Value, json};
    let mut rng = Rng(0x7777_1111_2222_3333);
    fn value(rng: &mut Rng, depth: u32) -> Value {
        match rng.below(if depth > 2 { 4 } else { 7 }) {
            0 => json!(rng.below(5)),
            1 => json!(format!("s{}", rng.below(4))),
            2 => json!(rng.below(2) == 0),
            3 => Value::Null,
            4 | 5 => {
                let n = rng.below(5);
                Value::Array((0..n).map(|_| value(rng, depth + 1)).collect())
            }
            _ => {
                let mut m = serde_json::Map::new();
                for _ in 0..rng.below(5) {
                    m.insert(
                        format!("k{}/~{}", rng.below(5), rng.below(2)),
                        value(rng, depth + 1),
                    );
                }
                Value::Object(m)
            }
        }
    }
    for case in 0..500 {
        let (a, b) = (value(&mut rng, 0), value(&mut rng, 0));
        let ops = core_tools::jsonpatch::diff(&a, &b);
        let got = core_tools::jsonpatch::apply(&a, &ops)
            .unwrap_or_else(|e| panic!("case {case}: {e}\n{a}\n{b}\n{ops:?}"));
        assert_eq!(got, b, "case {case}");
    }
}

#[test]
fn a_patch_written_for_crlf_files_applies_to_them_and_changes_nothing_else() {
    let old = "one\r\ntwo\r\nthree\r\n";
    let new = "one\r\n2\r\nthree\r\n";
    let out = run_texts(
        &args(&["-u", "a", "b"], Dialect::Gnu),
        old,
        new,
        &RunOptions::default(),
    );
    assert_eq!(apply_text(&out.stdout, old, "unified").unwrap(), new);
    // an LF patch (made on another machine) onto the CRLF file keeps the file's endings
    let lf = run_texts(
        &args(&["-u", "a", "b"], Dialect::Gnu),
        "one\ntwo\nthree\n",
        "one\n2\nthree\n",
        &RunOptions::default(),
    );
    assert_eq!(apply_text(&lf.stdout, old, "unified").unwrap(), new);
    let _ = Path::new("");
}
