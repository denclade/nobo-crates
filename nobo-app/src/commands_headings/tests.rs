// SPDX-FileCopyrightText: 2026 Dennis Burgermeister
// SPDX-License-Identifier: GPL-3.0-or-later

use super::maintenance::{is_configured_gtd_file, EXAMPLE_TITLE_LIMIT};
use super::read::resolve_effort_presets;
use super::*;
use nobo_core::config::settings::Config;
use nobo_core::core::history;
use nobo_core::core::history::record_best_effort;
use nobo_core::core::indexer::IndexManager;
use nobo_core::core::models::{
    BulkHeadingUpdate, BulkRefileSource, CustomIdBackfillPreview, FileSnapshot, Heading, HeadingCreatePayload, HeadingLocation,
    HeadingTarget, HeadingUpdatePayload, HistoryEntryKind, Note, RefilePosition, UnpromotedOrgFile,
};
use nobo_core::core::{heading_identity, heading_utils, refile, task_utils};
use nobo_core::utils::effort_utils;
use nobo_core::utils::fs_utils;
use nobo_core::utils::fs_utils::filename_of;
use nobo_core::utils::note_utils;
use nobo_core::utils::repeat_utils;
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tempfile::TempDir;

/// A bare line-number target, i.e. no identity at all -- what a test
/// that only cares about the surrounding logic passes. Real callers
/// come from the frontend's `headingTarget()` and always carry the
/// heading's CUSTOM_ID; see heading_identity::resolve_target for what
/// that buys them.
fn at_line(line_no: usize) -> HeadingTarget {
    HeadingTarget { line_no, custom_id: None, title: None }
}

/// The guarded target a real caller sends: line number plus the
/// identity it captured with it.
fn target_for(heading: &Heading) -> HeadingTarget {
    HeadingTarget {
        line_no: heading.line_no,
        custom_id: heading_identity::custom_id_of(heading).map(str::to_string),
        title: Some(heading.title.clone()),
    }
}

#[test]
fn resolve_effort_presets_prefers_the_files_own_effort_all_directive() {
    let content = "#+PROPERTY: Effort_ALL 0:10 0:20\n* TODO Task\n";
    assert_eq!(resolve_effort_presets(Some(content), Some("1:00 2:00")), vec![10, 20]);
}

#[test]
fn resolve_effort_presets_falls_back_to_settings_when_the_file_has_no_directive() {
    let content = "* TODO Task\n";
    assert_eq!(resolve_effort_presets(Some(content), Some("1:00 2:00")), vec![60, 120]);
}

#[test]
fn resolve_effort_presets_falls_back_to_the_hardcoded_default_when_settings_is_empty_too() {
    let content = "* TODO Task\n";
    assert_eq!(resolve_effort_presets(Some(content), Some("")), effort_utils::parse_effort_all(effort_utils::DEFAULT_EFFORT_PRESETS_ORG),);
}

#[test]
fn resolve_effort_presets_treats_an_unreadable_file_the_same_as_no_directive() {
    assert_eq!(resolve_effort_presets(None, Some("1:00 2:00")), vec![60, 120]);
}

#[test]
fn resolve_effort_presets_falls_back_to_the_hardcoded_default_when_nothing_at_all_is_available() {
    assert_eq!(resolve_effort_presets(None, None), effort_utils::parse_effort_all(effort_utils::DEFAULT_EFFORT_PRESETS_ORG),);
}

/// `backfill_custom_ids` without the `tauri::State` wrapper, which
/// cannot be constructed outside a running app. A default config is
/// fine here: no fixture matches a GTD filename, so every one counts
/// as a plain note for the level-1 restriction.
fn backfill_custom_ids_body(index: &IndexManager, db: &Mutex<Connection>) -> u32 {
    let config = Config::default();
    let mut total = 0u32;
    for (path_str, cached) in index.headings_by_file() {
        let path = PathBuf::from(&path_str);
        let is_gtd = is_configured_gtd_file(&path, &config);
        let wants_stamp = |h: &Heading| !heading_identity::has_custom_id(h) && (is_gtd || h.level == 1);
        if !cached.iter().any(|h| wants_stamp(h)) {
            continue;
        }

        let flat = heading_utils::parse_from_disk(&path);
        let mut missing: Vec<&Heading> = flat.iter().filter(|h| wants_stamp(h)).collect();
        if missing.is_empty() {
            continue;
        }
        missing.sort_unstable_by(|a, b| b.line_no.cmp(&a.line_no));

        let before = std::fs::read_to_string(&path).ok();
        let mut lines = fs_utils::read_lines(&path).unwrap();
        for heading in &missing {
            let block = heading_utils::stamp_custom_id_only(heading);
            let end = heading.last_line.min(lines.len());
            let start = (heading.line_no - 1).min(end);
            lines.splice(start..end, block);
        }
        fs_utils::write_lines(&path, &lines).unwrap();
        index.update_file(&path);
        let after = std::fs::read_to_string(&path).ok();
        record_best_effort(
            db,
            HistoryEntryKind::CustomIdsBackfilled { filename: filename_of(&path), count: missing.len() as u32 },
            false,
            vec![FileSnapshot { path: path_str, before, after }],
        );
        total += missing.len() as u32;
    }
    total
}

fn missing_custom_id_count(index: &IndexManager) -> usize {
    index.headings_by_file().values().flatten().filter(|h| !heading_identity::has_custom_id(h)).count()
}

#[test]
fn check_missing_custom_ids_counts_across_files_and_ignores_ones_already_set() {
    // Every "missing" heading here needs a real state -- realistic
    // production data for this scenario (a file predating CUSTOM_ID
    // support) always has one; a bare state-less sibling isn't real
    // structure per org_parser's own rule and would be absorbed as
    // its predecessor's own content instead of remaining its own,
    // separately-addressable heading.
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("a.org"), "* Has one\n:PROPERTIES:\n:CUSTOM_ID: existing\n:END:\n* TODO Missing one\n").unwrap();
    std::fs::write(dir.path().join("b.org"), "* TODO Also missing\n** NEXT Nested missing\n").unwrap();
    let index = IndexManager::new();
    index.scan_directory(dir.path());

    assert_eq!(missing_custom_id_count(&index), 3);
}

/// Same body as commands_headings::check_missing_custom_ids, minus the
/// tauri::State wrapper.
// Same body as commands_headings::check_missing_custom_ids -- see
// backfill_custom_ids_body's identical comment on Config::default().
fn check_missing_custom_ids_body(index: &IndexManager) -> CustomIdBackfillPreview {
    let config = Config::default();
    let missing: Vec<Heading> = index
        .headings_by_file()
        .iter()
        .flat_map(|(path_str, headings)| {
            let is_gtd = is_configured_gtd_file(Path::new(path_str), &config);
            headings.iter().filter(move |h| !heading_identity::has_custom_id(h) && (is_gtd || h.level == 1))
        })
        .cloned()
        .collect();
    let example_titles = missing.iter().take(EXAMPLE_TITLE_LIMIT).map(|h| h.title.clone()).collect();
    let example_block = missing.first().map(|h| heading_utils::stamp_custom_id_only(h).join("\n"));
    CustomIdBackfillPreview { missing_count: missing.len() as u32, example_titles, example_block }
}

#[test]
fn check_missing_custom_ids_reports_example_titles_and_a_rendered_preview_block() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("a.org"), "* Has one\n:PROPERTIES:\n:CUSTOM_ID: existing\n:END:\n* TODO Missing Heading\n").unwrap();
    let index = IndexManager::new();
    index.scan_directory(dir.path());

    let preview = check_missing_custom_ids_body(&index);
    assert_eq!(preview.missing_count, 1);
    assert_eq!(preview.example_titles, vec!["Missing Heading".to_string()]);
    let block = preview.example_block.unwrap();
    assert!(block.contains("Missing Heading") && block.contains("CUSTOM_ID"), "{block}");
    // Read-only -- no file was actually touched.
    assert!(std::fs::read_to_string(dir.path().join("a.org")).unwrap().contains("Missing Heading\n"));
}

#[test]
fn check_missing_custom_ids_caps_example_titles_but_not_the_count() {
    let dir = TempDir::new().unwrap();
    let content: String = (0..8).map(|i| format!("* TODO Missing {i}\n")).collect();
    std::fs::write(dir.path().join("a.org"), content).unwrap();
    let index = IndexManager::new();
    index.scan_directory(dir.path());

    let preview = check_missing_custom_ids_body(&index);
    assert_eq!(preview.missing_count, 8, "the real count is never truncated");
    assert_eq!(preview.example_titles.len(), EXAMPLE_TITLE_LIMIT, "only the examples list is capped");
}

/// A note file only ever gets a CUSTOM_ID on its top-level headings --
/// stamping a drawer onto every sub-heading clutters outline text
/// nobody meant to track individually.
#[test]
fn check_missing_custom_ids_ignores_sub_headings_in_a_plain_note() {
    let dir = TempDir::new().unwrap();
    // "notes.org" doesn't match any of Config::default()'s GTD
    // filenames (actions.org/projects.org/inbox.org/someday.org).
    std::fs::write(dir.path().join("notes.org"), "* Top heading\n** Nested sub-heading\n*** Deeper still\n").unwrap();
    let index = IndexManager::new();
    index.scan_directory(dir.path());

    let preview = check_missing_custom_ids_body(&index);
    assert_eq!(preview.missing_count, 1, "only the level-1 heading counts, both sub-headings are skipped");
    assert_eq!(preview.example_titles, vec!["Top heading".to_string()]);
}

/// The counterpart to the test above: a GTD file's sub-headings are
/// real subtasks the app's own identity tracking needs a stable
/// CUSTOM_ID for at every level, so they're exempt from the level-1
/// restriction -- unchanged from the original, pre-fix behavior.
#[test]
fn check_missing_custom_ids_still_covers_every_level_in_a_gtd_file() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("actions.org"), "* TODO Top action\n** NEXT Nested subtask\n").unwrap();
    let index = IndexManager::new();
    index.scan_directory(dir.path());

    let preview = check_missing_custom_ids_body(&index);
    assert_eq!(preview.missing_count, 2, "both levels count in a GTD file");
}

#[test]
fn backfill_custom_ids_only_stamps_the_level_1_heading_in_a_plain_note() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("notes.org");
    std::fs::write(&path, "* Top heading\n** Nested sub-heading\n").unwrap();
    let index = IndexManager::new();
    index.scan_directory(dir.path());
    let conn = Connection::open_in_memory().unwrap();
    history::store::init_schema(&conn).unwrap();
    let db = Mutex::new(conn);

    let count = backfill_custom_ids_body(&index, &db);
    assert_eq!(count, 1, "only the top heading was stamped");

    let result = std::fs::read_to_string(&path).unwrap();
    let top_block = result.split("** Nested sub-heading").next().unwrap();
    assert!(top_block.contains("CUSTOM_ID"), "top heading got its CUSTOM_ID: {result}");
    assert!(!result.contains("** Nested sub-heading\n:PROPERTIES:"), "sub-heading left untouched: {result}");
}

/// The backfill writes across the whole vault, and it does so after a
/// preview the user had to confirm -- a sync pull, a CalDAV pull or
/// another editor has a whole dialog's worth of time to rewrite a file
/// in between. Splicing the stamped block against the cached line
/// numbers then lands it on whatever occupies those lines now: what
/// arrived meanwhile is overwritten, and nothing says so.
///
/// Same rule as every other writer (heading_utils::parse_from_disk):
/// the cache may pick the files, never the line numbers.
#[test]
fn backfill_custom_ids_writes_against_the_file_on_disk_not_a_stale_index() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("a.org");
    std::fs::write(&path, "* TODO Erste\n* TODO Zweite\n").unwrap();

    let index = IndexManager::new();
    index.scan_directory(dir.path());

    // Something else rewrote the file and the index has not caught up:
    // both known headings sit two lines lower than it thinks.
    std::fs::write(&path, "* TODO Von auswärts\nMit einer Zeile Text.\n* TODO Erste\n* TODO Zweite\n").unwrap();

    let conn = Connection::open_in_memory().unwrap();
    history::store::init_schema(&conn).unwrap();
    let db = Mutex::new(conn);
    backfill_custom_ids_body(&index, &db);

    let result = std::fs::read_to_string(&path).unwrap();
    assert!(result.contains("* TODO Von auswärts"), "the heading that arrived meanwhile must survive:\n{result}");
    assert!(result.contains("Mit einer Zeile Text."), "and its body with it:\n{result}");
    assert!(result.contains("* TODO Erste"), "{result}");
    assert!(result.contains("* TODO Zweite"), "{result}");
    assert_eq!(result.matches("CUSTOM_ID").count(), 3, "every level-1 heading actually on disk got stamped:\n{result}");
}

#[test]
fn backfill_custom_ids_stamps_missing_ones_preserves_existing_and_skips_timestamps() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("a.org");
    std::fs::write(
        &path,
        concat!("* Has one\n:PROPERTIES:\n:CUSTOM_ID: existing\n:LAST_ACTIVITY: 2020-01-01 Wed 00:00\n:END:\n", "* TODO Missing one\n",),
    )
    .unwrap();
    let index = IndexManager::new();
    index.scan_directory(dir.path());
    let conn = Connection::open_in_memory().unwrap();
    history::store::init_schema(&conn).unwrap();
    let db = Mutex::new(conn);

    let count = backfill_custom_ids_body(&index, &db);
    assert_eq!(count, 1, "only the one missing heading was touched");

    let result = std::fs::read_to_string(&path).unwrap();
    assert!(result.contains(":CUSTOM_ID: existing"), "existing id preserved verbatim: {result}");
    assert!(result.contains(":LAST_ACTIVITY: 2020-01-01 Wed 00:00"), "untouched heading's own timestamp not bumped: {result}");
    assert_eq!(missing_custom_id_count(&index), 0);

    {
        let conn = db.lock().unwrap();
        assert_eq!(history::list(&conn, 10).unwrap().len(), 1, "recorded as one undo-able entry");
    }
}

#[test]
fn a_second_backfill_run_is_a_no_op() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("a.org"), "* Missing one\n").unwrap();
    let index = IndexManager::new();
    index.scan_directory(dir.path());
    let conn = Connection::open_in_memory().unwrap();
    history::store::init_schema(&conn).unwrap();
    let db = Mutex::new(conn);

    assert_eq!(backfill_custom_ids_body(&index, &db), 1);
    assert_eq!(backfill_custom_ids_body(&index, &db), 0, "nothing left to backfill, no duplicate ids added");
}

/* --- ensure_custom_id_for_heading / find_orphaned_custom_ids / remove_custom_ids --- */

/// Same body as commands_headings::ensure_custom_id_for_heading, minus
/// the tauri::State wrapper -- see backfill_custom_ids_body's own
/// comment for why a literal duplicate rather than a shared sync fn.
fn ensure_custom_id_for_heading_body(
    index: &IndexManager,
    db: &Mutex<Connection>,
    file_path: &Path,
    line_no: usize,
) -> Result<String, String> {
    let flat = index.get_headings_or_parse(file_path);
    let idx = heading_identity::resolve_target(&flat, &at_line(line_no), &nobo_core::core::errors::subject::heading())?;
    let mut heading = flat[idx].clone();

    if let Some(id) = heading_identity::custom_id_of(&heading) {
        return Ok(id.to_string());
    }

    heading_identity::ensure_custom_id(&mut heading.properties);
    let id = heading_identity::custom_id_of(&heading).unwrap().to_string();

    let block = heading_utils::stamp_custom_id_only(&heading);
    let before = std::fs::read_to_string(file_path).ok();
    fs_utils::splice_lines(file_path, heading.line_no - 1, heading.last_line, block).unwrap();
    index.update_file(file_path);
    let after = std::fs::read_to_string(file_path).ok();

    record_best_effort(
        db,
        HistoryEntryKind::CustomIdsBackfilled { filename: filename_of(file_path), count: 1 },
        false,
        vec![FileSnapshot { path: file_path.to_string_lossy().into_owned(), before, after }],
    );

    Ok(id)
}

#[test]
fn ensure_custom_id_for_heading_stamps_one_when_missing_and_returns_it() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("a.org");
    std::fs::write(&path, "* Some heading\nSome body text.\n").unwrap();
    let index = IndexManager::new();
    index.scan_directory(dir.path());
    let conn = Connection::open_in_memory().unwrap();
    history::store::init_schema(&conn).unwrap();
    let db = Mutex::new(conn);

    let id = ensure_custom_id_for_heading_body(&index, &db, &path, 1).unwrap();
    assert!(id.starts_with("h:"), "{id}");
    assert!(uuid::Uuid::parse_str(id.strip_prefix("h:").unwrap()).is_ok(), "{id}");

    let result = std::fs::read_to_string(&path).unwrap();
    assert!(result.contains(&format!(":CUSTOM_ID: {id}")), "{result}");
    assert!(result.contains("Some body text."), "body left untouched: {result}");
}

#[test]
fn ensure_custom_id_for_heading_returns_the_existing_id_unchanged() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("a.org");
    std::fs::write(&path, "* Some heading\n:PROPERTIES:\n:CUSTOM_ID: keep-me\n:END:\n").unwrap();
    let index = IndexManager::new();
    index.scan_directory(dir.path());
    let conn = Connection::open_in_memory().unwrap();
    history::store::init_schema(&conn).unwrap();
    let db = Mutex::new(conn);

    let id = ensure_custom_id_for_heading_body(&index, &db, &path, 1).unwrap();
    assert_eq!(id, "keep-me");
    assert_eq!(std::fs::read_to_string(&path).unwrap().matches("CUSTOM_ID").count(), 1);
}

#[test]
fn find_orphaned_custom_ids_command_delegates_to_note_utils() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("a.org"), "* Orphan\n:PROPERTIES:\n:CUSTOM_ID: abc-123\n:END:\n").unwrap();
    let index = IndexManager::new();
    index.scan_directory(dir.path());

    let orphaned = note_utils::find_orphaned_custom_ids(dir.path(), &index.headings_by_file()).unwrap();
    assert_eq!(orphaned.len(), 1);
    assert_eq!(orphaned[0].custom_id, "abc-123");
    assert_eq!(orphaned[0].location.file_path, dir.path().join("a.org").to_string_lossy());
}

/// Same body as commands_headings::remove_custom_ids, minus the
/// tauri::State wrapper.
fn remove_custom_ids_body(index: &IndexManager, db: &Mutex<Connection>, targets: Vec<HeadingLocation>) -> u32 {
    let mut by_file: std::collections::HashMap<String, Vec<HeadingTarget>> = std::collections::HashMap::new();
    for target in targets {
        by_file.entry(target.file_path).or_default().push(target.target);
    }

    let mut total = 0u32;
    for (path_str, targets) in by_file {
        let path = PathBuf::from(&path_str);
        let flat = index.get_headings_or_parse(&path);
        let before = std::fs::read_to_string(&path).ok();
        let mut lines = fs_utils::read_lines(&path).unwrap();
        let mut idxs: Vec<usize> = targets
            .iter()
            .filter_map(|target| heading_identity::resolve_target(&flat, target, &nobo_core::core::errors::subject::heading()).ok())
            .collect();
        idxs.sort_unstable_by(|a, b| flat[*b].line_no.cmp(&flat[*a].line_no));

        let mut touched = 0u32;
        for idx in idxs {
            if heading_utils::remove_custom_id_and_empty_drawer(&mut lines, &flat[idx]) {
                touched += 1;
            }
        }
        if touched == 0 {
            continue;
        }

        fs_utils::write_lines(&path, &lines).unwrap();
        index.update_file(&path);
        let after = std::fs::read_to_string(&path).ok();
        record_best_effort(
            db,
            HistoryEntryKind::CustomIdsRemoved { filename: filename_of(&path), count: touched },
            false,
            vec![FileSnapshot { path: path_str, before, after }],
        );
        total += touched;
    }
    total
}

#[test]
fn remove_custom_ids_deletes_only_the_custom_id_line_preserving_other_properties() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("a.org");
    std::fs::write(&path, "* Heading\n:PROPERTIES:\n:EFFORT: 0:30\n:CUSTOM_ID: abc-123\n:END:\n").unwrap();
    let index = IndexManager::new();
    index.scan_directory(dir.path());
    let conn = Connection::open_in_memory().unwrap();
    history::store::init_schema(&conn).unwrap();
    let db = Mutex::new(conn);

    let count =
        remove_custom_ids_body(&index, &db, vec![HeadingLocation { file_path: path.to_string_lossy().into_owned(), target: at_line(1) }]);
    assert_eq!(count, 1);

    let result = std::fs::read_to_string(&path).unwrap();
    assert!(!result.contains("CUSTOM_ID"), "{result}");
    assert!(result.contains(":EFFORT: 0:30"), "other property preserved: {result}");
}

#[test]
fn remove_custom_ids_handles_two_targets_in_the_same_file_without_line_shift_corruption() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("a.org");
    std::fs::write(
        &path,
        concat!("* First\n:PROPERTIES:\n:CUSTOM_ID: id-1\n:END:\n", "* Second\n:PROPERTIES:\n:CUSTOM_ID: id-2\n:END:\n",),
    )
    .unwrap();
    let index = IndexManager::new();
    index.scan_directory(dir.path());
    let conn = Connection::open_in_memory().unwrap();
    history::store::init_schema(&conn).unwrap();
    let db = Mutex::new(conn);

    let targets = vec![
        HeadingLocation { file_path: path.to_string_lossy().into_owned(), target: at_line(1) },
        HeadingLocation { file_path: path.to_string_lossy().into_owned(), target: at_line(5) },
    ];
    let count = remove_custom_ids_body(&index, &db, targets);
    assert_eq!(count, 2);

    let result = std::fs::read_to_string(&path).unwrap();
    assert!(!result.contains("CUSTOM_ID"), "{result}");
    assert!(result.contains("* First"), "{result}");
    assert!(result.contains("* Second"), "{result}");
}

#[test]
fn remove_custom_ids_also_removes_the_drawer_boundary_lines_once_it_is_left_empty() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("a.org");
    std::fs::write(&path, "* Heading\n:PROPERTIES:\n:CUSTOM_ID: abc-123\n:END:\nBody text.\n").unwrap();
    let index = IndexManager::new();
    index.scan_directory(dir.path());
    let conn = Connection::open_in_memory().unwrap();
    history::store::init_schema(&conn).unwrap();
    let db = Mutex::new(conn);

    let count =
        remove_custom_ids_body(&index, &db, vec![HeadingLocation { file_path: path.to_string_lossy().into_owned(), target: at_line(1) }]);
    assert_eq!(count, 1);

    let result = std::fs::read_to_string(&path).unwrap();
    assert_eq!(result, "* Heading\nBody text.\n", "the whole now-empty drawer is gone, not just its CUSTOM_ID line: {result}");
}

#[test]
fn remove_custom_ids_keeps_the_drawer_when_another_property_still_needs_it() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("a.org");
    std::fs::write(&path, "* Heading\n:PROPERTIES:\n:EFFORT: 0:30\n:CUSTOM_ID: abc-123\n:END:\n").unwrap();
    let index = IndexManager::new();
    index.scan_directory(dir.path());
    let conn = Connection::open_in_memory().unwrap();
    history::store::init_schema(&conn).unwrap();
    let db = Mutex::new(conn);

    remove_custom_ids_body(&index, &db, vec![HeadingLocation { file_path: path.to_string_lossy().into_owned(), target: at_line(1) }]);

    let result = std::fs::read_to_string(&path).unwrap();
    assert!(result.contains(":PROPERTIES:") && result.contains(":END:"), "drawer stays, EFFORT still needs it: {result}");
}

#[test]
fn remove_custom_ids_skips_a_stale_target_that_no_longer_matches_a_heading() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("a.org");
    std::fs::write(&path, "* Heading\n:PROPERTIES:\n:CUSTOM_ID: abc-123\n:END:\n").unwrap();
    let index = IndexManager::new();
    index.scan_directory(dir.path());
    let conn = Connection::open_in_memory().unwrap();
    history::store::init_schema(&conn).unwrap();
    let db = Mutex::new(conn);

    let count =
        remove_custom_ids_body(&index, &db, vec![HeadingLocation { file_path: path.to_string_lossy().into_owned(), target: at_line(99) }]);
    assert_eq!(count, 0);
    assert!(std::fs::read_to_string(&path).unwrap().contains("CUSTOM_ID"));
}

/* --- promote_org_file_to_note / find_unpromoted_org_files / remove_empty_properties_drawers --- */

/// Same body as commands_headings::promote_org_file_to_note, minus the
/// tauri::State wrapper.
fn promote_org_file_to_note_body(index: &IndexManager, db: &Mutex<Connection>, path: &Path) -> Note {
    let old_filename = filename_of(path);
    let note = note_utils::promote_org_file_to_note(path).unwrap();

    index.remove_file(path);
    let new_path = PathBuf::from(&note.path);
    index.update_file(&new_path);

    record_best_effort(
        db,
        HistoryEntryKind::FileRenamed { old_filename, new_filename: filename_of(&new_path) },
        false,
        vec![FileSnapshot { path: note.path.clone(), before: None, after: std::fs::read_to_string(&new_path).ok() }],
    );

    note
}

#[test]
fn promote_org_file_to_note_updates_the_index_removing_the_stale_old_path() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("install.org");
    std::fs::write(&path, "#+TITLE: Moodle\n\n* Generell\n").unwrap();
    let index = IndexManager::new();
    index.scan_directory(dir.path());
    assert!(index.headings_by_file().contains_key(&path.to_string_lossy().into_owned()));

    let conn = Connection::open_in_memory().unwrap();
    history::store::init_schema(&conn).unwrap();
    let db = Mutex::new(conn);

    let note = promote_org_file_to_note_body(&index, &db, &path);

    let by_file = index.headings_by_file();
    assert!(
        !by_file.contains_key(&path.to_string_lossy().into_owned()),
        "stale old path is gone from the index: {:?}",
        by_file.keys().collect::<Vec<_>>()
    );
    assert!(by_file.contains_key(&note.path), "new (renamed) path is indexed: {:?}", by_file.keys().collect::<Vec<_>>());
}

/// Same body as commands_headings::find_unpromoted_org_files, minus the
/// tauri::State wrapper.
fn find_unpromoted_org_files_body(base_dir: &Path, config: &Config) -> Vec<UnpromotedOrgFile> {
    note_utils::find_unpromoted_org_files(base_dir).into_iter().filter(|f| !is_configured_gtd_file(Path::new(&f.path), config)).collect()
}

#[test]
fn find_unpromoted_org_files_excludes_configured_gtd_files() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("install.org"), "#+TITLE: Moodle\n\n* Generell\n").unwrap();
    std::fs::write(dir.path().join("inbox.org"), "* Some inbox item\n").unwrap();

    let mut config = Config::default();
    config.inbox_file_path = dir.path().join("inbox.org").to_string_lossy().into_owned();

    let found = find_unpromoted_org_files_body(dir.path(), &config);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].path.ends_with("install.org"));
}

/// Same body as commands_headings::remove_empty_properties_drawers,
/// minus the tauri::State wrapper.
fn remove_empty_properties_drawers_body(index: &IndexManager, db: &Mutex<Connection>, targets: Vec<HeadingLocation>) -> u32 {
    let mut by_file: std::collections::HashMap<String, Vec<HeadingTarget>> = std::collections::HashMap::new();
    for target in targets {
        by_file.entry(target.file_path).or_default().push(target.target);
    }

    let mut total = 0u32;
    for (path_str, targets) in by_file {
        let path = PathBuf::from(&path_str);
        let flat = index.get_headings_or_parse(&path);
        let before = std::fs::read_to_string(&path).ok();
        let mut lines = fs_utils::read_lines(&path).unwrap();
        let mut idxs: Vec<usize> = targets
            .iter()
            .filter_map(|target| heading_identity::resolve_target(&flat, target, &nobo_core::core::errors::subject::heading()).ok())
            .collect();
        idxs.sort_unstable_by(|a, b| flat[*b].line_no.cmp(&flat[*a].line_no));

        let mut touched = 0u32;
        for idx in idxs {
            if heading_utils::remove_drawer_if_empty(&mut lines, &flat[idx]) {
                touched += 1;
            }
        }
        if touched == 0 {
            continue;
        }

        fs_utils::write_lines(&path, &lines).unwrap();
        index.update_file(&path);
        let after = std::fs::read_to_string(&path).ok();
        record_best_effort(
            db,
            HistoryEntryKind::EmptyPropertiesDrawersRemoved { filename: filename_of(&path), count: touched },
            false,
            vec![FileSnapshot { path: path_str, before, after }],
        );
        total += touched;
    }
    total
}

#[test]
fn remove_empty_properties_drawers_removes_only_the_empty_ones() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("a.org");
    std::fs::write(&path, "* Empty\n:PROPERTIES:\n:END:\n* Populated\n:PROPERTIES:\n:EFFORT: 0:30\n:END:\n").unwrap();
    let index = IndexManager::new();
    index.scan_directory(dir.path());
    let conn = Connection::open_in_memory().unwrap();
    history::store::init_schema(&conn).unwrap();
    let db = Mutex::new(conn);

    let targets = vec![
        HeadingLocation { file_path: path.to_string_lossy().into_owned(), target: at_line(1) },
        HeadingLocation { file_path: path.to_string_lossy().into_owned(), target: at_line(4) },
    ];
    let count = remove_empty_properties_drawers_body(&index, &db, targets);
    assert_eq!(count, 1);

    let result = std::fs::read_to_string(&path).unwrap();
    assert_eq!(result, "* Empty\n* Populated\n:PROPERTIES:\n:EFFORT: 0:30\n:END:\n", "{result}");
}

/// The body of update_heading_in_file, minus the tauri::State wrapper
/// (which cannot be constructed outside a running app) -- same literal
/// duplicate approach as backfill_custom_ids_body above.
fn update_heading_body(index: &IndexManager, path: &Path, target: &HeadingTarget, payload: HeadingUpdatePayload) -> Result<(), String> {
    let mut edit = heading_utils::open_edit(path, target, &nobo_core::core::errors::subject::heading())?;
    edit.write(&payload)?;
    edit.touch_ancestors()?;
    edit.commit()?;
    index.update_file(path);
    Ok(())
}

/// The reported bug, end to end: writing a description grows the file,
/// so every task below it moves down. A second task whose line number
/// was captured BEFORE that write then addressed the wrong heading --
/// and update_heading_in_file happily overwrote it, losing that
/// heading's own description.
#[test]
fn an_edit_with_a_line_number_stale_by_an_earlier_write_still_lands_on_its_own_heading() {
    let (dir, index, _db) = setup();
    let path = dir.path().join("actions.org");
    std::fs::write(
        &path,
        "* TODO Erste Aufgabe\n:PROPERTIES:\n:CUSTOM_ID: h:aaa\n:END:\n\
         * TODO Zweite Aufgabe\n:PROPERTIES:\n:CUSTOM_ID: h:bbb\n:END:\n\
         Wichtige Notizen der zweiten Aufgabe.\n",
    )
    .unwrap();

    // Both captured from the same load, exactly as the UI holds them.
    let flat = index.get_headings_or_parse(&path);
    let first = target_for(flat.iter().find(|h| h.title == "Erste Aufgabe").unwrap());
    let second = target_for(flat.iter().find(|h| h.title == "Zweite Aufgabe").unwrap());

    // A description written on the FIRST task pushes the second one
    // down by three lines -- `second` is now stale.
    update_heading_body(
        &index,
        &path,
        &first,
        HeadingUpdatePayload { content: Some("Zeile\nZeile\nZeile".to_string()), ..Default::default() },
    )
    .unwrap();

    // ...and the second task is edited with that stale reference.
    update_heading_body(&index, &path, &second, HeadingUpdatePayload { state: Some("DONE".to_string()), ..Default::default() }).unwrap();

    let after = index.get_headings_or_parse(&path);
    let first_after = after.iter().find(|h| h.title == "Erste Aufgabe").unwrap();
    let second_after = after.iter().find(|h| h.title == "Zweite Aufgabe").unwrap();
    assert_eq!(second_after.state.as_deref(), Some("DONE"), "the edit landed on its own heading");
    assert_eq!(second_after.content, "Wichtige Notizen der zweiten Aufgabe.", "and left that heading's description intact");
    assert_eq!(first_after.state.as_deref(), Some("TODO"), "the heading now sitting at the stale line was not touched");
    assert_eq!(first_after.content, "Zeile\nZeile\nZeile", "nor was its own freshly written description overwritten");
}

/// The other half of the same guarantee: even a caller whose line
/// number IS right for the index can be wrong about the file, because
/// the index is only as current as the last watcher event -- a sync
/// pull that inserted a heading above is enough. Writers re-read the
/// file first (heading_utils::parse_from_disk), so the block that gets
/// spliced is the one actually on disk.
#[test]
fn an_edit_after_the_file_changed_behind_the_index_still_lands_on_its_own_heading() {
    let (dir, index, _db) = setup();
    let path = dir.path().join("actions.org");
    std::fs::write(&path, "* TODO Meine Aufgabe\n:PROPERTIES:\n:CUSTOM_ID: h:aaa\n:END:\n").unwrap();
    index.scan_directory(dir.path());

    let flat = index.get_headings_or_parse(&path);
    let mine = target_for(&flat[0]);

    // A sync pull rewrites the file, pushing the heading down. The
    // index still holds the pre-pull parse (no watcher event yet).
    std::fs::write(
        &path,
        "* TODO Von woanders\n:PROPERTIES:\n:CUSTOM_ID: h:zzz\n:END:\nFremder Text.\n\
         * TODO Meine Aufgabe\n:PROPERTIES:\n:CUSTOM_ID: h:aaa\n:END:\n",
    )
    .unwrap();

    update_heading_body(&index, &path, &mine, HeadingUpdatePayload { content: Some("Meine Notiz".to_string()), ..Default::default() })
        .unwrap();

    let after = heading_utils::parse_from_disk(&path);
    let foreign = after.iter().find(|h| h.title == "Von woanders").unwrap();
    let mine_after = after.iter().find(|h| h.title == "Meine Aufgabe").unwrap();
    assert_eq!(mine_after.content, "Meine Notiz");
    assert_eq!(foreign.content, "Fremder Text.", "the heading pulled in from elsewhere was not overwritten");
}

/// The new heading's depth comes from the parent as it exists NOW, not
/// from the caller's arithmetic on a stale copy: a project that was
/// meanwhile refiled one level deeper would otherwise get its new
/// subtask as a SIBLING, with the description demoted by the wrong
/// offset on top.
#[test]
fn a_new_subtask_gets_its_level_from_the_parent_the_write_actually_resolved_to() {
    let (dir, index, _db) = setup();
    let path = dir.path().join("projects.org");
    std::fs::write(&path, "* TODO Projekt :project:\n:PROPERTIES:\n:CUSTOM_ID: h:proj\n:END:\n").unwrap();
    index.scan_directory(dir.path());
    let parent = target_for(&index.get_headings_or_parse(&path)[0]);

    // The parent moves a level deeper behind the caller's back.
    std::fs::write(
        &path,
        "* TODO Oberprojekt :project:\n\
         ** TODO Projekt :project:\n:PROPERTIES:\n:CUSTOM_ID: h:proj\n:END:\n",
    )
    .unwrap();

    // create_heading_in_file's own logic, with the caller still
    // believing the parent sits at level 1.
    let payload = HeadingCreatePayload {
        file_path: path.to_string_lossy().into_owned(),
        title: "Neue Unteraufgabe".into(),
        state: Some("TODO".into()),
        tags: vec![],
        parent: Some(parent),
        scheduled: None,
        effort_minutes: None,
        content: Some("* Notiz".into()),
        category: None,
    };
    let flat = heading_utils::parse_from_disk(&path);
    let idx =
        heading_identity::resolve_target(&flat, payload.parent.as_ref().unwrap(), &nobo_core::core::errors::subject::parent_heading())
            .unwrap();
    let level = flat[idx].level + 1;
    let block = heading_utils::build_new_heading_block(&payload, level);
    fs_utils::insert_lines_after(&path, heading_utils::find_subtree_end_line(&flat, idx), block).unwrap();

    let after = heading_utils::parse_from_disk(&path);
    let created = after.iter().find(|h| h.title == "Neue Unteraufgabe").unwrap();
    assert_eq!(created.level, 3, "one deeper than the parent's CURRENT level, not the caller's stale 2");
    assert_eq!(created.content, "* Notiz", "and the description is demoted against that same level");
}

/// A heading that is genuinely gone must abort the write, not fall
/// back to the line number and overwrite whatever took its place.
#[test]
fn an_edit_of_a_heading_that_no_longer_exists_is_refused_instead_of_misapplied() {
    let (dir, index, _db) = setup();
    let path = dir.path().join("actions.org");
    std::fs::write(&path, "* TODO Bleibt\n:PROPERTIES:\n:CUSTOM_ID: h:aaa\n:END:\nEigener Text.\n").unwrap();
    let gone = HeadingTarget { line_no: 1, custom_id: Some("h:weg".into()), title: Some("Gelöscht".into()) };

    let err = update_heading_body(
        &index,
        &path,
        &gone,
        HeadingUpdatePayload { content: Some("Text der gelöschten Aufgabe".to_string()), ..Default::default() },
    )
    .unwrap_err();

    assert!(err.contains("h:weg"), "{err}");
    let after = index.get_headings_or_parse(&path);
    assert_eq!(after[0].content, "Eigener Text.", "the heading at that line kept its own content");
}

/* --- a drop beside a sibling goes through the one reorder --- */

const LIST: &str = "* TODO Brot\n* TODO Milch\n* TODO Käse\n";

fn titles(path: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string(path).unwrap().lines().filter(|l| l.starts_with('*')).map(String::from).collect()
}

fn by_title(title: &str) -> HeadingTarget {
    HeadingTarget { line_no: 0, custom_id: None, title: Some(title.to_string()) }
}

fn refile_in(
    path: &std::path::Path,
    what: &str,
    beside: &str,
    position: RefilePosition,
    state: Option<&str>,
    index: &IndexManager,
    db: &Mutex<Connection>,
) -> Result<(), String> {
    let file = path.to_string_lossy().into_owned();
    refile_heading(file.clone(), by_title(what), file, Some(by_title(beside)), Some(position), state.map(String::from), index, db)
}

fn history_kinds(db: &Mutex<Connection>, path: &std::path::Path) -> Vec<String> {
    let conn = db.lock().unwrap();
    history::list_for_path(&conn, &path.to_string_lossy(), 10).unwrap().into_iter().map(|e| format!("{:?}", e.kind)).collect()
}

/// What the list's drag-and-drop sends: same file, Before/After, the
/// target a sibling. It is a reorder, recorded as the file edit it is
/// -- not a refile, which would run the trigger cascade and record
/// "Refiled" for something that never left its parent.
#[test]
fn a_drop_before_a_sibling_reorders_instead_of_refiling() {
    let (dir, index, db) = setup();
    let path = dir.path().join("liste.org");
    std::fs::write(&path, LIST).unwrap();

    refile_in(&path, "Käse", "Milch", RefilePosition::Before, None, &index, &db).unwrap();

    assert_eq!(titles(&path), vec!["* TODO Brot", "* TODO Käse", "* TODO Milch"]);
    let kinds = history_kinds(&db, &path);
    assert_eq!(kinds.len(), 1, "{kinds:?}");
    assert!(kinds[0].starts_with("FileEdited"), "a reorder is a file edit, not a refile: {kinds:?}");
}

/// "After" resolves to the sibling that follows the target -- and to
/// "last" when there is none.
#[test]
fn a_drop_after_a_sibling_lands_behind_it() {
    let (dir, index, db) = setup();
    let path = dir.path().join("liste.org");
    std::fs::write(&path, LIST).unwrap();

    refile_in(&path, "Brot", "Milch", RefilePosition::After, None, &index, &db).unwrap();
    assert_eq!(titles(&path), vec!["* TODO Milch", "* TODO Brot", "* TODO Käse"]);

    refile_in(&path, "Milch", "Käse", RefilePosition::After, None, &index, &db).unwrap();
    assert_eq!(titles(&path), vec!["* TODO Brot", "* TODO Käse", "* TODO Milch"]);
}

/// Dropped where it already is: no write, no entry. Committing anyway
/// would touch the file for no change and wake the sync.
#[test]
fn a_drop_where_it_already_is_writes_nothing() {
    let (dir, index, db) = setup();
    let path = dir.path().join("liste.org");
    std::fs::write(&path, LIST).unwrap();
    let before = std::fs::metadata(&path).unwrap().modified().unwrap();

    refile_in(&path, "Brot", "Milch", RefilePosition::Before, None, &index, &db).unwrap();

    assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), before, "the file was not rewritten");
    assert!(history_kinds(&db, &path).is_empty(), "nothing to undo");
}

/// A state handed in for a heading that has none is more than a
/// reorder -- that stays a refile, and the heading comes out with the
/// state, as it always did.
#[test]
fn a_drop_that_would_set_a_state_is_still_a_refile() {
    let (dir, index, db) = setup();
    let path = dir.path().join("liste.org");
    std::fs::write(&path, "* Brot\n* TODO Milch\n* TODO Käse\n").unwrap();

    refile_in(&path, "Brot", "Käse", RefilePosition::Before, Some("TODO"), &index, &db).unwrap();

    assert_eq!(titles(&path), vec!["* TODO Milch", "* TODO Brot", "* TODO Käse"], "moved AND given the state");
    assert!(history_kinds(&db, &path)[0].starts_with("Refiled"));
}

/// Beside something that is not a sibling -- a task next to a project
/// -- is a refile, with the level shift that comes with it.
#[test]
fn a_drop_beside_a_non_sibling_is_still_a_refile() {
    let (dir, index, db) = setup();
    let path = dir.path().join("projekte.org");
    std::fs::write(&path, "* Projekt :project:\n** TODO Aufgabe\n* Anderes :project:\n").unwrap();

    refile_in(&path, "Aufgabe", "Anderes", RefilePosition::Before, None, &index, &db).unwrap();

    assert_eq!(titles(&path), vec!["* Projekt :project:", "* TODO Aufgabe", "* Anderes :project:"], "lifted to the project's level");
    assert!(history_kinds(&db, &path)[0].starts_with("Refiled"));
}

fn setup() -> (TempDir, IndexManager, Mutex<Connection>) {
    let dir = TempDir::new().unwrap();
    let index = IndexManager::new();
    let conn = Connection::open_in_memory().unwrap();
    nobo_core::core::history::store::init_schema(&conn).unwrap();
    (dir, index, Mutex::new(conn))
}

/// Reproduces a reported bug report verbatim: change an existing
/// subtask's state (recorded fine), then add a brand-new subtask under
/// the same project (reported as missing from Verlauf/history
/// afterwards). Exercises the exact same heading_utils/fs_utils/history
/// calls update_heading_in_file / create_heading_in_file make, just
/// without the tauri::State wrapper (which can't be constructed outside
/// a running app), to isolate whether the bug is in this shared logic.
/// The whole feature, through the same calls update_heading_in_file
/// makes: a recurring task clicked DONE must come back as an open task
/// one interval later, with no CLOSED line and the completion logged.
#[test]
fn finishing_a_recurring_task_reschedules_it_instead_of_closing_it() {
    let (dir, index, _db) = setup();
    let path = dir.path().join("actions.org");
    std::fs::write(&path, "* TODO Pflanzen gießen im Büro\nSCHEDULED: <2020-01-06 Mon +1w>\n").unwrap();
    index.scan_directory(dir.path());

    let flat = index.get_headings_or_parse(&path);
    let heading = flat[0].clone();
    let payload = HeadingUpdatePayload { state: Some("DONE".to_string()), ..Default::default() };

    let edit = repeat_utils::repeat_on_finish(&heading, &payload).unwrap();
    let block = heading_utils::build_org_heading_block(&edit.heading, &edit.payload);
    fs_utils::splice_lines(&path, heading.line_no - 1, heading.last_line, block).unwrap();
    index.update_file(&path);

    let result = std::fs::read_to_string(&path).unwrap();
    assert!(result.contains("* TODO Pflanzen gießen im Büro"), "{result}");
    assert!(!result.contains("CLOSED:"), "a repeating task never closes: {result}");
    assert!(result.contains(":LAST_REPEAT:"), "{result}");
    assert!(result.contains("- State \"DONE"), "{result}");

    // +1w from the SCHEDULED date, exactly once, however late it is.
    let reparsed = index.get_headings_or_parse(&path);
    let scheduled = reparsed[0].scheduled.as_ref().unwrap();
    assert_eq!(scheduled.start_date, "2020-01-13");
    assert!(scheduled.repeater.is_some(), "the repeater has to survive, or it only repeats once");
    assert_eq!(reparsed[0].state.as_deref(), Some("TODO"));

    // Exactly one state note, not one for the completion plus a second
    // for the reopen.
    assert_eq!(result.matches("- State ").count(), 1, "{result}");
}

#[test]
fn creating_a_subtask_after_a_state_change_still_records_both_as_history_steps() {
    let (dir, index, db) = setup();
    let path = dir.path().join("projects.org");
    std::fs::write(&path, "* Project :project:\n** TODO Subtask\n").unwrap();
    let path_str = path.to_string_lossy().into_owned();

    // -- Step 1: update_heading_in_file's logic -- change Subtask's state.
    let flat = index.get_headings_or_parse(&path);
    let subtask = flat.iter().find(|h| h.title == "Subtask").unwrap().clone();
    let payload = HeadingUpdatePayload {
        state: Some("DONE".to_string()),
        priority: None,
        title: None,
        scheduled: None,
        deadline: None,
        content: None,
        closed: None,
        properties: None,
        history: None,
        clocks: None,
        tags: None,
        effort_minutes: None,
    };
    let before = std::fs::read_to_string(&path).ok();
    let new_block = heading_utils::build_org_heading_block(&subtask, &payload);
    fs_utils::splice_lines(&path, subtask.line_no - 1, subtask.last_line, new_block).unwrap();
    index.update_file(&path);
    let after = std::fs::read_to_string(&path).ok();
    record_best_effort(
        &db,
        HistoryEntryKind::HeadingUpdated { filename: "projects.org".into(), title: "Subtask".into() },
        false,
        vec![FileSnapshot { path: path_str.clone(), before, after }],
    );

    {
        let conn = db.lock().unwrap();
        assert_eq!(history::list_for_path(&conn, &path_str, 10).unwrap().len(), 1, "state change recorded");
    }

    // -- Step 2: create_heading_in_file's logic -- add a new subtask under Project.
    let flat = index.get_headings_or_parse(&path);
    let project = flat.iter().find(|h| h.title == "Project").unwrap().clone();
    let create_payload = HeadingCreatePayload {
        file_path: path_str.clone(),
        title: "New Subtask".into(),
        state: Some("TODO".into()),
        tags: vec![],
        parent: Some(target_for(&project)),
        scheduled: None,
        effort_minutes: None,
        content: None,
        category: None,
    };
    let before2 = std::fs::read_to_string(&path).ok();
    let new_lines = heading_utils::build_new_heading_block(&create_payload, project.level + 1);
    let parent_idx = flat.iter().position(|h| h.line_no == project.line_no).unwrap();
    let insert_after_line = heading_utils::find_subtree_end_line(&flat, parent_idx);
    fs_utils::insert_lines_after(&path, insert_after_line, new_lines).unwrap();
    index.update_file(&path);
    let after2 = std::fs::read_to_string(&path).ok();
    record_best_effort(
        &db,
        HistoryEntryKind::HeadingCreated { filename: "projects.org".into(), title: "New Subtask".into() },
        false,
        vec![FileSnapshot { path: path_str.clone(), before: before2, after: after2 }],
    );

    let conn = db.lock().unwrap();
    let entries = history::list_for_path(&conn, &path_str, 10).unwrap();
    assert_eq!(entries.len(), 2, "the subtask creation should ALSO show up as its own step");
}

/// Reported bug: sorting Projects by "last updated" never moved a
/// project up, because only edits made directly to the project heading
/// itself touched its own timestamp -- finishing/adding a subtask
/// nested underneath left it stale. Fixed via a separate LAST_ACTIVITY
/// property (see heading_utils::touch_ancestors) rather than reusing
/// UPDATED for both, specifically so UPDATED keeps meaning "I was
/// directly edited" -- verified here two levels deep: a subtask change
/// bumps LAST_ACTIVITY on every ancestor up to the project, while their
/// own UPDATED (a fact about direct edits, unrelated to this subtask)
/// stays exactly as it was.
#[test]
fn a_subtask_state_change_bumps_last_activity_but_not_updated_on_every_ancestor() {
    let (dir, index, _db) = setup();
    let path = dir.path().join("projects.org");
    // "Group" needs a real state -- otherwise it isn't real structure
    // per org_parser's own rule (state or :project: tag required once
    // something else already opened the heading before it, here
    // "Project"), and would be absorbed as Project's own content
    // instead of staying a separate heading with Subtask nested under
    // IT.
    std::fs::write(
        &path,
        "* Project :project:\n:PROPERTIES:\n:UPDATED: 2020-01-01 Wed 00:00\n:LAST_ACTIVITY: 2020-01-01 Wed 00:00\n:END:\n\
         ** TODO Group\n:PROPERTIES:\n:UPDATED: 2020-01-01 Wed 00:00\n:LAST_ACTIVITY: 2020-01-01 Wed 00:00\n:END:\n\
         *** TODO Subtask\n",
    )
    .unwrap();

    let flat = index.get_headings_or_parse(&path);
    let idx = flat.iter().position(|h| h.title == "Subtask").unwrap();
    let heading = flat[idx].clone();
    let ancestors = heading_utils::ancestors_of(&flat, idx);
    assert_eq!(ancestors.iter().map(|h| h.title.as_str()).collect::<Vec<_>>(), vec!["Group", "Project"], "nearest ancestor first");

    // -- update_heading_in_file's logic: TODO -> DONE.
    let payload = HeadingUpdatePayload { state: Some("DONE".to_string()), ..Default::default() };
    let mut edit = heading_utils::open_edit(&path, &at_line(heading.line_no), &nobo_core::core::errors::subject::heading()).unwrap();
    edit.write(&payload).unwrap();
    edit.touch_ancestors().unwrap();
    edit.commit().unwrap();
    index.update_file(&path);

    let flat_after = index.get_headings_or_parse(&path);
    let project = flat_after.iter().find(|h| h.title == "Project").unwrap();
    let group = flat_after.iter().find(|h| h.title == "Group").unwrap();
    assert_ne!(project.properties.get("LAST_ACTIVITY").unwrap(), "2020-01-01 Wed 00:00", "project's LAST_ACTIVITY bumped");
    assert_ne!(group.properties.get("LAST_ACTIVITY").unwrap(), "2020-01-01 Wed 00:00", "intermediate group's LAST_ACTIVITY bumped");
    assert_eq!(project.properties.get("UPDATED").unwrap(), "2020-01-01 Wed 00:00", "project's own UPDATED untouched");
    assert_eq!(group.properties.get("UPDATED").unwrap(), "2020-01-01 Wed 00:00", "group's own UPDATED untouched");

    // -- create_heading_in_file's logic: add a brand-new subtask under Group,
    // also bumps Group AND Project's LAST_ACTIVITY (the parent itself, not just its ancestors).
    std::fs::write(
        &path,
        "* Project :project:\n:PROPERTIES:\n:UPDATED: 2021-01-01 Wed 00:00\n:LAST_ACTIVITY: 2021-01-01 Wed 00:00\n:END:\n\
         ** TODO Group\n:PROPERTIES:\n:UPDATED: 2021-01-01 Wed 00:00\n:LAST_ACTIVITY: 2021-01-01 Wed 00:00\n:END:\n\
         *** TODO Subtask\n",
    )
    .unwrap();
    index.update_file(&path);
    let flat2 = index.get_headings_or_parse(&path);
    let group_idx = flat2.iter().position(|h| h.title == "Group").unwrap();
    let create_payload = HeadingCreatePayload {
        file_path: path.to_string_lossy().into_owned(),
        title: "New Subtask".into(),
        state: Some("TODO".into()),
        tags: vec![],
        parent: Some(target_for(&flat2[group_idx])),
        scheduled: None,
        effort_minutes: None,
        content: None,
        category: None,
    };
    let new_lines = heading_utils::build_new_heading_block(&create_payload, flat2[group_idx].level + 1);
    let mut edit =
        heading_utils::open_edit(&path, &at_line(flat2[group_idx].line_no), &nobo_core::core::errors::subject::parent_heading()).unwrap();
    edit.insert_after_subtree(new_lines).unwrap();
    edit.touch_self_and_ancestors().unwrap();
    edit.commit().unwrap();
    index.update_file(&path);

    let flat3 = index.get_headings_or_parse(&path);
    let project3 = flat3.iter().find(|h| h.title == "Project").unwrap();
    let group3 = flat3.iter().find(|h| h.title == "Group").unwrap();
    assert_ne!(project3.properties.get("LAST_ACTIVITY").unwrap(), "2021-01-01 Wed 00:00", "project's LAST_ACTIVITY bumped by new subtask");
    assert_ne!(
        group3.properties.get("LAST_ACTIVITY").unwrap(),
        "2021-01-01 Wed 00:00",
        "direct parent's LAST_ACTIVITY bumped by new subtask"
    );
    assert_eq!(project3.properties.get("UPDATED").unwrap(), "2021-01-01 Wed 00:00", "project's own UPDATED untouched by new subtask");
    assert_eq!(group3.properties.get("UPDATED").unwrap(), "2021-01-01 Wed 00:00", "group's own UPDATED untouched by new subtask");
}

/// Direct regression test for the single-item update path's most basic
/// safety property: editing one heading must never change so much as a
/// byte of an UNRELATED sibling elsewhere in the same file. The edit
/// here (TODO -> DONE) actually grows the file (adds a CLOSED line),
/// which shifts every heading after it to a new absolute line number --
/// deliberately, so this proves Project B's own CONTENT survives at its
/// new (shifted) location, not just that nothing happened to touch a
/// line number that would've stayed accidentally correct anyway.
#[test]
fn updating_one_heading_leaves_an_unrelated_sibling_project_byte_identical() {
    let (dir, index, _db) = setup();
    let path = dir.path().join("projects.org");
    std::fs::write(
        &path,
        "* Project A :project:\n** TODO Subtask A1\n\
         * Project B :project:\n:PROPERTIES:\n:CUSTOM_ID: proj-b\n:UPDATED: 2020-01-01 Wed 00:00\n:END:\n\
         ** DONE Subtask B1\n:PROPERTIES:\n:CLOSED: [2020-01-01 Wed]\n:END:\n\
         ** TODO Subtask B2 :urgent:\nSome body text.\n",
    )
    .unwrap();

    let text_of = |flat: &[Heading], title: &str| {
        let idx = flat.iter().position(|h| h.title == title).unwrap();
        let (start, end) = heading_utils::subtree_line_range(flat, idx);
        fs_utils::read_lines(&path).unwrap()[start - 1..end].join("\n")
    };
    let flat_before = index.get_headings_or_parse(&path);
    let project_b_before = text_of(&flat_before, "Project B");

    // -- update_heading_in_file's own logic: TODO -> DONE (grows the file).
    let a1 = flat_before.iter().find(|h| h.title == "Subtask A1").unwrap().clone();
    let payload = HeadingUpdatePayload { state: Some("DONE".to_string()), ..Default::default() };
    let new_block = heading_utils::build_org_heading_block(&a1, &payload);
    fs_utils::splice_lines(&path, a1.line_no - 1, a1.last_line, new_block).unwrap();
    index.update_file(&path);

    let flat_after = index.get_headings_or_parse(&path);
    let a1_after = flat_after.iter().find(|h| h.title == "Subtask A1").unwrap();
    assert_eq!(a1_after.state.as_deref(), Some("DONE"), "the actual edit took effect");

    assert_eq!(
        project_b_before,
        text_of(&flat_after, "Project B"),
        "unrelated Project B subtree byte-identical at its new, shifted location"
    );
}

/// Same concern as the update test above, but for create_heading_in_file
/// -- inserting a new heading under Project A must never leak so much as
/// a byte into unrelated Project B elsewhere in the file.
#[test]
fn creating_a_heading_leaves_an_unrelated_sibling_project_byte_identical() {
    let (dir, index, _db) = setup();
    let path = dir.path().join("projects.org");
    std::fs::write(
        &path,
        "* Project A :project:\n** TODO Subtask A1\n\
         * Project B :project:\n:PROPERTIES:\n:CUSTOM_ID: proj-b\n:END:\n\
         ** DONE Subtask B1\n** TODO Subtask B2 :urgent:\nSome body text.\n",
    )
    .unwrap();

    let text_of = |flat: &[Heading], title: &str| {
        let idx = flat.iter().position(|h| h.title == title).unwrap();
        let (start, end) = heading_utils::subtree_line_range(flat, idx);
        fs_utils::read_lines(&path).unwrap()[start - 1..end].join("\n")
    };
    let flat_before = index.get_headings_or_parse(&path);
    let project_b_before = text_of(&flat_before, "Project B");

    // -- create_heading_in_file's own logic: add a new subtask under Project A.
    let project_a = flat_before.iter().find(|h| h.title == "Project A").unwrap().clone();
    let create_payload = HeadingCreatePayload {
        file_path: path.to_string_lossy().into_owned(),
        title: "New Subtask".into(),
        state: Some("TODO".into()),
        tags: vec![],
        parent: Some(target_for(&project_a)),
        scheduled: None,
        effort_minutes: None,
        content: None,
        category: None,
    };
    let parent_idx = flat_before.iter().position(|h| h.line_no == project_a.line_no).unwrap();
    let new_lines = heading_utils::build_new_heading_block(&create_payload, flat_before[parent_idx].level + 1);
    let insert_after_line = heading_utils::find_subtree_end_line(&flat_before, parent_idx);
    fs_utils::insert_lines_after(&path, insert_after_line, new_lines).unwrap();
    index.update_file(&path);

    let flat_after = index.get_headings_or_parse(&path);
    assert!(flat_after.iter().any(|h| h.title == "New Subtask"), "the actual create took effect");

    assert_eq!(
        project_b_before,
        text_of(&flat_after, "Project B"),
        "unrelated Project B subtree byte-identical at its new, shifted location"
    );
}

/// delete_heading_in_file's own body has no test elsewhere in this file
/// (only its batched sibling heading_utils::apply_bulk_delete does).
/// This is the direct regression test for the single-item delete path,
/// covering the two "Nebeneffekte" failure modes explicitly called out
/// for this feature: leaking into an unrelated sibling ELSEWHERE in the
/// file, and -- the more dangerous one -- partially eating a NEIGHBORING
/// heading's own lines because subtree_line_range's end_line was off by
/// one (deleting a heading sandwiched directly between two others is
/// exactly the case that would expose that).
#[test]
fn deleting_a_heading_leaves_unrelated_siblings_byte_identical_and_does_not_leak_into_neighbors() {
    let (dir, index, _db) = setup();
    let path = dir.path().join("projects.org");
    std::fs::write(
        &path,
        "* Project A :project:\n** TODO Keep me A\n\
         * Project B :project:\n** TODO Delete me\n*** TODO Nested under delete me\n** TODO Keep me B\n\
         * Project C :project:\n:PROPERTIES:\n:CUSTOM_ID: proj-c\n:END:\n** DONE Keep me C\n",
    )
    .unwrap();

    let text_of = |flat: &[Heading], title: &str| {
        let idx = flat.iter().position(|h| h.title == title).unwrap();
        let (start, end) = heading_utils::subtree_line_range(flat, idx);
        fs_utils::read_lines(&path).unwrap()[start - 1..end].join("\n")
    };
    let flat_before = index.get_headings_or_parse(&path);
    let project_a_before = text_of(&flat_before, "Project A");
    let project_c_before = text_of(&flat_before, "Project C");
    let keep_me_b_before = text_of(&flat_before, "Keep me B");

    // -- delete_heading_in_file's own logic: remove "Delete me" (and its own nested child).
    let idx = flat_before.iter().position(|h| h.title == "Delete me").unwrap();
    let mut edit =
        heading_utils::open_edit(&path, &at_line(flat_before[idx].line_no), &nobo_core::core::errors::subject::heading()).unwrap();
    edit.delete_subtree().unwrap();
    edit.touch_ancestors().unwrap();
    edit.commit().unwrap();
    index.update_file(&path);

    let flat_after = index.get_headings_or_parse(&path);
    assert!(flat_after.iter().find(|h| h.title == "Delete me").is_none(), "the deleted heading is actually gone");
    assert!(flat_after.iter().find(|h| h.title == "Nested under delete me").is_none(), "its own nested child goes with it");

    assert_eq!(project_a_before, text_of(&flat_after, "Project A"), "unrelated Project A, entirely before the deletion, untouched");
    assert_eq!(project_c_before, text_of(&flat_after, "Project C"), "unrelated Project C, entirely after the deletion, untouched");
    assert_eq!(
        keep_me_b_before,
        text_of(&flat_after, "Keep me B"),
        "Keep me B's own text byte-identical -- proves the deleted range's end_line didn't spill into its immediate next sibling"
    );

    let project_b = flat_after.iter().find(|h| h.title == "Project B").unwrap();
    assert!(
        project_b.properties.contains_key("LAST_ACTIVITY"),
        "Project B, the deleted heading's own parent, gets touched per touch_ancestors"
    );
}

/// End-to-end command test for refile_heading (the single-item path) --
/// no existing test drives its own body: core::refile's tests cover
/// splice_refile as a pure function, and this exercises the same call
/// threaded through history-snapshotting/reindexing the way the real
/// command does, across two files, confirming an unrelated heading in
/// EACH file survives untouched.
#[test]
fn refile_heading_body_moves_one_item_and_leaves_unrelated_headings_in_both_files_untouched() {
    let (dir, index, db) = setup();
    let src_path = dir.path().join("inbox.org");
    let tgt_path = dir.path().join("projects.org");
    std::fs::write(&src_path, "* TODO Move me\nSome body.\n* TODO Stay in inbox\n").unwrap();
    std::fs::write(&tgt_path, "* Project :project:\n** TODO Existing subtask\n").unwrap();

    let src_flat = index.get_headings_or_parse(&src_path);
    let src_idx = src_flat.iter().position(|h| h.title == "Move me").unwrap();
    let (start_line, end_line) = heading_utils::subtree_line_range(&src_flat, src_idx);

    let tgt_flat = index.get_headings_or_parse(&tgt_path);
    let target_parent_line = tgt_flat.iter().find(|h| h.title == "Project").unwrap().line_no;
    let target_level = refile::resolve_target_level(&tgt_flat, Some(target_parent_line), RefilePosition::Into).unwrap();

    let all_src_lines = fs_utils::read_lines(&src_path).unwrap();
    let all_tgt_lines = fs_utils::read_lines(&tgt_path).unwrap();
    let before_src = all_src_lines.join("\n");
    let before_tgt = all_tgt_lines.join("\n");

    let params = refile::RefileParams { start_line, end_line, src_level: 1, target_level, default_state: None };
    let (new_src_lines, new_tgt_lines) =
        refile::splice_refile(all_src_lines, &params, Some(target_parent_line), RefilePosition::Into, false, &tgt_path, all_tgt_lines)
            .unwrap();
    fs_utils::write_lines(&src_path, &new_src_lines).unwrap();
    fs_utils::write_lines(&tgt_path, &new_tgt_lines).unwrap();
    index.update_file(&src_path);
    index.update_file(&tgt_path);
    record_best_effort(
        &db,
        HistoryEntryKind::Refiled { title: "Move me".into(), target_filename: "projects.org".into() },
        false,
        vec![
            FileSnapshot { path: tgt_path.to_string_lossy().into_owned(), before: Some(before_tgt), after: Some(new_tgt_lines.join("\n")) },
            FileSnapshot { path: src_path.to_string_lossy().into_owned(), before: Some(before_src), after: Some(new_src_lines.join("\n")) },
        ],
    );

    let src_after = index.get_headings_or_parse(&src_path);
    let tgt_after = index.get_headings_or_parse(&tgt_path);
    assert!(src_after.iter().find(|h| h.title == "Move me").is_none(), "moved out of the source file");
    assert!(tgt_after.iter().any(|h| h.title == "Move me"), "arrived in the target file");
    assert!(src_after.iter().any(|h| h.title == "Stay in inbox"), "unrelated source sibling untouched");
    assert!(tgt_after.iter().any(|h| h.title == "Existing subtask"), "unrelated pre-existing target subtask untouched");

    let conn = db.lock().unwrap();
    assert_eq!(
        history::list_for_path(&conn, &tgt_path.to_string_lossy(), 10).unwrap().len(),
        1,
        "one history entry recorded for the target file"
    );
}

/// Reported perf problem: bulk-selecting several subtasks under the
/// same project and changing their state took visibly long, because
/// the old path called update_heading_in_file once PER task -- each
/// its own full-file read/write/reparse/history entry, and each its
/// own touch_ancestors pass rewriting the SAME shared project heading
/// again. Exercises heading_utils::apply_bulk_update (the batched
/// replacement) directly against real parsed headings, with each
/// entry carrying its OWN, genuinely different payload -- not just a
/// uniform state change repeated -- since that per-entry independence
/// is the whole point of this being a general bulk_update_headings
/// command rather than a state-change-specific one. Also verifies the
/// shared project ancestor's LAST_ACTIVITY ends up bumped exactly once
/// for the whole batch, regardless of how many siblings under it were
/// touched.
#[test]
fn bulk_update_applies_independent_payloads_and_touches_shared_ancestor_once() {
    let (dir, index, _db) = setup();
    let path = dir.path().join("projects.org");
    std::fs::write(
        &path,
        "* Project :project:\n:PROPERTIES:\n:UPDATED: 2020-01-01 Wed 00:00\n:LAST_ACTIVITY: 2020-01-01 Wed 00:00\n:END:\n\
         ** TODO Subtask A\n\
         ** TODO Subtask B\n",
    )
    .unwrap();

    let flat = index.get_headings_or_parse(&path);
    let line_of = |title: &str| flat.iter().find(|h| h.title == title).unwrap().line_no;

    let updates = vec![
        BulkHeadingUpdate {
            target: at_line(line_of("Subtask A")),
            payload: HeadingUpdatePayload { state: Some("DONE".to_string()), ..Default::default() },
        },
        BulkHeadingUpdate {
            target: at_line(line_of("Subtask B")),
            payload: HeadingUpdatePayload { tags: Some(vec!["urgent".to_string()]), ..Default::default() },
        },
    ];

    let mut lines = fs_utils::read_lines(&path).unwrap();
    let count = heading_utils::apply_bulk_update(&flat, &updates, &mut lines).unwrap();
    fs_utils::write_lines(&path, &lines).unwrap();

    assert_eq!(count, 2);

    index.update_file(&path);
    let flat_after = index.get_headings_or_parse(&path);
    let project = flat_after.iter().find(|h| h.title == "Project").unwrap();
    let a = flat_after.iter().find(|h| h.title == "Subtask A").unwrap();
    let b = flat_after.iter().find(|h| h.title == "Subtask B").unwrap();

    assert_eq!(a.state.as_deref(), Some("DONE"), "A got its own state payload");
    assert_eq!(b.tags, vec!["urgent".to_string()], "B got its own, different tags payload");
    assert_eq!(b.state.as_deref(), Some("TODO"), "B's state untouched by A's payload -- each entry's patch stays independent");
    assert_ne!(
        project.properties.get("LAST_ACTIVITY").unwrap(),
        "2020-01-01 Wed 00:00",
        "shared project ancestor's LAST_ACTIVITY bumped once by the batch"
    );
    assert_eq!(project.properties.get("UPDATED").unwrap(), "2020-01-01 Wed 00:00", "project's own UPDATED untouched by a subtask batch");
}

/// Same reported-perf-problem shape as the bulk_update test above, but
/// for "Auswahl löschen" (bulk delete) -- the old useBulkDelete.ts path
/// called delete_heading_in_file once PER task, each its own full-file
/// read/write/reparse/history entry. Exercises
/// heading_utils::apply_bulk_delete directly: two independent subtasks
/// under one project removed in a single pass, and the shared project
/// ancestor's LAST_ACTIVITY bumped exactly once for the whole batch.
#[test]
fn bulk_delete_removes_independent_subtrees_and_touches_shared_ancestor_once() {
    let (dir, index, _db) = setup();
    let path = dir.path().join("projects.org");
    std::fs::write(
        &path,
        "* Project :project:\n:PROPERTIES:\n:UPDATED: 2020-01-01 Wed 00:00\n:LAST_ACTIVITY: 2020-01-01 Wed 00:00\n:END:\n\
         ** TODO Subtask A\n\
         ** TODO Subtask B\n\
         ** TODO Subtask C\n",
    )
    .unwrap();

    let flat = index.get_headings_or_parse(&path);
    let line_of = |title: &str| flat.iter().find(|h| h.title == title).unwrap().line_no;

    // Subtask C is deliberately left out of the selection, to confirm
    // it survives untouched.
    let targets = vec![at_line(line_of("Subtask A")), at_line(line_of("Subtask B"))];

    let mut lines = fs_utils::read_lines(&path).unwrap();
    let count = heading_utils::apply_bulk_delete(&flat, &targets, &mut lines).unwrap();
    fs_utils::write_lines(&path, &lines).unwrap();

    assert_eq!(count, 2);

    index.update_file(&path);
    let flat_after = index.get_headings_or_parse(&path);
    assert!(flat_after.iter().find(|h| h.title == "Subtask A").is_none(), "A removed");
    assert!(flat_after.iter().find(|h| h.title == "Subtask B").is_none(), "B removed");
    assert!(flat_after.iter().find(|h| h.title == "Subtask C").is_some(), "C untouched");
    let project = flat_after.iter().find(|h| h.title == "Project").unwrap();
    assert_ne!(
        project.properties.get("LAST_ACTIVITY").unwrap(),
        "2020-01-01 Wed 00:00",
        "shared project ancestor's LAST_ACTIVITY bumped once by the batch"
    );
    assert_eq!(project.properties.get("UPDATED").unwrap(), "2020-01-01 Wed 00:00", "project's own UPDATED untouched by a subtask batch");
}

/// The hazard useBulkDelete.ts's own doc comment calls out: a selection
/// containing BOTH a project AND one of its own subtasks at once. A
/// naive per-range removal would double-delete (the child's lines are
/// already inside the parent's own subtree range) and corrupt the
/// file -- apply_bulk_delete must recognize the overlap and only remove
/// the ancestor's subtree once.
#[test]
fn bulk_delete_of_a_selected_project_and_its_own_selected_subtask_only_removes_once() {
    let (dir, index, _db) = setup();
    let path = dir.path().join("projects.org");
    std::fs::write(
        &path,
        "* Project A :project:\n\
         ** TODO Subtask\n\
         * Project B :project:\n\
         ** TODO Keep me\n",
    )
    .unwrap();

    let flat = index.get_headings_or_parse(&path);
    let line_of = |title: &str| flat.iter().find(|h| h.title == title).unwrap().line_no;

    // Selected in the "wrong" (parent-first) order on purpose --
    // dedup must not depend on selection order.
    let targets = vec![at_line(line_of("Project A")), at_line(line_of("Subtask"))];

    let mut lines = fs_utils::read_lines(&path).unwrap();
    let count = heading_utils::apply_bulk_delete(&flat, &targets, &mut lines).unwrap();
    fs_utils::write_lines(&path, &lines).unwrap();

    assert_eq!(count, 1, "the subtask doesn't count separately -- it's covered by Project A's own subtree delete");

    index.update_file(&path);
    let flat_after = index.get_headings_or_parse(&path);
    assert!(flat_after.iter().find(|h| h.title == "Project A").is_none());
    assert!(flat_after.iter().find(|h| h.title == "Subtask").is_none());
    assert!(flat_after.iter().find(|h| h.title == "Project B").is_some(), "unrelated sibling project untouched");
    assert!(flat_after.iter().find(|h| h.title == "Keep me").is_some(), "unrelated sibling project's own subtask untouched");
}

/// End-to-end command test (mirrors the bulk_update command tests
/// elsewhere in this file) -- confirms bulk_delete_headings itself (not
/// just its pure apply_bulk_delete core) does one read/write/reparse
/// for the whole batch and records exactly one HeadingsBulkDeleted
/// history entry, not one per deleted heading.
#[test]
fn bulk_delete_headings_body_writes_once_and_records_one_history_entry() {
    let (dir, index, db) = setup();
    let path = dir.path().join("actions.org");
    std::fs::write(
        &path,
        "* TODO Task A\n\
         * TODO Task B\n\
         * TODO Task C\n",
    )
    .unwrap();

    let flat = index.get_headings_or_parse(&path);
    let line_of = |title: &str| flat.iter().find(|h| h.title == title).unwrap().line_no;
    let targets = vec![at_line(line_of("Task A")), at_line(line_of("Task B"))];

    let file_path = path.to_string_lossy().into_owned();
    let (before, result, after) = history::snapshot_around(&path, || {
        let flat = index.get_headings_or_parse(&path);
        let mut lines = fs_utils::read_lines(&path).unwrap();
        let count = heading_utils::apply_bulk_delete(&flat, &targets, &mut lines)?;
        if count > 0 {
            fs_utils::write_lines(&path, &lines).unwrap();
            index.update_file(&path);
        }
        Ok::<u32, String>(count)
    });
    let count = result.unwrap();
    assert_eq!(count, 2);
    if count > 0 {
        record_best_effort(
            &db,
            HistoryEntryKind::HeadingsBulkDeleted { filename: filename_of(&path), count },
            false,
            vec![FileSnapshot { path: file_path, before, after }],
        );
    }

    let flat_after = index.get_headings_or_parse(&path);
    assert_eq!(flat_after.len(), 1, "only Task C survives");
    assert_eq!(flat_after[0].title, "Task C");

    let entries = history::store::list(&db.lock().unwrap(), 10).unwrap();
    assert_eq!(entries.len(), 1, "exactly one history entry for the whole batch, not one per deleted heading");
    assert!(matches!(&entries[0].kind, HistoryEntryKind::HeadingsBulkDeleted { count, .. } if *count == 2));
}

/// End-to-end command test for bulk_refile_headings (mirrors the
/// bulk_delete_headings command test above) -- confirms the whole
/// batch does one source read/write and one target read/write (not one
/// per moved heading) and records exactly one HeadingsBulkRefiled
/// history entry covering both files.
#[test]
fn bulk_refile_headings_body_moves_the_whole_batch_in_one_pass() {
    let (dir, index, db) = setup();
    let src_path = dir.path().join("actions.org");
    let tgt_path = dir.path().join("projects.org");
    // Every task needs a real state -- otherwise only the first ("Task
    // A") stays real structure and B/C get absorbed as its own
    // content instead of remaining separate, addressable headings
    // (org_parser's rule: state or :project: tag required once
    // something else already opened the heading before them).
    std::fs::write(&src_path, "* TODO Task A\n* TODO Task B\n* TODO Task C\n").unwrap();
    std::fs::write(&tgt_path, "* Project :project:\n").unwrap();

    let src_flat = index.get_headings_or_parse(&src_path);
    let line_of = |title: &str| src_flat.iter().find(|h| h.title == title).unwrap().line_no;

    let sources = vec![
        BulkRefileSource { target: at_line(line_of("Task A")), default_state: Some("TODO".to_string()) },
        BulkRefileSource { target: at_line(line_of("Task C")), default_state: Some("TODO".to_string()) },
    ];
    let tgt_flat = index.get_headings_or_parse(&tgt_path);
    let target_parent_line = tgt_flat[0].line_no as u32;

    let source_file = src_path.to_string_lossy().into_owned();
    let target_file = tgt_path.to_string_lossy().into_owned();

    // Body of bulk_refile_headings, minus the tauri::State wrapper.
    let same_file = source_file == target_file;
    let src_flat = index.get_headings_or_parse(&src_path);
    let target_level = {
        let tgt_flat = index.get_headings_or_parse(&tgt_path);
        refile::resolve_target_level(&tgt_flat, Some(target_parent_line as usize), RefilePosition::Into).unwrap()
    };
    let all_src_lines = fs_utils::read_lines(&src_path).unwrap();
    let all_tgt_lines = fs_utils::read_lines(&tgt_path).unwrap();
    let before_src = all_src_lines.join("\n");
    let before_tgt = Some(all_tgt_lines.join("\n"));
    let source_entries: Vec<(usize, Option<String>)> = sources.into_iter().map(|s| (s.target.line_no, s.default_state)).collect();

    let (new_src_lines, new_tgt_lines, count) = refile::splice_bulk_refile(
        &src_flat,
        &source_entries,
        all_src_lines,
        Some(target_parent_line as usize),
        same_file,
        target_level,
        &tgt_path,
        all_tgt_lines,
    )
    .unwrap();
    assert_eq!(count, 2);
    let after_src = new_src_lines.join("\n");
    let after_tgt = new_tgt_lines.join("\n");
    fs_utils::write_lines(&src_path, &new_src_lines).unwrap();
    fs_utils::write_lines(&tgt_path, &new_tgt_lines).unwrap();
    index.update_file(&src_path);
    index.update_file(&tgt_path);
    record_best_effort(
        &db,
        HistoryEntryKind::HeadingsBulkRefiled { count, target_filename: filename_of(&tgt_path) },
        false,
        vec![
            FileSnapshot { path: target_file, before: before_tgt, after: Some(after_tgt) },
            FileSnapshot { path: source_file, before: Some(before_src), after: Some(after_src) },
        ],
    );

    let src_after = index.get_headings_or_parse(&src_path);
    assert_eq!(src_after.len(), 1, "only Task B stays behind");
    assert_eq!(src_after[0].title, "Task B");

    let tgt_after = index.get_headings_or_parse(&tgt_path);
    assert_eq!(tgt_after.iter().map(|h| h.title.as_str()).collect::<Vec<_>>(), vec!["Project", "Task A", "Task C"]);
    assert_eq!(tgt_after[1].level, 2);
    assert_eq!(tgt_after[1].state.as_deref(), Some("TODO"), "default state applied to the previously-stateless Task A");

    let entries = history::store::list(&db.lock().unwrap(), 10).unwrap();
    assert_eq!(entries.len(), 1, "exactly one history entry for the whole batch, not one per moved heading");
    assert!(matches!(&entries[0].kind, HistoryEntryKind::HeadingsBulkRefiled { count, .. } if *count == 2));
}

/* --- get_time_tracking_summary --- */

#[test]
fn summary_groups_by_category_with_a_fallback_bucket_for_uncategorized() {
    let (dir, index, _db) = setup();
    let path = dir.path().join("projects.org");
    // Both need the :project: tag -- otherwise only "Project A" (the
    // first heading in the file) stays real structure and "Project B"
    // gets absorbed as its own content instead of remaining a
    // separate, addressable level-1 heading (org_parser's rule: state
    // or :project: tag required once something else already opened
    // the heading before it).
    std::fs::write(
        &path,
        concat!(
            "* Project A :project:\n",
            "  :PROPERTIES:\n",
            "  :CATEGORY: Work\n",
            "  :END:\n",
            "  :LOGBOOK:\n",
            "  CLOCK: [2026-08-17 Mon 09:00]--[2026-08-17 Mon 10:00] =>  1:00\n",
            "  :END:\n",
            "* Project B :project:\n",
            "  :LOGBOOK:\n",
            "  CLOCK: [2026-08-17 Mon 11:00]--[2026-08-17 Mon 11:30] =>  0:30\n",
            "  :END:\n",
        ),
    )
    .unwrap();
    index.update_file(&path);

    let summary = task_utils::time_tracking_summary(&index.headings_by_file());
    assert_eq!(summary.grand_total_minutes, 90);
    let by_category = summary.by_category;

    assert_eq!(by_category.len(), 2);
    let work = by_category.iter().find(|c| c.category.as_deref() == Some("Work")).unwrap();
    assert_eq!(work.total_minutes, 60);
    let fallback = by_category.iter().find(|c| c.category.is_none()).unwrap();
    assert_eq!(fallback.total_minutes, 30, "Project B has no CATEGORY -- falls into the fallback bucket");
}

#[test]
fn summary_matches_category_case_insensitively() {
    let (dir, index, _db) = setup();
    let path = dir.path().join("projects.org");
    std::fs::write(
        &path,
        concat!(
            "* Schule\n",
            "  :PROPERTIES:\n",
            "  :Category: Schule\n",
            "  :END:\n",
            "  :LOGBOOK:\n",
            "  CLOCK: [2026-08-17 Mon 09:00]--[2026-08-17 Mon 10:00] =>  1:00\n",
            "  :END:\n",
        ),
    )
    .unwrap();
    index.update_file(&path);

    let flat = index.get_headings_or_parse(&path);
    // Through the shared lookup, i.e. the one production code uses.
    let category = nobo_core::core::models::property_value(&flat[0].properties, "CATEGORY");
    assert_eq!(category, Some("Schule"), "a hand-written lowercase-ish key is still found");
}
