// SPDX-FileCopyrightText: 2026 Dennis Burgermeister
// SPDX-License-Identifier: GPL-3.0-or-later

//! What a host can ask this feature for.
//!
//! Plain functions: the Tauri build wraps them (see `tauri_commands`),
//! harbour-nobo's dispatcher unpacks JSON into them. Neither shape is in
//! here, which is why both hosts can have the same feature.

use chrono::Utc;

use nobo_core::config::settings::Config;
use nobo_core::core::indexer::IndexManager;
use nobo_core::core::task_utils;
use nobo_core::core::vault::Vault;

use crate::state::{self, ReviewState};
use crate::summary::{self, Counts, Summary};

/// No vault configured yet (fresh install, still on Setup) reads as "never
/// reviewed" rather than an error -- same reasoning as `state::load`'s
/// own missing-file case, just one layer further out.
pub fn review_state() -> ReviewState {
    match Config::require_vault_root() {
        Ok(vault_root) => state::load(&vault_root),
        Err(_) => ReviewState::default(),
    }
}

pub fn review_save_state(state: ReviewState) -> Result<(), String> {
    let vault_root = Config::require_vault_root()?;
    state::save(&vault_root, &state)
}

/// How often the review is due on this device (see `crate::Settings`).
pub fn review_settings() -> crate::Settings {
    crate::SETTINGS.load()
}

pub fn review_save_settings(settings: crate::Settings) -> Result<(), String> {
    crate::SETTINGS.save(&settings)
}

/// Everything the review page and the hub badge need, in one answer.
///
/// One call rather than five: the counts, the progress and "is it
/// overdue" are one question asked three times over, and a host that
/// assembled them itself would be the second place that knows what
/// "stalled" means. Not a Tauri command yet -- the desktop frontend still
/// derives this in TypeScript (see src/features/review); this is what it
/// would call once it stops.
pub fn review_summary(index_manager: &IndexManager) -> Summary {
    let state = review_state();
    let settings = review_settings();
    summary::summarize(count(index_manager), &state, &settings, Utc::now())
}

fn count(index_manager: &IndexManager) -> Counts {
    let Some(config) = Config::load() else { return Counts::default() };
    let root = std::path::PathBuf::from(&config.notes_path);
    // Through the vault, not into the index: the narrow door a feature is
    // supposed to read the vault through (see `tools/check-layers-rust.mjs`
    // and how nobo-cookbook does it). Same answer -- `Vault::headings` is
    // the index's own `get_headings_or_parse` -- but the index keeps its
    // write half, its lock and its dozen entry points to itself.
    let vault = Vault::new(index_manager, &root);
    let open = |path: &str| -> Vec<nobo_core::core::models::Heading> { vault.headings(&root.join(path)) };

    let inbox = open(&config.inbox_file_path);
    let actions = open(&config.actions_file_path);
    let projects = open(&config.projects_file_path);
    let someday = open(&config.someday_file_path);

    let is_open = |heading: &&nobo_core::core::models::Heading| !task_utils::is_finished(heading.state.as_deref());

    Counts {
        inbox: inbox.iter().filter(is_open).count(),
        // What is waiting for somebody else is not what is waiting for
        // me, so the two steps do not count the same task twice.
        actions: actions.iter().filter(is_open).filter(|h| !summary::is_waiting_for(h)).count(),
        waiting: actions.iter().chain(projects.iter()).filter(is_open).filter(|h| summary::is_waiting_for(h)).count(),
        stalled: projects.iter().filter(|heading| heading.level == 1).filter(|heading| summary::is_stalled(heading, &projects)).count(),
        someday: someday.iter().filter(is_open).count(),
    }
}
