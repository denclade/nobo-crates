//! What a host can ask this feature for.
//!
//! Plain functions: the Tauri build wraps them (see `tauri_commands`),
//! harbour-nobo's dispatcher unpacks JSON into them.

// See nobo_babel::commands on the allow.
#![allow(clippy::needless_return)]

use crate::model::Recipe;
use crate::repair::CollapsedProperties;
use nobo_core::core::file_kind;
use nobo_core::core::history::{self, record_best_effort};
use nobo_core::core::models::{FileSnapshot, HistoryEntryKind};
use nobo_core::utils::fs_utils::filename_of;
use rusqlite::Connection;
use std::sync::Mutex;
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
pub fn repair_collapsed_properties(
    index_manager: &IndexManager,
    db: &Mutex<Connection>,
) -> Result<Vec<CollapsedProperties>, String> {

let vault_root = Config::require_vault_root()?;
let files: Vec<(std::path::PathBuf, String)> = Vault::new(index_manager, &vault_root)
    .files_of_kind(crate::KIND)
    .into_iter()
    .collect();

let mut repaired = Vec::new();
for (path, text) in files {
    let Some((fixed, report)) = crate::repair::repair(&path.to_string_lossy(), &text) else {
        continue;
    };
    let (before, result, after) = history::snapshot_around(&path, || {
        fs_utils::write_atomic_checked(&path, &fixed).map_err(|e| e.to_string())?;
        index_manager.update_file(&path);
        Ok(())
    });
    record_best_effort(
        db,
        HistoryEntryKind::FileEdited { filename: filename_of(&path) },
        false,
        vec![FileSnapshot { path: path.to_string_lossy().into_owned(), before, after }],
    );
    result?;
    repaired.push(report);
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
    let text = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    if !is_cookbook(&text) {
        return Err(not_a_cookbook());
    }

    let (updated, line_no) =
        crate::import::insert(&text, &category, |level| crate::import::to_org(&recipe, level, image_link.as_deref()));
    let (before, result, after) = history::snapshot_around(&path, || {
        fs_utils::write_atomic_checked(&path, &updated).map_err(|e| e.to_string())?;
        index_manager.update_file(&path);
        Ok(())
    });
    record_best_effort(
        db,
        HistoryEntryKind::FileEdited { filename: filename_of(&file_path) },
        false,
        vec![FileSnapshot { path: file_path.clone(), before, after }],
    );
    result?;
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
    let saved = fs_utils::place_attachment(&directory, &crate::import::image_filename(title, extension), |dest| {
        std::fs::write(dest, &bytes)
    })
    .map_err(|e| e.to_string())?;
    index_manager.update_file(std::path::Path::new(&saved));
    record_best_effort(db, HistoryEntryKind::AttachmentAdded { filename: filename_of(&saved) }, false, vec![]);
    Ok(format!("{folder}/{}", filename_of(&saved)))
}
