//! Changing a file in the vault, the one way a feature does it.
//!
//! Every write a user can undo is the same five steps: read the file,
//! make sure it is the kind of file the change is meant for, write it
//! atomically, tell the index, record before and after in the history.
//! Spelled out at each call site, one of them was eventually missing --
//! a kind toggle that left no history entry, a write that did not tell
//! the index -- and nothing caught it. Here they are one function.
//!
//! Two more things only a single door can give:
//!
//! - **One writer per file at a time.** A detail view that saves a note
//!   on blur while a checkbox click saves a flag reads the same text
//!   twice; the second write would silently drop the first. The lock is
//!   per path and waits -- unlike `once_at_a_time`, a second write is
//!   not the same work again, it is different work that has to happen.
//! - **Nothing outside the vault.** A path from the frontend is a string;
//!   the write refuses one that does not lie in the vault.
//!
//! ```ignore
//! let line = vault_write::edit_file(&path, Some(KIND), &index, &db, |text| {
//!     let (text, line) = my_change(text)?;
//!     Ok((text, line))
//! })?;
//! ```

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex, OnceLock};

use rusqlite::Connection;

use crate::config::settings::Config;
use crate::core::file_kind::{self, FileKind};
use crate::core::history::record_best_effort;
use crate::core::indexer::IndexManager;
use crate::core::models::{FileSnapshot, HistoryEntryKind};
use crate::utils::fs_utils;

/// The app's own folder in a vault: GTD lists, mirrors, bookmarks files.
/// Hidden from the listings, never renamed (see core::app_files).
pub fn app_dir(root: &Path) -> PathBuf {
    root.join(".nobo")
}

/// Changes the file at PATH: CHANGE gets its text and returns the new
/// text and an answer for the caller. Refused when the file is not in the
/// vault or not of KIND (`None`: any file the app may edit). An unchanged
/// text is not written and leaves no history entry.
pub fn edit_file<T>(
    path: &Path,
    kind: Option<FileKind>,
    index: &IndexManager,
    db: &Mutex<Connection>,
    change: impl FnOnce(&str) -> Result<(String, T), String>,
) -> Result<T, String> {
    edit_files(&[path], kind, index, db, |texts| {
        let (text, answer) = change(&texts[0])?;
        Ok((vec![text], answer))
    })
}

/// The same for a change that spans files -- a bookmark moving from one
/// category to another. All of them are read, changed and written under
/// their locks, and recorded as ONE history entry: one undo puts both
/// back, rather than the first undo leaving the entry in neither file.
pub fn edit_files<T>(
    paths: &[&Path],
    kind: Option<FileKind>,
    index: &IndexManager,
    db: &Mutex<Connection>,
    change: impl FnOnce(&[String]) -> Result<(Vec<String>, T), String>,
) -> Result<T, String> {
    edit_files_in(&Config::require_vault_root()?, paths, kind, index, db, change)
}

/// `edit_files` in the vault at ROOT -- what the two above come down to,
/// and what a test calls without a configured vault.
pub fn edit_files_in<T>(
    root: &Path,
    paths: &[&Path],
    kind: Option<FileKind>,
    index: &IndexManager,
    db: &Mutex<Connection>,
    change: impl FnOnce(&[String]) -> Result<(Vec<String>, T), String>,
) -> Result<T, String> {
    for path in paths {
        require_in_vault(root, path)?;
    }
    let _locks = lock_all(paths);

    let mut texts = Vec::with_capacity(paths.len());
    for path in paths {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        require_kind(path, &text, kind)?;
        texts.push(text);
    }
    let (updated, answer) = change(&texts)?;
    if updated.len() != paths.len() {
        return Err(format!("edit_files: {} texts for {} files", updated.len(), paths.len()));
    }

    let mut snapshots = Vec::new();
    let mut failed = None;
    for ((path, before), after) in paths.iter().zip(texts).zip(updated) {
        if before == after {
            continue;
        }
        match fs_utils::write_atomic_checked(path, &after) {
            Ok(()) => {
                index.update_file_with(path, &after);
                snapshots.push(FileSnapshot { path: path.to_string_lossy().into_owned(), before: Some(before), after: Some(after) });
            }
            // What was written before this one stays recorded: it is on
            // disk, and has to be one undo away like any other write.
            Err(e) => {
                failed = Some(format!("{}: {e}", path.display()));
                break;
            }
        }
    }
    if let Some(first) = snapshots.first() {
        let filename = fs_utils::filename_of(&first.path);
        record_best_effort(db, HistoryEntryKind::FileEdited { filename }, false, snapshots);
    }
    match failed {
        Some(error) => Err(error),
        None => Ok(answer),
    }
}

/// Refuses PATH unless it lies inside ROOT -- after resolving `..` and
/// links, so `vault/../elsewhere` is not inside.
pub fn require_in_vault(root: &Path, path: &Path) -> Result<(), String> {
    let resolve = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    if resolve(path).starts_with(resolve(root)) {
        return Ok(());
    }
    let name = path.display().to_string();
    Err(crate::t!("{name} liegt nicht im Notiz-Ordner", "{name} is not in the notes vault"))
}

fn require_kind(path: &Path, text: &str, kind: Option<FileKind>) -> Result<(), String> {
    let Some(kind) = kind else { return Ok(()) };
    if file_kind::from_lines(&text.lines().collect::<Vec<_>>()) == kind {
        return Ok(());
    }
    let name = path.display().to_string();
    let wanted = kind.keyword();
    Err(crate::t!("{name} ist keine Datei der Art „{wanted}“", "{name} is not a {wanted} file"))
}

/// The files being written right now. A writer takes all of its paths
/// at once, or waits until it can -- never one now and one later, so two
/// moves in opposite directions cannot each hold one file and wait for
/// the other.
struct Busy {
    paths: Mutex<HashSet<PathBuf>>,
    freed: Condvar,
}

fn busy() -> &'static Busy {
    static BUSY: OnceLock<Busy> = OnceLock::new();
    BUSY.get_or_init(|| Busy { paths: Mutex::new(HashSet::new()), freed: Condvar::new() })
}

/// Held while the files are read, changed and written; frees them on
/// drop, an early return or a panic included.
struct Locks(Vec<PathBuf>);

fn lock_all(paths: &[&Path]) -> Locks {
    let keys: Vec<PathBuf> = paths.iter().map(|path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())).collect();
    let busy = busy();
    let mut held = busy.paths.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    while keys.iter().any(|key| held.contains(key)) {
        held = busy.freed.wait(held).unwrap_or_else(|poisoned| poisoned.into_inner());
    }
    held.extend(keys.iter().cloned());
    Locks(keys)
}

impl Drop for Locks {
    fn drop(&mut self) {
        let busy = busy();
        let mut held = busy.paths.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        for key in &self.0 {
            held.remove(key);
        }
        busy.freed.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::history::store;
    use std::sync::Arc;

    fn setup(files: &[(&str, &str)]) -> (tempfile::TempDir, IndexManager, Mutex<Connection>) {
        let dir = tempfile::tempdir().unwrap();
        for (name, text) in files {
            std::fs::write(dir.path().join(name), text).unwrap();
        }
        let conn = Connection::open_in_memory().unwrap();
        store::init_schema(&conn).unwrap();
        (dir, IndexManager::new(), Mutex::new(conn))
    }

    fn entries(db: &Mutex<Connection>) -> i64 {
        db.lock().unwrap().query_row("SELECT COUNT(*) FROM history_actions", [], |row| row.get(0)).unwrap()
    }

    #[test]
    fn a_change_is_written_and_recorded_once() {
        let (dir, index, db) = setup(&[("a.org", "* A\n")]);
        let path = dir.path().join("a.org");
        let answer = edit_files_in(dir.path(), &[&path], None, &index, &db, |texts| Ok((vec![format!("{}* B\n", texts[0])], 7))).unwrap();
        assert_eq!(answer, 7);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "* A\n* B\n");
        assert_eq!(entries(&db), 1);
    }

    #[test]
    fn an_unchanged_text_leaves_no_trace() {
        let (dir, index, db) = setup(&[("a.org", "* A\n")]);
        let path = dir.path().join("a.org");
        edit_files_in(dir.path(), &[&path], None, &index, &db, |texts| Ok((texts.to_vec(), ()))).unwrap();
        assert_eq!(entries(&db), 0);
    }

    #[test]
    fn two_files_are_one_undo() {
        let (dir, index, db) = setup(&[("a.org", "a\n"), ("b.org", "b\n")]);
        let (a, b) = (dir.path().join("a.org"), dir.path().join("b.org"));
        edit_files_in(dir.path(), &[&a, &b], None, &index, &db, |_| Ok((vec!["a2\n".into(), "b2\n".into()], ()))).unwrap();
        assert_eq!(entries(&db), 1);
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "b2\n");
    }

    #[test]
    fn a_path_outside_the_vault_is_refused() {
        let (dir, index, db) = setup(&[]);
        let outside = tempfile::tempdir().unwrap();
        let path = outside.path().join("x.org");
        std::fs::write(&path, "x").unwrap();
        assert!(edit_files_in(dir.path(), &[&path], None, &index, &db, |texts| Ok((texts.to_vec(), ()))).is_err());
        let sneaky = dir.path().join("..").join(outside.path().file_name().unwrap()).join("x.org");
        assert!(require_in_vault(dir.path(), &sneaky).is_err());
    }

    #[test]
    fn a_file_of_another_kind_is_refused() {
        let (dir, index, db) = setup(&[("a.org", "* A\n")]);
        let path = dir.path().join("a.org");
        let err = edit_files_in(dir.path(), &[&path], Some(FileKind::new("vault-write-test")), &index, &db, |texts| Ok((texts.to_vec(), ()))).unwrap_err();
        assert!(err.contains("a.org"), "{err}");
    }

    /// Two writers of one file: the second sees what the first wrote.
    #[test]
    fn writers_of_one_file_take_turns() {
        let (dir, index, db) = setup(&[("n.org", "")]);
        let path = dir.path().join("n.org");
        let (index, db) = (Arc::new(index), Arc::new(db));
        let handles: Vec<_> = (0..8)
            .map(|n| {
                let (path, root, index, db) = (path.clone(), dir.path().to_path_buf(), index.clone(), db.clone());
                std::thread::spawn(move || {
                    edit_files_in(&root, &[&path], None, &index, &db, |texts| {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                        Ok((vec![format!("{}{n}\n", texts[0])], ()))
                    })
                    .unwrap()
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 8, "no write lost");
    }
}
