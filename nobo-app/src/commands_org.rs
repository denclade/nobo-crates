//! Editing org text from outside an editor.
//!
//! One command so far, and it exists because a rendered note is not a
//! read-only thing: a checklist wants to be ticked where it is read, not
//! only where it is written. The rewriting itself is
//! [`nobo_core::core::org_ast`]; what is here is the I/O around it, in the
//! same read/write/reparse/history shape as every other file-touching
//! command.

use std::path::PathBuf;
use std::sync::Mutex;

use rusqlite::Connection;

use nobo_core::core::history::{self, record_best_effort};
use nobo_core::core::indexer::IndexManager;
use nobo_core::core::models::{FileSnapshot, HistoryEntryKind};
use nobo_core::core::org_ast;
use nobo_core::utils::fs_utils;
use nobo_core::utils::ToCmdResult;

/// Ticks or clears the checkbox on LINE_NO (1-based) of FILE_PATH and
/// brings every parent checkbox above it up to date.
///
/// Returns the file's new text, so a reader that is already showing it can
/// re-render without a second read, or `None` when that line had no
/// checkbox of its own to set -- a plain item, or a parent whose state is
/// aggregated from its children.
pub fn toggle_checkbox_in_file(
    file_path: String,
    line_no: usize,
    index_manager: &IndexManager,
    db: &Mutex<Connection>,
) -> Result<Option<String>, String> {
    let path = PathBuf::from(&file_path);
    let original = std::fs::read_to_string(&path).to_cmd()?;
    let Some(updated) = org_ast::toggle_checkbox(&original, line_no) else {
        return Ok(None);
    };

    let (before, result, after) = history::snapshot_around(&path, || {
        fs_utils::write_atomic_checked(&path, &updated).to_cmd()?;
        index_manager.update_file_with(&path, &updated);
        Ok(())
    });
    result?;

    record_best_effort(
        db,
        HistoryEntryKind::FileEdited { filename: fs_utils::filename_of(&path) },
        false,
        vec![FileSnapshot { path: file_path, before, after }],
    );
    Ok(Some(updated))
}
