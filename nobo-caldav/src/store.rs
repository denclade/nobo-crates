// SPDX-FileCopyrightText: 2026 Dennis Burgermeister
// SPDX-License-Identifier: GPL-3.0-or-later

//! `caldav_state` -- what this device knew about a list at the end of the
//! last sync.
//!
//! Three things live here rather than in the org file, each for its own
//! reason:
//!
//! - **The ETag**, because it changes on every server-side edit. In the
//!   file, every pull would be a content change, which the vault's own
//!   cloud sync would upload and a second device would download and
//!   rewrite -- an endless loop with no human involved.
//! - **A hash of the task's org block**, because "has the user edited
//!   this?" cannot be answered with a timestamp the app writes itself: a
//!   pull stamps UPDATED, the next sync reads that as a local edit, pushes
//!   it, the server's LAST-MODIFIED changes, and the two sides ping-pong
//!   forever. A hash of what we last wrote has no such feedback.
//! - **The row's mere existence**, because without a record that a task
//!   once existed, "gone from the file" is indistinguishable from "new on
//!   the server" -- the deletion is either resurrected or propagated to
//!   the wrong side. A row is dropped (see `forget`) only once both sides
//!   agree the task is gone, which is what makes that question
//!   answerable at all.
//!
//! The `deleted` column is a soft-delete flag from an earlier design that
//! nothing sets any more: deletion is decided by comparing the file, the
//! server and the rows above, and a row that survives that is hard-
//! deleted. It is kept because old databases have it and because the
//! `!deleted` filters throughout are then a cheap invariant guard rather
//! than dead weight -- but do not reach for it as a mechanism; it is a
//! leftover, not a design.
//!
//! Shares the app's single sqlite connection, same convention as
//! nobo_sync::store.

use rusqlite::{params, Connection, OptionalExtension};

#[derive(Debug, Clone, PartialEq)]
pub struct TaskState {
    pub list_id: String,
    pub uid: String,
    /// Server version as of the last successful sync.
    pub etag: String,
    /// The object's URL. Kept because a collection listing answers with
    /// hrefs and ETags but no UIDs -- without a stored mapping every sync
    /// would have to download every object just to learn which task it is.
    pub url: String,
    /// Hash of the org block this device last wrote for the task. Differs
    /// from the file's current block exactly when the user edited it.
    pub local_hash: String,
    /// `X-APPLE-SORT-ORDER` as of the last sync -- the server's own
    /// ordering value (see sort.rs).
    pub sort_order: Option<i64>,
    /// Where the task sat in the FILE when this device last wrote it.
    ///
    /// Not the same thing as `sort_order`, and that is the point: the file
    /// is laid out as a tree (a child follows its parent) while the server
    /// orders flat by value, so the two sequences legitimately differ.
    /// Comparing the file against the flat order instead reads as "the
    /// user reordered this" on every sync, and pushes the tree layout back
    /// over whatever order the server had.
    pub position: i64,
    /// Categories the server has that no org tag can express, so a push
    /// can put them back (see model::CalDavTask).
    pub unmappable_categories: Vec<String>,
    /// Set when the task is known to be gone. The row is kept so the next
    /// sync can tell a deletion from a task it has never seen.
    pub deleted: bool,
}

pub fn init_schema(conn: &Connection) -> rusqlite::Result<()> {
    // Every column the table has ever gained, so a database from an older
    // build is brought up to this shape rather than failing every read
    // (see db::ensure_columns). New columns belong in BOTH lists.
    const ADDED_LATER: &[(&str, &str)] = &[
        ("url", "TEXT NOT NULL DEFAULT ''"),
        ("position", "INTEGER NOT NULL DEFAULT 0"),
        ("unmappable_categories", "TEXT NOT NULL DEFAULT ''"),
    ];

    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS caldav_state (
            list_id TEXT NOT NULL,
            uid TEXT NOT NULL,
            etag TEXT NOT NULL,
            url TEXT NOT NULL DEFAULT '',
            local_hash TEXT NOT NULL,
            sort_order INTEGER,
            position INTEGER NOT NULL DEFAULT 0,
            unmappable_categories TEXT NOT NULL DEFAULT '',
            deleted INTEGER NOT NULL DEFAULT 0,
            updated_at TEXT NOT NULL,
            PRIMARY KEY (list_id, uid)
         );
         CREATE TABLE IF NOT EXISTS caldav_list_state (
            list_id TEXT PRIMARY KEY,
            ctag TEXT,
            updated_at TEXT NOT NULL
         );",
    )?;
    nobo_core::core::db::ensure_columns(conn, "caldav_state", ADDED_LATER)
}

/// The collection tag of a list as of the last sync. When the server still
/// reports the same one, nothing changed remotely and the whole per-task
/// round trip can be skipped.
pub fn get_ctag(conn: &Connection, list_id: &str) -> rusqlite::Result<Option<String>> {
    conn.query_row("SELECT ctag FROM caldav_list_state WHERE list_id = ?1", params![list_id], |row| row.get(0))
        .optional()
        .map(Option::flatten)
}

pub fn set_ctag(conn: &Connection, list_id: &str, ctag: Option<&str>) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO caldav_list_state (list_id, ctag, updated_at)
         VALUES (?1, ?2, datetime('now'))
         ON CONFLICT(list_id) DO UPDATE SET ctag = excluded.ctag, updated_at = excluded.updated_at",
        params![list_id, ctag],
    )?;
    Ok(())
}

pub fn get_all(conn: &Connection, list_id: &str) -> rusqlite::Result<Vec<TaskState>> {
    let mut stmt = conn.prepare(
        "SELECT list_id, uid, etag, url, local_hash, sort_order, position, unmappable_categories, deleted
         FROM caldav_state WHERE list_id = ?1",
    )?;
    let rows = stmt
        .query_map(params![list_id], |row| {
            let categories: String = row.get(7)?;
            Ok(TaskState {
                list_id: row.get(0)?,
                uid: row.get(1)?,
                etag: row.get(2)?,
                url: row.get(3)?,
                local_hash: row.get(4)?,
                sort_order: row.get(5)?,
                position: row.get(6)?,
                unmappable_categories: decode_categories(&categories),
                deleted: row.get::<_, i64>(8)? != 0,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn upsert(conn: &Connection, state: &TaskState) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO caldav_state (list_id, uid, etag, url, local_hash, sort_order, position, unmappable_categories, deleted, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, datetime('now'))
         ON CONFLICT(list_id, uid) DO UPDATE SET
            etag = excluded.etag,
            url = excluded.url,
            local_hash = excluded.local_hash,
            sort_order = excluded.sort_order,
            position = excluded.position,
            unmappable_categories = excluded.unmappable_categories,
            deleted = excluded.deleted,
            updated_at = excluded.updated_at",
        params![
            state.list_id,
            state.uid,
            state.etag,
            state.url,
            state.local_hash,
            state.sort_order,
            state.position,
            encode_categories(&state.unmappable_categories),
            i64::from(state.deleted),
        ],
    )?;
    Ok(())
}

/// Drops a tombstone once both sides agree the task is gone. Called only
/// after a successful sync, when nothing can resurrect it any more.
pub fn forget(conn: &Connection, list_id: &str, uid: &str) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM caldav_state WHERE list_id = ?1 AND uid = ?2", params![list_id, uid])?;
    Ok(())
}

/// Everything about one list, for a subscription the user removed.
pub fn forget_list(conn: &Connection, list_id: &str) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM caldav_state WHERE list_id = ?1", params![list_id])?;
    conn.execute("DELETE FROM caldav_list_state WHERE list_id = ?1", params![list_id])?;
    Ok(())
}

/// Newline-separated: a category may contain a comma (that is exactly why
/// it has no org tag form), so a comma cannot be the separator here.
fn encode_categories(categories: &[String]) -> String {
    categories.join("\n")
}

fn decode_categories(raw: &str) -> Vec<String> {
    raw.split('\n').filter(|c| !c.is_empty()).map(String::from).collect()
}

/// Identifies the org block this device last wrote for a task.
///
/// Deliberately a hash of the text, not a timestamp: see the module doc.
pub fn hash_block(block: &str) -> String {
    blake3::hash(block.as_bytes()).to_hex().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape the table had before it recorded positions -- a database
    /// created by that build must keep working, not lose every CalDAV
    /// sync to "no such column: position".
    #[test]
    fn a_table_from_an_older_build_is_migrated_instead_of_left_broken() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE caldav_state (
                list_id TEXT NOT NULL,
                uid TEXT NOT NULL,
                etag TEXT NOT NULL,
                url TEXT NOT NULL DEFAULT '',
                local_hash TEXT NOT NULL,
                sort_order INTEGER,
                unmappable_categories TEXT NOT NULL DEFAULT '',
                deleted INTEGER NOT NULL DEFAULT 0,
                updated_at TEXT NOT NULL,
                PRIMARY KEY (list_id, uid)
             )",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO caldav_state (list_id, uid, etag, local_hash, updated_at)
             VALUES ('list-1', 'uid-1', '\"v1\"', 'hash', datetime('now'))",
            [],
        )
        .unwrap();

        init_schema(&conn).unwrap();

        let rows = get_all(&conn, "list-1").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].uid, "uid-1");
        assert_eq!(rows[0].position, 0);
    }

    fn conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        conn
    }

    fn state(uid: &str) -> TaskState {
        TaskState {
            list_id: "list-1".to_string(),
            uid: uid.to_string(),
            etag: "\"v1\"".to_string(),
            url: format!("https://cloud.example.com/dav/einkauf/{uid}.ics"),
            local_hash: hash_block("* TODO Milch\n"),
            sort_order: Some(1000),
            position: 0,
            unmappable_categories: Vec::new(),
            deleted: false,
        }
    }

    #[test]
    fn a_task_round_trips_through_the_table() {
        let conn = conn();
        let mut expected = state("abc");
        expected.unmappable_categories = vec!["Haus, Garten".to_string()];
        upsert(&conn, &expected).unwrap();

        assert_eq!(get_all(&conn, "list-1").unwrap(), vec![expected]);
    }

    #[test]
    fn upserting_the_same_task_updates_rather_than_duplicates() {
        let conn = conn();
        upsert(&conn, &state("abc")).unwrap();
        let mut newer = state("abc");
        newer.etag = "\"v2\"".to_string();
        upsert(&conn, &newer).unwrap();

        let all = get_all(&conn, "list-1").unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].etag, "\"v2\"");
    }

    #[test]
    fn lists_do_not_see_each_others_tasks() {
        let conn = conn();
        upsert(&conn, &state("abc")).unwrap();
        let mut other = state("abc");
        other.list_id = "list-2".to_string();
        upsert(&conn, &other).unwrap();

        assert_eq!(get_all(&conn, "list-1").unwrap().len(), 1);
        assert_eq!(get_all(&conn, "list-2").unwrap().len(), 1);
    }

    /// The record has to outlive the task's disappearance from the file:
    /// without a row, "gone from the file" cannot be told apart from "new
    /// on the server", and the deletion is either resurrected or
    /// propagated the wrong way. Only `forget` drops it, once both sides
    /// agree.
    #[test]
    fn a_task_keeps_its_row_until_both_sides_agree_it_is_gone() {
        let conn = conn();
        upsert(&conn, &state("abc")).unwrap();

        assert_eq!(get_all(&conn, "list-1").unwrap().len(), 1);

        forget(&conn, "list-1", "abc").unwrap();

        assert!(get_all(&conn, "list-1").unwrap().is_empty());
    }

    #[test]
    fn removing_a_subscription_clears_its_whole_state() {
        let conn = conn();
        upsert(&conn, &state("abc")).unwrap();
        set_ctag(&conn, "list-1", Some("ctag-1")).unwrap();

        forget_list(&conn, "list-1").unwrap();

        assert!(get_all(&conn, "list-1").unwrap().is_empty());
        assert_eq!(get_ctag(&conn, "list-1").unwrap(), None);
    }

    #[test]
    fn the_ctag_survives_a_round_trip_and_starts_out_unknown() {
        let conn = conn();
        assert_eq!(get_ctag(&conn, "list-1").unwrap(), None);

        set_ctag(&conn, "list-1", Some("ctag-1")).unwrap();
        assert_eq!(get_ctag(&conn, "list-1").unwrap(), Some("ctag-1".to_string()));

        set_ctag(&conn, "list-1", Some("ctag-2")).unwrap();
        assert_eq!(get_ctag(&conn, "list-1").unwrap(), Some("ctag-2".to_string()));
    }

    /// A category with a comma is precisely the case that has no org tag
    /// form, so a comma cannot be the separator that stores it.
    #[test]
    fn a_category_containing_a_comma_survives_storage() {
        let conn = conn();
        let mut with_comma = state("abc");
        with_comma.unmappable_categories = vec!["Haus, Garten".to_string(), "Büro".to_string()];
        upsert(&conn, &with_comma).unwrap();

        assert_eq!(get_all(&conn, "list-1").unwrap()[0].unmappable_categories, with_comma.unmappable_categories);
    }

    #[test]
    fn the_block_hash_reacts_to_any_change_and_only_to_changes() {
        assert_eq!(hash_block("* TODO Milch\n"), hash_block("* TODO Milch\n"));
        assert_ne!(hash_block("* TODO Milch\n"), hash_block("* DONE Milch\n"));
    }
}
