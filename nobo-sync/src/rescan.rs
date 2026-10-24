// SPDX-FileCopyrightText: 2026 Dennis Burgermeister
// SPDX-License-Identifier: GPL-3.0-or-later

//! Brings `sync_state`'s `local_hash`/`deleted_at` columns up to date with
//! what's actually on disk. Called on demand before every sync run (see
//! Module 4's orchestrator) rather than continuously watched -- there's no
//! mobile equivalent of the desktop `notify` watcher (see docs/mobile.md),
//! so "rescan on demand" is the one strategy that works identically on
//! every platform, not a mobile-only compromise bolted on afterward.

use rusqlite::Connection;
use std::collections::HashSet;
use std::path::Path;

use super::{hashing, store};
use nobo_core::utils::fs_utils;

pub fn rescan_local_state(conn: &Connection, vault_root: &Path) -> Result<(), String> {
    let entries = fs_utils::get_file_entries_recursive(vault_root);
    let mut seen = HashSet::new();

    // One transaction for the whole rescan. The record functions below do
    // a single plain `execute` each, so outside an explicit transaction
    // every row write is its own fsynced commit -- thousands of sequential
    // fsyncs for a large vault, on the one connection whose mutex this
    // function holds for its entire duration. Much worse on mobile, where
    // fsync latency is far higher than on a desktop SSD (see `mod perf`
    // for the synthetic-vault
    // benchmark this was verified against). `unchecked_transaction` (not
    // `transaction`, which needs `&mut Connection`) is the right call here:
    // the caller already holds this connection under exclusive Mutex lock
    // for the whole call, so there's no other writer that could interleave.
    let tx = conn.unchecked_transaction().map_err(|e| e.to_string())?;

    for entry in entries {
        if entry.is_dir {
            continue;
        }
        let abs_path = Path::new(&entry.path);
        let Ok(relative) = abs_path.strip_prefix(vault_root) else { continue };
        // Forward-slash-normalized so the SAME relative path is used
        // regardless of which OS/device produced it -- Windows would
        // otherwise store `notes\a.org` while macOS/Linux store
        // `notes/a.org` for the identical file, permanently desyncing them.
        let rel_path = relative.to_string_lossy().replace('\\', "/");

        let hash = hashing::hash_file(abs_path)?;
        store::record_local_change(&tx, &rel_path, &hash).map_err(|e| e.to_string())?;
        seen.insert(rel_path);
    }

    // Anything tracked from a previous rescan but not seen this time was
    // deleted since then -- record the tombstone so the diff can propagate
    // it (see diff.rs) instead of silently forgetting it ever existed.
    for row in store::get_all(&tx).map_err(|e| e.to_string())? {
        if row.deleted_at.is_none() && !seen.contains(&row.path) {
            store::record_local_delete(&tx, &row.path).map_err(|e| e.to_string())?;
        }
    }

    tx.commit().map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        store::init_schema(&conn).unwrap();
        conn
    }

    #[test]
    fn rescan_picks_up_new_files() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.org"), "hello").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/b.org"), "world").unwrap();

        let conn = conn();
        rescan_local_state(&conn, dir.path()).unwrap();

        let mut rows = store::get_all(&conn).unwrap();
        rows.sort_by(|a, b| a.path.cmp(&b.path));
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].path, "a.org");
        assert_eq!(rows[0].local_hash.as_deref(), Some(hashing::hash_bytes(b"hello").as_str()));
        assert_eq!(rows[1].path, "sub/b.org");
    }

    #[test]
    fn rescan_detects_content_changes() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("a.org");
        std::fs::write(&file, "v1").unwrap();

        let conn = conn();
        rescan_local_state(&conn, dir.path()).unwrap();
        let hash_v1 = store::get(&conn, "a.org").unwrap().unwrap().local_hash;

        std::fs::write(&file, "v2").unwrap();
        rescan_local_state(&conn, dir.path()).unwrap();
        let hash_v2 = store::get(&conn, "a.org").unwrap().unwrap().local_hash;

        assert_ne!(hash_v1, hash_v2);
    }

    #[test]
    fn rescan_tombstones_files_removed_from_disk() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("a.org");
        std::fs::write(&file, "hello").unwrap();

        let conn = conn();
        rescan_local_state(&conn, dir.path()).unwrap();
        assert!(store::get(&conn, "a.org").unwrap().unwrap().deleted_at.is_none());

        std::fs::remove_file(&file).unwrap();
        rescan_local_state(&conn, dir.path()).unwrap();

        let row = store::get(&conn, "a.org").unwrap().unwrap();
        assert!(row.deleted_at.is_some());
        assert!(row.local_hash.is_none());
    }

    #[test]
    fn rescan_clears_a_tombstone_when_the_file_reappears() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("a.org");
        std::fs::write(&file, "hello").unwrap();

        let conn = conn();
        rescan_local_state(&conn, dir.path()).unwrap();
        std::fs::remove_file(&file).unwrap();
        rescan_local_state(&conn, dir.path()).unwrap();
        assert!(store::get(&conn, "a.org").unwrap().unwrap().deleted_at.is_some());

        std::fs::write(&file, "back again").unwrap();
        rescan_local_state(&conn, dir.path()).unwrap();

        let row = store::get(&conn, "a.org").unwrap().unwrap();
        assert!(row.deleted_at.is_none());
        assert!(row.local_hash.is_some());
    }

    /* --- performance (regression canary, not a strict SLA) -- see
     * indexer.rs's own `mod perf` for the shared philosophy: synthetic
     * data in an isolated TempDir, a generous threshold meant to catch a
     * genuine regression rather than assert a tight timing budget. */
    mod perf {
        use super::*;
        use std::time::Instant;

        const SYNTHETIC_FILE_COUNT: usize = 3000;

        #[test]
        fn perf_rescan_with_several_thousand_files() {
            let dir = TempDir::new().unwrap();
            for i in 0..SYNTHETIC_FILE_COUNT {
                let sub = dir.path().join(format!("folder-{}", i / 100));
                std::fs::create_dir_all(&sub).unwrap();
                std::fs::write(sub.join(format!("note-{i}.org")), format!("* TODO Task {i}\nBody text.\n")).unwrap();
            }

            // A REAL, file-backed connection -- not conn()'s in-memory one.
            // Each individual (non-transactional) write below pays a real
            // fsync against actual storage this way, which is the whole
            // cost this test exists to catch; an in-memory DB has none of
            // that overhead and would silently hide a lost transaction. A
            // SEPARATE TempDir, not a file inside `dir` -- rescan would
            // otherwise pick up the db file itself as one more vault entry.
            let db_dir = TempDir::new().unwrap();
            let db_path = db_dir.path().join("sync_state.sqlite");
            let file_conn = Connection::open(&db_path).unwrap();
            store::init_schema(&file_conn).unwrap();

            let start = Instant::now();
            rescan_local_state(&file_conn, dir.path()).unwrap();
            let elapsed = start.elapsed();
            eprintln!("perf_rescan_with_several_thousand_files: {elapsed:?} for {SYNTHETIC_FILE_COUNT} files (file-backed db)");

            assert_eq!(store::get_all(&file_conn).unwrap().len(), SYNTHETIC_FILE_COUNT);
            // Generous, but tight enough to have actually caught the
            // per-file-individual-commit issue this was written to guard
            // against (see rescan_local_state's own doc comment on the fix):
            // several thousand individual auto-committed (fsync'd) SQLite
            // writes measurably exceeds this on real disk, even though the
            // hashing itself is cheap.
            assert!(elapsed.as_secs() < 8, "rescan_local_state took {elapsed:?} for {SYNTHETIC_FILE_COUNT} files -- investigate for a lost transaction/complexity regression");
        }
    }
}
