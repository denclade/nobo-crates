// SPDX-FileCopyrightText: 2026 Dennis Burgermeister
// SPDX-License-Identifier: GPL-3.0-or-later

use rusqlite::Connection;
use std::path::Path;
use std::sync::Mutex;

use nobo_core::core::history;
use nobo_core::core::indexer::IndexManager;
use nobo_core::core::models::{FileSnapshot, HeadingRestoreCheck, HistoryEntry, HistoryEntryKind};

pub fn history_list(limit: u32, db: &Mutex<Connection>) -> Result<Vec<HistoryEntry>, String> {
    let conn = db.lock().map_err(|e| e.to_string())?;
    history::list(&conn, limit as i64)
}

/// Scoped to one file -- powers the per-note/per-task history modal (see
/// HistoryModal.tsx), as opposed to history_list's whole-app view (Settings
/// > History).
pub fn history_list_for_path(path: String, limit: u32, db: &Mutex<Connection>) -> Result<Vec<HistoryEntry>, String> {
    let conn = db.lock().map_err(|e| e.to_string())?;
    history::list_for_path(&conn, &path, limit as i64)
}

/// One entry with its snapshot content.
///
/// The listings above deliberately return paths only -- inflating every
/// `before` and `after` of 300 entries is tens of megabytes for data the
/// history page merely counts. The one place that shows content is the
/// diff in `HistoryModal`, which shows one entry at a time and asks for
/// that one.
pub fn history_entry(
    // `u32` like `history_jump_to` next door: specta refuses a bare i64
    // across the boundary, and an id is a rowid.
    id: u32,
    db: &Mutex<Connection>,
) -> Result<Option<HistoryEntry>, String> {
    let conn = db.lock().map_err(|e| e.to_string())?;
    history::entry(&conn, id as i64)
}

/// "Restore to this point" in the history modal -- see core::history::jump_to
/// for why this can take several undo/redo steps and may touch files beyond
/// the one the modal is scoped to.
pub fn history_jump_to(id: u32, db: &Mutex<Connection>, index_manager: &IndexManager) -> Result<Vec<HistoryEntry>, String> {
    let conn = db.lock().map_err(|e| e.to_string())?;
    history::jump_to(&conn, &index_manager, id as i64)
}

pub fn history_undo(db: &Mutex<Connection>, index_manager: &IndexManager) -> Result<Option<HistoryEntry>, String> {
    let conn = db.lock().map_err(|e| e.to_string())?;
    history::undo(&conn, &index_manager)
}

pub fn history_redo(db: &Mutex<Connection>, index_manager: &IndexManager) -> Result<Option<HistoryEntry>, String> {
    let conn = db.lock().map_err(|e| e.to_string())?;
    history::redo(&conn, &index_manager)
}

/// Checks whether ONE specific (arbitrary, not necessarily the most recent)
/// `HeadingDeleted` entry can be reinserted against the file's CURRENT AST
/// -- e.g. a deleted subtask is only restorable if its project still
/// exists -- and if so builds a before/after preview. Read-only, powers
/// RestoreEntryModal.tsx's diff+raw-text preview in Settings > History
/// (SettingsHistory.tsx). See core::history::restore for the full "why".
pub fn history_check_heading_restore(id: u32, db: &Mutex<Connection>) -> Result<HeadingRestoreCheck, String> {
    let conn = db.lock().map_err(|e| e.to_string())?;
    history::restore::check(&conn, id as i64)
}

/// Actually performs the restore checked by history_check_heading_restore
/// above -- reruns the same feasibility computation rather than trusting a
/// possibly-stale earlier check (the file may have changed since).
pub fn history_apply_heading_restore(id: u32, db: &Mutex<Connection>, index_manager: &IndexManager) -> Result<(), String> {
    let conn = db.lock().map_err(|e| e.to_string())?;
    history::restore::apply(&conn, &index_manager, id as i64)
}

/// Records a coalesced editing-session snapshot (real save or draft-only
/// typing burst, see IS_DRAFT) -- the debounce/session-boundary logic
/// (60s idle, file switch, editor blur) lives entirely on the frontend
/// (useNoteEditor.ts), which is the only side with a live "is the user
/// still typing" signal; this command just persists what it decided.
pub fn record_note_history(path: String, before: String, after: String, is_draft: bool, db: &Mutex<Connection>) -> Result<(), String> {
    if before == after {
        return Ok(());
    }
    let filename = Path::new(&path).file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_else(|| path.clone());
    let conn = db.lock().map_err(|e| e.to_string())?;
    history::record(
        &conn,
        HistoryEntryKind::FileEdited { filename },
        is_draft,
        vec![FileSnapshot { path, before: Some(before), after: Some(after) }],
    )
}
