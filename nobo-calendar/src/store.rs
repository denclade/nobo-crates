//! What the last fetch of a feed left behind, so the next one can ask the
//! server "has this changed?" instead of downloading it again.
//!
//! A subscribed holiday or school-term calendar is a megabyte or two that
//! changes a few times a year. Downloading and parsing it on every refresh
//! is the actual battery cost of calendar subscriptions -- the request
//! itself is nothing next to it.
//!
//! Kept out of the vault deliberately: these are HTTP validators for one
//! device's last download, not part of the subscription (which lives in
//! the mirror file, see registry). Syncing them would tell another device
//! its own copy is current when it has none.

use rusqlite::{params, Connection, OptionalExtension};

/// Bumped when this side changes what it makes of a given download.
///
/// A validator answers "has the SERVER changed", which is only half of "is
/// the mirror still right" -- the other half is whether it was written
/// correctly at all. A stamp that no longer matches simply misses, and the
/// feed is fetched and written once more. Same guard as the CalDAV
/// calendars' (see caldav::events).
const MIRROR_FORMAT: &str = "v1";

#[derive(Debug, Clone, PartialEq)]
pub struct FeedState {
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    /// How many entries the mirror holds, so a refresh that changes
    /// nothing can still report what is in there.
    pub event_count: usize,
}

pub fn init_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS calendar_state (
            id TEXT PRIMARY KEY,
            etag TEXT,
            last_modified TEXT,
            event_count INTEGER NOT NULL DEFAULT 0,
            format TEXT NOT NULL DEFAULT '',
            updated_at TEXT NOT NULL
         );",
    )
}

/// What the last download of this feed left, or `None` when there is
/// nothing to go on -- including a row written by an older version of the
/// renderer (see MIRROR_FORMAT).
pub fn get(conn: &Connection, id: &str) -> rusqlite::Result<Option<FeedState>> {
    conn.query_row(
        "SELECT etag, last_modified, event_count FROM calendar_state WHERE id = ?1 AND format = ?2",
        params![id, MIRROR_FORMAT],
        |row| {
            Ok(FeedState {
                etag: row.get(0)?,
                last_modified: row.get(1)?,
                event_count: row.get::<_, i64>(2)? as usize,
            })
        },
    )
    .optional()
}

pub fn set(conn: &Connection, id: &str, state: &FeedState) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO calendar_state (id, etag, last_modified, event_count, format, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, datetime('now'))
         ON CONFLICT(id) DO UPDATE SET
            etag = excluded.etag,
            last_modified = excluded.last_modified,
            event_count = excluded.event_count,
            format = excluded.format,
            updated_at = excluded.updated_at",
        params![id, state.etag, state.last_modified, state.event_count as i64, MIRROR_FORMAT],
    )?;
    Ok(())
}

/// Everything about one feed, for a subscription that was removed.
pub fn forget(conn: &Connection, id: &str) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM calendar_state WHERE id = ?1", params![id])?;
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

    fn state() -> FeedState {
        FeedState { etag: Some("\"abc\"".into()), last_modified: None, event_count: 42 }
    }

    #[test]
    fn what_was_stored_comes_back() {
        let conn = conn();
        set(&conn, "feed-1", &state()).unwrap();

        assert_eq!(get(&conn, "feed-1").unwrap(), Some(state()));
    }

    #[test]
    fn a_feed_that_was_never_fetched_has_nothing() {
        assert_eq!(get(&conn(), "feed-1").unwrap(), None);
    }

    /// The repair path: a row written before the renderer changed must not
    /// keep a stale mirror alive by answering "unchanged" forever.
    #[test]
    fn a_row_from_an_older_format_is_ignored() {
        let conn = conn();
        conn.execute(
            "INSERT INTO calendar_state (id, etag, event_count, format, updated_at)
             VALUES ('feed-1', '\"abc\"', 42, 'v0', datetime('now'))",
            [],
        )
        .unwrap();

        assert_eq!(get(&conn, "feed-1").unwrap(), None);
    }

    #[test]
    fn forgetting_a_feed_leaves_nothing_behind() {
        let conn = conn();
        set(&conn, "feed-1", &state()).unwrap();

        forget(&conn, "feed-1").unwrap();

        assert_eq!(get(&conn, "feed-1").unwrap(), None);
    }
}
