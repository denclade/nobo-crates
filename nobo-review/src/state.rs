// SPDX-FileCopyrightText: 2026 Dennis Burgermeister
// SPDX-License-Identifier: GPL-3.0-or-later

//! Persists the GTD Weekly Review's cross-device state -- when it was last
//! completed, and which checklist steps are currently ticked mid-review
//! (see src/features/review). Stored at `<vault_root>/.nobo/review-state.json`
//! rather than alongside the rest of `Config` in settings.json: unlike a
//! device-local preference (sort mode, theme, the review cadence in this
//! crate's own `Settings`, ...),
//! "when did I last actually do the review" is a fact about the user's GTD
//! system as a whole and needs to read the same on every device, so it has
//! to live somewhere sync's file-based rescan (nobo_sync::rescan) picks up
//! automatically -- exactly the same reasoning the default
//! actions.org/projects.org/inbox.org/someday.org paths already follow,
//! all under `./.nobo/` (see config::settings::Config).

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "tauri", derive(specta::Type))]
pub struct ReviewState {
    #[serde(default)]
    pub last_review_completed_at: Option<String>,
    #[serde(default)]
    pub checked_steps: Vec<String>,
}

fn store_path(vault_root: &Path) -> PathBuf {
    nobo_core::core::vault_write::app_dir(vault_root).join("review-state.json")
}

/// Missing or corrupted file both read as "never reviewed yet" (the default)
/// rather than an error -- a fresh vault, or one that hasn't synced this
/// file down yet, is exactly that state, not a failure.
pub fn load(vault_root: &Path) -> ReviewState {
    fs::read_to_string(store_path(vault_root)).ok().and_then(|content| serde_json::from_str(&content).ok()).unwrap_or_default()
}

pub fn save(vault_root: &Path, state: &ReviewState) -> Result<(), String> {
    let path = store_path(vault_root);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let content = serde_json::to_string_pretty(state).map_err(|e| e.to_string())?;
    // Lives inside the vault, so sync reads it while the app writes it.
    nobo_core::utils::fs_utils::write_atomic(&path, &content).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn missing_file_returns_default() {
        let dir = TempDir::new().unwrap();
        let state = load(dir.path());
        assert!(state.last_review_completed_at.is_none());
        assert!(state.checked_steps.is_empty());
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = TempDir::new().unwrap();
        let state = ReviewState {
            last_review_completed_at: Some("2026-08-13T10:00:00Z".to_string()),
            checked_steps: vec!["collect".to_string(), "actions".to_string()],
        };
        save(dir.path(), &state).unwrap();

        let loaded = load(dir.path());
        assert_eq!(loaded.last_review_completed_at, state.last_review_completed_at);
        assert_eq!(loaded.checked_steps, state.checked_steps);
    }

    #[test]
    fn save_creates_the_missing_nobo_directory() {
        let dir = TempDir::new().unwrap();
        assert!(!dir.path().join(".nobo").exists());

        save(dir.path(), &ReviewState::default()).unwrap();

        assert!(store_path(dir.path()).exists());
    }

    #[test]
    fn corrupted_file_reads_as_default_not_a_crash() {
        let dir = TempDir::new().unwrap();
        let path = store_path(dir.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "not json").unwrap();

        assert!(load(dir.path()).last_review_completed_at.is_none());
    }
}
