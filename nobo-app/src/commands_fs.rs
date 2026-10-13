use nobo_core::core::indexer::IndexManager;
use nobo_core::core::models::DirEntry;
use nobo_core::utils::fs_utils;
use std::path::PathBuf;

pub fn get_directory_contents(dir: String, show_hidden: bool, index_state: &IndexManager) -> Vec<DirEntry> {
    index_state.directory_contents(&PathBuf::from(dir), show_hidden)
}

/// Same as get_directory_contents, but walks every subdirectory of `dir` too
/// -- backs the Notes sidebar's Ctrl+F "include subfolders" filter toggle.
pub fn get_directory_contents_recursive(dir: String, show_hidden: bool, index_state: &IndexManager) -> Vec<DirEntry> {
    index_state.directory_contents_recursive(&PathBuf::from(dir), show_hidden)
}

pub fn get_suggestions(input: String) -> Vec<String> {
    fs_utils::get_path_suggestions(&input)
}

/// Every file under `base_dir` (recursively), as `/`-joined paths relative
/// to it -- feeds the editor's `[[file:` completion (see
/// src/components/editor/completion/filePathSource.ts), which fuzzy-matches
/// client-side against this list. `/` regardless of OS, matching org-mode's
/// own link convention (so a link written on one OS still resolves on another).
pub fn list_files_for_completion(base_dir: String, index_state: &IndexManager) -> Vec<String> {
    index_state.files_relative_to(&PathBuf::from(base_dir))
}

/// What the app may do with the file at `file_path`, on this device.
///
/// Asked by the frontend before opening an editor, so it can offer the
/// affordances the file actually supports -- a read-only mirror gets no
/// editor at all, a synced task list gets a checkbox instead of the full
/// todo workflow. Enforced on the write side independently (see
/// fs_utils::write_lines): a check the UI performs is a convenience, not
/// a rule.
///
/// Some of the answer is not a property of the file but of this device --
/// a synced list nobody has logged in to here cannot be written, even
/// though the same file is editable elsewhere. Whoever knows that
/// registers a veto; this command knows none of them by name (see
/// core::file_kind::WriteVeto).
pub async fn file_capabilities(file_path: String) -> nobo_core::core::file_kind::FileCapabilities {
    nobo_core::core::file_kind::capabilities_for_path(std::path::Path::new(&file_path)).await
}
