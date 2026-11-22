//! The command line of `diff` and `git diff`, parsed into one request (backlog E11.5-S6.1).
//!
//! The program that wraps the core (a CLI, an MCP server) passes its arguments here and gets a
//! [`DiffRequest`] it can hand to [`crate::run::run`]; nothing about the flags lives in the wrapper.
//! Unknown or unsupported flags are errors with the wording GNU diff uses, not guesses, because a
//! wrong guess would give a model a wrong diff.
//!
//! Supported, GNU diffutils: `-u -U N --unified[=N] -c -C N --context[=N]` (the default is the normal
//! format), `-r -N -q -s -i -w -b -Z -B -p -d -x PAT -X FILE -L LABEL`, `--strip-trailing-cr`,
//! `--exclude`, `--label`, `--minimal`, `--patience`, `--histogram`, `--brief`, `--recursive`,
//! `--new-file`, `--ignore-*`, `--show-c-function`, `--report-identical-files`; short flags may be
//! combined (`-ruN`) and take their value attached or separate (`-U3`, `-U 3`).
//! Git: `--no-index --stat --numstat --shortstat --name-only --name-status -U<n> --unified=<n>
//! --diff-algorithm=<a> --ignore-cr-at-eol --exit-code --quiet --no-prefix --src-prefix --dst-prefix
//! --no-color --color`.

use core_diff::options::{AlignmentName, EolName, WhitespaceName};
use core_folder::udiff::{PatchFormat, Report};

/// Which program's flags and output conventions to follow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Dialect {
    /// `diff` (GNU diffutils).
    #[default]
    Gnu,
    /// `git diff` (also `git diff --no-index`).
    Git,
}

/// A fully parsed diff command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffRequest {
    pub dialect: Dialect,
    pub left: String,
    pub right: String,
    pub format: PatchFormat,
    pub report: Report,
    pub context: u32,
    pub recursive: bool,
    pub new_file: bool,
    /// `-s`: say so when the files are the same.
    pub report_identical: bool,
    pub ignore_case: bool,
    pub whitespace: WhitespaceName,
    pub ignore_blank_lines: bool,
    pub eol: EolName,
    pub alignment: AlignmentName,
    pub func_header: bool,
    pub exclude: Vec<String>,
    /// `-L`: the labels that replace the file names in the headers.
    pub labels: Vec<String>,
    /// The flags as GNU prints them in `diff -ru a b` lines.
    pub cmdline_flags: String,
    /// git's `--exit-code` / `--quiet`: only then does the exit status say "different".
    pub exit_code_flag: bool,
    /// git's `--quiet`: no output at all.
    pub quiet: bool,
    /// `--no-prefix` / `--src-prefix` / `--dst-prefix` (git).
    pub src_prefix: String,
    pub dst_prefix: String,
}

impl DiffRequest {
    pub fn new(left: &str, right: &str) -> DiffRequest {
        DiffRequest {
            dialect: Dialect::Gnu,
            left: left.to_string(),
            right: right.to_string(),
            format: PatchFormat::Unified,
            report: Report::Patch,
            context: 3,
            recursive: false,
            new_file: false,
            report_identical: false,
            ignore_case: false,
            whitespace: WhitespaceName::All,
            ignore_blank_lines: false,
            eol: EolName::Strict,
            alignment: AlignmentName::Myers,
            func_header: false,
            exclude: Vec::new(),
            labels: Vec::new(),
            cmdline_flags: String::new(),
            exit_code_flag: false,
            quiet: false,
            src_prefix: "a/".into(),
            dst_prefix: "b/".into(),
        }
    }
}

/// A command line that cannot be run. The message is GNU's wording where there is one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArgError(pub String);

impl std::fmt::Display for ArgError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ArgError {}

fn err<T>(m: impl Into<String>) -> Result<T, ArgError> {
    Err(ArgError(m.into()))
}

fn number(v: &str, flag: &str) -> Result<u32, ArgError> {
    v.parse::<u32>()
        .or_else(|_| err(format!("invalid context length '{v}' for {flag}")))
}

/// Parses the arguments after the program name (and after `diff` for `git diff`).
///
/// `dialect` picks the conventions: `Git` makes the output git-shaped (`diff --git`, new and deleted
/// files, `a/` and `b/` prefixes), unified by default, always recursive over folders.
pub fn parse_diff_args(args: &[String], dialect: Dialect) -> Result<DiffRequest, ArgError> {
    let mut req = DiffRequest::new("", "");
    req.dialect = dialect;
    if dialect == Dialect::Git {
        // git: unified, recursive, orphans are new/deleted files
        req.recursive = true;
        req.new_file = true;
    }
    let mut unified_default = dialect == Dialect::Git;
    let mut operands: Vec<String> = Vec::new();
    let mut flags_seen = String::new();
    let mut it = args.iter().peekable();
    let mut only_operands = false;
    while let Some(a) = it.next() {
        if only_operands || a == "-" || !a.starts_with('-') {
            operands.push(a.clone());
            continue;
        }
        if a == "--" {
            only_operands = true;
            continue;
        }
        if let Some(long) = a.strip_prefix("--") {
            let (name, value) = match long.split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (long, None),
            };
            let take = |it: &mut std::iter::Peekable<std::slice::Iter<'_, String>>| -> Result<String, ArgError> {
                match value.clone() {
                    Some(v) => Ok(v),
                    None => it.next().cloned().map_or_else(|| err(format!("option '--{name}' requires an argument")), Ok),
                }
            };
            match name {
                "unified" => {
                    req.format = PatchFormat::Unified;
                    unified_default = true;
                    if let Some(v) = &value {
                        req.context = number(v, "--unified")?;
                    }
                    flags_seen.push('u');
                }
                "context" => {
                    req.format = PatchFormat::Context;
                    unified_default = false;
                    if let Some(v) = &value {
                        req.context = number(v, "--context")?;
                    }
                    flags_seen.push('c');
                }
                "normal" => {
                    req.format = PatchFormat::Normal;
                    unified_default = false;
                }
                "recursive" => {
                    req.recursive = true;
                    flags_seen.push('r');
                }
                "new-file" => {
                    req.new_file = true;
                    flags_seen.push('N');
                }
                "brief" => {
                    req.report = Report::Brief;
                    flags_seen.push('q');
                }
                "quiet" => {
                    req.quiet = true;
                    req.exit_code_flag = true;
                }
                "report-identical-files" => req.report_identical = true,
                "ignore-case" => req.ignore_case = true,
                "ignore-all-space" => req.whitespace = WhitespaceName::IgnoreAll,
                "ignore-space-change" => req.whitespace = WhitespaceName::Changes,
                "ignore-trailing-space" => req.whitespace = WhitespaceName::LeadingTrailing,
                "ignore-blank-lines" => req.ignore_blank_lines = true,
                "strip-trailing-cr" | "ignore-cr-at-eol" => req.eol = EolName::CrLfAsLf,
                "show-c-function" => req.func_header = true,
                "minimal" => req.alignment = AlignmentName::Myers,
                "patience" => req.alignment = AlignmentName::Patience,
                "histogram" => req.alignment = AlignmentName::Histogram,
                "diff-algorithm" => {
                    req.alignment = match take(&mut it)?.as_str() {
                        "myers" | "default" | "minimal" => AlignmentName::Myers,
                        "patience" => AlignmentName::Patience,
                        "histogram" => AlignmentName::Histogram,
                        o => return err(format!("option diff-algorithm: unknown value '{o}'")),
                    };
                }
                "exclude" => req.exclude.push(take(&mut it)?),
                "label" => req.labels.push(take(&mut it)?),
                "no-index" => {
                    req.dialect = Dialect::Git;
                    req.recursive = true;
                    req.new_file = true;
                    unified_default = true;
                }
                "stat" => req.report = Report::Stat,
                "numstat" => req.report = Report::NumStat,
                "shortstat" => req.report = Report::ShortStat,
                "name-only" => req.report = Report::NameOnly,
                "name-status" => req.report = Report::NameStatus,
                "patch" => req.report = Report::Patch,
                "exit-code" => req.exit_code_flag = true,
                "no-prefix" => {
                    req.src_prefix.clear();
                    req.dst_prefix.clear();
                }
                "src-prefix" => req.src_prefix = take(&mut it)?,
                "dst-prefix" => req.dst_prefix = take(&mut it)?,
                // accepted and without effect: this output is never colored or paged
                "function-context"
                | "no-color"
                | "color"
                | "no-pager"
                | "no-ext-diff"
                | "no-textconv"
                | "text"
                | "no-dereference"
                | "speed-large-files"
                | "no-renames"
                | "full-index"
                | "binary"
                | "indent-heuristic"
                | "no-indent-heuristic"
                | "find-copies-harder"
                | "irreversible-delete" => {}
                "help" | "version" => {
                    return err(format!(
                        "--{name}: not handled by the core; the program prints it"
                    ));
                }
                o => return err(format!("unrecognized option '--{o}'")),
            }
            continue;
        }
        // short flags, possibly combined; a flag with a value takes the rest or the next argument
        let chars: Vec<char> = a[1..].chars().collect();
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            let rest: String = chars[i + 1..].iter().collect();
            let value = |it: &mut std::iter::Peekable<std::slice::Iter<'_, String>>| -> Result<String, ArgError> {
                if rest.is_empty() {
                    it.next().cloned().map_or_else(|| err(format!("option requires an argument -- '{c}'")), Ok)
                } else {
                    Ok(rest.clone())
                }
            };
            match c {
                'u' => {
                    req.format = PatchFormat::Unified;
                    unified_default = true;
                    flags_seen.push('u');
                }
                'U' => {
                    let v = value(&mut it)?;
                    req.context = number(&v, "-U")?;
                    req.format = PatchFormat::Unified;
                    unified_default = true;
                    flags_seen.push_str(&format!("U{v}"));
                    break;
                }
                'c' => {
                    req.format = PatchFormat::Context;
                    unified_default = false;
                    flags_seen.push('c');
                }
                'C' => {
                    let v = value(&mut it)?;
                    req.context = number(&v, "-C")?;
                    req.format = PatchFormat::Context;
                    unified_default = false;
                    flags_seen.push_str(&format!("C{v}"));
                    break;
                }
                'r' => {
                    req.recursive = true;
                    flags_seen.push('r');
                }
                'N' => {
                    req.new_file = true;
                    flags_seen.push('N');
                }
                'q' => {
                    req.report = Report::Brief;
                    flags_seen.push('q');
                }
                's' => req.report_identical = true,
                'i' => req.ignore_case = true,
                'w' => req.whitespace = WhitespaceName::IgnoreAll,
                'b' => req.whitespace = WhitespaceName::Changes,
                'Z' => req.whitespace = WhitespaceName::LeadingTrailing,
                'B' => req.ignore_blank_lines = true,
                'p' => req.func_header = true,
                'd' => req.alignment = AlignmentName::Myers,
                'a' | 'T' | 'M' | 'W' => {}
                'x' => {
                    req.exclude.push(value(&mut it)?);
                    break;
                }
                'L' => {
                    req.labels.push(value(&mut it)?);
                    break;
                }
                'X' | 'F' | 'I' | 'D' | 'y' | 'e' | 'f' | 'n' | 'E' | 't' | 'l' | 'v' | 'h' => {
                    return err(format!("option '-{c}' is not supported by the core"));
                }
                o => return err(format!("invalid option -- '{o}'")),
            }
            i += 1;
        }
    }
    if operands.len() != 2 {
        return err(match operands.len() {
            0 | 1 => "missing operand after the first file name: two files or folders are needed"
                .to_string(),
            n => format!("extra operand '{}'", operands[n.min(3) - 1]),
        });
    }
    if req.labels.len() > 2 {
        return err("extra operand: at most two --label options");
    }
    if dialect == Dialect::Gnu
        && req.dialect == Dialect::Gnu
        && req.format == PatchFormat::Unified
        && !unified_default
    {
        // GNU's default is the normal format; `-u`/`-U` turned unified on, so this is the default case
        req.format = PatchFormat::Normal;
    }
    if req.format == PatchFormat::Normal {
        // the normal format has no context lines
        req.context = 0;
    }
    req.left.clone_from(&operands[0]);
    req.right.clone_from(&operands[1]);
    req.cmdline_flags = if flags_seen.is_empty() {
        String::new()
    } else {
        format!("-{flags_seen}")
    };
    Ok(req)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(a: &[&str], d: Dialect) -> Result<DiffRequest, ArgError> {
        parse_diff_args(&a.iter().map(|s| (*s).to_string()).collect::<Vec<_>>(), d)
    }

    #[test]
    fn gnu_defaults_to_the_normal_format() {
        let r = p(&["a", "b"], Dialect::Gnu).unwrap();
        assert_eq!(
            (r.format, r.context, r.recursive),
            (PatchFormat::Normal, 0, false)
        );
        assert_eq!((r.left.as_str(), r.right.as_str()), ("a", "b"));
    }

    #[test]
    fn combined_short_flags_and_values() {
        let r = p(&["-ruN", "d1", "d2"], Dialect::Gnu).unwrap();
        assert_eq!(
            (r.format, r.recursive, r.new_file, r.cmdline_flags.as_str()),
            (PatchFormat::Unified, true, true, "-ruN")
        );
        let r = p(&["-U5", "a", "b"], Dialect::Gnu).unwrap();
        assert_eq!((r.context, r.format), (5, PatchFormat::Unified));
        let r = p(&["-U", "7", "a", "b"], Dialect::Gnu).unwrap();
        assert_eq!(r.context, 7);
        let r = p(&["--unified=2", "a", "b"], Dialect::Gnu).unwrap();
        assert_eq!(r.context, 2);
        let r = p(
            &[
                "-x",
                "*.o",
                "-x*.obj",
                "--exclude=build",
                "-L",
                "old",
                "--label=new",
                "a",
                "b",
            ],
            Dialect::Gnu,
        )
        .unwrap();
        assert_eq!(r.exclude, ["*.o", "*.obj", "build"]);
        assert_eq!(r.labels, ["old", "new"]);
    }

    #[test]
    fn whitespace_case_and_algorithm_flags() {
        let r = p(
            &[
                "-u",
                "-w",
                "-i",
                "-B",
                "--patience",
                "--strip-trailing-cr",
                "a",
                "b",
            ],
            Dialect::Gnu,
        )
        .unwrap();
        assert_eq!(r.whitespace, WhitespaceName::IgnoreAll);
        assert!(r.ignore_case && r.ignore_blank_lines);
        assert_eq!(
            (r.alignment, r.eol),
            (AlignmentName::Patience, EolName::CrLfAsLf)
        );
        let r = p(
            &["-b", "--diff-algorithm=histogram", "x", "y"],
            Dialect::Git,
        )
        .unwrap();
        assert_eq!(
            (r.whitespace, r.alignment),
            (WhitespaceName::Changes, AlignmentName::Histogram)
        );
    }

    #[test]
    fn git_is_unified_recursive_and_makes_orphans_new_files() {
        let r = p(
            &["--no-index", "--stat", "folder1/", "folder2/"],
            Dialect::Gnu,
        )
        .unwrap();
        assert_eq!(
            (r.dialect, r.format, r.report, r.recursive, r.new_file),
            (Dialect::Git, PatchFormat::Unified, Report::Stat, true, true)
        );
        let r = p(&["-U1", "--name-status", "a", "b"], Dialect::Git).unwrap();
        assert_eq!((r.context, r.report), (1, Report::NameStatus));
        let r = p(&["--no-prefix", "a", "b"], Dialect::Git).unwrap();
        assert!(r.src_prefix.is_empty() && r.dst_prefix.is_empty());
    }

    #[test]
    fn bad_command_lines_are_errors_with_gnu_wording() {
        assert!(
            p(&["-u", "a"], Dialect::Gnu)
                .unwrap_err()
                .0
                .starts_with("missing operand")
        );
        assert!(
            p(&["a", "b", "c"], Dialect::Gnu)
                .unwrap_err()
                .0
                .starts_with("extra operand 'c'")
        );
        assert_eq!(
            p(&["--bogus", "a", "b"], Dialect::Gnu).unwrap_err().0,
            "unrecognized option '--bogus'"
        );
        assert_eq!(
            p(&["-k", "a", "b"], Dialect::Gnu).unwrap_err().0,
            "invalid option -- 'k'"
        );
        assert_eq!(
            p(&["-U", "x", "a", "b"], Dialect::Gnu).unwrap_err().0,
            "invalid context length 'x' for -U"
        );
        assert!(
            p(&["-y", "a", "b"], Dialect::Gnu)
                .unwrap_err()
                .0
                .contains("not supported")
        );
        // `--` ends the flags: a file may be called `-x`
        assert_eq!(p(&["--", "-x", "b"], Dialect::Gnu).unwrap().left, "-x");
        // `-` is the standard input, an operand
        assert_eq!(p(&["-", "b"], Dialect::Gnu).unwrap().left, "-");
    }
}
