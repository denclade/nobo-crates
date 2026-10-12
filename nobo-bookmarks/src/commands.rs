//! What a host can ask this feature for.
//!
//! Plain functions: the Tauri build wraps them (see `tauri_commands`), any
//! other host calls them directly.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::Connection;

use nobo_core::config::settings::Config;
use nobo_core::core::file_kind;
use nobo_core::core::history::{self, record_best_effort};
use nobo_core::core::indexer::IndexManager;
use nobo_core::core::models::{FileSnapshot, HistoryEntryKind};
use nobo_core::core::org_insert::insert_under;
use nobo_core::core::vault::Vault;
use nobo_core::t;
use nobo_core::utils::{fs_utils, note_utils};

use crate::model::{Bookmark, BookmarkFile, Draft, Located, Patch, Target};
use crate::{parse, url, write};

/// The note titles of the vault, by path -- what a category is called.
fn titles(vault: &Vault) -> HashMap<String, String> {
    vault.notes().into_iter().map(|note| (note.path, note.title)).collect()
}

fn category_of(titles: &HashMap<String, String>, path: &str) -> String {
    titles.get(path).cloned().unwrap_or_else(|| fs_utils::filename_of(path).trim_end_matches(".org").to_string())
}

/// Every bookmark in every bookmarks file: files by title, bookmarks in
/// the order they are written.
pub fn bookmarks_list(index_manager: &IndexManager) -> Result<Vec<Bookmark>, String> {
    let root = Config::require_vault_root()?;
    let vault = Vault::new(index_manager, &root);
    let titles = titles(&vault);
    let mut files: Vec<(String, String)> = vault
        .files_of_kind(crate::KIND)
        .into_iter()
        .map(|(path, text)| (path.to_string_lossy().into_owned(), text))
        .collect();
    files.sort_by_key(|(path, _)| category_of(&titles, path).to_lowercase());
    Ok(files
        .iter()
        .flat_map(|(path, text)| parse::bookmarks_of(path, &category_of(&titles, path), text))
        .collect())
}

/// Every org file that could be a bookmarks file, with the ones that are
/// marked. Generated mirrors are left out -- their own sync rewrites them.
pub fn bookmarks_files(index_manager: &IndexManager) -> Result<Vec<BookmarkFile>, String> {
    let root = Config::require_vault_root()?;
    let vault = Vault::new(index_manager, &root);
    let titles = titles(&vault);
    let mut files: Vec<BookmarkFile> = vault
        .org_files()
        .into_iter()
        .filter_map(|(path, text)| {
            let kind = file_kind::from_lines(&text.lines().collect::<Vec<_>>());
            if !file_kind::capabilities(kind).editable {
                return None;
            }
            let path = path.to_string_lossy().into_owned();
            Some(BookmarkFile { title: category_of(&titles, &path), path, is_bookmarks: kind == crate::KIND })
        })
        .collect();
    files.sort_by_key(|file| file.title.to_lowercase());
    Ok(files)
}

/// Declares a file a bookmarks file -- a category -- or takes that back.
/// Only the declaration is written; the bookmarks are the user's text and
/// stay where they are either way.
pub fn bookmarks_set_file(path: String, bookmarks: bool, index_manager: &IndexManager) -> Result<(), String> {
    let path = PathBuf::from(&path);
    let value = bookmarks.then(|| crate::KIND.keyword().to_string());
    file_kind::set_leading_keyword(&path, file_kind::KIND_KEYWORD, value.as_deref())?;
    index_manager.update_file(&path);
    Ok(())
}

/// A new, empty bookmarks file called TITLE ("Arbeit", "Privat") at the
/// top of the vault -- a new category. A note like any other, declared in
/// its header. Never written over an existing file; recorded as created.
pub fn bookmarks_create_file(title: String, index_manager: &IndexManager, db: &Mutex<Connection>) -> Result<BookmarkFile, String> {
    let title = title.trim().to_string();
    let title = if title.is_empty() { t!("Lesezeichen", "Bookmarks") } else { title };
    let root = Config::require_vault_root()?;
    let author = Config::load().and_then(|config| config.default_author).unwrap_or_default();
    let declaration = format!("{} {}", file_kind::KIND_KEYWORD, crate::KIND.keyword());
    let filename = note_utils::create_note_file(&root.to_string_lossy(), &title, &[], &author, &[declaration], "")
        .map_err(|e| e.to_string())?;
    let path = root.join(&filename);
    let path_string = path.to_string_lossy().into_owned();
    match std::fs::read_to_string(&path) {
        Ok(content) => {
            index_manager.update_file_with(&path, &content);
            record_best_effort(
                db,
                HistoryEntryKind::FileCreated { filename },
                false,
                vec![FileSnapshot { path: path_string.clone(), before: None, after: Some(content) }],
            );
        }
        Err(_) => index_manager.update_file(&path),
    }
    Ok(BookmarkFile { path: path_string, title, is_bookmarks: true })
}

/// The bookmarks that already point to URL, in any category -- compared
/// without fragments, tracking parameters and the like (see url::normalize).
pub fn bookmarks_find(url: String, index_manager: &IndexManager) -> Result<Vec<Bookmark>, String> {
    let wanted = url::normalize(&url);
    Ok(bookmarks_list(index_manager)?.into_iter().filter(|bookmark| url::normalize(&bookmark.url) == wanted).collect())
}

/// Reads the file at PATH, makes sure it is a bookmarks file, and writes
/// what CHANGE makes of its text -- atomically, into the index, and as one
/// history entry, so every change here is one undo away.
fn rewrite<T>(
    path: &Path,
    index_manager: &IndexManager,
    db: &Mutex<Connection>,
    change: impl FnOnce(&str) -> Result<(String, T), String>,
) -> Result<T, String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    if file_kind::from_lines(&text.lines().collect::<Vec<_>>()) != crate::KIND {
        let name = path.display().to_string();
        return Err(t!("{name} ist keine Lesezeichen-Datei", "{name} is not a bookmarks file"));
    }
    let (updated, answer) = change(&text)?;
    let file_path = path.to_string_lossy().into_owned();
    let (before, result, after) = history::snapshot_around(path, || {
        fs_utils::write_atomic_checked(path, &updated).map_err(|e| e.to_string())?;
        index_manager.update_file(path);
        Ok(())
    });
    record_best_effort(
        db,
        HistoryEntryKind::FileEdited { filename: fs_utils::filename_of(&file_path) },
        false,
        vec![FileSnapshot { path: file_path, before, after }],
    );
    result?;
    Ok(answer)
}

/// DRAFT as a new bookmark in FILE_PATH, at the end of COLLECTION (a path
/// of headings, empty for the top level).
pub fn bookmarks_save(
    draft: Draft,
    file_path: String,
    collection: Vec<String>,
    index_manager: &IndexManager,
    db: &Mutex<Connection>,
) -> Result<Located, String> {
    if draft.url.trim().is_empty() {
        return Err(t!("Ein Lesezeichen braucht eine Adresse.", "A bookmark needs an address."));
    }
    let mut draft = draft;
    if draft.title.trim().is_empty() {
        // Offline, or a page without a title: the site is a better name
        // than nothing, and it can be changed afterwards.
        draft.title = url::host(&draft.url);
    }
    let created = note_utils::inactive_timestamp(chrono::Local::now());
    let line_no = rewrite(Path::new(&file_path), index_manager, db, |text| {
        Ok(insert_under(text, &collection, |level| write::entry(&draft, level, &created)))
    })?;
    Ok(Located { file_path, line_no })
}

/// PATCH applied to the bookmark TARGET names.
pub fn bookmarks_update(target: Target, patch: Patch, index_manager: &IndexManager, db: &Mutex<Connection>) -> Result<Located, String> {
    let line_no = rewrite(Path::new(&target.file_path), index_manager, db, |text| write::apply(text, &target, &patch))?;
    Ok(Located { file_path: target.file_path, line_no })
}

/// Removes the bookmark TARGET names. The app asks first (see the
/// frontend); the history keeps it, one undo away.
pub fn bookmarks_delete(target: Target, index_manager: &IndexManager, db: &Mutex<Connection>) -> Result<(), String> {
    rewrite(Path::new(&target.file_path), index_manager, db, |text| {
        write::remove(text, &target).map(|(text, ..)| (text, ()))
    })
}

/// Moves the bookmark TARGET names to the end of COLLECTION in FILE_PATH
/// -- the same file or another category. Into the new place first, out of
/// the old one second: if the second write fails, the bookmark is there
/// twice rather than nowhere.
pub fn bookmarks_move(
    target: Target,
    file_path: String,
    collection: Vec<String>,
    index_manager: &IndexManager,
    db: &Mutex<Connection>,
) -> Result<Located, String> {
    if Path::new(&file_path) == Path::new(&target.file_path) {
        let line_no = rewrite(Path::new(&file_path), index_manager, db, |text| write::move_within(text, &target, &collection))?;
        return Ok(Located { file_path, line_no });
    }
    let source_text = std::fs::read_to_string(&target.file_path).map_err(|e| e.to_string())?;
    let (_, block, level) = write::remove(&source_text, &target)?;
    let line_no = rewrite(Path::new(&file_path), index_manager, db, |text| {
        Ok(insert_under(text, &collection, |to| write::relevel(&block, level, to)))
    })?;
    rewrite(Path::new(&target.file_path), index_manager, db, |text| {
        write::remove(text, &target).map(|(text, ..)| (text, ()))
    })?;
    Ok(Located { file_path, line_no })
}

/// What the page at URL says about itself -- title and description for
/// the save dialog. Nothing is written.
#[cfg(feature = "fetch")]
pub async fn bookmarks_preview(url: String) -> Result<crate::model::Preview, String> {
    /// Says what it is, as the recipe import does, rather than posing as
    /// a browser.
    const USER_AGENT: &str = "Mozilla/5.0 (compatible; nobo bookmarks)";
    let html = nobo_http::fetch::page(url.trim(), USER_AGENT).await?;
    Ok(crate::preview::read(url.trim(), &html))
}
