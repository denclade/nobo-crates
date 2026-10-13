//! Reading directories: entries, the recursive file list, path suggestions.

use super::paths::{has_hidden_ancestor, is_hidden};
use crate::core::models::FileEntry;
use crate::utils::temp_file_utils::is_temp_file;
use chrono::{DateTime, Utc};
use ignore::WalkBuilder;
use rayon::prelude::*;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub(super) fn system_time_to_iso(t: SystemTime) -> String {
    DateTime::<Utc>::from(t).to_rfc3339()
}

/*
Get all entries in the current directory DIR.
Does not check subdirectories! Used for the settings' arbitrary-path
autocomplete (get_path_suggestions), which browses the whole filesystem, not
just the notes vault -- so unlike directory listings inside the vault (see
core::indexer::IndexManager::directory_contents), this stays a live disk
walk rather than an index lookup.
 */
pub fn get_dir_entries(dir: &Path) -> Vec<(PathBuf, String, bool)> {
    let walker = WalkBuilder::new(dir).max_depth(Some(1)).hidden(true).git_ignore(true).build();

    let mut entries: Vec<(PathBuf, String, bool)> = walker
        .into_iter()
        .filter_map(|res| res.ok())
        .par_bridge()
        .filter(|entry| entry.path() != dir)
        .filter(|entry| !is_temp_file(&entry.file_name().to_string_lossy()))
        .map(|entry| {
            let path = entry.path();
            let is_dir = entry.file_type().map_or(false, |ft| ft.is_dir());
            let name = entry.file_name().to_string_lossy().into_owned();

            (path.to_path_buf(), name, is_dir)
        })
        .collect();

    entries.par_sort_unstable_by(|a, b| b.2.cmp(&a.2).then(a.1.cmp(&b.1)));

    entries
}

/// A `FileEntry` for one path, read from the filesystem -- for the
/// watcher's incremental updates, where only one path is known at a time.
/// `None` when the path is gone or its metadata is unreadable.
///
/// `root` computes `is_in_hidden_dir` relative to the vault rather than
/// the absolute path; `None` leaves it false.
pub fn build_file_entry(path: &Path, root: Option<&Path>) -> Option<FileEntry> {
    let name = path.file_name()?.to_string_lossy().into_owned();
    let metadata = fs::metadata(path).ok()?;
    let is_in_hidden_dir = root.and_then(|r| path.strip_prefix(r).ok()).is_some_and(has_hidden_ancestor);

    Some(FileEntry {
        path: path.to_string_lossy().into_owned(),
        is_hidden: is_hidden(&name),
        is_in_hidden_dir,
        name,
        is_dir: metadata.is_dir(),
        created_at: metadata.created().ok().map(system_time_to_iso),
        modified_at: metadata.modified().ok().map(system_time_to_iso),
    })
}

/// Every file and directory under DIR, recursively, as a FileEntry each --
/// the one full walk core::indexer::IndexManager::scan_directory builds the
/// whole index from (files, notes and headings alike come out of this same
/// pass, rather than each doing their own separate walk). Includes hidden
/// files/dirs (e.g. `.attachments`) -- the index is meant to be a complete
/// record of the vault; callers that need to hide these from a
/// folder-browsing UI filter on `is_hidden`/`is_in_hidden_dir` themselves
/// (see IndexManager::directory_contents*).
pub fn get_file_entries_recursive(dir: &Path) -> Vec<FileEntry> {
    WalkBuilder::new(dir)
        .hidden(false)
        .git_ignore(true)
        .build()
        .into_iter()
        .filter_map(|res| res.ok())
        .par_bridge()
        .filter(|entry| entry.path() != dir)
        .filter(|entry| !is_temp_file(&entry.file_name().to_string_lossy()))
        .map(|entry| {
            let metadata = entry.metadata().ok();
            let is_dir = entry.file_type().map_or(false, |ft| ft.is_dir());
            let name = entry.file_name().to_string_lossy().into_owned();
            let relative = entry.path().strip_prefix(dir).unwrap_or_else(|_| entry.path());

            FileEntry {
                path: entry.path().to_string_lossy().into_owned(),
                is_hidden: is_hidden(&name),
                is_in_hidden_dir: has_hidden_ancestor(relative),
                name,
                is_dir,
                created_at: metadata.as_ref().and_then(|m| m.created().ok()).map(system_time_to_iso),
                modified_at: metadata.as_ref().and_then(|m| m.modified().ok()).map(system_time_to_iso),
            }
        })
        .collect()
}

/*
Get all files in DIR with extension EXT. Searches in ALL subdirectories of DIR.
 */
pub fn get_files_by_extension(dir: &Path, ext: &str) -> Vec<PathBuf> {
    WalkBuilder::new(dir)
        .hidden(true)
        .git_ignore(true)
        .build()
        .into_iter()
        .filter_map(|res| res.ok())
        .par_bridge()
        .filter(|entry| {
            let path = entry.path();
            path.is_file() && path.extension().and_then(|e| e.to_str()) == Some(ext)
        })
        .map(|entry| entry.path().to_path_buf())
        .collect()
}

/*
Checks path INPUT for latest valid path and returns a list of possible
continuations to the path.
e.g.
INPUT: /home/user
checks all folders in /home for files starting with de
 */
pub fn get_path_suggestions(input: &str) -> Vec<String> {
    if input.is_empty() {
        return vec![];
    }

    let input_path = Path::new(input);

    let (dir_to_search, prefix) = if input.ends_with('/') || input.ends_with('\\') {
        (input_path, "")
    } else {
        (input_path.parent().unwrap_or_else(|| Path::new("")), input_path.file_name().and_then(|n| n.to_str()).unwrap_or(""))
    };

    let search_dir = if dir_to_search.as_os_str().is_empty() { Path::new(".") } else { dir_to_search };
    let prefix_lower = prefix.to_lowercase();

    let entries = get_dir_entries(search_dir);

    let mut suggestions: Vec<String> = entries
        .into_iter()
        .filter(|(_, name, is_dir)| *is_dir && name.to_lowercase().starts_with(&prefix_lower))
        .map(|(path, _, _)| {
            let mut full_path = path.to_string_lossy().to_string();
            if !full_path.ends_with(std::path::MAIN_SEPARATOR) {
                full_path.push(std::path::MAIN_SEPARATOR);
            }
            full_path
        })
        .take(5)
        .collect();

    suggestions.sort();
    suggestions
}
