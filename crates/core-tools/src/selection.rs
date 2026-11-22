//! The "left side" chosen in Explorer (backlog E11.5-S4.2), the way Beyond Compare lets you pick
//! "Select Left Side" in one folder and "Compare to <left>" in another. One small file that the
//! shell extension, a CLI and the UI all read and write, so the three agree without talking to each
//! other. It lives in the user's local data folder (`%LOCALAPPDATA%\DiffSync` on Windows,
//! `$XDG_STATE_HOME` or `~/.local/state` elsewhere) and holds the path and when it was chosen.
//!
//! A selection that no longer exists, or is older than a day, is treated as no selection: the
//! context menu must not offer to compare with a file that is gone.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// A selection older than this is forgotten.
pub const SELECTION_LIFETIME: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Selection {
    pub left: String,
    /// Seconds since the Unix epoch when the side was chosen.
    pub chosen_at: u64,
}

/// The file the selection is kept in.
#[derive(Debug, Clone)]
pub struct SelectionStore {
    file: PathBuf,
}

impl SelectionStore {
    /// A store in `dir` (created on the first write).
    pub fn in_dir(dir: &Path) -> SelectionStore {
        SelectionStore {
            file: dir.join("selection.json"),
        }
    }

    /// The store of the current user. `None` when no local data folder can be found.
    pub fn for_current_user() -> Option<SelectionStore> {
        let base = if cfg!(windows) {
            std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
        } else {
            std::env::var_os("XDG_STATE_HOME")
                .map(PathBuf::from)
                .or_else(|| {
                    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local").join("state"))
                })
        }?;
        Some(SelectionStore::in_dir(&base.join("DiffSync")))
    }

    pub fn file(&self) -> &Path {
        &self.file
    }

    fn now() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
    }

    /// Remembers `path` as the left side.
    pub fn set_left(&self, path: &str) -> std::io::Result<()> {
        if let Some(dir) = self.file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let json = serde_json::to_string(&Selection {
            left: path.to_string(),
            chosen_at: Self::now(),
        })
        .map_err(std::io::Error::other)?;
        // write beside the file and rename: two Explorer processes never read half a file
        let tmp = self.file.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &self.file)
    }

    /// The left side, when one was chosen, is recent and still exists.
    pub fn left(&self) -> Option<Selection> {
        let text = std::fs::read_to_string(&self.file).ok()?;
        let s: Selection = serde_json::from_str(&text).ok()?;
        let age = Self::now().saturating_sub(s.chosen_at);
        (age <= SELECTION_LIFETIME.as_secs() && Path::new(&s.left).exists()).then_some(s)
    }

    /// Forgets the selection (after a compare was started).
    pub fn clear(&self) {
        let _ = std::fs::remove_file(&self.file);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_chosen_side_is_remembered_and_forgotten() {
        let d = tempfile::tempdir().unwrap();
        let target = d.path().join("left.txt");
        std::fs::write(&target, "x").unwrap();
        let store = SelectionStore::in_dir(&d.path().join("state"));
        assert_eq!(store.left(), None);
        store.set_left(target.to_str().unwrap()).unwrap();
        assert_eq!(store.left().unwrap().left, target.to_str().unwrap());
        // a second store over the same folder (another process) sees it
        assert!(
            SelectionStore::in_dir(&d.path().join("state"))
                .left()
                .is_some()
        );
        store.clear();
        assert_eq!(store.left(), None);
    }

    #[test]
    fn a_missing_target_or_an_old_selection_is_no_selection() {
        let d = tempfile::tempdir().unwrap();
        let store = SelectionStore::in_dir(d.path());
        store
            .set_left(d.path().join("gone.txt").to_str().unwrap())
            .unwrap();
        assert_eq!(store.left(), None, "the file does not exist");
        let t = d.path().join("t.txt");
        std::fs::write(&t, "x").unwrap();
        let old = Selection {
            left: t.to_str().unwrap().to_string(),
            chosen_at: SelectionStore::now() - SELECTION_LIFETIME.as_secs() - 60,
        };
        std::fs::write(store.file(), serde_json::to_string(&old).unwrap()).unwrap();
        assert_eq!(store.left(), None, "older than a day");
        std::fs::write(store.file(), "not json").unwrap();
        assert_eq!(store.left(), None, "damaged file");
    }
}
