// SPDX-FileCopyrightText: 2026 Dennis Burgermeister
// SPDX-License-Identifier: GPL-3.0-or-later

//! `sync_state` -- one row per vault-relative path this device has ever
//! seen, tracking enough to do a proper 3-way diff against the remote index
//! (see diff.rs) instead of the naive "missing locally -> must be new
//! remotely" comparison that would resurrect deliberate local deletions.
//!
//! Shares the app's existing single sqlite connection/Mutex (see
//! `history::open_connection`, `lib.rs`) rather than opening a second
//! database file -- same convention `nobo_babel::commands`/`nobo_app::commands_headings`
//! already follow for their own unrelated tables in the same file.

use rusqlite::{params, Connection, OptionalExtension};
use std::sync::{Mutex, MutexGuard};

/// The one lock-and-map-the-poison-error incantation every `sync_state`
/// caller needs -- `orchestrator::run_sync` alone does this ~9 times per
/// run (once per diff action), plus a couple more in nobo_sync::commands, all
/// with the same literal error string. A poisoned lock here always means
/// an earlier panic while holding it, not a normal runtime condition, so
/// callers don't need anything more specific than "sync is unusable until
/// restart" from the error.
pub fn lock(db: &Mutex<Connection>) -> Result<MutexGuard<'_, Connection>, String> {
    db.lock().map_err(|_| "sync_state database lock poisoned".to_string())
}

#[derive(Debug, Clone, PartialEq)]
pub struct SyncStateRow {
    pub path: String,
    /// Content hash as of the last successful sync of this path. `None`
    /// means "never synced" -- a brand new local file, or one whose sync
    /// history was cleared (e.g. after switching sync accounts).
    pub base_hash: Option<String>,
    /// Current on-disk content hash, kept fresh by `rescan_local_state`.
    /// `None` together with `deleted_at` set means the file is gone.
    pub local_hash: Option<String>,
    pub deleted_at: Option<String>,
}

pub fn init_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS sync_state (
            path TEXT PRIMARY KEY,
            base_hash TEXT,
            local_hash TEXT,
            deleted_at TEXT,
            updated_at TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS sync_meta (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
         );",
    )
}

const REMOTE_IDENTITY_KEY: &str = "remote_identity";

/// Wipes `sync_state` when the remote identity differs from last time.
///
/// A `base_hash` only means anything relative to the remote that produced
/// it. Left over after switching remotes, an empty new remote plus
/// "previously synced, unchanged locally" reads as "the remote deleted
/// this" and produces a `DeleteLocal` for every tracked file.
///
/// Called once after a successful login. Always safe: the next rescan
/// repopulates `local_hash` and leaves `base_hash` at `None`, so the worst
/// case is a redundant re-upload, never a deletion.
pub fn ensure_remote_identity(conn: &Connection, identity: &str) -> rusqlite::Result<()> {
    let previous: Option<String> =
        conn.query_row("SELECT value FROM sync_meta WHERE key = ?1", params![REMOTE_IDENTITY_KEY], |row| row.get(0)).optional()?;

    if previous.as_deref() != Some(identity) {
        conn.execute("DELETE FROM sync_state", [])?;
        conn.execute(
            "INSERT INTO sync_meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![REMOTE_IDENTITY_KEY, identity],
        )?;
    }
    Ok(())
}

pub fn get_all(conn: &Connection) -> rusqlite::Result<Vec<SyncStateRow>> {
    let mut stmt = conn.prepare("SELECT path, base_hash, local_hash, deleted_at FROM sync_state")?;
    let rows = stmt
        .query_map([], |row| {
            Ok(SyncStateRow { path: row.get(0)?, base_hash: row.get(1)?, local_hash: row.get(2)?, deleted_at: row.get(3)? })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn get(conn: &Connection, path: &str) -> rusqlite::Result<Option<SyncStateRow>> {
    conn.query_row("SELECT path, base_hash, local_hash, deleted_at FROM sync_state WHERE path = ?1", params![path], |row| {
        Ok(SyncStateRow { path: row.get(0)?, base_hash: row.get(1)?, local_hash: row.get(2)?, deleted_at: row.get(3)? })
    })
    .optional()
}

/// A file was seen on disk with this content -- clears any tombstone (a
/// file recreated at the same path after being deleted is just an edit,
/// not still a pending deletion), leaves `base_hash` untouched (that's only
/// ever updated by `mark_synced`, once the sync actually completes).
pub fn record_local_change(conn: &Connection, path: &str, local_hash: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO sync_state (path, local_hash, deleted_at, updated_at)
         VALUES (?1, ?2, NULL, datetime('now'))
         ON CONFLICT(path) DO UPDATE SET local_hash = excluded.local_hash, deleted_at = NULL, updated_at = excluded.updated_at",
        params![path, local_hash],
    )?;
    Ok(())
}

/// The file is gone from disk. The row is kept, so the next diff still
/// knows this path existed and can propagate the deletion; `local_hash` is
/// cleared.
pub fn record_local_delete(conn: &Connection, path: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO sync_state (path, local_hash, deleted_at, updated_at)
         VALUES (?1, NULL, datetime('now'), datetime('now'))
         ON CONFLICT(path) DO UPDATE SET local_hash = NULL, deleted_at = excluded.deleted_at, updated_at = excluded.updated_at",
        params![path],
    )?;
    Ok(())
}

/// Called once a file has actually been uploaded or downloaded
/// successfully -- `new_hash` becomes the new `base_hash` AND `local_hash`
/// (the two are equal exactly when there's nothing left to sync for this
/// path), and any tombstone is cleared.
pub fn mark_synced(conn: &Connection, path: &str, new_hash: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO sync_state (path, base_hash, local_hash, deleted_at, updated_at)
         VALUES (?1, ?2, ?2, NULL, datetime('now'))
         ON CONFLICT(path) DO UPDATE SET base_hash = excluded.base_hash, local_hash = excluded.local_hash, deleted_at = NULL, updated_at = excluded.updated_at",
        params![path, new_hash],
    )?;
    Ok(())
}

/// Resolves a conflict by rewriting ONLY `base_hash`, leaving `local_hash`
/// untouched -- the next `compute_diff` then sees exactly one side as
/// "changed since base" and produces a plain Upload or Download instead of
/// another Conflict. Setting `base_hash` to the remote hash makes the next
/// diff an Upload (local now looks changed, remote doesn't); setting it to
/// the current local hash makes it a Download (local now looks unchanged,
/// remote does) -- see `orchestrator::resolve_conflict` for which is which.
pub fn set_base_hash(conn: &Connection, path: &str, base_hash: &str) -> rusqlite::Result<()> {
    conn.execute("UPDATE sync_state SET base_hash = ?2, updated_at = datetime('now') WHERE path = ?1", params![path, base_hash])?;
    Ok(())
}

/// Clears `base_hash`, making the row look never-synced -- resolves a
/// local-edit-vs-remote-deletion conflict toward the local edit, since the
/// next diff then sees a new local-only file and uploads it.
pub fn clear_base_hash(conn: &Connection, path: &str) -> rusqlite::Result<()> {
    conn.execute("UPDATE sync_state SET base_hash = NULL, updated_at = datetime('now') WHERE path = ?1", params![path])?;
    Ok(())
}

/// Clears every `base_hash`, making the whole vault look never-synced.
///
/// The escape hatch for a remote index that really is gone (a
/// server-side deletion, a restored backup). `run_sync` refuses to diff
/// against an empty index while any `base_hash` survives, because that
/// reading turns every synced file into a local deletion -- so this is
/// what lets the user say "yes, it is gone, publish this vault again".
///
/// Deliberately not `DELETE FROM sync_state`: the rows carry `local_hash`
/// and `deleted_at` too, and a deletion this device has not yet
/// propagated should survive being republished.
pub fn clear_all_base_hashes(conn: &Connection) -> rusqlite::Result<usize> {
    conn.execute("UPDATE sync_state SET base_hash = NULL, updated_at = datetime('now') WHERE base_hash IS NOT NULL", [])
}

/// A deletion was propagated (either direction) and is now fully settled --
/// drops the row entirely, since there's nothing left to reconcile for this
/// path on either side.
pub fn remove(conn: &Connection, path: &str) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM sync_state WHERE path = ?1", params![path])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        conn
    }

    #[test]
    fn same_identity_across_logins_keeps_sync_state() {
        let conn = conn();
        // Establishes the baseline identity first (itself wipes the empty
        // table, a no-op) -- only the SECOND call, after real sync history
        // exists, is what this test actually exercises.
        ensure_remote_identity(&conn, "pocketbase|https://x|a@x.com").unwrap();
        mark_synced(&conn, "a.org", "h1").unwrap();

        ensure_remote_identity(&conn, "pocketbase|https://x|a@x.com").unwrap();

        assert_eq!(get(&conn, "a.org").unwrap().unwrap().base_hash.as_deref(), Some("h1"));
    }

    /// The exact incident this exists to prevent: logging into a different
    /// remote (a fresh PocketBase account here) must not leave stale
    /// `base_hash` values around that would make the new, empty remote
    /// look like "it deleted everything".
    #[test]
    fn switching_remote_identity_wipes_stale_sync_state() {
        let conn = conn();
        mark_synced(&conn, "a.org", "h1").unwrap();
        ensure_remote_identity(&conn, "pocketbase|https://old|a@x.com").unwrap();

        ensure_remote_identity(&conn, "pocketbase|https://new|b@x.com").unwrap();

        assert_eq!(get_all(&conn).unwrap(), vec![]);
    }

    #[test]
    fn first_ever_login_wipes_any_pre_existing_untracked_state() {
        // No identity recorded yet is treated like a switch: a
        // sync_state already contaminated by a previous account is exactly
        // as dangerous as a real one.
        let conn = conn();
        mark_synced(&conn, "a.org", "h1").unwrap();

        ensure_remote_identity(&conn, "pocketbase|https://x|a@x.com").unwrap();

        assert_eq!(get_all(&conn).unwrap(), vec![]);
    }
}
