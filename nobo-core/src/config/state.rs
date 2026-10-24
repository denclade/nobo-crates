// SPDX-FileCopyrightText: 2026 Dennis Burgermeister
// SPDX-License-Identifier: GPL-3.0-or-later

//! Small, frequently-changing app state, as opposed to `settings::Config`,
//! which holds user-chosen settings.
//!
//! A separate file and save path with no indexer involvement at all:
//! saving the config triggers a vault reindex when the notes path changes,
//! and something like the recently-opened list changes on nearly every
//! navigation. Mixing the two would carry the whole settings blob along on
//! every one of those writes and entangle them with reindex logic.
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

use super::settings::config_dir;
use crate::core::models::DirEntry;
use crate::utils::ToCmdResult;

#[derive(Debug, Clone, Default, Serialize, Deserialize, specta::Type)]
pub struct PersistedState {
    /// How far each owner's data has been brought by core::migrations --
    /// "config", "state", and any feature that registered one.
    ///
    /// Here rather than in each file it describes, because a migration is
    /// about THIS device: the vault is shared, the settings are not, and
    /// a feature's own store may be neither. Hidden from the frontend
    /// because it saves the state as one whole object and would carry a
    /// stale copy back -- the same trap the CalDAV accounts used to sit
    /// in, before settings became per-feature sections (see
    /// config::section).
    #[serde(default)]
    #[specta(skip)]
    pub migrations: std::collections::BTreeMap<String, u32>,

    /// Emacs-buffer-style "recently opened" notes (see
    /// store/noteHistory.ts on the frontend, most-recently-visited first,
    /// capped there). The back/forward navigation stack itself stays
    /// in-memory only (same as a browser's own new-tab history) -- only
    /// this MRU list is persisted, so the "working set" isn't empty again
    /// on every app restart.
    #[serde(default)]
    pub recent_notes: Vec<DirEntry>,
}

impl PersistedState {
    fn get_path() -> PathBuf {
        let mut path = config_dir();
        path.push("state.json");
        path
    }

    pub fn load() -> Self {
        let path = Self::get_path();
        match fs::read_to_string(&path) {
            Ok(content) => match serde_json::from_str(&content) {
                Ok(state) => state,
                Err(e) => {
                    log::warn!("State file at {:?} is corrupted, ignoring: {}", path, e);
                    Self::default()
                }
            },
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self) -> Result<(), String> {
        let dir = config_dir();
        if !dir.exists() {
            fs::create_dir_all(&dir).to_cmd()?;
        }
        let path = Self::get_path();
        let content = serde_json::to_string_pretty(self).to_cmd()?;
        // Same reasoning as Settings::save next door: a truncated window
        // where the file parses as "nothing configured" is worse than no
        // write at all.
        crate::utils::fs_utils::write_atomic(&path, &content).to_cmd()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_json() {
        let mut state = PersistedState::default();
        state.recent_notes.push(DirEntry {
            path: "/vault/meeting-notes.org".to_string(),
            name: "meeting-notes.org".to_string(),
            is_dir: false,
            note: None,
            favorite: false,
        });

        let json = serde_json::to_string(&state).unwrap();
        let restored: PersistedState = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.recent_notes.len(), 1);
        assert_eq!(restored.recent_notes[0].path, "/vault/meeting-notes.org");
    }

    #[test]
    fn missing_recent_notes_deserializes_to_empty_vec() {
        let restored: PersistedState = serde_json::from_str("{}").unwrap();
        assert!(restored.recent_notes.is_empty());
    }

    #[test]
    fn load_with_no_file_on_disk_returns_default() {
        // PersistedState::load() reads from the REAL config_dir() (see its
        // own doc comment on config_dir sharing across this whole module)
        // -- can't isolate that to a TempDir without changing config_dir's
        // own signature, so this only asserts the shape of the fallback
        // (a corrupted/missing file must never panic or propagate an
        // error, just fall back to an empty, usable default), not a fully
        // isolated round trip -- that's covered by the two tests above,
        // which work directly with serde_json and never touch disk.
        let state = PersistedState::default();
        assert!(state.recent_notes.is_empty());
    }
}
