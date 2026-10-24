// SPDX-FileCopyrightText: 2026 Dennis Burgermeister
// SPDX-License-Identifier: GPL-3.0-or-later

//! The same commands, as Tauri sees them.
//!
//! One line each: everything that decides anything lives in `commands`.
//! The names and doc comments are what tauri-specta writes into
//! `bindings.ts`, so they describe the contract the frontend reads.

use std::sync::{Arc, Mutex};

use rusqlite::Connection;
use tauri::State;

use nobo_core::core::indexer::IndexManager;

use crate::commands;
use crate::model::{Bookmark, BookmarkFile, Collection, Draft, Located, Patch, Target};

/// Every bookmark in every bookmarks file (category).
#[tauri::command]
#[specta::specta]
pub async fn bookmarks_list(index_manager: State<'_, Arc<IndexManager>>) -> Result<Vec<Bookmark>, String> {
    commands::bookmarks_list(index_manager.inner())
}

/// Every org file that could be a bookmarks file, the ones that are marked.
#[tauri::command]
#[specta::specta]
pub async fn bookmarks_files(index_manager: State<'_, Arc<IndexManager>>) -> Result<Vec<BookmarkFile>, String> {
    commands::bookmarks_files(index_manager.inner())
}

/// Every collection in every bookmarks file, empty ones included.
#[tauri::command]
#[specta::specta]
pub async fn bookmarks_collections(index_manager: State<'_, Arc<IndexManager>>) -> Result<Vec<Collection>, String> {
    commands::bookmarks_collections(index_manager.inner())
}

/// Declares a file a bookmarks file (a category), or takes that back.
#[tauri::command]
#[specta::specta]
pub async fn bookmarks_set_file(
    path: String,
    bookmarks: bool,
    index_manager: State<'_, Arc<IndexManager>>,
    db: State<'_, Arc<Mutex<Connection>>>,
) -> Result<(), String> {
    commands::bookmarks_set_file(path, bookmarks, index_manager.inner(), db.inner())
}

/// A new, empty bookmarks file called TITLE -- a new category, kept as
/// `.nobo/bookmarks-<title>.org`.
#[tauri::command]
#[specta::specta]
pub async fn bookmarks_create_file(
    title: String,
    index_manager: State<'_, Arc<IndexManager>>,
    db: State<'_, Arc<Mutex<Connection>>>,
) -> Result<BookmarkFile, String> {
    commands::bookmarks_create_file(title, index_manager.inner(), db.inner())
}

/// The bookmarks that already point to URL, in any category.
#[tauri::command]
#[specta::specta]
pub async fn bookmarks_find(url: String, index_manager: State<'_, Arc<IndexManager>>) -> Result<Vec<Bookmark>, String> {
    commands::bookmarks_find(url, index_manager.inner())
}

/// DRAFT as a new bookmark in FILE_PATH, under COLLECTION. One history entry.
#[tauri::command]
#[specta::specta]
pub async fn bookmarks_save(
    draft: Draft,
    file_path: String,
    collection: Vec<String>,
    index_manager: State<'_, Arc<IndexManager>>,
    db: State<'_, Arc<Mutex<Connection>>>,
) -> Result<Located, String> {
    commands::bookmarks_save(draft, file_path, collection, index_manager.inner(), db.inner())
}

/// PATCH applied to the bookmark TARGET names. One history entry.
#[tauri::command]
#[specta::specta]
pub async fn bookmarks_update(
    target: Target,
    patch: Patch,
    index_manager: State<'_, Arc<IndexManager>>,
    db: State<'_, Arc<Mutex<Connection>>>,
) -> Result<Located, String> {
    commands::bookmarks_update(target, patch, index_manager.inner(), db.inner())
}

/// Removes the bookmark TARGET names. One history entry, so one undo away.
#[tauri::command]
#[specta::specta]
pub async fn bookmarks_delete(
    target: Target,
    index_manager: State<'_, Arc<IndexManager>>,
    db: State<'_, Arc<Mutex<Connection>>>,
) -> Result<(), String> {
    commands::bookmarks_delete(target, index_manager.inner(), db.inner())
}

/// Moves the bookmark TARGET names under COLLECTION in FILE_PATH.
#[tauri::command]
#[specta::specta]
pub async fn bookmarks_move(
    target: Target,
    file_path: String,
    collection: Vec<String>,
    index_manager: State<'_, Arc<IndexManager>>,
    db: State<'_, Arc<Mutex<Connection>>>,
) -> Result<Located, String> {
    commands::bookmarks_move(target, file_path, collection, index_manager.inner(), db.inner())
}

/// What the page at URL says about itself. Nothing is written. In a
/// build without networking, registered all the same and refused with a
/// reason -- the save dialog then falls back to the site as the title.
#[tauri::command]
#[specta::specta]
pub async fn bookmarks_preview(url: String) -> Result<crate::model::Preview, String> {
    #[cfg(feature = "fetch")]
    return commands::bookmarks_preview(url).await;
    #[cfg(not(feature = "fetch"))]
    return {
        let _ = url;
        Err(nobo_core::t!("Diese Version von nobo ruft keine Seiten ab.", "This build of nobo does not fetch pages."))
    };
}

/// A Linkwarden backup (the file's text) into the bookmarks file
/// FILE_PATH; with DRY_RUN only the report of what would happen. One
/// history entry for the whole import.
#[tauri::command]
#[specta::specta]
pub async fn bookmarks_import_linkwarden(
    json: String,
    file_path: String,
    dry_run: bool,
    index_manager: State<'_, Arc<IndexManager>>,
    db: State<'_, Arc<Mutex<Connection>>>,
) -> Result<crate::import_linkwarden::ImportReport, String> {
    commands::bookmarks_import_linkwarden(json, file_path, dry_run, index_manager.inner(), db.inner())
}
