use std::path::PathBuf;
use rusqlite::Connection;
use crate::config::settings::config_dir;

pub fn history_db_path() -> PathBuf {
    config_dir().join("nobo.sqlite")
}

pub fn open_connection() -> rusqlite::Result<Connection> {
    open_at(history_db_path())
}

/// The same, at a path the caller already knows.
///
/// The background worker is handed its config directory rather than
/// reading the process-wide one (see features::background), so it opens the
/// database it was told about, not the one a previous call happened to
/// register first.
pub fn open_at(path: PathBuf) -> rusqlite::Result<Connection> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let conn = Connection::open(path)?;

    conn.execute_batch(
        // busy_timeout, because there is more than one connection on this
        // file: on Android the background worker (see features::background)
        // runs inside the app's own process and opens its own. WAL lets
        // them read concurrently, but a writer still locks out a writer --
        // without a timeout the loser fails instantly with SQLITE_BUSY
        // instead of waiting the moment out.
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA busy_timeout = 5000;
         PRAGMA foreign_keys = ON;"
    )?;
    
    Ok(conn)
}
/// Adds columns a table is missing, so a schema that grew after a database
/// was created still matches the code that reads it.
///
/// `CREATE TABLE IF NOT EXISTS` is a no-op on a table that already exists
/// -- including one created before a column was added. Every statement
/// naming the new column then fails with "no such column", which is how a
/// finished feature can be dead on exactly the installations that used its
/// predecessor. SQLite has no `ADD COLUMN IF NOT EXISTS`, so the existing
/// columns have to be read first.
///
/// Each definition must carry a default (or be nullable): SQLite cannot
/// add a `NOT NULL` column to a table that already has rows otherwise.
/// Used by the feature stores; a build without any of them keeps it
/// rather than growing a second way to create tables.
#[allow(dead_code)]
pub fn ensure_columns(conn: &Connection, table: &str, columns: &[(&str, &str)]) -> rusqlite::Result<()> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let existing: Vec<String> =
        stmt.query_map([], |row| row.get::<_, String>(1))?.collect::<Result<_, _>>()?;

    for (name, definition) in columns {
        if existing.iter().any(|e| e == name) {
            continue;
        }
        conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {name} {definition}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The case that happened: a table created by an older build, read by
    /// a newer one that knows one more column.
    #[test]
    fn a_column_added_after_the_table_was_created_is_filled_in() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE t (id TEXT PRIMARY KEY, a TEXT NOT NULL)").unwrap();
        conn.execute("INSERT INTO t (id, a) VALUES ('1', 'x')", []).unwrap();

        ensure_columns(&conn, "t", &[("b", "INTEGER NOT NULL DEFAULT 0")]).unwrap();

        let b: i64 = conn.query_row("SELECT b FROM t WHERE id = '1'", [], |r| r.get(0)).unwrap();
        assert_eq!(b, 0, "the existing row keeps its data and gets the default");
    }

    #[test]
    fn a_table_that_is_already_current_is_left_alone() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE t (id TEXT PRIMARY KEY, b INTEGER NOT NULL DEFAULT 0)").unwrap();
        conn.execute("INSERT INTO t (id, b) VALUES ('1', 7)", []).unwrap();

        ensure_columns(&conn, "t", &[("b", "INTEGER NOT NULL DEFAULT 0")]).unwrap();

        let b: i64 = conn.query_row("SELECT b FROM t WHERE id = '1'", [], |r| r.get(0)).unwrap();
        assert_eq!(b, 7, "an existing column is not reset");
    }
}

/// One store's tables, registered by whoever owns them.
///
/// The database file is the core's; what is in it is not. A feature that
/// keeps a table announces it the same way it announces a file kind or a
/// migration, and the app opens the connection without knowing who will
/// want a table in it:
///
/// ```ignore
/// inventory::submit! {
///     nobo_core::core::db::Schema {
///         owner: "caldav",
///         init: |connection| store::init_schema(connection),
///     }
/// }
/// ```
///
/// Must be idempotent: it runs on every start, and the tables are usually
/// there already.
pub struct Schema {
    /// Whose tables these are -- for the log line when one fails.
    pub owner: &'static str,
    pub init: fn(&Connection) -> rusqlite::Result<()>,
}

inventory::collect!(Schema);

/// Creates every registered store's tables.
///
/// A failure is logged and skipped rather than fatal: one feature's
/// broken table must not stop the app from opening notes. Whatever needed
/// that table will fail loudly on its own.
pub fn init_registered_schemas(connection: &Connection) {
    for schema in inventory::iter::<Schema> {
        if let Err(e) = (schema.init)(connection) {
            log::error!("could not prepare the {} tables: {e}", schema.owner);
        }
    }
}
