// SPDX-FileCopyrightText: 2026 Dennis Burgermeister
// SPDX-License-Identifier: GPL-3.0-or-later

use nobo_core::config::settings::Config;
use nobo_core::config::state::PersistedState;
use nobo_core::core::indexer::{IndexListener, IndexManager};
use nobo_core::utils::fs_utils;
use nobo_core::utils::ToCmdResult;
use std::path::PathBuf;
use std::sync::Arc;

pub fn get_config() -> Option<Config> {
    Config::load()
}

pub fn save_config(
    index_manager: &Arc<IndexManager>,
    listener: Arc<dyn IndexListener>,
    scope: &dyn crate::VaultScope,
    config: Config,
) -> Result<(), String> {
    // Nothing is read back from disk here any more. The frontend sends
    // the whole config from a copy it loaded at startup, so anything the
    // backend owns used to be reverted by an unrelated setting change --
    // the CalDAV accounts were re-read here for exactly that reason. They
    // are a feature section now: not part of this type at all, and kept
    // by Config::save itself (see config::section).
    config.save().to_cmd()?;

    if !config.notes_path.is_empty() {
        // A freshly typed vault path may not exist yet, and an existing one
        // can be gone (an unmounted drive, a moved folder) -- see
        // fs_utils::ensure_dir_exists. Created before canonicalizing, which
        // otherwise hands back a path that resolves to nothing.
        if let Err(e) = fs_utils::ensure_dir_exists(std::path::Path::new(&config.notes_path)) {
            log::warn!("Could not create the configured notes directory: {}", e);
        }

        let notes_dir = fs_utils::canonicalize_or_self(&PathBuf::from(&config.notes_path));

        // Before the walk, not after: whatever the host has to open up
        // for this directory has to be open while it is read (see
        // crate::VaultScope).
        scope.allow(&notes_dir)?;

        index_manager.clone().reindex_and_watch(listener, &notes_dir);
    }

    Ok(())
}

/// Small, frequently-changing app STATE (e.g. the recently-opened notes
/// list) -- deliberately separate from get_config/save_config above, see
/// PersistedState's own doc comment for why: nothing here ever touches
/// IndexManager, so persisting it can never trigger a vault reindex the
/// way changing `notes_path` through save_config does.
pub fn get_persisted_state() -> PersistedState {
    PersistedState::load()
}

/// Saves the part of the persisted state the frontend actually owns.
///
/// Only `recent_notes`. The struct also carries the migration ledger, and
/// `#[specta(skip)]` removes that field from the generated TypeScript but
/// not from serde -- so whatever the frontend sent was deserialized with
/// `#[serde(default)]` filling an EMPTY map, and the whole struct written
/// back. It survived only because `persistedState.ts` happens to spread a
/// key TypeScript does not know about.
///
/// The dominant way to lose it was never a frontend bug though: a corrupt
/// or half-written state.json makes `PersistedState::load` return
/// `Default`, and the next navigation wrote `"migrations": {}` back. Every
/// owner is then `from == 0`, which skips the too-old refusal, disables
/// the downgrade check, and re-opens the failed-migration path.
///
/// `Config::save` has `carry_sections_from_disk` for exactly this trap;
/// this is the same idea, done by not accepting the field at all.
pub fn save_persisted_state(state: PersistedState) -> Result<(), String> {
    let mut on_disk = PersistedState::load();
    on_disk.recent_notes = state.recent_notes;
    on_disk.save().to_cmd()
}
