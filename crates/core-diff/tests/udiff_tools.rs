//! Differential tests of the patch output against the real tools (backlog E11.5-S1.4): GNU diff,
//! git and `patch`. Random but reproducible file pairs; a tool that is not installed skips its
//! checks (the CI runners have all three; the output says what was skipped).
//!
//! What must hold for *every* pair:
//! * `patch` applies our unified, context and normal output to the left file and gives exactly the
//!   right file (the patch is correct);
//! * our script changes as few lines as `diff --minimal` does (our Myers is minimal).
//!
//! Byte-for-byte equality with GNU diff and git is checked too, but a minimal script is not unique
//! (a changed line can often be matched in more than one place), so it is a rate with a floor.

use std::path::Path;
use std::process::{Command, Stdio};

use core_diff::options::{AlignmentName, WhitespaceName};
use core_diff::udiff::{
    HunkOptions, Labels, counts, diff_text_options, hunks, render_context, render_normal,
    render_unified,
};
use core_diff::{DocSpec, TextCompare};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

const POOL: [&str; 12] = [
    "alpha",
    "beta",
    "gamma",
    "delta",
    "",
    "int main() {",
    "  return 0;",
    "}",
    "foo bar",
    "x = 1;",
    "# comment",
    "end",
];

/// A file of random lines from a small pool (so lines repeat, which is where scripts get
/// ambiguous), with optional CRLF and a missing final newline.
fn file(rng: &mut Rng, lines: usize, crlf: bool, final_newline: bool) -> Vec<u8> {
    let eol = if crlf { "\r\n" } else { "\n" };
    let mut s = String::new();
    for i in 0..lines {
        s.push_str(POOL[rng.below(POOL.len() as u64) as usize]);
        if i + 1 < lines || final_newline {
            s.push_str(eol);
        }
    }
    s.into_bytes()
}

/// The right file: the left one with a few random edits of whole lines.
fn mutate(rng: &mut Rng, left: &[u8]) -> Vec<u8> {
    let text = String::from_utf8_lossy(left).into_owned();
    let mut lines: Vec<String> = text.split_inclusive('\n').map(String::from).collect();
    let edits = 1 + rng.below(4);
    for _ in 0..edits {
        match rng.below(3) {
            0 if !lines.is_empty() => {
                let i = rng.below(lines.len() as u64) as usize;
                lines.remove(i);
            }
            1 => {
                let i = rng.below(lines.len() as u64 + 1) as usize;
                lines.insert(
                    i,
                    format!("{}\n", POOL[rng.below(POOL.len() as u64) as usize]),
                );
            }
            _ if !lines.is_empty() => {
                let i = rng.below(lines.len() as u64) as usize;
                lines[i] = format!("changed {}\n", rng.below(100));
            }
            _ => {}
        }
    }
    lines.concat().into_bytes()
}

fn has(tool: &str, arg: &str) -> bool {
    Command::new(tool)
        .arg(arg)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn run(cmd: &mut Command) -> (i32, Vec<u8>) {
    let out = cmd.output().expect("tool starts");
    (out.status.code().unwrap_or(-1), out.stdout)
}

fn ours(
    left: &[u8],
    right: &[u8],
    alignment: AlignmentName,
    ctx: u32,
    func: bool,
) -> (String, String, String, (u32, u32)) {
    let tc = TextCompare::start(
        DocSpec::Bytes(left.to_vec()),
        DocSpec::Bytes(right.to_vec()),
        diff_text_options(alignment, WhitespaceName::All, false),
        None,
    )
    .unwrap();
    tc.wait();
    let labels = Labels {
        old: "a".into(),
        new: "b".into(),
        ..Labels::default()
    };
    let h = hunks(
        &tc,
        &HunkOptions {
            context: ctx,
            func_header: func,
            ..HunkOptions::default()
        },
    )
    .unwrap();
    let c0 = hunks(
        &tc,
        &HunkOptions {
            context: 0,
            ..HunkOptions::default()
        },
    )
    .unwrap();
    (
        render_unified(&h, &labels),
        render_context(&h, &labels),
        render_normal(&c0),
        counts(&h),
    )
}

/// Applies `patch_text` to `left` with GNU patch and returns the result.
fn apply(dir: &Path, left: &[u8], patch_text: &str, extra: &[&str]) -> Option<Vec<u8>> {
    let (lf, pf, of) = (dir.join("l.txt"), dir.join("p.diff"), dir.join("o.txt"));
    std::fs::write(&lf, left).unwrap();
    std::fs::write(&pf, patch_text.as_bytes()).unwrap();
    let _ = std::fs::remove_file(&of);
    let st = Command::new("patch")
        .args(["--binary", "-s", "-o"])
        .arg(&of)
        .args(extra)
        .arg(&lf)
        .arg(&pf)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()?;
    st.success().then(|| std::fs::read(&of).unwrap())
}

fn minimal_count(gnu_unified: &[u8]) -> (u32, u32) {
    let t = String::from_utf8_lossy(gnu_unified);
    let (mut a, mut d) = (0, 0);
    for l in t.lines() {
        if l.starts_with("+++") || l.starts_with("---") {
            continue;
        }
        if l.starts_with('+') {
            a += 1;
        } else if l.starts_with('-') {
            d += 1;
        }
    }
    (a, d)
}

#[test]
fn our_patches_apply_and_match_gnu_diff_and_git() {
    let (have_diff, have_patch, have_git) = (
        has("diff", "--version"),
        has("patch", "--version"),
        has("git", "--version"),
    );
    if !(have_diff || have_patch || have_git) {
        eprintln!("skipped: none of diff, patch and git is installed");
        return;
    }
    eprintln!("tools: diff={have_diff} patch={have_patch} git={have_git}");
    let dir = tempfile::tempdir().unwrap();
    let mut rng = Rng(0x2545_F491_4F6C_DD1D);
    let (mut cases, mut gnu_same, mut git_same, mut ctx_same, mut normal_same) =
        (0u32, 0u32, 0u32, 0u32, 0u32);
    // `DS_UDIFF_CASES` raises the number of random pairs for a longer run (the default keeps the
    // test about 20 s: every case starts GNU diff three times, patch three times and git once)
    let tries: u32 = std::env::var("DS_UDIFF_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(90);
    for _ in 0..tries {
        let n = rng.below(40) as usize;
        let crlf = rng.below(6) == 0;
        let final_newline = rng.below(5) != 0;
        let left = file(&mut rng, n, crlf, final_newline);
        let mut right = mutate(&mut rng, &left);
        if rng.below(7) == 0 {
            // drop the final newline of the right file
            if right.last() == Some(&b'\n') {
                right.pop();
                if right.last() == Some(&b'\r') {
                    right.pop();
                }
            }
        }
        if left == right {
            continue;
        }
        cases += 1;
        let (lf, rf) = (dir.path().join("a.txt"), dir.path().join("b.txt"));
        std::fs::write(&lf, &left).unwrap();
        std::fs::write(&rf, &right).unwrap();
        let (unified, context, normal, ours_count) =
            ours(&left, &right, AlignmentName::Myers, 3, false);
        let case = format!(
            "case {cases}\n--- left ---\n{:?}\n--- right ---\n{:?}\n--- ours ---\n{unified}",
            String::from_utf8_lossy(&left),
            String::from_utf8_lossy(&right)
        );

        if have_patch {
            assert_eq!(
                apply(dir.path(), &left, &unified, &["-u"]).as_deref(),
                Some(&right[..]),
                "unified patch does not apply: {case}"
            );
            assert_eq!(
                apply(dir.path(), &left, &context, &["-c"]).as_deref(),
                Some(&right[..]),
                "context patch does not apply: {case}"
            );
            assert_eq!(
                apply(dir.path(), &left, &normal, &["-n"]).as_deref(),
                Some(&right[..]),
                "normal patch does not apply: {case}"
            );
        }
        if have_diff {
            let (code, gnu) = run(Command::new("diff")
                .args(["-u", "--minimal", "--label", "a", "--label", "b"])
                .arg(&lf)
                .arg(&rf));
            assert_eq!(code, 1, "{case}");
            assert_eq!(
                ours_count,
                minimal_count(&gnu),
                "not a minimal script (GNU --minimal differs): {case}\n--- gnu ---\n{}",
                String::from_utf8_lossy(&gnu)
            );
            let (_, gnu_default) = run(Command::new("diff")
                .args(["-u", "--label", "a", "--label", "b"])
                .arg(&lf)
                .arg(&rf));
            gnu_same += u32::from(gnu_default == unified.as_bytes());
            let (_, gnu_c) = run(Command::new("diff")
                .args(["-c", "--label", "a", "--label", "b"])
                .arg(&lf)
                .arg(&rf));
            ctx_same += u32::from(gnu_c == context.as_bytes());
            let (_, gnu_n) = run(Command::new("diff").arg(&lf).arg(&rf));
            normal_same += u32::from(gnu_n == normal.as_bytes());
        }
        if have_git {
            let (u, _, _, _) = ours(&left, &right, AlignmentName::Myers, 3, true);
            let (_, git) = run(Command::new("git")
                .args([
                    "-c",
                    "core.autocrlf=false",
                    "diff",
                    "--no-index",
                    "--no-color",
                    "--no-indent-heuristic",
                    "--minimal",
                    "-U3",
                ])
                .arg(&lf)
                .arg(&rf));
            let git = String::from_utf8_lossy(&git).into_owned();
            // git prints its own header lines; the hunks start at the first `@@`
            let body = |s: &str| {
                s.find("\n@@")
                    .map(|i| s[i + 1..].to_string())
                    .unwrap_or_default()
            };
            git_same += u32::from(body(&git) == body(&u));
        }
    }
    assert!(
        cases * 10 >= tries * 8,
        "only {cases} usable cases of {tries}"
    );
    eprintln!(
        "{cases} cases; byte-identical to GNU diff -u: {gnu_same}, -c: {ctx_same}, normal: {normal_same}; to git: {git_same}"
    );
    if have_diff {
        assert!(
            gnu_same * 100 >= cases * 80,
            "only {gnu_same}/{cases} unified outputs equal GNU diff's"
        );
        assert!(
            ctx_same * 100 >= cases * 80,
            "only {ctx_same}/{cases} context outputs equal GNU diff's"
        );
        assert!(
            normal_same * 100 >= cases * 80,
            "only {normal_same}/{cases} normal outputs equal GNU diff's"
        );
    }
    if have_git {
        assert!(
            git_same * 100 >= cases * 75,
            "only {git_same}/{cases} hunks equal git's"
        );
    }
}
