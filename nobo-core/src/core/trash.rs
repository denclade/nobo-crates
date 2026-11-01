// SPDX-FileCopyrightText: 2026 Dennis Burgermeister
// SPDX-License-Identifier: GPL-3.0-or-later

//! The local recycle bin, independent of history's size/count retention.
//! A durable pre-image is committed BEFORE a destructive write. Attachment
//! bytes are likewise committed before unlinking. Expiry uses UTC instants.

mod reinsert;
#[cfg(test)]
mod tests;

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use rusqlite::{params, Connection, OptionalExtension};
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
            history_id INTEGER, attachments_dir TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS trash_files (
            entry_id TEXT NOT NULL REFERENCES trash_entries(id) ON DELETE CASCADE,
            path TEXT NOT NULL, before_bytes BLOB NOT NULL, after_bytes BLOB,
            role TEXT NOT NULL, PRIMARY KEY(entry_id, path)
         );
         CREATE INDEX IF NOT EXISTS idx_trash_expiry ON trash_entries(expires_at);
         CREATE INDEX IF NOT EXISTS idx_trash_file_path ON trash_files(path);",
    )
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
        let tx = conn.unchecked_transaction().to_cmd()?;
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
            if path.extension().is_some_and(|ext| ext == "org") {
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
        tx.commit().to_cmd()?;
        Ok(protection)
    }

    pub fn finish(self, kind: Option<&HistoryEntryKind>, index: &IndexManager, db: &Mutex<Connection>) -> Result<(), String> {
        let conn = db.lock().map_err(|e| e.to_string())?;
        finish(&conn, &self.id, &self.policy, kind, Some(index))
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
    recover_references(conn, &policy.root, &snapshots, index)?;
    if let Some(HistoryEntryKind::HeadingDeleted { title, .. }) = kind {
        label = Some(title.clone());
    }
    // Refiles/moves must never turn into deletions just because the source
    // has fewer headings. Across multiple files compare the total as well.
    if snapshots.len() > 1
        && snapshots.iter().map(|s| heading_count(s.before.as_deref().unwrap_or(""))).sum::<usize>()
            <= snapshots.iter().map(|s| heading_count(s.after.as_deref().unwrap_or(""))).sum::<usize>()
    {
        structural = stored.iter().any(|f| f.role == "owner" && !f.path.exists());
    }
    let candidates = attachments::orphaned(&policy.root, &policy.attachments_dir, &snapshots).unwrap_or_else(|e| {
        log::warn!("Attachment reference scan failed; keeping files: {e}");
        Vec::new()
    });
    // Save every candidate before deleting any. A backup failure leaves
    // attachments in place and the pre-image recoverable in the bin.
    let tx = conn.unchecked_transaction().to_cmd()?;
    for path in &candidates {
        let bytes = fs::read(path).map_err(|e| e.to_string())?;
        tx.execute(
            "INSERT OR IGNORE INTO trash_files(entry_id, path, before_bytes, role) VALUES (?1, ?2, ?3, 'attachment')",
            params![id, path.to_string_lossy(), bytes],
        )
        .to_cmd()?;
        share_retained_attachment(&tx, &policy.root, path, &bytes)?;
    }
    tx.execute("UPDATE trash_entries SET pending = 0, label = COALESCE(?1, label) WHERE id = ?2", params![label, id]).to_cmd()?;
    if !structural {
        tx.execute("UPDATE trash_files SET role = 'context' WHERE entry_id = ?1 AND role = 'owner'", [id]).to_cmd()?;
        tx.execute("UPDATE trash_files SET role = 'draft_context' WHERE entry_id = ?1 AND role = 'draft'", [id]).to_cmd()?;
    }
    tx.commit().to_cmd()?;
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
    let tx = conn.unchecked_transaction().to_cmd()?;
    share_retained_attachment(&tx, &policy.root, path, &bytes)?;
    tx.commit().to_cmd()
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
            if let Err(e) = finish(conn, &id, &Policy::new(Path::new(&root), &dir, DEFAULT_RETENTION_DAYS), None, None) {
                log::warn!("Could not reconcile pending recycle-bin entry {id}: {e}");
            }
        }
    }
    let expired: Vec<String> = {
        let mut stmt = conn.prepare("SELECT id FROM trash_entries WHERE expires_at <= ?1").to_cmd()?;
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
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                }
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
            "SELECT EXISTS(SELECT 1 FROM trash_entries WHERE history_id = ?1 AND expires_at > ?2)",
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
    finish(conn, &protection.id, &policy, Some(&entry.kind), Some(index))?;
    associate(conn, entry.id, &entry.snapshots)?;
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
            let current = current.as_deref().and_then(|b| std::str::from_utf8(b).ok()).unwrap_or("");
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
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
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
            "UPDATE trash_entries SET history_id = ?1 WHERE history_id IS NULL AND pending = 0 AND id IN
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
    let mut stmt = conn.prepare("SELECT id, root FROM trash_entries WHERE history_id = ?1 AND expires_at > ?2").to_cmd()?;
    let entries: Vec<(String, String)> = stmt
        .query_map(params![action_id, chrono::Utc::now().timestamp()], |r| Ok((r.get(0)?, r.get(1)?)))
        .to_cmd()?
        .collect::<rusqlite::Result<_>>()
        .to_cmd()?;
    for (id, root) in entries {
        let root = Path::new(&root);
        let live = if undo { None } else { Some(attachments::live_references(root).map_err(|e| e.to_string())?) };
        for file in files(conn, &id)? {
            if file.role == "context" || file.role == "draft_context" {
                continue;
            }
            if file.role != "attachment" && file.after.is_some() {
                continue;
            }
            require_safe_path(root, &file.path)?;
            if undo {
                if let Ok(current) = fs::read(&file.path) {
                    if current != file.before {
                        return Err(crate::t!(
                            "Wiederherstellen würde eine Datei überschreiben: {}",
                            "Restoring would overwrite a file: {}",
                            file.path.display()
                        ));
                    }
                } else {
                    if let Some(parent) = file.path.parent() {
                        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                    }
                    fs_utils::write_atomic_bytes(&file.path, &file.before).to_cmd()?;
                    index.update_file(&file.path);
                }
            } else if file.role == "attachment"
                && !live.as_ref().unwrap().contains(&file.path)
                && fs::read(&file.path).ok().as_deref() == Some(file.before.as_slice())
            {
                fs::remove_file(&file.path).to_cmd()?;
                index.remove_file(&file.path);
            }
        }
        conn.execute("UPDATE trash_entries SET restored = ?1 WHERE id = ?2", params![undo, id]).to_cmd()?;
    }
    Ok(())
}
