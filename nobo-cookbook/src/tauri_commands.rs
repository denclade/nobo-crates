//! The same commands, as Tauri sees them.
//!
//! One line each: everything that decides anything lives in `commands`,
//! so the Tauri build and a non-Tauri host cannot answer the same
//! question differently. The names and the doc comments are what
//! tauri-specta reads off the command to write `bindings.ts`, so both are
//! the same text as in `commands` -- they describe the contract, and the
//! frontend is who reads them.

use std::sync::Arc;

use tauri::State;

use nobo_core::core::indexer::IndexManager;

use crate::commands::{self, CookbookFile};
use crate::model::Recipe;

/// Every org file in the vault that could hold recipes, with the ones that
/// already declare themselves marked.
///
/// Generated mirrors are left out: a CalDAV list or a calendar file is
/// rewritten wholesale by its own sync, so declaring one a cookbook would
/// be undone on the next refresh -- and its kind is already taken.
#[tauri::command]
#[specta::specta]
pub async fn cookbook_files(index_manager: State<'_, Arc<IndexManager>>) -> Result<Vec<CookbookFile>, String> {
    commands::cookbook_files(index_manager.inner())
}

/// Declares a file a cookbook, or takes the declaration back.
///
/// Writes `#+NOBO_TYPE: cookbook` into the file's header (see
/// core::file_kind) -- which is what the whole feature keys on, so this is
/// also how a file stops being one. Undeclaring only removes what this
/// wrote; the recipes themselves are the user's text and are never
/// touched.
#[tauri::command]
#[specta::specta]
pub async fn set_cookbook(
    path: String,
    cookbook: bool,
    index_manager: State<'_, Arc<IndexManager>>,
) -> Result<(), String> {
    commands::set_cookbook(path, cookbook, index_manager.inner())
}

/// Every recipe in every file that declares itself a cookbook, sorted by
/// title within each file, files in path order -- one stable list the
/// sidebar can group and filter however it likes.
///
/// Reads the bodies the index already holds rather than walking the vault:
/// which files are cookbooks is a property of their content (see
/// file_kind), and that content is exactly what is cached.
#[tauri::command]
#[specta::specta]
pub async fn cookbook_recipes(index_manager: State<'_, Arc<IndexManager>>) -> Result<Vec<Recipe>, String> {
    commands::cookbook_recipes(index_manager.inner())
}
