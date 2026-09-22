//! What a host can ask this feature for.
//!
//! Plain functions: the Tauri build wraps them (see `tauri_commands`),
//! harbour-nobo's dispatcher unpacks JSON into them.

// See nobo_babel::commands on the allow.
#![allow(clippy::needless_return)]

use crate::model::Recipe;
use nobo_core::core::file_kind;
use nobo_core::config::settings::Config;
use nobo_core::core::indexer::IndexManager;
use nobo_core::core::vault::Vault;
use nobo_core::utils::fs_utils;

/// One org file the user could turn into a cookbook, and whether it
/// already is one.
#[derive(Debug, Clone, serde::Serialize)]
#[cfg_attr(feature = "tauri", derive(specta::Type))]
pub struct CookbookFile {
    pub path: String,
    /// The note's own title, or the file name for a plain org file.
    pub title: String,
    pub is_cookbook: bool,
}

/// Every org file in the vault that could hold recipes, with the ones that
/// already declare themselves marked.
///
/// Generated mirrors are left out: a CalDAV list or a calendar file is
/// rewritten wholesale by its own sync, so declaring one a cookbook would
/// be undone on the next refresh -- and its kind is already taken.
pub fn cookbook_files(index_manager: &IndexManager) -> Result<Vec<CookbookFile>, String> {

let vault_root = Config::require_vault_root()?;
let vault = Vault::new(index_manager, &vault_root);
let titles: std::collections::HashMap<String, String> =
    vault.notes().into_iter().map(|note| (note.path, note.title)).collect();
let mut files: Vec<CookbookFile> = vault
    .org_files()
    .into_iter()
    .map(|(path, text)| (path.to_string_lossy().into_owned(), text))
    .filter_map(|(path, text)| {
        let kind = file_kind::from_lines(&text.lines().collect::<Vec<_>>());
        // Anything the app generates and rewrites wholesale is out:
        // declaring one a cookbook would be undone on its next
        // refresh. Asked as a capability rather than by naming the
        // mirrors, so this holds for whatever the build has (see
        // core::file_kind).
        if !file_kind::capabilities(kind).editable {
            return None;
        }
        Some(CookbookFile {
            title: titles
                .get(&path)
                .cloned()
                .unwrap_or_else(|| fs_utils::filename_of(&path)),
            path,
            is_cookbook: kind == crate::KIND,
        })
    })
    .collect();
files.sort_by_key(|file| file.title.to_lowercase());
Ok(files)
}

/// Declares a file a cookbook, or takes the declaration back.
///
/// Writes `#+NOBO_TYPE: cookbook` into the file's header (see
/// core::file_kind) -- which is what the whole feature keys on, so this is
/// also how a file stops being one. Undeclaring only removes what this
/// wrote; the recipes themselves are the user's text and are never
/// touched.
pub fn set_cookbook(path: String, cookbook: bool, index_manager: &IndexManager) -> Result<(), String> {

let path = std::path::PathBuf::from(&path);
let value = cookbook.then(|| crate::KIND.keyword().to_string());
file_kind::set_leading_keyword(&path, file_kind::KIND_KEYWORD, value.as_deref())?;
index_manager.update_file(&path);
Ok(())
}

/// Every recipe in every file that declares itself a cookbook, sorted by
/// title within each file, files in path order -- one stable list the
/// sidebar can group and filter however it likes.
///
/// Reads the bodies the index already holds rather than walking the vault:
/// which files are cookbooks is a property of their content (see
/// file_kind), and that content is exactly what is cached.
pub fn cookbook_recipes(index_manager: &IndexManager) -> Result<Vec<Recipe>, String> {

let vault_root = Config::require_vault_root()?;
let mut bodies: Vec<(String, String)> = Vault::new(index_manager, &vault_root)
    .files_of_kind(crate::KIND)
    .into_iter()
    .map(|(path, text)| (path.to_string_lossy().into_owned(), text))
    .collect();
// Files in path order, which is the one order a list of everything
// can be relied on to keep between runs.
bodies.sort_by(|(a, _), (b, _)| a.cmp(b));

Ok(bodies
    .iter()
    .flat_map(|(path, text)| crate::parse::recipes_of(path, text))
    .collect())
}
