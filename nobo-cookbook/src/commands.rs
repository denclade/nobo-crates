// SPDX-FileCopyrightText: 2026 Dennis Burgermeister
// SPDX-License-Identifier: GPL-3.0-or-later

//! What a host can ask this feature for.
//!
//! Plain functions: the Tauri build wraps them (see `tauri_commands`),
//! harbour-nobo's dispatcher unpacks JSON into them.

// See nobo_babel::commands on the allow.
#![allow(clippy::needless_return)]

use crate::model::Recipe;
use crate::repair::CollapsedProperties;
use nobo_core::config::settings::Config;
use nobo_core::core::file_kind;
use nobo_core::core::history::record_best_effort;
use nobo_core::core::indexer::IndexManager;
use nobo_core::core::models::{FileSnapshot, HistoryEntryKind};
use nobo_core::core::vault::Vault;
use nobo_core::core::vault_write;
use nobo_core::utils::fs_utils;
#[cfg(feature = "import")]
use nobo_core::utils::fs_utils::filename_of;
use nobo_core::utils::note_utils;
use rusqlite::Connection;
use std::sync::Mutex;

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
    // What could be a cookbook: no mirror (declaring one would be undone on
    // its next refresh), none of the app's own files (see
    // Vault::kind_candidates).
    let mut files: Vec<CookbookFile> = Vault::new(index_manager, &vault_root)
        .kind_candidates(crate::KIND)
        .into_iter()
        .map(|candidate| {
            let path = candidate.path.to_string_lossy().into_owned();
            CookbookFile { title: candidate.title.unwrap_or_else(|| fs_utils::filename_of(&path)), path, is_cookbook: candidate.is_kind }
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
pub fn cookbook_set_file(path: String, cookbook: bool, index_manager: &IndexManager, db: &Mutex<Connection>) -> Result<(), String> {
    vault_write::declare_kind(std::path::Path::new(&path), crate::KIND, cookbook, index_manager, db)
}

/// A new, empty cookbook at the top of the vault, for a vault that has
/// none yet -- so the first recipe has somewhere to go.
///
/// A note like any other (Denote name, the usual header, the author from
/// the settings), declared a cookbook in that header and nothing else in
/// it: a recipe is a heading, and an empty one would be an empty recipe.
/// Never written over an existing file (see note_utils::create_note_file).
/// Recorded as created, so it is one undo away.
pub fn create_cookbook(index_manager: &IndexManager, db: &Mutex<Connection>) -> Result<CookbookFile, String> {
    let vault_root = Config::require_vault_root()?;
    let title = nobo_core::t!("Kochbuch", "Cookbook");
    let author = Config::load().and_then(|config| config.default_author).unwrap_or_default();
    let declaration = format!("{} {}", file_kind::KIND_KEYWORD, crate::KIND.keyword());
    let filename =
        note_utils::create_note_file(&vault_root.to_string_lossy(), &title, &[], &author, &[declaration], "").map_err(|e| e.to_string())?;

    let path = vault_root.join(&filename);
    let path_string = path.to_string_lossy().into_owned();
    match std::fs::read_to_string(&path) {
        Ok(content) => {
            index_manager.update_file_with(&path, &content);
            record_best_effort(
                db,
                HistoryEntryKind::FileCreated { filename: filename.clone() },
                false,
                vec![FileSnapshot { path: path_string.clone(), before: None, after: Some(content) }],
            );
        }
        // Written, only not read back: the index does its own read.
        Err(_) => index_manager.update_file(&path),
    }
    Ok(CookbookFile { path: path_string, title, is_cookbook: true })
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

    Ok(bodies.iter().flat_map(|(path, text)| crate::parse::recipes_of(path, text)).collect())
}

/// Every cookbook in the vault whose first recipe is holding everybody
/// else's properties, and what could be handed back from it.
///
/// Read-only, and cheap: it is the same text the index already has.
pub fn collapsed_properties(index_manager: &IndexManager) -> Result<Vec<CollapsedProperties>, String> {
    let vault_root = Config::require_vault_root()?;
    Ok(Vault::new(index_manager, &vault_root)
        .files_of_kind(crate::KIND)
        .into_iter()
        .filter_map(|(path, text)| crate::repair::find(&path.to_string_lossy(), &text))
        .collect())
}

/// Every cookbook file, and what is wrong with it (see `crate::check`).
///
/// All of them, including the ones with nothing to say -- the settings
/// page shows each chosen file with its verdict, and "in order" is an
/// answer somebody looking for a problem wants to read too.
pub fn check_cookbooks(index_manager: &IndexManager) -> Result<Vec<crate::check::FileCheck>, String> {
    let vault_root = Config::require_vault_root()?;
    Ok(Vault::new(index_manager, &vault_root)
        .files_of_kind(crate::KIND)
        .into_iter()
        .map(|(path, text)| crate::check::check(&path.to_string_lossy(), &text))
        .collect())
}

/// Hands them back, and says for which files.
///
/// One write per file, recorded in the history like any other edit -- so
/// a repair the user did not want is one undo away, which is what makes
/// it safe to do without asking (see `crate::repair` on what it refuses
/// to guess).
pub fn repair_collapsed_properties(index_manager: &IndexManager, db: &Mutex<Connection>) -> Result<Vec<CollapsedProperties>, String> {
    let vault_root = Config::require_vault_root()?;
    let files: Vec<(std::path::PathBuf, String)> = Vault::new(index_manager, &vault_root).files_of_kind(crate::KIND).into_iter().collect();

    let mut repaired = Vec::new();
    for (path, text) in files {
        // Only the files that need it are opened for writing at all.
        if crate::repair::repair(&path.to_string_lossy(), &text).is_none() {
            continue;
        }
        // Repaired from the text as it is when written, under the file's lock
        // -- the index's copy may be a moment behind an edit just made.
        let report = vault_write::edit_file(&path, Some(crate::KIND), index_manager, db, |current| {
            Ok(match crate::repair::repair(&path.to_string_lossy(), current) {
                Some((fixed, report)) => (fixed, Some(report)),
                None => (current.to_string(), None),
            })
        })?;
        if let Some(report) = report {
            repaired.push(report);
        }
    }
    Ok(repaired)
}

/// The recipe on the page at URL, read but not saved anywhere -- what the
/// import shows before anything is written.
#[cfg(feature = "import")]
pub async fn import_preview(url: String) -> Result<crate::import::ImportedRecipe, String> {
    let url = url.trim().to_string();
    let html = crate::import::fetch::page(&url).await?;
    crate::import::parse_page(&url, &html)
}

/// Whether URL is a recipe site the import knows by its address (see
/// import::known_site). Nothing is fetched.
pub fn import_known_site(url: String) -> bool {
    crate::import::known_site(url.trim())
}

/// Where a saved import landed, so the caller can open it.
#[derive(Debug, Clone, serde::Serialize)]
#[cfg_attr(feature = "tauri", derive(specta::Type))]
pub struct ImportedAt {
    pub file_path: String,
    #[cfg_attr(feature = "tauri", specta(type = specta_typescript::Number))]
    pub line_no: usize,
}

/// RECIPE (as `import_preview` gave it) into the cookbook FILE_PATH,
/// under CATEGORY (a heading path, empty for the top level), with its
/// picture when WITH_IMAGE.
///
/// The picture is a nicety, never a reason to fail: a page whose image
/// cannot be fetched still gives its recipe, without one. One history
/// entry for the cookbook file, so an import is one undo away.
#[cfg(feature = "import")]
pub async fn import_save(
    recipe: crate::import::ImportedRecipe,
    file_path: String,
    category: Vec<String>,
    with_image: bool,
    index_manager: &IndexManager,
    db: &Mutex<Connection>,
) -> Result<ImportedAt, String> {
    let path = std::path::PathBuf::from(&file_path);
    let is_cookbook = |text: &str| file_kind::from_lines(&text.lines().collect::<Vec<_>>()) == crate::KIND;
    let not_a_cookbook = || nobo_core::t!("{file_path} ist kein Kochbuch", "{file_path} is not a cookbook");
    // Checked before the download too, so a wrong target costs no traffic.
    if !is_cookbook(&std::fs::read_to_string(&path).map_err(|e| e.to_string())?) {
        return Err(not_a_cookbook());
    }

    let image_link = match recipe.image_url.as_deref().filter(|_| with_image) {
        Some(url) => save_picture(url, &recipe.title, &path, index_manager, db)
            .await
            .map_err(|error| log::warn!("[import] recipe saved without its picture: {error}"))
            .ok(),
        None => None,
    };

    // Read AFTER the picture, not before it: the download is a network
    // round trip of seconds, and a file read before it and written after
    // it overwrote whatever happened to the cookbook in between -- an
    // edit in the other pane, a sync pull. Only the in-memory insert lies
    // between this read and the write now.
    let line_no = vault_write::edit_file(&path, Some(crate::KIND), index_manager, db, |text| {
        Ok(crate::import::insert(text, &category, |level| crate::import::to_org(&recipe, level, image_link.as_deref())))
    })?;
    Ok(ImportedAt { file_path, line_no })
}

/// The picture, into the cookbook's attachment folder. Returns the link
/// target relative to the cookbook file.
#[cfg(feature = "import")]
async fn save_picture(
    url: &str,
    title: &str,
    cookbook: &std::path::Path,
    index_manager: &IndexManager,
    db: &Mutex<Connection>,
) -> Result<String, String> {
    let (bytes, extension) = crate::import::fetch::image(url).await?;
    let folder = Config::load().map(|config| config.attachments_dir).unwrap_or_else(|| ".attachments".to_string());
    let directory = cookbook.parent().unwrap_or(std::path::Path::new("")).join(&folder);
    let saved =
        fs_utils::place_attachment(&directory, &crate::import::image_filename(title, extension), |dest| std::fs::write(dest, &bytes))
            .map_err(|e| e.to_string())?;
    index_manager.update_file(std::path::Path::new(&saved));
    record_best_effort(db, HistoryEntryKind::AttachmentAdded { filename: filename_of(&saved) }, false, vec![]);
    Ok(format!("{folder}/{}", filename_of(&saved)))
}
