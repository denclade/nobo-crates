// SPDX-FileCopyrightText: 2026 Dennis Burgermeister
// SPDX-License-Identifier: GPL-3.0-or-later

//! The disk-writing logic behind the `clock_in` and `clock_out` commands,
//! kept out of the commands layer so that stays a thin state-extraction
//! wrapper. Distinct from `utils::clock_utils`, which is pure line parsing
//! and formatting with no I/O -- this is I/O orchestration built on it.

use crate::core::heading_utils;
use crate::core::indexer::IndexManager;
use crate::core::models::{HeadingTarget, HeadingUpdatePayload, HistoryEntryKind};
use crate::core::vault_write;
use crate::utils::clock_utils;
use crate::utils::fs_utils::filename_of;
use rusqlite::Connection;
use std::path::PathBuf;
use std::sync::Mutex;

/// Closes FILE_PATH/LINE_NO's own open CLOCK entry (`--[now] =>  H:MM`
/// appended, see clock_utils::format_clock_line) -- shared by the
/// `clock_out` command itself and clock_in's own auto-close-the-other-one
/// step. A session that would round down to zero minutes is discarded
/// instead of closed (the minimum-time rule, see the body's own doc
/// comment on why that's checked as minute-rounding, not a literal
/// seconds comparison).
pub fn clock_out(file_path: &str, target: &HeadingTarget, index_manager: &IndexManager, db: &Mutex<Connection>) -> Result<(), String> {
    let target_path = PathBuf::from(file_path);
    // Same basis as every other write: one read of the file as it is now,
    // and the heading found by identity in it (see
    // heading_utils::open_edit). A LOGBOOK entry spliced into the wrong
    // heading is as lost as any other misplaced write.
    let mut edit = heading_utils::open_edit(&target_path, target, &crate::core::errors::subject::heading())?;
    let heading = edit.heading().clone();
    let title = heading.title.clone();

    let open_idx = heading
        .clocks
        .iter()
        .position(|line| clock_utils::parse_clock_line(line).map(|entry| entry.end.is_none()).unwrap_or(false))
        .ok_or_else(|| crate::t!("Diese Aufgabe hat gerade keinen laufenden Timer.", "This task has no timer running right now."))?;
    let entry = clock_utils::parse_clock_line(&heading.clocks[open_idx]).ok_or_else(|| {
        crate::t!("Der laufende Timer-Eintrag konnte nicht gelesen werden.", "The running timer entry could not be read.")
    })?;
    let now = chrono::Local::now().naive_local();

    let mut clocks = heading.clocks.clone();
    // No 00:00 entries. Checked as "would the written duration round to
    // zero minutes", not against raw elapsed seconds: CLOCK timestamps are
    // minute-precise, so the recorded start has already lost its seconds
    // and a raw comparison is off by up to 59 depending on where in the
    // minute clock-in landed.
    if (now - entry.start).num_minutes() <= 0 {
        // A session under a minute isn't worth a permanent "=>  0:00"
        // line sitting in the LOGBOOK forever -- the still-open entry is
        // discarded outright rather than closed. Applies equally to an
        // explicit clock_out AND to clock_in's own
        // auto-close-the-other-one step (this function is the one place
        // both go through), so accidentally clocking in on the wrong
        // task and immediately switching never leaves a junk entry
        // behind either.
        clocks.remove(open_idx);
    } else {
        clocks[open_idx] = clock_utils::format_clock_line(entry.start, Some(now));
    }

    let payload = HeadingUpdatePayload { clocks: Some(clocks), ..Default::default() };

    let entry = HistoryEntryKind::ClockStopped { filename: filename_of(&target_path), title };
    vault_write::recorded(
        &target_path,
        |_| entry,
        index_manager,
        db,
        || {
            edit.write(&payload)?;
            edit.touch_ancestors()?;
            edit.commit()
        },
    )
}

/// Starts an open CLOCK entry, closing whatever else was running first --
/// exactly one timer runs globally, derived fresh from the index rather
/// than a persistent flag.
///
/// Resolves the target by identity rather than reusing its `line_no` after
/// that auto-close: closing an earlier heading in the same file can change
/// its line count, since a too-short entry takes the whole :LOGBOOK:
/// drawer with it, shifting everything below. Captured before the close,
/// resolved after.
pub fn clock_in(file_path: &str, target: &HeadingTarget, index_manager: &IndexManager, db: &Mutex<Connection>) -> Result<(), String> {
    let target_path = PathBuf::from(file_path);

    // Stopping the other timer first can shift this heading's line (same
    // file, above it) -- which is exactly why the parse and the lookup
    // below happen AFTERWARDS, against the file as it then is. TARGET
    // survives that shift on its own, it names the heading by CUSTOM_ID.
    if let Some((active_path, active_target)) = index_manager.find_active_clock() {
        if !(active_path == file_path && active_target.custom_id == target.custom_id && active_target.line_no == target.line_no) {
            clock_out(&active_path, &active_target, index_manager, db)?;
        }
    }

    let mut edit = heading_utils::open_edit(&target_path, target, &crate::core::errors::subject::heading())?;
    let heading = edit.heading().clone();
    let title = heading.title.clone();

    let now = chrono::Local::now().naive_local();
    let mut clocks = heading.clocks.clone();
    clocks.insert(0, clock_utils::format_clock_line(now, None));

    let payload = HeadingUpdatePayload { clocks: Some(clocks), ..Default::default() };

    let entry = HistoryEntryKind::ClockStarted { filename: filename_of(&target_path), title };
    vault_write::recorded(
        &target_path,
        |_| entry,
        index_manager,
        db,
        || {
            edit.write(&payload)?;
            edit.touch_ancestors()?;
            edit.commit()
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::heading_identity;
    use crate::core::history as history_mod;
    use crate::utils::fs_utils;
    use std::sync::Mutex;
    use tempfile::TempDir;

    /// The target a real caller sends -- line number plus the identity
    /// that outlives it (see heading_identity::target_of).
    fn target_at(index: &IndexManager, path: &std::path::Path) -> HeadingTarget {
        heading_identity::target_of(&index.get_headings_or_parse(path)[0])
    }

    fn setup() -> (TempDir, IndexManager, Mutex<Connection>) {
        let dir = TempDir::new().unwrap();
        let index = IndexManager::new();
        let conn = Connection::open_in_memory().unwrap();
        history_mod::store::init_schema(&conn).unwrap();
        (dir, index, Mutex::new(conn))
    }

    /// The timer is written into the file, so which heading it belongs to
    /// is decided against the file -- not against an index that a sync
    /// pull or an external edit may have left behind. Otherwise the
    /// LOGBOOK entry lands in whatever moved onto the remembered line.
    #[test]
    fn clock_in_after_the_file_changed_behind_the_index_still_lands_on_its_own_heading() {
        let (dir, index, db) = setup();
        let path = dir.path().join("actions.org");
        std::fs::write(&path, "* TODO Meine Aufgabe\n:PROPERTIES:\n:CUSTOM_ID: h:mine\n:END:\n").unwrap();
        index.update_file(&path);
        let target = target_at(&index, &path);

        std::fs::write(
            &path,
            "* TODO Fremde Aufgabe\n:PROPERTIES:\n:CUSTOM_ID: h:other\n:END:\n\
             * TODO Meine Aufgabe\n:PROPERTIES:\n:CUSTOM_ID: h:mine\n:END:\n",
        )
        .unwrap();

        clock_in(&path.to_string_lossy(), &target, &index, &db).unwrap();

        let after = index.get_headings_or_parse(&path);
        let mine = after.iter().find(|h| h.title == "Meine Aufgabe").unwrap();
        let other = after.iter().find(|h| h.title == "Fremde Aufgabe").unwrap();
        assert_eq!(mine.clocks.len(), 1, "the timer went to the heading it was started on");
        assert!(other.clocks.is_empty(), "and not to the one that moved onto its old line");
    }

    #[test]
    fn clock_in_prepends_a_new_open_clock_entry() {
        let (dir, index, db) = setup();
        let path = dir.path().join("actions.org");
        std::fs::write(&path, "* TODO Write report\n").unwrap();
        index.update_file(&path);

        let path_str = path.to_string_lossy().into_owned();
        let target = target_at(&index, &path);

        clock_in(&path_str, &target, &index, &db).unwrap();

        let after = index.get_headings_or_parse(&path);
        assert_eq!(after[0].clocks.len(), 1);
        assert!(after[0].clocks[0].starts_with("CLOCK: ["));
        assert!(!after[0].clocks[0].contains("--"), "open entry has no end/duration yet");
    }

    #[test]
    fn clock_out_closes_the_open_entry_with_a_duration() {
        let (dir, index, db) = setup();
        let path = dir.path().join("actions.org");
        std::fs::write(&path, "* TODO Write report\n  :LOGBOOK:\n  CLOCK: [2020-01-01 Wed 10:00]\n  :END:\n").unwrap();
        index.update_file(&path);

        let path_str = path.to_string_lossy().into_owned();
        let target = target_at(&index, &path);
        assert_eq!(index.get_headings_or_parse(&path)[0].clocks.len(), 1, "sanity: the open entry parsed");

        clock_out(&path_str, &target, &index, &db).unwrap();

        let after = index.get_headings_or_parse(&path);
        assert_eq!(after[0].clocks.len(), 1, "same entry, now closed -- not a second line");
        assert!(after[0].clocks[0].contains("--"), "now has an end timestamp");
        assert!(after[0].clocks[0].contains("=>"), "now has a duration");
    }

    /// Writes a file with one heading whose open CLOCK entry started
    /// `minutes_ago`, counted from the current minute boundary rather than
    /// full-precision now.
    ///
    /// CLOCK timestamps are minute-granular, so anchoring to the truncated
    /// minute keeps "how many whole minutes passed" deterministic. A raw
    /// "N seconds ago" is not: 30 seconds ago can land in the previous
    /// minute purely by truncation.
    fn write_org_with_open_clock_started(path: &std::path::Path, minutes_ago: i64) {
        use chrono::Timelike;
        let now = chrono::Local::now().naive_local();
        let current_minute_start = now.with_second(0).unwrap().with_nanosecond(0).unwrap();
        let start = current_minute_start - chrono::Duration::minutes(minutes_ago);
        std::fs::write(path, format!("* TODO Task\n  :LOGBOOK:\n  {}\n  :END:\n", clock_utils::format_clock_line(start, None),)).unwrap();
    }

    #[test]
    fn clock_out_discards_a_session_started_within_the_current_minute() {
        let (dir, index, db) = setup();
        let path = dir.path().join("actions.org");
        write_org_with_open_clock_started(&path, 0);
        index.update_file(&path);
        let path_str = path.to_string_lossy().into_owned();
        let target = target_at(&index, &path);

        clock_out(&path_str, &target, &index, &db).unwrap();

        let after = index.get_headings_or_parse(&path);
        assert!(after[0].clocks.is_empty(), "clock-in immediately followed by clock-out leaves no 0:00 entry behind");
    }

    #[test]
    fn clock_out_keeps_a_session_at_exactly_the_minimum() {
        let (dir, index, db) = setup();
        let path = dir.path().join("actions.org");
        write_org_with_open_clock_started(&path, 1);
        index.update_file(&path);
        let path_str = path.to_string_lossy().into_owned();
        let target = target_at(&index, &path);

        clock_out(&path_str, &target, &index, &db).unwrap();

        let after = index.get_headings_or_parse(&path);
        assert_eq!(after[0].clocks.len(), 1, "exactly one minute is kept, not discarded");
        assert!(after[0].clocks[0].contains("--"), "closed with an end timestamp");
    }

    #[test]
    fn clock_out_keeps_a_session_comfortably_above_the_minimum() {
        let (dir, index, db) = setup();
        let path = dir.path().join("actions.org");
        write_org_with_open_clock_started(&path, 2);
        index.update_file(&path);
        let path_str = path.to_string_lossy().into_owned();
        let target = target_at(&index, &path);

        clock_out(&path_str, &target, &index, &db).unwrap();

        let after = index.get_headings_or_parse(&path);
        assert_eq!(after[0].clocks.len(), 1, "a two-minute session is kept as a real closed entry");
        assert!(after[0].clocks[0].contains("--"), "closed with an end timestamp");
        assert!(!after[0].clocks[0].contains("=>  0:00"), "not written as a zero-duration entry");
    }

    #[test]
    fn a_discarded_short_session_does_not_leave_an_empty_logbook_drawer() {
        // The write side (heading_utils::build_org_heading_block_inner)
        // only emits :LOGBOOK: when clocks or history is non-empty --
        // confirms discarding the sole clock entry doesn't leave a
        // dangling empty drawer on disk.
        let (dir, index, db) = setup();
        let path = dir.path().join("actions.org");
        write_org_with_open_clock_started(&path, 0);
        index.update_file(&path);
        let path_str = path.to_string_lossy().into_owned();
        let target = target_at(&index, &path);

        clock_out(&path_str, &target, &index, &db).unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains(":LOGBOOK:"), "no empty drawer left behind:\n{raw}");
    }

    #[test]
    fn clock_out_fails_with_no_active_clock_on_that_heading() {
        let (dir, index, db) = setup();
        let path = dir.path().join("actions.org");
        std::fs::write(&path, "* TODO Write report\n").unwrap();
        index.update_file(&path);

        let path_str = path.to_string_lossy().into_owned();
        let target = target_at(&index, &path);

        assert!(clock_out(&path_str, &target, &index, &db).is_err());
    }

    #[test]
    fn clock_in_on_a_different_heading_auto_stops_the_previously_running_one_same_file() {
        let (dir, index, db) = setup();
        let path = dir.path().join("actions.org");
        std::fs::write(&path, "* TODO Task A\n* TODO Task B\n").unwrap();
        index.update_file(&path);
        let path_str = path.to_string_lossy().into_owned();

        let by_title =
            |title: &str| heading_identity::target_of(index.get_headings_or_parse(&path).iter().find(|h| h.title == title).unwrap());
        clock_in(&path_str, &by_title("Task A"), &index, &db).unwrap();
        clock_in(&path_str, &by_title("Task B"), &index, &db).unwrap();

        let after = index.get_headings_or_parse(&path);
        let a = after.iter().find(|h| h.title == "Task A").unwrap();
        let b = after.iter().find(|h| h.title == "Task B").unwrap();
        // Auto-closed within the same test run/minute, so it rounds down
        // to zero minutes -- discarded, not closed with "=>  0:00".
        assert!(a.clocks.is_empty(), "Task A's near-instant timer was auto-closed AND discarded, not kept as a 0:00 entry");
        assert!(!b.clocks[0].contains("--"), "Task B's timer is the one still running");
    }

    #[test]
    fn clock_in_on_a_different_heading_auto_stops_the_previously_running_one_cross_file() {
        let (dir, index, db) = setup();
        let projects_path = dir.path().join("projects.org");
        let actions_path = dir.path().join("actions.org");
        std::fs::write(&projects_path, "* Project :project:\n** TODO Subtask\n").unwrap();
        std::fs::write(&actions_path, "* TODO Standalone action\n").unwrap();
        index.update_file(&projects_path);
        index.update_file(&actions_path);

        let projects_str = projects_path.to_string_lossy().into_owned();
        let actions_str = actions_path.to_string_lossy().into_owned();

        let subtask =
            heading_identity::target_of(index.get_headings_or_parse(&projects_path).iter().find(|h| h.title == "Subtask").unwrap());
        clock_in(&projects_str, &subtask, &index, &db).unwrap();

        let action = heading_identity::target_of(
            index.get_headings_or_parse(&actions_path).iter().find(|h| h.title == "Standalone action").unwrap(),
        );
        clock_in(&actions_str, &action, &index, &db).unwrap();

        let subtask_after = index.get_headings_or_parse(&projects_path);
        let subtask = subtask_after.iter().find(|h| h.title == "Subtask").unwrap();
        assert!(subtask.clocks.is_empty(), "Subtask's near-instant timer (a different FILE) was auto-closed AND discarded too");

        let action_after = index.get_headings_or_parse(&actions_path);
        let action = action_after.iter().find(|h| h.title == "Standalone action").unwrap();
        assert!(!action.clocks[0].contains("--"), "the newly-started timer is the one still running");

        assert!(index.find_active_clock().is_some());
        let (active_path, active_target) = index.find_active_clock().unwrap();
        assert_eq!(active_path, fs_utils::canonicalize_or_self(&actions_path).to_string_lossy());
        assert_eq!(active_target.line_no, action.line_no);
        assert_eq!(active_target.title.as_deref(), Some("Standalone action"));
    }

    #[test]
    fn clock_in_on_the_already_running_heading_does_not_close_and_restart_it() {
        let (dir, index, db) = setup();
        let path = dir.path().join("actions.org");
        std::fs::write(&path, "* TODO Task A\n").unwrap();
        index.update_file(&path);
        let path_str = path.to_string_lossy().into_owned();

        let target = target_at(&index, &path);
        clock_in(&path_str, &target, &index, &db).unwrap();
        clock_in(&path_str, &target, &index, &db).unwrap();

        let after = index.get_headings_or_parse(&path);
        assert_eq!(after[0].clocks.len(), 2, "a second clock-in just starts a new entry, doesn't touch the first");
    }
}
