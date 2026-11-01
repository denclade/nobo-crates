// SPDX-FileCopyrightText: 2026 Dennis Burgermeister
// SPDX-License-Identifier: GPL-3.0-or-later

//! The local recycle bin, independent of history's size/count retention.
//! A durable pre-image is committed BEFORE a destructive write. Attachment
//! bytes are likewise committed before unlinking. Expiry uses UTC instants.

mod reinsert;
#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde::Serialize;

use super::attachments;
use super::indexer::IndexManager;
use super::models::{FileSnapshot, HistoryEntry, HistoryEntryKind};
use crate::config::settings::Config;
use crate::utils::{fs_utils, ToCmdResult};

pub const DEFAULT_RETENTION_DAYS: u32 = 31;

#[derive(Debug, Clone, Serialize, specta::Type)]
pub struct TrashEntry {
    pub id: String,
    pub label: String,
    pub original_path: String,
    pub deleted_at: String,
    pub expires_at: String,
    pub attachment_count: u32,
}

inventory::submit! { super::db::Schema { owner: "trash", init: init_schema } }

pub fn init_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS trash_entries (
            id TEXT PRIMARY KEY, root TEXT NOT NULL, label TEXT NOT NULL,
            deleted_at INTEGER NOT NULL, expires_at INTEGER NOT NULL,
            pending INTEGER NOT NULL DEFAULT 1, restored INTEGER NOT NULL DEFAULT 0,
            history_id INTEGER, attachments_dir TEXT NOT NULL,
            history_reversed INTEGER NOT NULL DEFAULT 0
         );
         CREATE TABLE IF NOT EXISTS trash_files (
            entry_id TEXT NOT NULL REFERENCES trash_entries(id) ON DELETE CASCADE,
            path TEXT NOT NULL, before_bytes BLOB NOT NULL, after_bytes BLOB,
            role TEXT NOT NULL, PRIMARY KEY(entry_id, path)
         );
         CREATE INDEX IF NOT EXISTS idx_trash_expiry ON trash_entries(expires_at);
         CREATE INDEX IF NOT EXISTS idx_trash_file_path ON trash_files(path);",
    )?;
    let has_direction: bool =
        conn.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_info('trash_entries') WHERE name = 'history_reversed')", [], |r| {
            r.get(0)
        })?;
    if !has_direction {
        conn.execute_batch("ALTER TABLE trash_entries ADD COLUMN history_reversed INTEGER NOT NULL DEFAULT 0")?;
    }
    Ok(())
}

#[derive(Clone)]
pub struct Policy {
    pub root: PathBuf,
    pub attachments_dir: String,
    pub retention_days: u32,
}

impl Policy {
    pub fn new(root: &Path, attachments_dir: &str, retention_days: u32) -> Self {
        Self {
            root: fs_utils::canonicalize_or_self(root),
            attachments_dir: attachments_dir.to_string(),
            retention_days: retention_days.clamp(1, 3650),
        }
    }

    pub fn configured(index: &IndexManager) -> Option<Self> {
        let config = Config::load();
        let root = index.vault_root().or_else(|| config.as_ref()?.vault_root().ok())?;
        Some(Self::new(
            &root,
            config.as_ref().map_or(".attachments", |c| c.attachments_dir.as_str()),
            config.as_ref().map_or(DEFAULT_RETENTION_DAYS, |c| c.trash_retention_days),
        ))
    }

    pub fn for_path(index: &IndexManager, path: &Path) -> Option<Self> {
        Self::configured(index).filter(|policy| attachments::normalize(path).starts_with(&policy.root))
    }
}

/// A backup must reach stable storage before the filesystem mutation.
/// The app normally uses WAL/NORMAL; that only promises durability after
/// a checkpoint. FULL syncs this commit's WAL before we unlink any bytes.
fn durable_transaction<T>(conn: &Connection, work: impl FnOnce(&Transaction<'_>) -> Result<T, String>) -> Result<T, String> {
    if !conn.is_autocommit() {
        return Err("A recycle-bin backup requires its own durable transaction".to_string());
    }
    let previous: i64 = conn.pragma_query_value(None, "synchronous", |row| row.get(0)).to_cmd()?;
    struct RestoreSync<'a>(&'a Connection, i64);
    impl Drop for RestoreSync<'_> {
        fn drop(&mut self) {
            if let Err(e) = self.0.pragma_update(None, "synchronous", self.1) {
                log::warn!("Could not restore SQLite synchronous setting: {e}");
            }
        }
    }
    conn.pragma_update(None, "synchronous", previous.max(2)).to_cmd()?;
    let _restore = RestoreSync(conn, previous);
    let tx = conn.unchecked_transaction().to_cmd()?;
    let result = work(&tx)?;
    tx.commit().to_cmd()?;
    Ok(result)
}

/// Kept on disk even if the process dies between the write and finish.
/// On the next list/maintenance pass pending entries are reconciled with
/// the real files. Unchanged files are discarded, actual deletions retained.
pub struct Protection {
    id: String,
    policy: Policy,
}

fn active() -> &'static Mutex<HashSet<String>> {
    static ACTIVE: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    ACTIVE.get_or_init(|| Mutex::new(HashSet::new()))
}

impl Drop for Protection {
    fn drop(&mut self) {
        active().lock().unwrap_or_else(|e| e.into_inner()).remove(&self.id);
    }
}

impl Protection {
    pub fn prepare(policy: &Policy, paths: &[&Path], db: &Mutex<Connection>) -> Result<Self, String> {
        let conn = db.lock().map_err(|e| e.to_string())?;
        Self::prepare_with_connection(policy, paths, &conn)
    }

    fn prepare_with_connection(policy: &Policy, paths: &[&Path], conn: &Connection) -> Result<Self, String> {
        init_schema(conn).to_cmd()?;
        let id = uuid::Uuid::new_v4().to_string();
        let protection = Self { id: id.clone(), policy: policy.clone() };
        active().lock().unwrap_or_else(|e| e.into_inner()).insert(id.clone());
        let now = chrono::Utc::now().timestamp();
        durable_transaction(conn, |tx| {
            tx.execute(
                "INSERT INTO trash_entries(id, root, label, deleted_at, expires_at, attachments_dir) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    id,
                    policy.root.to_string_lossy(),
                    paths.first().map_or_else(String::new, fs_utils::filename_of),
                    now,
                    now + i64::from(policy.retention_days) * 86400,
                    policy.attachments_dir
                ],
            )
            .to_cmd()?;
            let mut protected: Vec<(PathBuf, &str)> = paths.iter().map(|path| (path.to_path_buf(), "owner")).collect();
            for path in paths {
                if !crate::utils::temp_file_utils::is_temp_file(&path.file_name().unwrap_or_default().to_string_lossy()) {
                    let draft = PathBuf::from(crate::utils::temp_file_utils::temp_path_for(&path.to_string_lossy()));
                    if draft.is_file() {
                        protected.push((draft, "draft"));
                    }
                }
            }
            for (path, role) in &protected {
                require_safe_path(&policy.root, path)?;
                let bytes = match fs::read(path) {
                    Ok(bytes) => bytes,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(e) => return Err(e.to_string()),
                };
                tx.execute(
                    "INSERT INTO trash_files(entry_id, path, before_bytes, role) VALUES (?1, ?2, ?3, ?4)",
                    params![id, path.to_string_lossy(), bytes, role],
                )
                .to_cmd()?;
            }
            Ok(())
        })?;
        Ok(protection)
    }

    pub fn finish(self, kind: Option<&HistoryEntryKind>, index: &IndexManager, db: &Mutex<Connection>) -> Result<(), String> {
        let conn = db.lock().map_err(|e| e.to_string())?;
        finish(&conn, &self.id, &self.policy, kind, Some(index), true)
    }
}

#[derive(Debug)]
struct StoredFile {
    path: PathBuf,
    before: Vec<u8>,
    after: Option<Vec<u8>>,
    role: String,
}

fn files(conn: &Connection, id: &str) -> Result<Vec<StoredFile>, String> {
    let mut stmt =
        conn.prepare("SELECT path, before_bytes, after_bytes, role FROM trash_files WHERE entry_id = ?1 ORDER BY role, path").to_cmd()?;
    let result = stmt
        .query_map([id], |r| {
            Ok(StoredFile { path: PathBuf::from(r.get::<_, String>(0)?), before: r.get(1)?, after: r.get(2)?, role: r.get(3)? })
        })
        .to_cmd()?
        .collect::<rusqlite::Result<Vec<_>>>()
        .to_cmd()?;
    Ok(result)
}

fn text_snapshot(file: &StoredFile, after: Option<&[u8]>) -> FileSnapshot {
    FileSnapshot {
        path: file.path.to_string_lossy().into_owned(),
        before: String::from_utf8(file.before.clone()).ok(),
        after: after.and_then(|bytes| String::from_utf8(bytes.to_vec()).ok()),
    }
}

fn heading_count(text: &str) -> usize {
    super::org_parser::parse_headings_from_lines(Path::new("trash.org"), text.lines()).len()
}

fn finish(
    conn: &Connection,
    id: &str,
    policy: &Policy,
    kind: Option<&HistoryEntryKind>,
    index: Option<&IndexManager>,
    mutate_files: bool,
) -> Result<(), String> {
    let stored = files(conn, id)?;
    let mut snapshots = Vec::new();
    let mut changed = false;
    let mut structural = matches!(kind, Some(HistoryEntryKind::HeadingDeleted { .. } | HistoryEntryKind::HeadingsBulkDeleted { .. }));
    let mut label = None;
    for file in &stored {
        if file.role != "owner" && file.role != "draft" {
            continue;
        }
        let after = match fs::read(&file.path) {
            Ok(bytes) => Some(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.to_string()),
        };
        changed |= after.as_deref() != Some(file.before.as_slice());
        let snap = text_snapshot(file, after.as_deref());
        if label.is_none() {
            label = reinsert::deleted_title(&file.path, snap.before.as_deref().unwrap_or(""), snap.after.as_deref().unwrap_or(""));
        }
        structural |= file.role == "owner" && after.is_none();
        if file.role == "owner" && heading_count(snap.before.as_deref().unwrap_or("")) > heading_count(snap.after.as_deref().unwrap_or(""))
        {
            structural = true;
        }
        conn.execute(
            "UPDATE trash_files SET after_bytes = ?1 WHERE entry_id = ?2 AND path = ?3",
            params![after, id, file.path.to_string_lossy()],
        )
        .to_cmd()?;
        snapshots.push(snap);
    }
    if !changed {
        return remove_entry(conn, id);
    }
    if mutate_files {
        recover_references(conn, &policy.root, &snapshots, index)?;
    }
    if let Some(HistoryEntryKind::HeadingDeleted { title, .. }) = kind {
        label = Some(title.clone());
    }
    let owners: Vec<_> =
        snapshots.iter().filter(|snap| stored.iter().any(|f| f.role == "owner" && f.path == Path::new(&snap.path))).collect();
    let headings = |after: bool| {
        owners
            .iter()
            .flat_map(|snap| {
                let text = if after { &snap.after } else { &snap.before };
                super::org_parser::parse_headings_from_lines(Path::new(&snap.path), text.as_deref().unwrap_or("").lines())
            })
            .collect::<Vec<_>>()
    };
    let old_headings = headings(false);
    let new_headings = headings(true);
    // App headings carry stable IDs. Handwritten headings fall back to
    // their title; renaming one conservatively retains the old pre-image.
    let new_ids: HashSet<_> = new_headings.iter().filter_map(super::heading_identity::custom_id_of).collect();
    let title_counts = |headings: &[super::models::Heading]| {
        let mut counts = HashMap::new();
        for heading in headings {
            *counts.entry(heading.title.clone()).or_insert(0usize) += 1;
        }
        counts
    };
    let old_titles = title_counts(&old_headings);
    let new_titles = title_counts(&new_headings);
    let missing_identity = old_headings.iter().any(|old| {
        if let Some(id) = super::heading_identity::custom_id_of(old) {
            !new_ids.contains(id)
        } else {
            old_titles.get(&old.title).copied().unwrap_or_default() > new_titles.get(&old.title).copied().unwrap_or_default()
        }
    });
    if owners.len() > 1 {
        structural = owners.iter().any(|s| s.after.is_none())
            || owners.iter().map(|s| heading_count(s.before.as_deref().unwrap_or(""))).sum::<usize>()
                > owners.iter().map(|s| heading_count(s.after.as_deref().unwrap_or(""))).sum::<usize>();
    }
    structural |= missing_identity;
    // Reconciliation runs with a database connection, without the vault
    // writer lock. It may retain pre-images but must not mutate files or
    // collect attachments based on a concurrently changing reference scan.
    let candidates = if mutate_files {
        attachments::orphaned(&policy.root, &policy.attachments_dir, &snapshots).unwrap_or_else(|e| {
            log::warn!("Attachment reference scan failed; keeping files: {e}");
            Vec::new()
        })
    } else {
        Vec::new()
    };
    // Save every candidate before deleting any. A backup failure leaves
    // attachments in place and the pre-image recoverable in the bin.
    durable_transaction(conn, |tx| {
        for path in &candidates {
            let bytes = fs::read(path).map_err(|e| e.to_string())?;
            tx.execute(
                "INSERT OR IGNORE INTO trash_files(entry_id, path, before_bytes, role) VALUES (?1, ?2, ?3, 'attachment')",
                params![id, path.to_string_lossy(), bytes],
            )
            .to_cmd()?;
            share_retained_attachment(tx, &policy.root, path, &bytes)?;
        }
        tx.execute("UPDATE trash_entries SET pending = 0, label = COALESCE(?1, label) WHERE id = ?2", params![label, id]).to_cmd()?;
        if !structural {
            tx.execute("UPDATE trash_files SET role = 'context' WHERE entry_id = ?1 AND role = 'owner'", [id]).to_cmd()?;
            tx.execute("UPDATE trash_files SET role = 'draft_context' WHERE entry_id = ?1 AND role = 'draft'", [id]).to_cmd()?;
        }
        Ok(())
    })?;
    for path in candidates {
        match fs::remove_file(&path) {
            Ok(()) => {
                if let Some(index) = index {
                    index.remove_file(&path);
                }
            }
            Err(e) => log::warn!("Could not move attachment {} to trash: {e}", path.display()),
        }
    }
    if !structural && files(conn, id)?.iter().all(|f| f.role == "context" || f.role == "draft_context") {
        remove_entry(conn, id)?;
    }
    Ok(())
}

/// An earlier deleted note may still need an attachment that has only now
/// become unused. Keep its bytes with every such entry, so maintenance and
/// differing retention periods cannot break those notes' restoration.
fn share_retained_attachment(conn: &Connection, root: &Path, path: &Path, bytes: &[u8]) -> Result<(), String> {
    let owners: Vec<(String, String, Vec<u8>)> = {
        let mut stmt = conn
            .prepare(
                "SELECT f.entry_id, f.path, f.before_bytes FROM trash_files f JOIN trash_entries e ON e.id = f.entry_id
            WHERE e.root = ?1 AND e.expires_at > ?2 AND f.role IN ('owner', 'draft')",
            )
            .to_cmd()?;
        let rows = stmt
            .query_map(params![root.to_string_lossy(), chrono::Utc::now().timestamp()], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .to_cmd()?
            .collect::<rusqlite::Result<_>>()
            .to_cmd()?;
        rows
    };
    for (id, owner, text) in owners {
        if std::str::from_utf8(&text).is_ok_and(|text| attachments::references(Path::new(&owner), text).contains(path)) {
            conn.execute(
                "INSERT OR IGNORE INTO trash_files(entry_id, path, before_bytes, role) VALUES (?1, ?2, ?3, 'attachment')",
                params![id, path.to_string_lossy(), bytes],
            )
            .to_cmd()?;
        }
    }
    Ok(())
}

pub(crate) fn retain_attachment_for_deleted_entries(policy: &Policy, path: &Path, db: &Mutex<Connection>) -> Result<(), String> {
    let conn = db.lock().map_err(|e| e.to_string())?;
    init_schema(&conn).to_cmd()?;
    let bytes = fs::read(path).to_cmd()?;
    durable_transaction(&conn, |tx| share_retained_attachment(tx, &policy.root, path, &bytes))
}

fn remove_entry(conn: &Connection, id: &str) -> Result<(), String> {
    // Explicit children deletion also works for test/non-Tauri connections
    // that did not enable foreign_keys.
    let tx = conn.unchecked_transaction().to_cmd()?;
    tx.execute("DELETE FROM trash_files WHERE entry_id = ?1", [id]).to_cmd()?;
    tx.execute("DELETE FROM trash_entries WHERE id = ?1", [id]).to_cmd()?;
    tx.commit().to_cmd()
}

/// Permanent deletion must also invalidate undo snapshots capable of
/// bringing the deleted content back. Later edits remain in history.
fn forget_entry(conn: &Connection, id: &str) -> Result<(), String> {
    super::history::store::init_schema(conn).to_cmd()?;
    let tx = conn.unchecked_transaction().to_cmd()?;
    tx.execute(
        "DELETE FROM history_actions WHERE id IN (
        SELECT s.action_id FROM history_snapshots s JOIN trash_files f ON f.path = s.file_path
        JOIN trash_entries e ON e.id = f.entry_id
        WHERE e.id = ?1 AND e.restored = 0 AND f.role = 'owner'
          AND (e.history_id IS NULL OR s.action_id <= e.history_id)
    ) OR id = (SELECT history_id FROM trash_entries WHERE id = ?1 AND restored = 0)",
        [id],
    )
    .to_cmd()?;
    tx.execute("DELETE FROM trash_files WHERE entry_id = ?1", [id]).to_cmd()?;
    tx.execute("DELETE FROM trash_entries WHERE id = ?1", [id]).to_cmd()?;
    // Connections supplied by another host may have foreign_keys off.
    tx.execute("DELETE FROM history_snapshots WHERE action_id NOT IN (SELECT id FROM history_actions)", []).to_cmd()?;
    tx.commit().to_cmd()
}

fn require_safe_path(root: &Path, path: &Path) -> Result<(), String> {
    let path = attachments::normalize(path);
    if !path.starts_with(root) || path == root || path.strip_prefix(root).is_ok_and(|p| p.components().any(|c| c.as_os_str() == ".git")) {
        return Err(crate::t!("Pfad liegt nicht im Notiz-Ordner.", "Path is outside the notes vault."));
    }
    // Check every existing ancestor too, including a destination whose
    // own file no longer exists but whose folder became a symlink.
    let mut ancestor = Some(path.as_path());
    while let Some(p) = ancestor {
        if p.exists() {
            if !fs::canonicalize(p).map_err(|e| e.to_string())?.starts_with(root) {
                return Err(crate::t!("Pfad liegt nicht im Notiz-Ordner.", "Path is outside the notes vault."));
            }
            break;
        }
        ancestor = p.parent();
    }
    Ok(())
}

pub fn maintain(conn: &Connection, now: i64) -> Result<(), String> {
    init_schema(conn).to_cmd()?;
    let pending: Vec<(String, String, String)> = {
        let mut stmt = conn.prepare("SELECT id, root, attachments_dir FROM trash_entries WHERE pending = 1").to_cmd()?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).to_cmd()?.collect::<rusqlite::Result<_>>().to_cmd()?;
        rows
    };
    for (id, root, dir) in pending {
        if active().lock().unwrap_or_else(|e| e.into_inner()).contains(&id) {
            continue;
        }
        // An unavailable/removable vault is not evidence of deletion.
        if Path::new(&root).is_dir() {
            if let Err(e) = finish(conn, &id, &Policy::new(Path::new(&root), &dir, DEFAULT_RETENTION_DAYS), None, None, false) {
                log::warn!("Could not reconcile pending recycle-bin entry {id}: {e}");
            }
        }
    }
    let expired: Vec<String> = {
        let mut stmt = conn.prepare("SELECT id FROM trash_entries WHERE expires_at <= ?1 AND pending = 0").to_cmd()?;
        let rows = stmt.query_map([now], |r| r.get(0)).to_cmd()?.collect::<rusqlite::Result<_>>().to_cmd()?;
        rows
    };
    for id in expired {
        forget_entry(conn, &id)?;
    }
    Ok(())
}

pub fn list(conn: &Connection, root: &Path) -> Result<Vec<TrashEntry>, String> {
    maintain(conn, chrono::Utc::now().timestamp())?;
    let root = fs_utils::canonicalize_or_self(root);
    let mut stmt = conn
        .prepare(
            "SELECT e.id, e.label, e.deleted_at, e.expires_at,
            COALESCE((SELECT path FROM trash_files WHERE entry_id = e.id AND role IN ('owner', 'context') LIMIT 1),
                     (SELECT path FROM trash_files WHERE entry_id = e.id LIMIT 1)),
            (SELECT COUNT(*) FROM trash_files WHERE entry_id = e.id AND role = 'attachment')
         FROM trash_entries e WHERE e.root = ?1 AND e.pending = 0 AND e.restored = 0 ORDER BY e.deleted_at DESC, e.id",
        )
        .to_cmd()?;
    let rows = stmt
        .query_map([root.to_string_lossy()], |r| {
            let deleted: i64 = r.get(2)?;
            let expires: i64 = r.get(3)?;
            Ok(TrashEntry {
                id: r.get(0)?,
                label: r.get(1)?,
                original_path: r.get(4)?,
                deleted_at: chrono::DateTime::from_timestamp(deleted, 0).unwrap_or_default().to_rfc3339(),
                expires_at: chrono::DateTime::from_timestamp(expires, 0).unwrap_or_default().to_rfc3339(),
                attachment_count: r.get(5)?,
            })
        })
        .to_cmd()?
        .collect::<rusqlite::Result<_>>()
        .to_cmd()?;
    Ok(rows)
}

fn require_entry(conn: &Connection, root: &Path, id: &str) -> Result<(), String> {
    let present: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM trash_entries WHERE id = ?1 AND root = ?2 AND expires_at > ?3 AND restored = 0)",
            params![id, fs_utils::canonicalize_or_self(root).to_string_lossy(), chrono::Utc::now().timestamp()],
            |r| r.get(0),
        )
        .to_cmd()?;
    if !present {
        return Err(crate::t!("Papierkorb-Eintrag nicht gefunden oder bereits abgelaufen.", "Trash entry not found or already expired."));
    }
    Ok(())
}

/// A queued save can introduce a link just after another mutation removed
/// its last reference. Recover the still-retained bytes rather than saving
/// a broken link. Also used when restoring older history snapshots.
pub(crate) fn recover_references(
    conn: &Connection,
    root: &Path,
    snapshots: &[FileSnapshot],
    index: Option<&IndexManager>,
) -> Result<(), String> {
    for snapshot in snapshots {
        for path in attachments::references(Path::new(&snapshot.path), snapshot.after.as_deref().unwrap_or("")) {
            if path.exists() || !path.starts_with(root) {
                continue;
            }
            require_safe_path(root, &path)?;
            let bytes: Option<Vec<u8>> = conn
                .query_row(
                    "SELECT f.before_bytes FROM trash_files f JOIN trash_entries e ON e.id = f.entry_id
                WHERE f.path = ?1 AND f.role = 'attachment' AND e.root = ?2 AND e.expires_at > ?3
                ORDER BY e.deleted_at DESC LIMIT 1",
                    params![path.to_string_lossy(), root.to_string_lossy(), chrono::Utc::now().timestamp()],
                    |r| r.get(0),
                )
                .optional()
                .to_cmd()?;
            if let Some(bytes) = bytes {
                fs_utils::write_atomic_bytes(&path, &bytes).to_cmd()?;
                if let Some(index) = index {
                    index.update_file(&path);
                }
            }
        }
    }
    Ok(())
}

/// A restored entry's old backup may have expired while its history action
/// remained redoable. Redo is a new deletion then and needs a new durable
/// backup and retention period before it can remove the restored content.
pub(crate) fn protect_redo<T>(
    conn: &Connection,
    index: &IndexManager,
    entry: &HistoryEntry,
    work: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    let existing: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM trash_entries WHERE history_id = ?1 AND history_reversed = 0 AND expires_at > ?2)",
            params![entry.id, chrono::Utc::now().timestamp()],
            |r| r.get(0),
        )
        .to_cmd()?;
    if entry.is_draft || existing || entry.snapshots.is_empty() {
        return work();
    }
    let Some(policy) = Policy::configured(index) else { return work() };
    let paths: Vec<_> = entry.snapshots.iter().map(|s| Path::new(&s.path)).collect();
    if paths.iter().any(|p| !p.starts_with(&policy.root)) {
        return work();
    }
    let protection = Protection::prepare_with_connection(&policy, &paths, conn)?;
    let result = work();
    finish(conn, &protection.id, &policy, Some(&entry.kind), Some(index), true)?;
    associate(conn, entry.id, &entry.snapshots)?;
    result
}

/// Undoing file creation/duplication is itself a deletion. Keep that
/// file in the independent bin even when its redo history is later pruned.
/// Reverse-direction entries belong to the same action but their files
/// need restoration on redo, rather than on undo.
pub(crate) fn protect_undo<T>(
    conn: &Connection,
    index: &IndexManager,
    entry: &HistoryEntry,
    work: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    if entry.is_draft {
        return work();
    }
    let paths: Vec<_> = entry.snapshots.iter().filter(|s| s.before.is_none() && s.after.is_some()).map(|s| Path::new(&s.path)).collect();
    let Some(policy) = Policy::configured(index) else { return work() };
    if paths.is_empty() || paths.iter().any(|p| !p.starts_with(&policy.root)) {
        return work();
    }
    let protection = Protection::prepare_with_connection(&policy, &paths, conn)?;
    let result = work();
    finish(conn, &protection.id, &policy, None, Some(index), true)?;
    conn.execute("UPDATE trash_entries SET history_id = ?1, history_reversed = 1 WHERE id = ?2", params![entry.id, protection.id])
        .to_cmd()?;
    result
}

/// Reverse the deletion against today's file, preserving subsequent edits.
/// A conflict refuses the restore instead of overwriting newer content.
pub fn restore(conn: &Connection, root: &Path, id: &str, index: &IndexManager) -> Result<(), String> {
    maintain(conn, chrono::Utc::now().timestamp())?;
    require_entry(conn, root, id)?;
    let root = fs_utils::canonicalize_or_self(root);
    let stored = files(conn, id)?;
    let saved: HashSet<_> = stored.iter().filter(|f| f.role == "attachment").map(|f| f.path.clone()).collect();
    let mut writes: Vec<(PathBuf, Vec<u8>)> = Vec::new();
    for file in &stored {
        let path = if file.role == "draft_context" {
            let name = file.path.file_name().unwrap_or_default().to_string_lossy();
            let name = name.strip_prefix('#').and_then(|s| s.strip_suffix('#')).ok_or_else(|| "Invalid draft path".to_string())?;
            file.path.with_file_name(name)
        } else {
            file.path.clone()
        };
        require_safe_path(&root, &path)?;
        let current = match writes.iter().find(|(p, _)| p == &path).map(|(_, bytes)| Ok(bytes.clone())).unwrap_or_else(|| fs::read(&path)) {
            Ok(bytes) => Some(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.to_string()),
        };
        let bytes = if file.role == "context" || file.role == "draft_context" {
            let current = current.as_deref().and_then(|b| std::str::from_utf8(b).ok()).ok_or_else(|| {
                crate::t!("Die ursprüngliche Notiz muss zuerst wiederhergestellt werden.", "Restore the original note first.")
            })?;
            reinsert::restore_links(&path, std::str::from_utf8(&file.before).map_err(|e| e.to_string())?, current, &saved)?.into_bytes()
        } else if file.role == "attachment" || file.after.is_none() {
            if current.as_deref().is_some_and(|bytes| bytes != file.before) {
                return Err(crate::t!(
                    "Am ursprünglichen Pfad existiert bereits eine andere Datei: {}",
                    "A different file already exists at the original path: {}",
                    file.path.display()
                ));
            }
            file.before.clone()
        } else {
            let before = std::str::from_utf8(&file.before).map_err(|e| e.to_string())?;
            let after = std::str::from_utf8(file.after.as_deref().unwrap()).map_err(|e| e.to_string())?;
            let current = current.as_deref().and_then(|b| std::str::from_utf8(b).ok()).ok_or_else(|| {
                crate::t!(
                    "Die ursprüngliche Notiz fehlt oder ist keine Textdatei. Bitte zuerst wiederherstellen.",
                    "The original note is missing or is not a text file. Restore it first."
                )
            })?;
            if current == before {
                continue;
            }
            reinsert::reinsert(&file.path, before, after, current)?
                .map(Ok)
                .unwrap_or_else(|| {
                    diffy::apply(current, &diffy::create_patch(after, before)).map_err(|_| {
                        crate::t!(
                            "Die Datei wurde inzwischen geändert; Wiederherstellen würde Änderungen überschreiben: {}",
                            "The file has changed; restoring would overwrite edits: {}",
                            file.path.display()
                        )
                    })
                })?
                .into_bytes()
        };
        if let Some((_, existing)) = writes.iter_mut().find(|(p, _)| p == &path) {
            *existing = bytes;
        } else {
            writes.push((path, bytes));
        }
    }
    // All conflicts checked before changing any file; attachment bytes are
    // restored first so newly reinserted descriptions immediately work.
    for (path, bytes) in writes {
        fs_utils::write_atomic_bytes(&path, &bytes).to_cmd()?;
        index.update_file(&path);
    }
    conn.execute("UPDATE trash_entries SET restored = 1 WHERE id = ?1", [id]).to_cmd()?;
    Ok(())
}

pub fn purge(conn: &Connection, root: &Path, id: &str) -> Result<(), String> {
    require_entry(conn, root, id)?;
    forget_entry(conn, id)
}

/// Attach a completed bin entry to the corresponding global undo action.
/// The bin itself still survives history pruning and clearing the redo tail.
pub(crate) fn associate(conn: &Connection, history_id: i64, snapshots: &[FileSnapshot]) -> Result<(), String> {
    init_schema(conn).to_cmd()?;
    for snapshot in snapshots {
        conn.execute(
            "UPDATE trash_entries SET history_id = ?1 WHERE history_id IS NULL AND history_reversed = 0 AND pending = 0 AND id IN
            (SELECT entry_id FROM trash_files WHERE path = ?2 AND
                (role IN ('context', 'draft_context') OR after_bytes IS ?3))",
            params![history_id, snapshot.path, snapshot.after.as_deref().map(str::as_bytes)],
        )
        .to_cmd()?;
    }
    Ok(())
}

/// Hosts can call this once after opening their database. Retention lives
/// in core just like the bin, including hosts without a filesystem watcher.
pub fn start_maintenance(conn: &Connection) {
    if let Err(e) = maintain(conn, chrono::Utc::now().timestamp()) {
        log::warn!("Recycle bin maintenance failed: {e}");
    }
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        std::thread::spawn(|| loop {
            std::thread::sleep(std::time::Duration::from_secs(3600));
            match super::db::open_connection() {
                Ok(conn) => {
                    if let Err(e) = maintain(&conn, chrono::Utc::now().timestamp()) {
                        log::warn!("Recycle bin maintenance failed: {e}");
                    }
                }
                Err(e) => log::warn!("Recycle bin maintenance could not open database: {e}"),
            }
        });
    });
}

/// Undo needs binary attachments as well as history's text snapshots.
/// Never overwrite a path reused since deletion. Redo only removes saved
/// attachment bytes if no live Org document/draft still references them.
pub(crate) fn restore_history_files(conn: &Connection, action_id: i64, index: &IndexManager, undo: bool) -> Result<(), String> {
    init_schema(conn).to_cmd()?;
    let mut stmt = conn
        .prepare(
            "SELECT id, root, history_reversed FROM trash_entries WHERE history_id = ?1 AND expires_at > ?2
        AND (history_reversed = 0 OR rowid = (SELECT MAX(rowid) FROM trash_entries
            WHERE history_id = ?1 AND history_reversed = 1 AND expires_at > ?2))",
        )
        .to_cmd()?;
    let entries: Vec<(String, String, bool)> = stmt
        .query_map(params![action_id, chrono::Utc::now().timestamp()], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .to_cmd()?
        .collect::<rusqlite::Result<_>>()
        .to_cmd()?;
    let mut writes = Vec::new();
    let mut removals = HashMap::new();
    for (id, root, reversed) in entries {
        let undo = undo != reversed;
        let root = Path::new(&root);
        let live = if undo { None } else { Some(attachments::live_references(root).map_err(|e| e.to_string())?) };
        for file in files(conn, &id)? {
            if file.role == "context" || file.role == "draft_context" || (file.role != "attachment" && file.after.is_some()) {
                continue;
            }
            require_safe_path(root, &file.path)?;
            let current = match fs::read(&file.path) {
                Ok(bytes) => Some(bytes),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => return Err(e.to_string()),
            };
            if undo {
                if current.as_deref().is_some_and(|bytes| bytes != file.before)
                    || writes.iter().any(|(p, bytes)| p == &file.path && bytes != &file.before)
                {
                    return Err(crate::t!(
                        "Wiederherstellen würde eine Datei überschreiben: {}",
                        "Restoring would overwrite a file: {}",
                        file.path.display()
                    ));
                }
                if current.is_none() {
                    writes.push((file.path, file.before));
                }
            } else if file.role == "attachment"
                && !live.as_ref().unwrap().contains(&file.path)
                && current.as_deref() == Some(file.before.as_slice())
            {
                removals.insert(file.path, root.to_path_buf());
            }
        }
    }
    for (path, bytes) in writes {
        fs_utils::write_atomic_bytes(&path, &bytes).to_cmd()?;
        index.update_file(&path);
    }
    // Restoring a forward entry can publish a reference needed by an
    // inverse entry. Recheck after all staged writes before collecting.
    let mut live_after_writes = HashMap::new();
    for (path, root) in removals {
        if !live_after_writes.contains_key(&root) {
            live_after_writes.insert(root.clone(), attachments::live_references(&root).to_cmd()?);
        }
        if !live_after_writes[&root].contains(&path) {
            fs::remove_file(&path).to_cmd()?;
            index.remove_file(&path);
        }
    }
    Ok(())
}

/// Mark completion only after BOTH the history snapshots and their
/// retained binary files have been restored successfully by the caller.
pub(crate) fn mark_history_restored(conn: &Connection, action_id: i64, restored: bool) -> Result<(), String> {
    // Undo creates a fresh reverse-direction entry. Older reverse entries
    // remain hidden after an earlier redo; only the new deletion is shown.
    conn.execute(
        "UPDATE trash_entries SET restored = CASE WHEN history_reversed = 1 THEN 1 ELSE ?1 END
        WHERE history_id = ?2 AND (history_reversed = 0 OR ?1 = 0)",
        params![restored, action_id],
    )
    .to_cmd()?;
    Ok(())
}
