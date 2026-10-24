// SPDX-FileCopyrightText: 2026 Dennis Burgermeister
// SPDX-License-Identifier: GPL-3.0-or-later

//! What a host can ask this feature for.
//!
//! Plain functions: the Tauri build wraps them (see `tauri_commands`), any
//! other host calls them directly. Every write goes through
//! core::vault_write -- one file at a time, only bookmarks files, one
//! history entry each, so every change here is one undo away.

use std::path::Path;
use std::sync::Mutex;

use rusqlite::Connection;

use nobo_core::core::indexer::IndexManager;
use nobo_core::core::vault_write::{edit_file, edit_files};
use nobo_core::t;
use nobo_core::utils::note_utils;

use crate::categories::bookmark_files;
pub use crate::categories::{bookmarks_create_file, bookmarks_files, bookmarks_set_file, category_file_name};
use crate::model::{Bookmark, Collection, Draft, Located, Patch, Target};
use crate::{parse, url, write};

/// Every bookmark in every bookmarks file: files by title, bookmarks in
/// the order they are written.
pub fn bookmarks_list(index_manager: &IndexManager) -> Result<Vec<Bookmark>, String> {
    let mut files: Vec<(String, String, String)> =
        bookmark_files(index_manager)?.into_iter().map(|(path, text)| (parse::category_title(&path, &text), path, text)).collect();
    files.sort_by_key(|(title, ..)| title.to_lowercase());
    Ok(files.iter().flat_map(|(title, path, text)| parse::bookmarks_of(path, title, text)).collect())
}

/// Every collection in every bookmarks file, empty ones included -- what
/// the save and move dialogs offer to file under.
pub fn bookmarks_collections(index_manager: &IndexManager) -> Result<Vec<Collection>, String> {
    Ok(bookmark_files(index_manager)?
        .iter()
        .flat_map(|(path, text)| {
            parse::collections_of(text).into_iter().map(|(collection, line_no)| Collection {
                file_path: path.clone(),
                path: collection,
                line_no,
            })
        })
        .collect())
}

/// The bookmarks that already point to URL, in any category -- compared
/// without fragments, tracking parameters and the like (see url::normalize).
pub fn bookmarks_find(url: String, index_manager: &IndexManager) -> Result<Vec<Bookmark>, String> {
    let wanted = url::normalize(&url);
    Ok(bookmarks_list(index_manager)?.into_iter().filter(|bookmark| url::normalize(&bookmark.url) == wanted).collect())
}

/// Refuses URL when a bookmark other than EXCEPT (a key) already points
/// there: a page is saved once, and found again where it is.
fn refuse_duplicate(url: &str, except: Option<&str>, index_manager: &IndexManager) -> Result<(), String> {
    let Some(existing) = bookmarks_find(url.to_string(), index_manager)?.into_iter().find(|bookmark| Some(bookmark.key.as_str()) != except)
    else {
        return Ok(());
    };
    let place = std::iter::once(existing.category.clone()).chain(existing.collection.iter().cloned()).collect::<Vec<_>>().join(" › ");
    let title = existing.title;
    Err(t!("Diese Adresse ist schon gespeichert: „{title}“ in {place}.", "This address is already saved: \"{title}\" in {place}."))
}

/// Where the bookmark at LINE (1-based) of TEXT is now, by its key.
fn located(file_path: &str, text: &str, line_no: usize) -> Located {
    let key = parse::bookmarks_of(file_path, "", text)
        .into_iter()
        .find(|bookmark| bookmark.line_no == line_no)
        .map(|bookmark| bookmark.key)
        .unwrap_or_default();
    Located { key, file_path: file_path.to_string(), line_no }
}

/// Now, as `:CREATED:` and `:UPDATED:` say it.
fn now() -> String {
    note_utils::inactive_timestamp(chrono::Local::now())
}

/// DRAFT as a new bookmark in FILE_PATH, at the end of COLLECTION (a path
/// of headings, empty for the top level). Refused for an address that is
/// already saved -- the app offers to open that one instead.
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
    refuse_duplicate(&draft.url, None, index_manager)?;
    let mut draft = draft;
    draft.url = draft.url.trim().to_string();
    if draft.title.trim().is_empty() {
        // Offline, or a page without a title: the site is a better name
        // than nothing, and it can be changed afterwards.
        draft.title = url::host(&draft.url);
    }
    let created = now();
    edit_file(Path::new(&file_path), Some(crate::KIND), index_manager, db, |text| {
        let (text, line_no) = write::file_under(text, &collection, |level| write::entry(&draft, level, &created));
        let place = located(&file_path, &text, line_no);
        Ok((text, place))
    })
}

/// PATCH applied to the bookmark TARGET names. A new address that another
/// bookmark already has is refused, as on saving.
pub fn bookmarks_update(target: Target, patch: Patch, index_manager: &IndexManager, db: &Mutex<Connection>) -> Result<Located, String> {
    if let Some(url) = patch.url.as_deref().filter(|url| url::normalize(url) != url::normalize(&target.url)) {
        let own = bookmarks_list(index_manager)?
            .into_iter()
            .find(|bookmark| bookmark.file_path == target.file_path && bookmark.url == target.url)
            .map(|bookmark| bookmark.key);
        refuse_duplicate(url, own.as_deref(), index_manager)?;
    }
    let now = now();
    edit_file(Path::new(&target.file_path), Some(crate::KIND), index_manager, db, |text| {
        let (text, line_no) = write::apply(text, &target, &patch, &now)?;
        let place = located(&target.file_path, &text, line_no);
        Ok((text, place))
    })
}

/// Removes the bookmark TARGET names. The app asks first (see the
/// frontend); the history keeps it, one undo away.
pub fn bookmarks_delete(target: Target, index_manager: &IndexManager, db: &Mutex<Connection>) -> Result<(), String> {
    edit_file(Path::new(&target.file_path), Some(crate::KIND), index_manager, db, |text| {
        write::remove(text, &target).map(|(text, ..)| (text, ()))
    })
}

/// Moves the bookmark TARGET names to the end of COLLECTION in FILE_PATH
/// -- the same file or another category. Across files both are written
/// under one lock and one history entry: one undo puts it back.
pub fn bookmarks_move(
    target: Target,
    file_path: String,
    collection: Vec<String>,
    index_manager: &IndexManager,
    db: &Mutex<Connection>,
) -> Result<Located, String> {
    if Path::new(&file_path) == Path::new(&target.file_path) {
        return edit_file(Path::new(&file_path), Some(crate::KIND), index_manager, db, |text| {
            let (text, line_no) = write::move_within(text, &target, &collection)?;
            let place = located(&file_path, &text, line_no);
            Ok((text, place))
        });
    }
    let paths = [Path::new(&file_path), Path::new(&target.file_path)];
    edit_files(&paths, Some(crate::KIND), index_manager, db, |texts| {
        let (source, block, level) = write::remove(&texts[1], &target)?;
        let (destination, line_no) = write::file_under(&texts[0], &collection, |to| write::relevel(&block, level, to));
        let place = located(&file_path, &destination, line_no);
        Ok((vec![destination, source], place))
    })
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
        let text = std::fs::read_to_string(&file_path).unwrap_or_default();
        return crate::import_linkwarden::preview(&text, &json, &existing);
    }
    edit_file(Path::new(&file_path), Some(crate::KIND), index_manager, db, |text| crate::import_linkwarden::import(text, &json, &existing))
}
