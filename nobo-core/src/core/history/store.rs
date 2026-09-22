use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use rusqlite::{params, Connection, OptionalExtension};
use std::io::{Read, Write};

use crate::core::models::{FileSnapshot, HistoryEntry, HistoryEntryKind};

pub fn init_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS history_actions (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp TEXT NOT NULL,
            kind TEXT NOT NULL,
            is_draft INTEGER NOT NULL DEFAULT 0,
            undone INTEGER NOT NULL DEFAULT 0
         );
         CREATE TABLE IF NOT EXISTS history_snapshots (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            action_id INTEGER NOT NULL REFERENCES history_actions(id) ON DELETE CASCADE,
            file_path TEXT NOT NULL,
            before_gzip BLOB,
            after_gzip BLOB
         );
         -- `snapshots_for` filters on it, `list` joins on it, and with
         -- `foreign_keys = ON` every ON DELETE CASCADE from the prune (which
         -- runs after every edit) has to find the children too.
         CREATE INDEX IF NOT EXISTS idx_history_snapshots_action
             ON history_snapshots(action_id);",
    )
}

fn compress(s: &str) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(s.as_bytes()).expect("gzip write to an in-memory Vec cannot fail");
    encoder.finish().expect("gzip finish on an in-memory Vec cannot fail")
}

fn decompress(bytes: &[u8]) -> String {
    let mut decoder = GzDecoder::new(bytes);
    let mut out = String::new();
    // A corrupt/foreign blob decoding to nothing is preferable to a panic --
    // this is a local cache, not the source of truth for the file itself.
    let _ = decoder.read_to_string(&mut out);
    out
}

/// Appends one action + its snapshots, first discarding any currently
/// "undone" (redo-able) tail -- starting a new action after an undo clears
/// the redo branch, same as any standard undo manager.
pub fn record(conn: &Connection, kind: &HistoryEntryKind, is_draft: bool, snapshots: &[FileSnapshot]) -> rusqlite::Result<()> {
    // All of it or none of it.
    //
    // Every statement below used to commit on its own, so an interruption
    // mid-loop (a full disk, an I/O error, SQLITE_BUSY past the 5 s
    // timeout against the worker's second connection, or plain process
    // death on Android) left the action row standing with only SOME of its
    // snapshots. An EMPTY snapshot list is a documented, meaningful state
    // ("shown, not restorable"); a partial one is not: the history page
    // offers Restore as soon as there is one, so a refile would put back
    // one side and leave the heading in both files or in neither. Only
    // multi-snapshot actions are affected -- refile, bulk refile, rename,
    // move.
    //
    // `unchecked_transaction` because this takes `&Connection`, not
    // `&mut` -- the same reason `sync::rescan` gives.
    let tx = conn.unchecked_transaction()?;

    tx.execute("DELETE FROM history_actions WHERE undone = 1", [])?;

    let kind_json = serde_json::to_string(kind).unwrap_or_default();
    let timestamp = chrono::Local::now().to_rfc3339();
    tx.execute(
        "INSERT INTO history_actions (timestamp, kind, is_draft, undone) VALUES (?1, ?2, ?3, 0)",
        params![timestamp, kind_json, is_draft as i64],
    )?;
    let action_id = tx.last_insert_rowid();

    for snap in snapshots {
        let before_gzip = snap.before.as_deref().map(compress);
        let after_gzip = snap.after.as_deref().map(compress);
        tx.execute(
            "INSERT INTO history_snapshots (action_id, file_path, before_gzip, after_gzip) VALUES (?1, ?2, ?3, ?4)",
            params![action_id, snap.path, before_gzip, after_gzip],
        )?;
    }
    // `prune` stays outside, in the caller: a prune that fails must not
    // roll back an action that was recorded correctly.
    tx.commit()
}

/// Deletes the oldest actions beyond MAX_ENTRIES (FIFO), and then the
/// oldest of what is left until the stored snapshots fit MAX_BYTES --
/// cascades to their snapshots via the foreign key.
///
/// Two caps because the count alone measures the wrong thing. 300 actions
/// is "a few weeks of editing" on ordinary notes and a large multiple of
/// that on big ones, and nothing in between tells them apart; the byte
/// budget is what the file on disk actually costs. Measured on the
/// COMPRESSED blobs, which is what is stored -- `MAX_SNAPSHOT_BYTES`
/// caps a single action on its uncompressed text instead, because that
/// one is about how much work an undo would be.
///
/// The file does not shrink when rows go: sqlite keeps the freed pages
/// for reuse (no `auto_vacuum`, and no `VACUUM` anywhere -- rewriting the
/// whole file would double the peak space needed on exactly the device
/// this is for). What this bounds is the high-water mark.
pub fn prune(conn: &Connection, max_entries: i64, max_bytes: i64) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM history_actions WHERE id NOT IN (SELECT id FROM history_actions ORDER BY id DESC LIMIT ?1)",
        params![max_entries],
    )?;

    // Newest-first running total, so the oldest actions are the ones that
    // fall out -- and the newest action is always kept whatever it
    // weighs, because the alternative is an undo stack that silently
    // discards the edit that was just made.
    let mut stmt = conn.prepare(
        // COALESCE per column, not once around the sum: a snapshot with
        // no `before` (a file that did not exist) stores NULL there, and
        // NULL + n is NULL -- which weighed every creation at zero.
        "SELECT a.id, COALESCE(SUM(COALESCE(LENGTH(s.before_gzip), 0) + COALESCE(LENGTH(s.after_gzip), 0)), 0) \
         FROM history_actions a LEFT JOIN history_snapshots s ON s.action_id = a.id \
         GROUP BY a.id ORDER BY a.id DESC",
    )?;
    let weights: Vec<(i64, i64)> = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
    drop(stmt);

    let mut running = 0;
    let mut cutoff = None;
    for (index, (id, bytes)) in weights.iter().enumerate() {
        running += bytes;
        if index > 0 && running > max_bytes {
            cutoff = Some(*id);
            break;
        }
    }
    if let Some(cutoff) = cutoff {
        conn.execute("DELETE FROM history_actions WHERE id <= ?1", params![cutoff])?;
    }
    Ok(())
}

fn snapshots_for(conn: &Connection, action_id: i64) -> rusqlite::Result<Vec<FileSnapshot>> {
    let mut stmt = conn.prepare("SELECT file_path, before_gzip, after_gzip FROM history_snapshots WHERE action_id = ?1")?;
    let rows = stmt.query_map(params![action_id], |row| {
        let before_gzip: Option<Vec<u8>> = row.get(1)?;
        let after_gzip: Option<Vec<u8>> = row.get(2)?;
        Ok(FileSnapshot {
            path: row.get(0)?,
            before: before_gzip.as_deref().map(decompress),
            after: after_gzip.as_deref().map(decompress),
        })
    })?
    .collect();
    rows
}

/// Which files an action touched, without their content.
///
/// What a LISTING needs. Inflating every `before` and `after` of up to 300
/// entries -- which `list` did -- costs tens of megabytes per open, for
/// data the history page only ever counts (`snapshots.length`), and it
/// runs again after every undo. The one screen that shows content
/// (`HistoryModal`) asks for the single entry it is showing.
fn snapshot_paths_for(conn: &Connection, action_id: i64) -> rusqlite::Result<Vec<FileSnapshot>> {
    let mut stmt = conn.prepare("SELECT file_path FROM history_snapshots WHERE action_id = ?1")?;
    let rows: rusqlite::Result<Vec<FileSnapshot>> = stmt
        .query_map(params![action_id], |row| {
            Ok(FileSnapshot { path: row.get(0)?, before: None, after: None })
        })?
        .collect();
    rows
}

fn to_entry(conn: &Connection, id: i64, timestamp: String, kind_json: String, is_draft: bool, undone: bool) -> rusqlite::Result<HistoryEntry> {
    to_entry_with(conn, id, timestamp, kind_json, is_draft, undone, snapshots_for)
}

/// The same without snapshot content -- see `snapshot_paths_for`.
fn to_listed_entry(conn: &Connection, id: i64, timestamp: String, kind_json: String, is_draft: bool, undone: bool) -> rusqlite::Result<HistoryEntry> {
    to_entry_with(conn, id, timestamp, kind_json, is_draft, undone, snapshot_paths_for)
}

fn to_entry_with(
    conn: &Connection,
    id: i64,
    timestamp: String,
    kind_json: String,
    is_draft: bool,
    undone: bool,
    snapshots: fn(&Connection, i64) -> rusqlite::Result<Vec<FileSnapshot>>,
) -> rusqlite::Result<HistoryEntry> {
    let kind: HistoryEntryKind = serde_json::from_str(&kind_json)
        .unwrap_or(HistoryEntryKind::FileEdited { filename: String::new() });
    Ok(HistoryEntry { id, timestamp, kind, is_draft, undone, snapshots: snapshots(conn, id)? })
}

type ActionRow = (i64, String, String, bool);

fn action_row(conn: &Connection, sql: &str) -> rusqlite::Result<Option<ActionRow>> {
    conn.query_row(sql, [], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, i64>(3)? != 0))
    })
    .optional()
}

/// Marks the most recent active (`undone = 0`) action as undone and returns
/// it (with `undone: true`) -- the caller applies the actual disk restore.
pub fn undo_toggle(conn: &Connection) -> rusqlite::Result<Option<HistoryEntry>> {
    let Some((id, timestamp, kind_json, is_draft)) =
        action_row(conn, "SELECT id, timestamp, kind, is_draft FROM history_actions WHERE undone = 0 ORDER BY id DESC LIMIT 1")?
    else {
        return Ok(None);
    };
    conn.execute("UPDATE history_actions SET undone = 1 WHERE id = ?1", params![id])?;
    Ok(Some(to_entry(conn, id, timestamp, kind_json, is_draft, true)?))
}

/// Marks the oldest undone action (the one right after the active
/// boundary) as active again and returns it (with `undone: false`).
pub fn redo_toggle(conn: &Connection) -> rusqlite::Result<Option<HistoryEntry>> {
    let Some((id, timestamp, kind_json, is_draft)) =
        action_row(conn, "SELECT id, timestamp, kind, is_draft FROM history_actions WHERE undone = 1 ORDER BY id ASC LIMIT 1")?
    else {
        return Ok(None);
    };
    conn.execute("UPDATE history_actions SET undone = 0 WHERE id = ?1", params![id])?;
    Ok(Some(to_entry(conn, id, timestamp, kind_json, is_draft, false)?))
}

pub fn list(conn: &Connection, limit: i64) -> rusqlite::Result<Vec<HistoryEntry>> {
    let mut stmt = conn.prepare("SELECT id, timestamp, kind, is_draft, undone FROM history_actions ORDER BY id DESC LIMIT ?1")?;
    let rows: Vec<(i64, String, String, bool, bool)> = stmt
        .query_map(params![limit], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)? != 0,
                row.get::<_, i64>(4)? != 0,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;

    rows.into_iter()
        .map(|(id, ts, kind_json, is_draft, undone)| to_listed_entry(conn, id, ts, kind_json, is_draft, undone))
        .collect()
}

/// Same as `list`, narrowed to actions that touched FILE_PATH (via a
/// snapshot's `file_path`) -- powers the per-note/per-task history modal
/// (SettingsHistory.tsx shows the whole global stack; the modal instead
/// scopes to one file, at the granularity the backend actually stores:
/// whole-file snapshots, not individual headings -- see HistoryModal.tsx).
pub fn list_for_path(conn: &Connection, file_path: &str, limit: i64) -> rusqlite::Result<Vec<HistoryEntry>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT a.id, a.timestamp, a.kind, a.is_draft, a.undone
         FROM history_actions a JOIN history_snapshots s ON s.action_id = a.id
         WHERE s.file_path = ?1 ORDER BY a.id DESC LIMIT ?2",
    )?;
    let rows: Vec<(i64, String, String, bool, bool)> = stmt
        .query_map(params![file_path, limit], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)? != 0,
                row.get::<_, i64>(4)? != 0,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;

    rows.into_iter()
        .map(|(id, ts, kind_json, is_draft, undone)| to_listed_entry(conn, id, ts, kind_json, is_draft, undone))
        .collect()
}

/// A single action by id, `None` if it was pruned or never existed.
pub fn get(conn: &Connection, id: i64) -> rusqlite::Result<Option<HistoryEntry>> {
    let row: Option<(i64, String, String, bool, bool)> = conn
        .query_row(
            "SELECT id, timestamp, kind, is_draft, undone FROM history_actions WHERE id = ?1",
            params![id],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)? != 0,
                    row.get::<_, i64>(4)? != 0,
                ))
            },
        )
        .optional()?;
    let Some((id, timestamp, kind_json, is_draft, undone)) = row else { return Ok(None) };
    to_entry(conn, id, timestamp, kind_json, is_draft, undone).map(Some)
}

/// An action's current `undone` flag, `None` if it was pruned. Decides
/// which direction `jump_to` has to move in.
pub fn is_undone(conn: &Connection, id: i64) -> rusqlite::Result<Option<bool>> {
    conn.query_row("SELECT undone FROM history_actions WHERE id = ?1", params![id], |row| row.get::<_, i64>(0))
        .optional()
        .map(|v| v.map(|n| n != 0))
}

/// Whether any currently-active (undone = 0) action is newer than ID --
/// `jump_to`'s undo loop keeps going while this is true, since the linear
/// stack invariant guarantees active ids form a contiguous prefix (any
/// redo tail is always fully cleared on the next `record`, see there).
pub fn has_active_entry_after(conn: &Connection, id: i64) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM history_actions WHERE undone = 0 AND id > ?1)",
        params![id],
        |row| row.get::<_, i64>(0),
    )
    .map(|n| n != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        conn
    }

    fn snap(path: &str, before: Option<&str>, after: Option<&str>) -> FileSnapshot {
        FileSnapshot { path: path.to_string(), before: before.map(String::from), after: after.map(String::from) }
    }

    /// A listing names the files an action touched and carries none of
    /// their content -- see `snapshot_paths_for` for why.
    #[test]
    fn a_listing_names_the_files_without_their_content() {
        let conn = conn();
        record(&conn, &HistoryEntryKind::FileEdited { filename: "a.org".into() }, false,
            &[snap("/a.org", Some("old"), Some("new"))]).unwrap();

        let entries = list(&conn, 10).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].snapshots.len(), 1, "the count the history page shows is intact");
        assert_eq!(entries[0].snapshots[0].path, "/a.org");
        assert_eq!(entries[0].snapshots[0].before, None, "content stays out of a listing");
        assert_eq!(entries[0].snapshots[0].after, None);
        assert!(!entries[0].undone);
    }

    /// And asking for the one entry gives the content back.
    #[test]
    fn a_single_entry_carries_its_content() {
        let conn = conn();
        record(&conn, &HistoryEntryKind::FileEdited { filename: "a.org".into() }, false,
            &[snap("/a.org", Some("old"), Some("new"))]).unwrap();
        let id = list(&conn, 1).unwrap()[0].id;

        let entry = get(&conn, id).unwrap().expect("just recorded");

        assert_eq!(entry.snapshots[0].before.as_deref(), Some("old"));
        assert_eq!(entry.snapshots[0].after.as_deref(), Some("new"));
    }

    #[test]
    fn undo_then_redo_toggles_the_flag() {
        let conn = conn();
        record(&conn, &HistoryEntryKind::FileEdited { filename: "a.org".into() }, false,
            &[snap("/a.org", Some("old"), Some("new"))]).unwrap();

        let undone = undo_toggle(&conn).unwrap().unwrap();
        assert!(undone.undone);
        assert!(undo_toggle(&conn).unwrap().is_none(), "nothing left to undo");

        let redone = redo_toggle(&conn).unwrap().unwrap();
        assert!(!redone.undone);
        assert!(redo_toggle(&conn).unwrap().is_none(), "nothing left to redo");
    }

    #[test]
    fn recording_a_new_action_discards_the_redo_tail() {
        let conn = conn();
        record(&conn, &HistoryEntryKind::FileEdited { filename: "a.org".into() }, false,
            &[snap("/a.org", Some("v1"), Some("v2"))]).unwrap();
        undo_toggle(&conn).unwrap();

        record(&conn, &HistoryEntryKind::FileEdited { filename: "b.org".into() }, false,
            &[snap("/b.org", None, Some("v1"))]).unwrap();

        assert!(redo_toggle(&conn).unwrap().is_none(), "the undone v1->v2 action should be gone");
        assert_eq!(list(&conn, 10).unwrap().len(), 1);
    }

    #[test]
    fn prune_keeps_only_the_newest_n() {
        let conn = conn();
        for i in 0..5 {
            record(&conn, &HistoryEntryKind::FileEdited { filename: format!("{i}.org") }, false,
                &[snap(&format!("/{i}.org"), None, Some("x"))]).unwrap();
        }
        prune(&conn, 2, i64::MAX).unwrap();
        let entries = list(&conn, 10).unwrap();
        assert_eq!(entries.len(), 2);
        // newest-first
        assert!(matches!(&entries[0].kind, HistoryEntryKind::FileEdited { filename } if filename == "4.org"));
        assert!(matches!(&entries[1].kind, HistoryEntryKind::FileEdited { filename } if filename == "3.org"));
    }

    /// The count cap says nothing about size: 300 one-line edits and 300
    /// edits of a book-length note are the same number.
    #[test]
    fn the_byte_budget_drops_the_oldest_until_it_fits() {
        let conn = conn();
        // Incompressible, so the stored blob really is about this big.
        let bulky: String = (0..40_000).map(|i| char::from(b'a' + (i * 7 % 26) as u8)).collect();
        for i in 0..6 {
            record(&conn, &HistoryEntryKind::FileEdited { filename: format!("{i}.org") }, false,
                &[snap(&format!("/{i}.org"), None, Some(&bulky))]).unwrap();
        }
        let stored: i64 = conn
            .query_row("SELECT SUM(LENGTH(after_gzip)) FROM history_snapshots", [], |r| r.get(0))
            .unwrap();
        assert!(stored > 0, "the test text has to actually cost something");

        // A budget for roughly half of them.
        prune(&conn, 300, stored / 2).unwrap();

        let entries = list(&conn, 10).unwrap();
        assert!(entries.len() < 6 && !entries.is_empty(), "some dropped, not all: {}", entries.len());
        assert!(
            matches!(&entries[0].kind, HistoryEntryKind::FileEdited { filename } if filename == "5.org"),
            "the newest survives"
        );
        let left: i64 = conn
            .query_row("SELECT COALESCE(SUM(LENGTH(after_gzip)), 0) FROM history_snapshots", [], |r| r.get(0))
            .unwrap();
        assert!(left <= stored / 2, "what is left fits the budget: {left} > {}", stored / 2);
    }

    /// Whatever the budget, the edit that was just made stays undoable --
    /// a single action over budget must not delete itself.
    #[test]
    fn the_newest_action_survives_any_budget() {
        let conn = conn();
        record(&conn, &HistoryEntryKind::FileEdited { filename: "a.org".into() }, false,
            &[snap("/a.org", None, Some("something worth undoing"))]).unwrap();

        prune(&conn, 300, 0).unwrap();

        assert_eq!(list(&conn, 10).unwrap().len(), 1);
    }

    #[test]
    fn list_for_path_excludes_actions_touching_other_files() {
        let conn = conn();
        record(&conn, &HistoryEntryKind::FileEdited { filename: "a.org".into() }, false,
            &[snap("/a.org", Some("v1"), Some("v2"))]).unwrap();
        record(&conn, &HistoryEntryKind::FileEdited { filename: "b.org".into() }, false,
            &[snap("/b.org", Some("v1"), Some("v2"))]).unwrap();
        record(&conn, &HistoryEntryKind::Refiled { title: "x".into(), target_filename: "a.org".into() }, false,
            &[snap("/a.org", Some("v2"), Some("v3")), snap("/c.org", Some("v1"), Some("v2"))]).unwrap();

        let entries = list_for_path(&conn, "/a.org", 10).unwrap();
        assert_eq!(entries.len(), 2, "only the two actions touching /a.org");
        assert!(matches!(&entries[0].kind, HistoryEntryKind::Refiled { .. }), "newest first");
    }

    #[test]
    fn get_returns_the_entry_with_its_snapshots() {
        let conn = conn();
        record(&conn, &HistoryEntryKind::HeadingDeleted { filename: "a.org".into(), title: "Task".into() }, false,
            &[snap("/a.org", Some("before"), Some("after"))]).unwrap();
        let id = list(&conn, 1).unwrap()[0].id;

        let entry = get(&conn, id).unwrap().unwrap();
        assert!(matches!(&entry.kind, HistoryEntryKind::HeadingDeleted { title, .. } if title == "Task"));
        assert_eq!(entry.snapshots[0].before.as_deref(), Some("before"));
    }

    #[test]
    fn get_returns_none_for_a_missing_id() {
        let conn = conn();
        assert!(get(&conn, 999).unwrap().is_none());
    }

    #[test]
    fn is_undone_reflects_toggled_state_and_none_for_missing_id() {
        let conn = conn();
        record(&conn, &HistoryEntryKind::FileEdited { filename: "a.org".into() }, false,
            &[snap("/a.org", Some("v1"), Some("v2"))]).unwrap();
        let id = list(&conn, 1).unwrap()[0].id;

        assert_eq!(is_undone(&conn, id).unwrap(), Some(false));
        undo_toggle(&conn).unwrap();
        assert_eq!(is_undone(&conn, id).unwrap(), Some(true));
        assert_eq!(is_undone(&conn, id + 999).unwrap(), None);
    }

    #[test]
    fn has_active_entry_after_reports_whether_a_newer_action_is_still_active() {
        let conn = conn();
        record(&conn, &HistoryEntryKind::FileEdited { filename: "a.org".into() }, false,
            &[snap("/a.org", Some("v1"), Some("v2"))]).unwrap();
        let first_id = list(&conn, 1).unwrap()[0].id;
        record(&conn, &HistoryEntryKind::FileEdited { filename: "a.org".into() }, false,
            &[snap("/a.org", Some("v2"), Some("v3"))]).unwrap();

        assert!(has_active_entry_after(&conn, first_id).unwrap());
        undo_toggle(&conn).unwrap();
        assert!(!has_active_entry_after(&conn, first_id).unwrap());
    }
}
