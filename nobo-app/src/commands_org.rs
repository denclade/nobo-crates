//! Editing org text, from outside an editor and for one.
//!
//! `toggle_checkbox_in_file` exists because a rendered note is not a
//! read-only thing: a checklist wants to be ticked where it is read, not
//! only where it is written. The rewriting itself is
//! [`nobo_core::core::org_ast`]; what is here is the I/O around it, in the
//! same read/write/reparse/history shape as every other file-touching
//! command.
//!
//! The sorting pair is an editor's: text and caret in, an edit out,
//! nothing read or written -- the editor applies it as one undoable
//! change (see [`nobo_core::core::org_sort`]).

use std::path::PathBuf;
use std::sync::Mutex;

use rusqlite::Connection;

use nobo_core::core::vault_write;
use nobo_core::core::indexer::IndexManager;
use nobo_core::core::org_ast;
use nobo_core::core::org_edit::{self, Edit};
use nobo_core::core::org_sort::{self, SortKey, SortTarget};

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
    vault_write::edit_file(&PathBuf::from(&file_path), None, index_manager, db, |original| {
        Ok(match org_ast::toggle_checkbox(original, line_no) {
            Some(updated) => (updated.clone(), Some(updated)),
            None => (original.to_string(), None),
        })
    })
}

/// What would be sorted at CURSOR (a UTF-16 offset into TEXT) -- a table
/// column, a list, a heading's children -- and the orders that fit it.
/// `None` when there is nothing to sort there.
pub fn sort_target(text: String, cursor: u32) -> Option<SortTarget> {
    org_sort::target(&text, cursor)
}

/// Sorts what is at CURSOR by KEY, as one edit for the editor to apply.
/// `None` when there is nothing to sort there or KEY does not fit it.
pub fn sort_text(text: String, cursor: u32, key: SortKey, descending: bool) -> Option<Edit> {
    org_edit::sort(&text, cursor, key, descending)
}
