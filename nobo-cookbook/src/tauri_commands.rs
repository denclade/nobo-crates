//! The same commands, as Tauri sees them.
//!
//! One line each: everything that decides anything lives in `commands`,
//! so the Tauri build and a non-Tauri host cannot answer the same
//! question differently. The names and the doc comments are what
//! tauri-specta reads off the command to write `bindings.ts`, so both are
//! the same text as in `commands` -- they describe the contract, and the
//! frontend is who reads them.

use std::sync::{Arc, Mutex};

use rusqlite::Connection;
use tauri::State;

use nobo_core::core::indexer::IndexManager;

use crate::commands::{self, CookbookFile};
use crate::model::Recipe;
use crate::repair::CollapsedProperties;

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
pub async fn cookbook_set_file(
    path: String,
    cookbook: bool,
    index_manager: State<'_, Arc<IndexManager>>,
    db: State<'_, Arc<Mutex<Connection>>>,
) -> Result<(), String> {
    commands::cookbook_set_file(path, cookbook, index_manager.inner(), db.inner())
}

/// A new, empty cookbook at the top of the vault -- for a vault without
/// one, so the first recipe has somewhere to go. Never written over an
/// existing file; recorded as created.
#[tauri::command]
#[specta::specta]
pub async fn cookbook_create(
    index_manager: State<'_, Arc<IndexManager>>,
    db: State<'_, Arc<Mutex<Connection>>>,
) -> Result<CookbookFile, String> {
    commands::create_cookbook(index_manager.inner(), db.inner())
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

/// Every cookbook in the vault whose first recipe is holding everybody
/// else's properties, and what could be handed back from it.
///
/// Read-only, and cheap: it is the same text the index already has.
#[tauri::command]
#[specta::specta]
pub async fn cookbook_collapsed_properties(index_manager: State<'_, Arc<IndexManager>>) -> Result<Vec<CollapsedProperties>, String> {
    commands::collapsed_properties(index_manager.inner())
}

/// Hands them back, and says for which files.
///
/// One write per file, recorded in the history like any other edit -- so
/// a repair the user did not want is one undo away. Keys that cannot be
/// distributed without guessing are left exactly where they are and named
/// in `left_alone` (see `crate::repair`).
#[tauri::command]
#[specta::specta]
pub async fn cookbook_repair_collapsed_properties(
    index_manager: State<'_, Arc<IndexManager>>,
    db: State<'_, Arc<Mutex<Connection>>>,
) -> Result<Vec<CollapsedProperties>, String> {
    commands::repair_collapsed_properties(index_manager.inner(), db.inner())
}

/// The recipe on the page at URL, read but not saved -- what the import
/// shows before anything is written. In a build without the import
/// (no networking), always registered all the same and refused with a
/// reason: one command list for every build (see the app's build.rs).
#[tauri::command]
#[specta::specta]
pub async fn cookbook_import_preview(url: String) -> Result<crate::import::ImportedRecipe, String> {
    #[cfg(feature = "import")]
    return commands::import_preview(url).await;
    #[cfg(not(feature = "import"))]
    return {
        let _ = url;
        Err(crate::import_unavailable())
    };
}

/// RECIPE (as the preview gave it) into the cookbook FILE_PATH, under
/// CATEGORY (a heading path, empty for the top level), with its picture
/// when WITH_IMAGE. One history entry, so an import is one undo away.
#[tauri::command]
#[specta::specta]
pub async fn cookbook_import_save(
    recipe: crate::import::ImportedRecipe,
    file_path: String,
    category: Vec<String>,
    with_image: bool,
    index_manager: State<'_, Arc<IndexManager>>,
    db: State<'_, Arc<Mutex<Connection>>>,
) -> Result<commands::ImportedAt, String> {
    #[cfg(feature = "import")]
    return commands::import_save(recipe, file_path, category, with_image, index_manager.inner(), db.inner()).await;
    #[cfg(not(feature = "import"))]
    return {
        let _ = (recipe, file_path, category, with_image, index_manager, db);
        Err(crate::import_unavailable())
    };
}
