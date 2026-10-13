//! The bookmarks files themselves -- the categories: which files there
//! are, which could become one, a new one, declaring one.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::Connection;

use nobo_core::config::settings::Config;
use nobo_core::core::file_kind;
use nobo_core::core::history::record_best_effort;
use nobo_core::core::indexer::IndexManager;
use nobo_core::core::models::{FileSnapshot, HistoryEntryKind};
use nobo_core::core::vault::Vault;
use nobo_core::core::vault_write::{self, app_dir};
use nobo_core::t;
use nobo_core::utils::fs_utils;

use crate::model::BookmarkFile;
use crate::parse;

/// The bookmarks files with their text, as the index holds them.
pub fn bookmark_files(index_manager: &IndexManager) -> Result<Vec<(String, String)>, String> {
    let root = Config::require_vault_root()?;
    Ok(Vault::new(index_manager, &root)
        .files_of_kind(crate::KIND)
        .into_iter()
        .map(|(path, text)| (path.to_string_lossy().into_owned(), text))
        .collect())
}

/// Every org file that could be a bookmarks file, with the ones that are
/// marked. Generated mirrors are left out -- their own sync rewrites them.
pub fn bookmarks_files(index_manager: &IndexManager) -> Result<Vec<BookmarkFile>, String> {
    let root = Config::require_vault_root()?;
    let mut files: Vec<BookmarkFile> = Vault::new(index_manager, &root)
        .kind_candidates(crate::KIND)
        .into_iter()
        .map(|candidate| {
            let path = candidate.path.to_string_lossy().into_owned();
            let title = if candidate.is_kind {
                parse::category_title(&path, &candidate.text)
            } else {
                candidate.title.unwrap_or_else(|| fs_utils::filename_of(&path).trim_end_matches(".org").to_string())
            };
            BookmarkFile { title, path, is_bookmarks: candidate.is_kind }
        })
        .collect();
    files.sort_by_key(|file| file.title.to_lowercase());
    Ok(files)
}

/// Declares a file a bookmarks file -- a category -- or takes that back.
/// Only the declaration is written; the bookmarks are the user's text and
/// stay where they are either way. One undo away, like every other write.
pub fn bookmarks_set_file(path: String, bookmarks: bool, index_manager: &IndexManager, db: &Mutex<Connection>) -> Result<(), String> {
    vault_write::declare_kind(&PathBuf::from(&path), crate::KIND, bookmarks, index_manager, db)
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
    let dir = app_dir(&Config::require_vault_root()?);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join(category_file_name(&title));
    let path_string = path.to_string_lossy().into_owned();
    let content = format!("#+TITLE:      {title}\n{} {}\n", file_kind::KIND_KEYWORD, crate::KIND.keyword());

    match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(mut file) => {
            use std::io::Write;
            file.write_all(content.as_bytes()).map_err(|e| e.to_string())?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return existing_category(&path),
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

/// The category at PATH, which is already there -- or a refusal when the
/// file there is something else.
fn existing_category(path: &Path) -> Result<BookmarkFile, String> {
    let path_string = path.to_string_lossy().into_owned();
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    if file_kind::from_lines(&existing.lines().collect::<Vec<_>>()) == crate::KIND {
        return Ok(BookmarkFile { title: parse::category_title(&path_string, &existing), path: path_string, is_bookmarks: true });
    }
    let name = path.display().to_string();
    Err(t!("{name} gibt es schon und ist keine Lesezeichen-Datei", "{name} already exists and is not a bookmarks file"))
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
