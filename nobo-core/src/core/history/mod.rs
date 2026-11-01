// SPDX-FileCopyrightText: 2026 Dennis Burgermeister
// SPDX-License-Identifier: GPL-3.0-or-later

pub mod restore;
pub mod store;

use rusqlite::Connection;
use std::path::Path;
use std::sync::Mutex;

use crate::core::indexer::IndexManager;
use crate::core::models::{FileSnapshot, HistoryEntry, HistoryEntryKind};
use crate::utils::fs_utils;
use crate::utils::temp_file_utils;
use crate::utils::ToCmdResult;

/// Number of remembered entries -- a rough "a few weeks of normal editing"
const MAX_ENTRIES: i64 = 300;

/// Above this (combined before+after length across every snapshot in one
/// action), the action is still recorded -- visible in the list -- but
/// WITHOUT snapshot content, same "informational, not restorable" shape as
/// an AttachmentAdded entry (see record() below). Keeps one outlier (e.g.
/// replace_link_references touching many files) from bloating history.sqlite
/// or slowing undo/redo down.
const MAX_SNAPSHOT_BYTES: usize = 5 * 1024 * 1024;

/// Total size of the COMPRESSED snapshots kept across all actions -- the
/// second half of retention, see `store::prune` for why the count above
/// is not enough on its own. Sized so history.sqlite stays a rounding
/// error next to a vault, on a phone as much as a desktop.
const MAX_HISTORY_BYTES: i64 = 64 * 1024 * 1024;

/// Records one history action -- a no-op if nothing actually changed
/// (every snapshot's before == after), so e.g. re-saving unchanged content
/// never pollutes the stack.
pub fn record(conn: &Connection, kind: HistoryEntryKind, is_draft: bool, snapshots: Vec<FileSnapshot>) -> Result<(), String> {
    if !snapshots.is_empty() && snapshots.iter().all(|s| s.before == s.after) {
        return Ok(());
    }

    let total_bytes: usize =
        snapshots.iter().map(|s| s.before.as_deref().map_or(0, str::len) + s.after.as_deref().map_or(0, str::len)).sum();
    // Drop content rather than storing a misleading `before: None` (which
    // undo would read as "this file didn't exist") -- an empty snapshot
    // list is the same "shown, not restorable" shape AttachmentAdded uses.
    let to_store: Vec<FileSnapshot> = if total_bytes > MAX_SNAPSHOT_BYTES { Vec::new() } else { snapshots };

    let action_id = store::record(conn, &kind, is_draft, &to_store).to_cmd()?;
    if !is_draft {
        super::trash::associate(conn, action_id, &to_store)?;
    }
    store::prune(conn, MAX_ENTRIES, MAX_HISTORY_BYTES).to_cmd()
}

/// Check every target before touching any of them. History is a snapshot,
/// not permission to overwrite edits made since the action was recorded.
fn preflight_restore(entry: &HistoryEntry, is_undo: bool) -> Result<(), String> {
    for snap in &entry.snapshots {
        let expected = if is_undo { &snap.after } else { &snap.before };
        let desired = if is_undo { &snap.before } else { &snap.after };
        let target = if entry.is_draft { temp_file_utils::temp_path_for(&snap.path) } else { snap.path.clone() };
        let current = read_optional(Path::new(&target))?;
        let expected = expected.as_deref().map(str::as_bytes);
        let desired = desired.as_deref().map(str::as_bytes);
        if current.as_deref() != expected && current.as_deref() != desired {
            return Err(restore_conflict(&target));
        }
        if !entry.is_draft {
            let draft = temp_file_utils::temp_path_for(&snap.path);
            if read_optional(Path::new(&draft))?.as_deref().is_some_and(|bytes| Some(bytes) != expected && Some(bytes) != desired) {
                return Err(restore_conflict(&draft));
            }
        }
    }
    Ok(())
}

fn restore_conflict(path: &str) -> String {
    crate::t!(
        "Die Datei wurde inzwischen geändert; Wiederherstellen würde Änderungen überschreiben: {}",
        "The file has changed; restoring would overwrite edits: {}",
        path
    )
}

fn read_optional(path: &Path) -> Result<Option<Vec<u8>>, String> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

fn apply_restore(entry: &Option<HistoryEntry>, index_manager: &IndexManager, is_undo: bool) -> Result<(), String> {
    let Some(entry) = entry else { return Ok(()) };

    for snap in &entry.snapshots {
        let content = if is_undo { &snap.before } else { &snap.after };
        let target = if entry.is_draft { temp_file_utils::temp_path_for(&snap.path) } else { snap.path.clone() };

        match content {
            // Unguarded like the bulk restore above, and for the same
            // reason -- but atomic: undo is what the user reaches for
            // after something already went wrong, so it is the last write
            // in the app that may leave a file half-written.
            Some(c) => {
                fs_utils::write_atomic(Path::new(&target), c).to_cmd()?;
            }
            None => match std::fs::remove_file(&target) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(format!("{target}: {e}")),
            },
        }

        if !entry.is_draft {
            // A stale #file# temp draft would otherwise "win" over the
            // content just restored here the next time this note is
            // opened (see temp_file_utils::load_with_temp_check) --
            // undo/redo would look like it silently did nothing.
            temp_file_utils::remove_temp_if_exists(&snap.path)?;

            let p = Path::new(&snap.path);
            match content {
                Some(_) => index_manager.update_file(p),
                None => index_manager.remove_file(p),
            }
        }
    }
    Ok(())
}

pub fn undo(conn: &Connection, index_manager: &IndexManager) -> Result<Option<HistoryEntry>, String> {
    super::trash::maintain(conn, chrono::Utc::now().timestamp())?;
    let entry = store::undo_toggle(conn).to_cmd()?;
    let restored = (|| {
        if let Some(action) = &entry {
            preflight_restore(action, true)?;
            super::trash::protect_undo(conn, index_manager, action, || {
                super::trash::restore_history_files(conn, action.id, index_manager, true)?;
                apply_restore(&entry, index_manager, true)
            })?;
            super::trash::mark_history_restored(conn, action.id, true)?;
        }
        Ok::<_, String>(())
    })();
    if restored.is_err() {
        if let Some(entry) = &entry {
            let _ = conn.execute("UPDATE history_actions SET undone = 0 WHERE id = ?1", [entry.id]);
        }
    }
    restored?;
    Ok(entry)
}

pub fn redo(conn: &Connection, index_manager: &IndexManager) -> Result<Option<HistoryEntry>, String> {
    super::trash::maintain(conn, chrono::Utc::now().timestamp())?;
    let entry = store::redo_toggle(conn).to_cmd()?;
    let restored = (|| {
        if let Some(action) = &entry {
            preflight_restore(action, false)?;
            super::trash::protect_redo(conn, index_manager, action, || apply_restore(&entry, index_manager, false))?;
        }
        if let Some(entry) = &entry {
            super::trash::restore_history_files(conn, entry.id, index_manager, false)?;
            super::trash::mark_history_restored(conn, entry.id, false)?;
        }
        Ok::<_, String>(())
    })();
    if restored.is_err() {
        if let Some(entry) = &entry {
            let _ = conn.execute("UPDATE history_actions SET undone = 1 WHERE id = ?1", [entry.id]);
        }
    }
    restored?;
    Ok(entry)
}

pub fn list(conn: &Connection, limit: i64) -> Result<Vec<HistoryEntry>, String> {
    store::list(conn, limit).to_cmd()
}

/// One entry, snapshot content included -- see `commands_history::history_entry`.
pub fn entry(conn: &Connection, id: i64) -> Result<Option<HistoryEntry>, String> {
    store::get(conn, id).to_cmd()
}

pub fn list_for_path(conn: &Connection, file_path: &str, limit: i64) -> Result<Vec<HistoryEntry>, String> {
    store::list_for_path(conn, file_path, limit).to_cmd()
}

/// Undoes or redoes until `target_id` is the most recently applied action:
/// it and everything before it active, everything after it undone.
///
/// The stack is global rather than per-file, so jumping to a point in one
/// file's timeline can take several steps and touch other files along the
/// way. Returns every toggled entry in application order, so the frontend
/// can resync without a second round trip.
pub fn jump_to(conn: &Connection, index_manager: &IndexManager, target_id: i64) -> Result<Vec<HistoryEntry>, String> {
    let Some(target_undone) = store::is_undone(conn, target_id).to_cmd()? else {
        // Pruned out from under the modal (extremely old entry) -- nothing
        // to do, same as undo/redo hitting an empty stack.
        return Ok(Vec::new());
    };

    let mut touched = Vec::new();
    if target_undone {
        loop {
            let Some(entry) = redo(conn, index_manager)? else { break };
            let reached_target = entry.id == target_id;
            touched.push(entry);
            if reached_target {
                break;
            }
        }
    } else {
        while store::has_active_entry_after(conn, target_id).to_cmd()? {
            let Some(entry) = undo(conn, index_manager)? else { break };
            touched.push(entry);
        }
    }
    Ok(touched)
}

/// Convenience for the many structural commands (create/delete/rename/
/// refile/archive/...) that want to record history as an incidental
/// side effect of their real work -- locks the connection and swallows
/// any error. A history-recording hiccup (DB locked, disk full for
/// history.sqlite specifically) must never fail the actual file operation
/// that already succeeded; losing one history entry is a much smaller
/// problem than surfacing a spurious error for a save/delete/rename that
/// otherwise worked fine.
pub fn record_best_effort(db: &Mutex<Connection>, kind: HistoryEntryKind, is_draft: bool, snapshots: Vec<FileSnapshot>) {
    if let Ok(conn) = db.lock() {
        let _ = record(&conn, kind, is_draft, snapshots);
    }
}

/// The read-before / mutate / read-after shape every single-file
/// structural command shares. `mutate` does the write and whatever has to
/// happen alongside it, and returns its own `Result`.
///
/// Both snapshots are taken even when `mutate` fails partway: a write that
/// landed before a later step errored still needs to be undoable.
pub fn snapshot_around<T>(path: &Path, mutate: impl FnOnce() -> Result<T, String>) -> (Option<String>, Result<T, String>, Option<String>) {
    let before = std::fs::read_to_string(path).ok();
    let result = mutate();
    let after = std::fs::read_to_string(path).ok();
    (before, result, after)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::indexer::IndexManager;
    use tempfile::TempDir;

    fn conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        store::init_schema(&conn).unwrap();
        conn
    }

    #[test]
    fn undo_restores_file_content_and_redo_reapplies_it() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("a.org");
        std::fs::write(&path, "new content").unwrap();
        let path_str = path.to_string_lossy().into_owned();

        let conn = conn();
        let index = IndexManager::new();
        record(
            &conn,
            HistoryEntryKind::FileEdited { filename: "a.org".into() },
            false,
            vec![FileSnapshot { path: path_str.clone(), before: Some("old content".into()), after: Some("new content".into()) }],
        )
        .unwrap();

        undo(&conn, &index).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "old content");

        redo(&conn, &index).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new content");
    }

    #[test]
    fn undo_of_a_file_creation_deletes_it() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("new.org");
        std::fs::write(&path, "hello").unwrap();
        let path_str = path.to_string_lossy().into_owned();

        let conn = conn();
        let index = IndexManager::new();
        record(
            &conn,
            HistoryEntryKind::FileCreated { filename: "new.org".into() },
            false,
            vec![FileSnapshot { path: path_str, before: None, after: Some("hello".into()) }],
        )
        .unwrap();

        undo(&conn, &index).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn draft_undo_only_touches_the_temp_file() {
        let dir = TempDir::new().unwrap();
        let real_path = dir.path().join("a.org");
        std::fs::write(&real_path, "on disk, untouched").unwrap();
        let path_str = real_path.to_string_lossy().into_owned();

        let conn = conn();
        let index = IndexManager::new();
        record(
            &conn,
            HistoryEntryKind::FileEdited { filename: "a.org".into() },
            true,
            vec![FileSnapshot { path: path_str.clone(), before: Some("draft v1".into()), after: Some("draft v2".into()) }],
        )
        .unwrap();

        temp_file_utils::write_temp(&path_str, "draft v2").unwrap();
        undo(&conn, &index).unwrap();

        assert_eq!(std::fs::read_to_string(&real_path).unwrap(), "on disk, untouched", "real file must never be touched for a draft entry");
        let temp_path = temp_file_utils::temp_path_for(&path_str);
        assert_eq!(std::fs::read_to_string(&temp_path).unwrap(), "draft v1");
    }

    #[test]
    fn oversized_snapshots_are_recorded_without_restorable_content() {
        let conn = conn();
        let huge = "x".repeat(MAX_SNAPSHOT_BYTES + 1);
        record(
            &conn,
            HistoryEntryKind::FileEdited { filename: "big.org".into() },
            false,
            vec![FileSnapshot { path: "/big.org".into(), before: None, after: Some(huge) }],
        )
        .unwrap();

        let entries = list(&conn, 10).unwrap();
        assert_eq!(entries.len(), 1, "still shown in the list");
        assert!(entries[0].snapshots.is_empty(), "but without restorable content");
    }

    #[test]
    fn jump_to_an_older_entry_undoes_everything_after_it() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("a.org");
        let path_str = path.to_string_lossy().into_owned();

        let conn = conn();
        let index = IndexManager::new();
        record(
            &conn,
            HistoryEntryKind::FileEdited { filename: "a.org".into() },
            false,
            vec![FileSnapshot { path: path_str.clone(), before: None, after: Some("v1".into()) }],
        )
        .unwrap();
        let target_id = list(&conn, 1).unwrap()[0].id;
        record(
            &conn,
            HistoryEntryKind::FileEdited { filename: "a.org".into() },
            false,
            vec![FileSnapshot { path: path_str.clone(), before: Some("v1".into()), after: Some("v2".into()) }],
        )
        .unwrap();
        record(
            &conn,
            HistoryEntryKind::FileEdited { filename: "a.org".into() },
            false,
            vec![FileSnapshot { path: path_str.clone(), before: Some("v2".into()), after: Some("v3".into()) }],
        )
        .unwrap();

        std::fs::write(&path, "v3").unwrap();
        let touched = jump_to(&conn, &index, target_id).unwrap();
        assert_eq!(touched.len(), 2, "undid the v2->v3 and v1->v2 actions");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "v1");
        assert!(!store::has_active_entry_after(&conn, target_id).unwrap());
    }

    #[test]
    fn jump_to_a_redone_entry_redoes_up_to_it() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("a.org");
        let path_str = path.to_string_lossy().into_owned();

        let conn = conn();
        let index = IndexManager::new();
        record(
            &conn,
            HistoryEntryKind::FileEdited { filename: "a.org".into() },
            false,
            vec![FileSnapshot { path: path_str.clone(), before: None, after: Some("v1".into()) }],
        )
        .unwrap();
        record(
            &conn,
            HistoryEntryKind::FileEdited { filename: "a.org".into() },
            false,
            vec![FileSnapshot { path: path_str.clone(), before: Some("v1".into()), after: Some("v2".into()) }],
        )
        .unwrap();
        let target_id = list(&conn, 1).unwrap()[0].id;

        std::fs::write(&path, "v2").unwrap();
        undo(&conn, &index).unwrap();
        undo(&conn, &index).unwrap();
        assert!(!path.exists());

        let touched = jump_to(&conn, &index, target_id).unwrap();
        assert_eq!(touched.len(), 2, "redid both actions back up to the target");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "v2");
    }

    #[test]
    fn jump_to_the_current_point_is_a_no_op() {
        let conn = conn();
        let index = IndexManager::new();
        record(
            &conn,
            HistoryEntryKind::FileEdited { filename: "a.org".into() },
            false,
            vec![FileSnapshot { path: "/a.org".into(), before: Some("v1".into()), after: Some("v2".into()) }],
        )
        .unwrap();
        let id = list(&conn, 1).unwrap()[0].id;

        assert!(jump_to(&conn, &index, id).unwrap().is_empty());
    }

    #[test]
    fn list_for_path_scopes_to_one_file() {
        let conn = conn();
        record(
            &conn,
            HistoryEntryKind::FileEdited { filename: "a.org".into() },
            false,
            vec![FileSnapshot { path: "/a.org".into(), before: Some("v1".into()), after: Some("v2".into()) }],
        )
        .unwrap();
        record(
            &conn,
            HistoryEntryKind::FileEdited { filename: "b.org".into() },
            false,
            vec![FileSnapshot { path: "/b.org".into(), before: Some("v1".into()), after: Some("v2".into()) }],
        )
        .unwrap();

        let entries = list_for_path(&conn, "/a.org", 10).unwrap();
        assert_eq!(entries.len(), 1);
        assert!(matches!(&entries[0].kind, HistoryEntryKind::FileEdited { filename } if filename == "a.org"));
    }
    #[test]
    fn undo_preflights_all_files_and_preserves_external_edits_and_history_state() {
        let dir = TempDir::new().unwrap();
        let a = dir.path().join("a.org");
        let b = dir.path().join("b.org");
        std::fs::write(&a, "after").unwrap();
        std::fs::write(&b, "external edit").unwrap();
        let conn = conn();
        record(
            &conn,
            HistoryEntryKind::HeadingsBulkDeleted { count: 2, filename: "a.org".into() },
            false,
            [&a, &b]
                .into_iter()
                .map(|path| FileSnapshot {
                    path: path.to_string_lossy().into_owned(),
                    before: Some("before".into()),
                    after: Some("after".into()),
                })
                .collect(),
        )
        .unwrap();
        assert!(undo(&conn, &IndexManager::new()).is_err());
        assert_eq!(std::fs::read_to_string(a).unwrap(), "after");
        assert_eq!(std::fs::read_to_string(b).unwrap(), "external edit");
        assert!(!list(&conn, 1).unwrap()[0].undone);
    }

    #[test]
    fn undo_preserves_a_newer_autosaved_draft() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("a.org");
        std::fs::write(&path, "after").unwrap();
        let path = path.to_string_lossy().into_owned();
        temp_file_utils::write_temp(&path, "unsaved new work").unwrap();
        let conn = conn();
        record(
            &conn,
            HistoryEntryKind::FileEdited { filename: "a.org".into() },
            false,
            vec![FileSnapshot { path: path.clone(), before: Some("before".into()), after: Some("after".into()) }],
        )
        .unwrap();
        assert!(undo(&conn, &IndexManager::new()).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "after");
        assert_eq!(std::fs::read_to_string(temp_file_utils::temp_path_for(&path)).unwrap(), "unsaved new work");
    }

    #[test]
    fn redo_refuses_a_file_replaced_since_undo() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("a.org");
        std::fs::write(&path, "after").unwrap();
        let conn = conn();
        let index = IndexManager::new();
        record(
            &conn,
            HistoryEntryKind::FileEdited { filename: "a.org".into() },
            false,
            vec![FileSnapshot { path: path.to_string_lossy().into_owned(), before: Some("before".into()), after: Some("after".into()) }],
        )
        .unwrap();
        undo(&conn, &index).unwrap();
        std::fs::write(&path, [255, 254]).unwrap();
        assert!(redo(&conn, &index).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), [255, 254]);
        assert!(list(&conn, 1).unwrap()[0].undone);
    }
}
