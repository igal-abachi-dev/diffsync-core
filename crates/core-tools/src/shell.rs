//! What the Explorer context-menu entries do (backlog E11.5-S7), as plain functions: when an entry
//! is shown, what its title says, and what a click leads to. The COM handler (`diffsync_shell.dll`)
//! only translates Explorer's calls into these, so the behaviour is the same on every platform and
//! is tested without Explorer.
//!
//! Two entries, like Beyond Compare's:
//!
//! * **Select left side**: shown for exactly one selected file or folder; remembers it
//!   ([`SelectionStore`]).
//! * **Compare with "<left>"**: shown for exactly one selected item when a left side is remembered
//!   and is of the same kind (file with file, folder with folder); starts the compare tool.
//!
//! The compare tool is a command line template (`%L` left, `%R` right), by default
//! `"<folder of the DLL>\DeltaForge.exe" "%L" "%R"`, which the program's own argument parser
//! ([`crate::parse_invocation`]) understands as a compare.

use std::path::{Path, PathBuf};

use crate::selection::SelectionStore;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellCommand {
    SelectLeft,
    CompareWithSelected,
}

impl ShellCommand {
    /// A fixed name of the command (the canonical name Explorer asks for).
    pub fn canonical_name(self) -> &'static str {
        match self {
            ShellCommand::SelectLeft => "DiffSync.SelectLeft",
            ShellCommand::CompareWithSelected => "DiffSync.CompareWithSelected",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellState {
    Enabled,
    /// Not offered (nothing to compare with, several items selected).
    Hidden,
}

fn kind_of(p: &Path) -> Option<bool> {
    std::fs::metadata(p).ok().map(|m| m.is_dir())
}

/// Whether the entry is shown for these selected items.
pub fn state(cmd: ShellCommand, items: &[PathBuf], store: &SelectionStore) -> ShellState {
    let [item] = items else {
        return ShellState::Hidden;
    };
    match cmd {
        ShellCommand::SelectLeft => {
            if kind_of(item).is_some() {
                ShellState::Enabled
            } else {
                ShellState::Hidden
            }
        }
        ShellCommand::CompareWithSelected => match store.left() {
            Some(sel) => {
                let left = Path::new(&sel.left);
                let same_kind = kind_of(left).is_some() && kind_of(left) == kind_of(item);
                if same_kind && left != item.as_path() {
                    ShellState::Enabled
                } else {
                    ShellState::Hidden
                }
            }
            None => ShellState::Hidden,
        },
    }
}

/// The text of the entry. "Compare with" names the remembered side.
pub fn title(cmd: ShellCommand, store: &SelectionStore) -> String {
    match cmd {
        ShellCommand::SelectLeft => "Select left side".to_string(),
        ShellCommand::CompareWithSelected => {
            let name = store
                .left()
                .and_then(|s| {
                    Path::new(&s.left)
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                })
                .unwrap_or_default();
            if name.is_empty() {
                "Compare with selected".to_string()
            } else {
                format!("Compare with \"{name}\"")
            }
        }
    }
}

/// Where the compare tool is, and how to start it.
#[derive(Debug, Clone, Default)]
pub struct LaunchConfig {
    /// The user's command line template (`%L`, `%R`); e.g. from `HKCU\Software\DiffSync\Shell`.
    pub template: Option<String>,
    /// The folder of the shell extension DLL, where `DeltaForge.exe` is expected by default.
    pub module_dir: Option<PathBuf>,
}

/// What a click led to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellAction {
    /// The left side was remembered.
    Remembered(String),
    /// Start this program.
    Launch { program: PathBuf, args: Vec<String> },
    /// Nothing happened; show the message.
    Failed(String),
}

/// Splits a command line the way Windows programs read theirs: spaces separate, double quotes group
/// (`\"` inside quotes is a quote).
pub fn split_command_line(s: &str) -> Vec<String> {
    let (mut out, mut cur, mut in_q, mut had) = (Vec::new(), String::new(), false, false);
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'"') => {
                cur.push('"');
                chars.next();
                had = true;
            }
            '"' => {
                in_q = !in_q;
                had = true;
            }
            c if c.is_whitespace() && !in_q => {
                if had || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                    had = false;
                }
            }
            c => {
                cur.push(c);
                had = true;
            }
        }
    }
    if had || !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// The program and arguments for comparing `left` with `right`.
pub fn launch_command(
    left: &str,
    right: &str,
    cfg: &LaunchConfig,
) -> Result<(PathBuf, Vec<String>), String> {
    let template = match (&cfg.template, &cfg.module_dir) {
        (Some(t), _) if !t.trim().is_empty() => t.clone(),
        (_, Some(dir)) => format!("\"{}\" \"%L\" \"%R\"", dir.join("DeltaForge.exe").display()),
        _ => return Err("no compare tool is configured: set the command template (HKCU\\Software\\DiffSync\\Shell\\Command)".into()),
    };
    // expand after splitting: a path with spaces or a quote in it stays one argument
    let mut parts = split_command_line(&template).into_iter();
    let program = parts.next().ok_or("the command template is empty")?;
    let args: Vec<String> = parts
        .map(|a| {
            if a == "%L" {
                left.to_string()
            } else if a == "%R" {
                right.to_string()
            } else {
                a.replace("%L", left).replace("%R", right)
            }
        })
        .collect();
    let program = PathBuf::from(program);
    if program.is_absolute() && !program.exists() {
        return Err(format!(
            "the compare tool was not found: {} (set HKCU\\Software\\DiffSync\\Shell\\Command)",
            program.display()
        ));
    }
    Ok((program, args))
}

/// What happens when the entry is clicked on `items`.
pub fn invoke(
    cmd: ShellCommand,
    items: &[PathBuf],
    store: &SelectionStore,
    cfg: &LaunchConfig,
) -> ShellAction {
    let [item] = items else {
        return ShellAction::Failed("select exactly one file or folder".into());
    };
    let Some(path) = item.to_str() else {
        return ShellAction::Failed("the path is not valid Unicode".into());
    };
    match cmd {
        ShellCommand::SelectLeft => match store.set_left(path) {
            Ok(()) => ShellAction::Remembered(path.to_string()),
            Err(e) => ShellAction::Failed(format!("cannot remember the selection: {e}")),
        },
        ShellCommand::CompareWithSelected => {
            let Some(sel) = store.left() else {
                return ShellAction::Failed("no left side is selected (or it no longer exists): use \"Select left side\" first".into());
            };
            match (kind_of(Path::new(&sel.left)), kind_of(item)) {
                (Some(a), Some(b)) if a != b => {
                    return ShellAction::Failed("a file cannot be compared with a folder".into());
                }
                (Some(_), Some(_)) => {}
                _ => return ShellAction::Failed("one of the two items no longer exists".into()),
            }
            match launch_command(&sel.left, path, cfg) {
                Ok((program, args)) => ShellAction::Launch { program, args },
                Err(m) => ShellAction::Failed(m),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (tempfile::TempDir, SelectionStore, PathBuf, PathBuf, PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let store = SelectionStore::in_dir(&d.path().join("state"));
        let (f1, f2, dir) = (
            d.path().join("one.txt"),
            d.path().join("two.txt"),
            d.path().join("folder"),
        );
        std::fs::write(&f1, "1").unwrap();
        std::fs::write(&f2, "2").unwrap();
        std::fs::create_dir(&dir).unwrap();
        (d, store, f1, f2, dir)
    }

    #[test]
    fn select_left_is_offered_for_one_existing_item() {
        let (_d, store, f1, f2, _dir) = setup();
        assert_eq!(
            state(ShellCommand::SelectLeft, std::slice::from_ref(&f1), &store),
            ShellState::Enabled
        );
        assert_eq!(
            state(ShellCommand::SelectLeft, &[f1.clone(), f2], &store),
            ShellState::Hidden
        );
        assert_eq!(
            state(ShellCommand::SelectLeft, &[], &store),
            ShellState::Hidden
        );
        assert_eq!(
            state(
                ShellCommand::SelectLeft,
                &[f1.with_file_name("missing")],
                &store
            ),
            ShellState::Hidden
        );
    }

    #[test]
    fn compare_with_is_offered_after_a_selection_of_the_same_kind() {
        let (_d, store, f1, f2, dir) = setup();
        assert_eq!(
            state(
                ShellCommand::CompareWithSelected,
                std::slice::from_ref(&f2),
                &store
            ),
            ShellState::Hidden,
            "nothing selected"
        );
        assert_eq!(
            title(ShellCommand::CompareWithSelected, &store),
            "Compare with selected"
        );
        let cfg = LaunchConfig::default();
        assert_eq!(
            invoke(
                ShellCommand::SelectLeft,
                std::slice::from_ref(&f1),
                &store,
                &cfg
            ),
            ShellAction::Remembered(f1.to_str().unwrap().into())
        );
        assert_eq!(
            state(
                ShellCommand::CompareWithSelected,
                std::slice::from_ref(&f2),
                &store
            ),
            ShellState::Enabled
        );
        assert_eq!(
            title(ShellCommand::CompareWithSelected, &store),
            "Compare with \"one.txt\""
        );
        assert_eq!(
            state(
                ShellCommand::CompareWithSelected,
                std::slice::from_ref(&f1),
                &store
            ),
            ShellState::Hidden,
            "not with itself"
        );
        assert_eq!(
            state(
                ShellCommand::CompareWithSelected,
                std::slice::from_ref(&dir),
                &store
            ),
            ShellState::Hidden,
            "a file with a folder"
        );
        match invoke(
            ShellCommand::CompareWithSelected,
            std::slice::from_ref(&dir),
            &store,
            &cfg,
        ) {
            ShellAction::Failed(m) => {
                assert!(m.contains("file cannot be compared with a folder"), "{m}")
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_click_launches_the_tool_with_both_paths_as_single_arguments() {
        let (d, store, f1, f2, _dir) = setup();
        store.set_left(f1.to_str().unwrap()).unwrap();
        let tool = d.path().join("DeltaForge.exe");
        std::fs::write(&tool, "").unwrap();
        // default: next to the DLL
        let cfg = LaunchConfig {
            template: None,
            module_dir: Some(d.path().to_path_buf()),
        };
        let ShellAction::Launch { program, args } = invoke(
            ShellCommand::CompareWithSelected,
            std::slice::from_ref(&f2),
            &store,
            &cfg,
        ) else {
            panic!()
        };
        assert_eq!(program, tool);
        assert_eq!(args, [f1.to_str().unwrap(), f2.to_str().unwrap()]);
        // a user template with its own switches
        let cfg = LaunchConfig {
            template: Some(format!(
                "\"{}\" /readonly \"%L\" \"%R\" --title2=\"Their file\"",
                tool.display()
            )),
            module_dir: None,
        };
        let ShellAction::Launch { args, .. } = invoke(
            ShellCommand::CompareWithSelected,
            std::slice::from_ref(&f2),
            &store,
            &cfg,
        ) else {
            panic!()
        };
        assert_eq!(
            args,
            [
                "/readonly",
                f1.to_str().unwrap(),
                f2.to_str().unwrap(),
                "--title2=Their file"
            ]
        );
        // the tool is missing
        let cfg = LaunchConfig {
            template: None,
            module_dir: Some(d.path().join("nowhere")),
        };
        match invoke(
            ShellCommand::CompareWithSelected,
            std::slice::from_ref(&f2),
            &store,
            &cfg,
        ) {
            ShellAction::Failed(m) => assert!(m.contains("not found"), "{m}"),
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            invoke(
                ShellCommand::CompareWithSelected,
                std::slice::from_ref(&f2),
                &store,
                &LaunchConfig::default()
            ),
            ShellAction::Failed(_)
        ));
    }

    #[test]
    fn command_lines_split_like_windows_programs_read_them() {
        assert_eq!(
            split_command_line(r#""C:\Program Files\X\x.exe" a "b c" d"#),
            [r"C:\Program Files\X\x.exe", "a", "b c", "d"]
        );
        assert_eq!(split_command_line(r#"x "" y"#), ["x", "", "y"]);
        assert_eq!(split_command_line(r#"say \"hi\""#), ["say", "\"hi\""]);
        assert!(split_command_line("   ").is_empty());
    }
}
