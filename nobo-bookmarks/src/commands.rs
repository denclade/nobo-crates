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


use crate::model::{Bookmark, BookmarkFile, Collection, Draft, Located, Patch, Target};
use crate::{parse, url, write};

/// Every bookmark in every bookmarks file: files by title, bookmarks in
/// the order they are written.
pub fn bookmarks_list(index_manager: &IndexManager) -> Result<Vec<Bookmark>, String> {
    let mut files = bookmark_files(index_manager)?;
    files.sort_by_key(|(path, text)| parse::category_title(path, text).to_lowercase());
    Ok(files
        .iter()
        .flat_map(|(path, text)| parse::bookmarks_of(path, &parse::category_title(path, text), text))
        .collect())
}

/// The bookmarks files with their text, as the index holds them.
fn bookmark_files(index_manager: &IndexManager) -> Result<Vec<(String, String)>, String> {
    let root = Config::require_vault_root()?;
    Ok(Vault::new(index_manager, &root)
        .files_of_kind(crate::KIND)
        .into_iter()
        .map(|(path, text)| (path.to_string_lossy().into_owned(), text))
        .collect())
}

/// Every collection in every bookmarks file, empty ones included -- what
/// the save and move dialogs offer to file under.
pub fn bookmarks_collections(index_manager: &IndexManager) -> Result<Vec<Collection>, String> {
    Ok(bookmark_files(index_manager)?
        .iter()
        .flat_map(|(path, text)| {
            parse::collections_of(text).into_iter().map(|collection| Collection { file_path: path.clone(), path: collection })
        })
        .collect())
}

/// Every org file that could be a bookmarks file, with the ones that are
/// marked. Generated mirrors are left out -- their own sync rewrites them.
pub fn bookmarks_files(index_manager: &IndexManager) -> Result<Vec<BookmarkFile>, String> {
    let root = Config::require_vault_root()?;
    let vault = Vault::new(index_manager, &root);
    let titles: HashMap<String, String> = vault.notes().into_iter().map(|note| (note.path, note.title)).collect();
    let mut files: Vec<BookmarkFile> = vault
        .org_files()
        .into_iter()
        .filter_map(|(path, text)| {
            let kind = file_kind::from_lines(&text.lines().collect::<Vec<_>>());
            let path = path.to_string_lossy().into_owned();
            let is_bookmarks = kind == crate::KIND;
            // The app's own files (in `.nobo/`) other than bookmarks files
            // are no candidates: the GTD lists, the mirrors, the archive.
            if !file_kind::capabilities(kind).editable || (!is_bookmarks && nobo_core::utils::fs_utils::has_hidden_ancestor(Path::new(&path).strip_prefix(&root).unwrap_or(Path::new(&path)))) {
                return None;
            }
            let title = if is_bookmarks {
                parse::category_title(&path, &text)
            } else {
                titles.get(&path).cloned().unwrap_or_else(|| fs_utils::filename_of(&path).trim_end_matches(".org").to_string())
            };
            Some(BookmarkFile { title, path, is_bookmarks })
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

/// The file name a category is kept under: `bookmarks-work.org` for
/// "Work", umlauts spelled out, anything else a dash.
pub fn category_file_name(title: &str) -> String {
    let mut slug = String::new();
    for c in title.trim().to_lowercase().chars() {
        let part = match c {
            'ä' => "ae".to_string(),
            'ö' => "oe".to_string(),
            'ü' => "ue".to_string(),
            'ß' => "ss".to_string(),
            c if c.is_alphanumeric() => c.to_string(),
            _ => "-".to_string(),
        };
        if part == "-" && (slug.is_empty() || slug.ends_with('-')) {
            continue;
        }
        slug.push_str(&part);
    }
    let slug = slug.trim_end_matches('-');
    format!("bookmarks-{}.org", if slug.is_empty() { "bookmarks" } else { slug })
}

/// A new, empty bookmarks file called TITLE ("Arbeit", "Privat") -- a new
/// category -- kept with the app's own files: `.nobo/bookmarks-arbeit.org`.
/// There it is the app's, never renamed by the notes editor (see
/// core::app_files), and found by its declaration like any bookmarks file.
///
/// Never written over an existing file: one that is already a bookmarks
/// file is simply that category; anything else there is refused.
pub fn bookmarks_create_file(title: String, index_manager: &IndexManager, db: &Mutex<Connection>) -> Result<BookmarkFile, String> {
    let title = title.trim().to_string();
    let title = if title.is_empty() { t!("Lesezeichen", "Bookmarks") } else { title };
    let root = Config::require_vault_root()?;
    let dir = root.join(".nobo");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join(category_file_name(&title));
    let path_string = path.to_string_lossy().into_owned();
    let content = format!("#+TITLE:      {title}\n{} {}\n", file_kind::KIND_KEYWORD, crate::KIND.keyword());

    let created = std::fs::OpenOptions::new().write(true).create_new(true).open(&path);
    match created {
        Ok(mut file) => {
            use std::io::Write;
            file.write_all(content.as_bytes()).map_err(|e| e.to_string())?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let existing = std::fs::read_to_string(&path).unwrap_or_default();
            if file_kind::from_lines(&existing.lines().collect::<Vec<_>>()) == crate::KIND {
                return Ok(BookmarkFile { title: parse::category_title(&path_string, &existing), path: path_string, is_bookmarks: true });
            }
            let name = path.display().to_string();
            return Err(t!("{name} gibt es schon und ist keine Lesezeichen-Datei", "{name} already exists and is not a bookmarks file"));
        }
        Err(e) => return Err(e.to_string()),
    }
    index_manager.update_file_with(&path, &content);
    record_best_effort(
        db,
        HistoryEntryKind::FileCreated { filename: fs_utils::filename_of(&path_string) },
        false,
        vec![FileSnapshot { path: path_string.clone(), before: None, after: Some(content) }],
    );
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
        let text = write::ensure_collection(text.to_string(), &collection, None);
        Ok(insert_under(&text, &collection, |level| write::entry(&draft, level, &created)))
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
        let text = write::ensure_collection(text.to_string(), &collection, None);
        Ok(insert_under(&text, &collection, |to| write::relevel(&block, level, to)))
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

/// A Linkwarden backup (JSON, the file's text) into the bookmarks file
/// FILE_PATH: its collections as headings, its links as bookmarks, what
/// is already saved anywhere skipped (see import_linkwarden). With
/// DRY_RUN nothing is written -- the report says what would happen. One
/// write, one history entry: the whole import is one undo away.
pub fn bookmarks_import_linkwarden(
    json: String,
    file_path: String,
    dry_run: bool,
    index_manager: &IndexManager,
    db: &Mutex<Connection>,
) -> Result<crate::import_linkwarden::ImportReport, String> {
    let existing: std::collections::HashSet<String> =
        bookmarks_list(index_manager)?.iter().map(|bookmark| url::normalize(&bookmark.url)).collect();
    if dry_run {
        return crate::import_linkwarden::preview(&json, &existing);
    }
    rewrite(Path::new(&file_path), index_manager, db, |text| crate::import_linkwarden::import(text, &json, &existing))
}

#[cfg(test)]
mod tests {
    use super::category_file_name;

    #[test]
    fn a_category_has_a_file_name_of_its_own() {
        assert_eq!(category_file_name("Work"), "bookmarks-work.org");
        assert_eq!(category_file_name(" Privates & Schule "), "bookmarks-privates-schule.org");
        assert_eq!(category_file_name("Bücher"), "bookmarks-buecher.org");
        assert_eq!(category_file_name("!!"), "bookmarks-bookmarks.org");
    }
}
