// SPDX-FileCopyrightText: 2026 Dennis Burgermeister
// SPDX-License-Identifier: GPL-3.0-or-later

//! Reading headings: lists for the views, one heading's source, categories,
//! effort presets, the time-tracking summary. Nothing here writes.

use nobo_core::config::settings::Config;
use nobo_core::core::indexer::IndexManager;
use nobo_core::core::models::{Heading, HeadingTarget, TimeTrackingSummary};
use nobo_core::core::{heading_identity, heading_utils, task_utils};
use nobo_core::utils::effort_utils;
use nobo_core::utils::fs_utils;
use nobo_core::utils::ToCmdResult;
use std::path::PathBuf;

pub fn get_all_actions(index_manager: &IndexManager) -> Result<Vec<Heading>, String> {
    Ok(index_manager.all_headings_with_state())
}

pub fn get_headings_from_file(file_path: String, index_manager: &IndexManager) -> Result<Vec<Heading>, String> {
    let target_path = PathBuf::from(&file_path);
    Ok(index_manager.get_headings_or_parse(&target_path))
}

/// Returns a hierarchical project tree for a given `file_path`.
///
/// The structure looks like this:
/// ```text
/// project
///  ├── headingA
///  ├── headingB
///  │    ├── headingB-A
///  │    └── headingB-B
///  └── headingC
/// ```
pub fn get_projects_tree_from_file(file_path: String, index_manager: &IndexManager) -> Result<Vec<Heading>, String> {
    let target_path = PathBuf::from(&file_path);
    let flat = index_manager.get_headings_or_parse(&target_path);
    Ok(heading_utils::projects_tree(flat))
}

/// The heading's line plus its entire subtree, verbatim -- raw org syntax
/// exactly as it sits on disk, not a re-serialized approximation. For
/// sharing a project, where the subtasks are most of what is worth
/// sharing.
pub fn get_heading_source_text(file_path: String, target: HeadingTarget) -> Result<String, String> {
    let target_path = PathBuf::from(&file_path);
    // Parsed from disk, not from the cache, for the same reason the writers
    // do it (see flat_from_disk): the line range resolved here is used to
    // slice the file below, so the two have to be the same snapshot -- and
    // resolved by identity too, or a stale line hands out another task's
    // text.
    let flat = heading_utils::parse_from_disk(&target_path);
    let idx = heading_identity::resolve_target(&flat, &target, &nobo_core::core::errors::subject::heading())?;

    let (start_line, end_line) = heading_utils::subtree_line_range(&flat, idx);
    let lines = fs_utils::read_lines(&target_path).to_cmd()?;
    let selected: Vec<String> = lines
        .into_iter()
        .enumerate()
        .filter(|(i, _)| {
            let n = i + 1;
            n >= start_line && n <= end_line
        })
        .map(|(_, l)| l)
        .collect();

    Ok(selected.join("\n"))
}

/// Thin IO wrapper -- the actual grouping/summation lives in
/// task_utils::time_tracking_summary (pure, unit-tested there) so it's
/// consistent with every other command in this file.
pub fn get_time_tracking_summary(index_manager: &IndexManager) -> Result<TimeTrackingSummary, String> {
    Ok(task_utils::time_tracking_summary(&index_manager.headings_by_file()))
}

/// Vault-wide, state-unfiltered heading list for the Search "all tasks"
/// scope (see IndexManager::all_headings's own doc comment) -- the frontend
/// narrows this down to the configured GTD files itself, the same way
/// Actions.tsx/Projects.tsx already compare against their own configured
/// path rather than the backend hardcoding file scope.
pub fn get_all_headings(index_manager: &IndexManager) -> Result<Vec<Heading>, String> {
    Ok(index_manager.all_headings())
}

/// Resolves effort presets from already-read inputs, in priority order:
/// `file_content`'s own `#+PROPERTY: Effort_ALL`, then
/// `settings_presets_org`, then the hardcoded default.
///
/// Pure, so it is testable without the filesystem or the real global
/// config, which lives at a fixed OS path a test cannot sandbox.
pub(super) fn resolve_effort_presets(file_content: Option<&str>, settings_presets_org: Option<&str>) -> Vec<u32> {
    if let Some(values) = file_content.and_then(effort_utils::scan_effort_all) {
        return values;
    }

    let from_settings = settings_presets_org.map(effort_utils::parse_effort_all).filter(|v| !v.is_empty());

    from_settings.unwrap_or_else(|| effort_utils::parse_effort_all(effort_utils::DEFAULT_EFFORT_PRESETS_ORG))
}

/// Every `:CATEGORY:` the vault already uses, alphabetically.
///
/// The area of a task is a closed list in practice: a vault accumulates a
/// handful of them, and free text is how it ends up with both "Arbeit"
/// and "arbeit". So a picker offers what is there and lets a new one be
/// typed, which is also what nobo's own CategoryPicker does -- this is
/// the list behind it.
///
/// Counted off the index rather than handed out as headings: the
/// alternative is serialising every heading in the vault so that a caller
/// can look at one property of each.
pub fn list_categories(index_state: &IndexManager) -> Vec<String> {
    let Ok(read_lock) = index_state.index.read() else { return Vec::new() };
    let mut found: Vec<String> = read_lock
        .headings
        .values()
        .flatten()
        .filter_map(|heading| nobo_core::core::models::property_value(&heading.properties, "CATEGORY"))
        .map(|value| value.to_string())
        .collect();
    // Case-insensitively, because org's drawer keys are and because
    // "Arbeit" and "arbeit" are one area to everybody but a byte
    // comparison.
    found.sort_by_key(|value| value.to_lowercase());
    found.dedup_by_key(|value| value.to_lowercase());
    found
}

/// Resolves the Effort quick-select chip values (minutes) for a task in
/// FILE_PATH -- see resolve_effort_presets for the priority order. A
/// missing/unreadable file is treated the same as "no directive" rather
/// than an error -- this is a read-only convenience lookup, not a
/// correctness-critical operation.
pub fn get_effort_presets(file_path: String) -> Result<Vec<u32>, String> {
    let file_content = std::fs::read_to_string(&file_path).ok();
    let settings_presets_org = Config::load().map(|c| c.effort_presets_org);
    Ok(resolve_effort_presets(file_content.as_deref(), settings_presets_org.as_deref()))
}
