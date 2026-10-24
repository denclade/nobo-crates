// SPDX-FileCopyrightText: 2026 Dennis Burgermeister
// SPDX-License-Identifier: GPL-3.0-or-later

//! Archiving what is done.

use nobo_core::core::indexer::IndexManager;
use nobo_core::core::models::HeadingTarget;
use nobo_core::core::{heading_identity, heading_utils, task_utils};
use rusqlite::Connection;
use std::path::PathBuf;
use std::sync::Mutex;

pub fn archive_done_tasks(
    file_path: String,
    archive_after_days: u32,
    index_manager: &IndexManager,
    db: &Mutex<Connection>,
) -> Result<u32, String> {
    let target_path = PathBuf::from(&file_path);
    let (lines, flat) = heading_utils::read_and_parse(&target_path)?;
    let ranges = task_utils::find_archivable_ranges(&flat, archive_after_days);
    task_utils::archive_ranges(&target_path, &lines, ranges, &index_manager, &db).map(|n| n as u32)
}

pub fn archive_done_projects(file_path: String, index_manager: &IndexManager, db: &Mutex<Connection>) -> Result<u32, String> {
    let target_path = PathBuf::from(&file_path);
    let (lines, flat) = heading_utils::read_and_parse(&target_path)?;
    let ranges = task_utils::find_archivable_project_ranges(&flat);
    task_utils::archive_ranges(&target_path, &lines, ranges, &index_manager, &db).map(|n| n as u32)
}

pub fn archive_single_project(
    file_path: String,
    target: HeadingTarget,
    index_manager: &IndexManager,
    db: &Mutex<Connection>,
) -> Result<(), String> {
    let target_path = PathBuf::from(&file_path);
    let (lines, flat) = heading_utils::read_and_parse(&target_path)?;
    let idx = heading_identity::resolve_target(&flat, &target, &nobo_core::core::errors::subject::heading())?;
    let range = heading_utils::subtree_line_range(&flat, idx);
    task_utils::archive_ranges(&target_path, &lines, vec![range], &index_manager, &db)?;
    Ok(())
}
