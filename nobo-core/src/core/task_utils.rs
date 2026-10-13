use crate::core::heading_utils;
use crate::core::history::record_best_effort;
use crate::core::indexer::IndexManager;
use crate::core::models::{
    has_project_tag, property_value, CategoryTimeTotal, FileSnapshot, Heading, HistoryEntryKind, ProgressStats, TimeTrackingSummary,
    FINISHED_STATES, ORG_CATEGORY_PROPERTY,
};
use crate::utils::clock_utils;
use crate::utils::fs_utils;
use crate::utils::ToCmdResult;
use chrono::NaiveDate;
use rusqlite::Connection;
use std::collections::HashMap;
use std::num::NonZero;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Extracts ARCHIVABLE line RANGES out of LINES, appends them to a sibling
/// `<file>_archive` file and updates the index.
/// Shared by `archive_done_tasks` and `archive_done_projects`, which only
/// differ in how they compute the ranges to archive.
///
/// LINES and RANGES must come from the same read (`read_and_parse`): the
/// ranges are line numbers, and re-reading the file here would let them
/// point into a version that has moved on -- archiving whatever happens
/// to occupy those lines now.
pub fn archive_ranges(
    target_path: &Path,
    lines: &[String],
    ranges: Vec<(usize, usize)>,
    index_manager: &IndexManager,
    db: &Mutex<Connection>,
) -> Result<usize, String> {
    if ranges.is_empty() {
        return Ok(0);
    }

    // The same lines the ranges were computed from, so the undo snapshot
    // describes exactly what this write replaced. Reconstructed rather
    // than re-read: `read_lines` drops the trailing newline and every
    // write puts one back, so a re-read `before` would make each undo
    // churn the file by one byte.
    let before_target = Some(fs_utils::lines_to_content(lines));
    let archive_file_path = PathBuf::from(format!("{}_archive", target_path.display()));
    let before_archive = std::fs::read_to_string(&archive_file_path).ok();

    let (keep_content, archived_content) = fs_utils::extract_line_ranges(lines, &ranges);

    // Grow the archive first, shorten the source second.
    //
    // Either write can fail -- a full disk, a permission revoked mid-run,
    // a vault on a drive that just went away -- and the order is what
    // decides the cost of that. This way the worst case is a task that
    // exists twice, which is visible in the app and fixable by hand. The
    // other way round it is a task that exists nowhere.
    fs_utils::append_to_file(&archive_file_path, &archived_content).to_cmd()?;
    let shortened = fs_utils::write_atomic_checked(target_path, &keep_content);

    index_manager.update_file(target_path);

    // Recorded before that failure is propagated, and describing what is
    // on disk NOW rather than what was intended. A bare `?` on the write
    // above would skip this entirely and produce the one state with no way
    // back: the archive already grown, and nothing in the history saying
    // it happened.
    record_best_effort(
        db,
        HistoryEntryKind::Archived { filename: fs_utils::filename_of(target_path), count: ranges.len() as u32 },
        false,
        vec![
            FileSnapshot {
                path: target_path.to_string_lossy().into_owned(),
                before: before_target,
                after: if shortened.is_ok() { Some(keep_content) } else { std::fs::read_to_string(target_path).ok() },
            },
            FileSnapshot {
                path: archive_file_path.to_string_lossy().into_owned(),
                before: before_archive,
                after: std::fs::read_to_string(&archive_file_path).ok(),
            },
        ],
    );

    shortened.to_cmd()?;
    Ok(ranges.len())
}

pub fn is_older_than_n_days(closed: &Option<String>, n_days: u32) -> bool {
    if let Some(c) = closed {
        let cleaned = c.trim().trim_start_matches('[');
        // `get` rather than `&cleaned[0..10]`: `len()` counts bytes, so a
        // CLOSED value that is not a timestamp at all can put a character
        // boundary inside the slice and panic. Same shape as the Denote
        // identifier in note_utils, and with `panic = "abort"` the same
        // cost.
        if let Some(date_str) = cleaned.get(0..10) {
            if let Ok(naive_date) = NaiveDate::parse_from_str(date_str, "%Y-%m-%d") {
                let now = chrono::Local::now().date_naive();
                // Inclusive: "archive after 7 days" means on the seventh
                // day, not on the eighth. It was strictly `>` before,
                // which is a day nobody counts -- an entry finished a
                // week ago sat there while the button offered to archive
                // it and then did not.
                return now.signed_duration_since(naive_date).num_days() >= n_days as i64;
            }
        }
        false
    } else {
        true
    }
}

/// Two rules, both about never archiving something unfinished:
///
/// 1. Any finished state counts, not just the literal `DONE` -- an action
///    marked "won't do" is finished too.
/// 2. Only top-level headings are independently archivable, and only once
///    their entire subtree is finished as well. Otherwise a finished
///    subtask gets orphaned out from under its still-open parent, and a
///    finished parent sweeps an unfinished subtask along with it.
pub fn find_archivable_ranges(flat_headings: &[Heading], archive_after_days: u32) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut i = 0;

    while i < flat_headings.len() {
        let heading = &flat_headings[i];

        if heading.level != 1 {
            i += 1;
            continue;
        }

        let mut j = i + 1;
        while j < flat_headings.len() && flat_headings[j].level > heading.level {
            j += 1;
        }

        // progress_stats.done == .total means every STATED descendant in
        // this exact subtree is also finished; `None` means there were no
        // stated descendants at all (a plain leaf action with no
        // subtasks), which is vacuously "fully done" here -- unlike
        // find_archivable_project_ranges's own check, where a PROJECT with
        // zero subtasks isn't really a project to judge completion by, a
        // plain action with zero subtasks is just... a normal, done action.
        let subtree_fully_finished = heading.progress_stats.map(|stats| stats.done == stats.total.get()).unwrap_or(true);

        if is_finished(heading.state.as_deref()) && subtree_fully_finished && is_older_than_n_days(&heading.closed, archive_after_days) {
            ranges.push((heading.line_no, flat_headings[j - 1].last_line));
        }

        i = j;
    }

    ranges
}

pub fn is_finished(state: Option<&str>) -> bool {
    state.is_some_and(|s| FINISHED_STATES.contains(&s))
}

/// Percentage of children of heading at IDX of FLAT with finished state.
pub fn subtree_progress(flat: &[Heading], idx: usize) -> Option<ProgressStats> {
    let level = flat[idx].level;
    let mut total = 0u32;
    let mut done = 0u32;

    for h in flat.iter().skip(idx + 1) {
        if h.level <= level {
            break;
        }
        if h.state.is_some() {
            total += 1;
            if is_finished(h.state.as_deref()) {
                done += 1;
            }
        }
    }

    if total == 0 {
        return None;
    }
    Some(ProgressStats { done, total: NonZero::new(total)? })
}

/// Set `progress_stats` on all headings in `flat`.
pub fn annotate_progress(flat: &mut [Heading]) {
    for i in 0..flat.len() {
        flat[i].progress_stats = subtree_progress(flat, i);
    }
}

/// Sums a heading's own `clocks` (closed entries only -- a still-running
/// one doesn't count until it's stopped, see ClockEntry::duration_minutes),
/// `None` if it has none at all.
fn own_clocked_minutes(heading: &Heading) -> Option<u32> {
    let mut total: i64 = 0;
    let mut any = false;
    for line in &heading.clocks {
        if let Some(minutes) = clock_utils::parse_clock_line(line).and_then(|e| e.duration_minutes()) {
            total += minutes;
            any = true;
        }
    }
    if !any {
        return None;
    }
    Some(total.max(0) as u32)
}

/// Sets `own_clocked_minutes`/`total_clocked_minutes` on all headings in
/// `flat` -- exact precedent: annotate_progress/subtree_progress above,
/// same two-pass shape (own value first, then a forward scan per heading
/// summing every descendant's already-computed own value, stopping at the
/// first heading whose level drops back to or below the subtree root's).
pub fn annotate_clocked_time(flat: &mut [Heading]) {
    for i in 0..flat.len() {
        flat[i].own_clocked_minutes = own_clocked_minutes(&flat[i]);
    }
    for i in 0..flat.len() {
        let level = flat[i].level;
        let mut total = flat[i].own_clocked_minutes.unwrap_or(0);
        for h in flat[i + 1..].iter() {
            if h.level <= level {
                break;
            }
            total += h.own_clocked_minutes.unwrap_or(0);
        }
        flat[i].total_clocked_minutes = if total > 0 { Some(total) } else { None };
    }
}

/// subtree_progress's own DONE/total counting, but treating any heading
/// present in OVERLAY (keyed by line_no) as having that overridden state
/// instead of its own captured one (a `None` entry means "this batch
/// clears its state") -- used by heading_utils::apply_bulk_update so an
/// ancestor's LAST_ACTIVITY-only touch gets a progress cookie reflecting
/// the batch's own PENDING state changes, not what was on disk before the
/// batch started. Needed because a bulk update's descendant edits and its
/// ancestor touches all happen against the SAME in-memory LINES with no
/// reparse in between (that's the whole point of it being one batched
/// pass, see apply_bulk_update's own doc comment) -- so, unlike a
/// single-item edit (see heading_utils::touch_ancestors, which CAN just
/// reread the file since its own primary edit is already written by the
/// time it runs), there's no freshly-parsed FLAT to read an up-to-date
/// state from; this reconstructs the equivalent from FLAT (still the
/// pre-batch parse) plus the pending updates themselves.
pub fn subtree_progress_with_overlay(
    flat: &[Heading],
    idx: usize,
    overlay: &std::collections::HashMap<usize, Option<String>>,
) -> Option<ProgressStats> {
    let level = flat[idx].level;
    let mut total = 0u32;
    let mut done = 0u32;

    for h in flat.iter().skip(idx + 1) {
        if h.level <= level {
            break;
        }
        let state = overlay.get(&h.line_no).cloned().unwrap_or_else(|| h.state.clone());
        if let Some(s) = state {
            total += 1;
            if is_finished(Some(&s)) {
                done += 1;
            }
        }
    }

    if total == 0 {
        return None;
    }
    Some(ProgressStats { done, total: NonZero::new(total)? })
}

/// subtree_progress's own DONE/total counting, but skipping every heading
/// whose own index (into FLAT) is in REMOVED -- the deletion counterpart to
/// subtree_progress_with_overlay above, used by
/// heading_utils::apply_bulk_delete so a still-surviving ancestor's
/// LAST_ACTIVITY-only touch reflects the batch's own pending removals
/// (descendants disappearing, not changing state) rather than counting
/// headings that are about to be removed from LINES in the very same pass.
pub fn subtree_progress_excluding(flat: &[Heading], idx: usize, removed: &std::collections::BTreeSet<usize>) -> Option<ProgressStats> {
    let level = flat[idx].level;
    let mut total = 0u32;
    let mut done = 0u32;

    for (h_idx, h) in flat.iter().enumerate().skip(idx + 1) {
        if h.level <= level {
            break;
        }
        if removed.contains(&h_idx) {
            continue;
        }
        if let Some(s) = h.state.as_deref() {
            total += 1;
            if is_finished(Some(s)) {
                done += 1;
            }
        }
    }

    if total == 0 {
        return None;
    }
    Some(ProgressStats { done, total: NonZero::new(total)? })
}

/// Line ranges of all projects with all children finished.
/// Projects without any children won't be archived.
/// Make sure to have set progress_stats (by caling annotate_progress before)
pub fn find_archivable_project_ranges(flat: &[Heading]) -> Vec<(usize, usize)> {
    flat.iter()
        .enumerate()
        .filter(|(_, h)| h.level == 1 && has_project_tag(&h.tags) && h.progress_stats.is_some_and(|stats| stats.done == stats.total.get()))
        .map(|(idx, _)| heading_utils::subtree_line_range(flat, idx))
        .collect()
}

/// Vault-wide time-tracking rollup, grouped by :CATEGORY: (matched
/// case-insensitively) -- iterates ONLY level-1 headings (projects in
/// Projects.org, standalone actions in Actions.org) across the full index,
/// each already carrying its own rolled-up `total_clocked_minutes` (own +
/// every descendant, see annotate_clocked_time), so a project's subtasks'
/// clocked time is counted through their project, never double-counted by
/// also being iterated here individually. Headings with no category at all
/// bucket into `category: None` -- the fallback bucket for uncategorized
/// time.
pub fn time_tracking_summary(headings_by_file: &HashMap<String, Vec<Heading>>) -> TimeTrackingSummary {
    let mut totals: HashMap<Option<String>, u32> = HashMap::new();
    let mut grand_total_minutes = 0u32;

    for headings in headings_by_file.values() {
        for heading in headings.iter().filter(|h| h.level == 1) {
            let minutes = heading.total_clocked_minutes.unwrap_or(0);
            if minutes == 0 {
                continue;
            }
            let category =
                property_value(&heading.properties, ORG_CATEGORY_PROPERTY).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());

            *totals.entry(category).or_insert(0) += minutes;
            grand_total_minutes += minutes;
        }
    }

    let mut by_category: Vec<CategoryTimeTotal> =
        totals.into_iter().map(|(category, total_minutes)| CategoryTimeTotal { category, total_minutes }).collect();
    by_category.sort_by(|a, b| a.category.cmp(&b.category));

    TimeTrackingSummary { by_category, grand_total_minutes }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::history;
    use tempfile::TempDir;

    /* --- archive_ranges --- */

    /// archive_ranges is the actual disk-writing function shared by
    /// archive_done_tasks/archive_done_projects/archive_single_project --
    /// its own range-finding helpers (find_archivable_ranges etc., tested
    /// further down) already prove a sibling isn't accidentally INCLUDED in
    /// a range, but nothing exercised the write itself: this proves that
    /// once a range is archived, everything else in the source file is
    /// still there byte-for-byte, and the _archive file only received the
    /// archived range, not a copy of the whole document.
    #[test]
    fn archive_ranges_moves_only_the_given_range_and_leaves_the_rest_of_the_file_untouched() {
        let dir = TempDir::new().unwrap();
        let index = IndexManager::new();
        let conn = Connection::open_in_memory().unwrap();
        history::store::init_schema(&conn).unwrap();
        let db = Mutex::new(conn);

        let path = dir.path().join("actions.org");
        std::fs::write(
            &path,
            "* DONE Old finished task\n:PROPERTIES:\n:CLOSED: [2020-01-01 Wed]\n:END:\n\
             * TODO Still open task\nSome notes here.\n\
             * TODO Another open task\n",
        )
        .unwrap();

        let (lines, flat) = heading_utils::read_and_parse(&path).unwrap();
        let idx = flat.iter().position(|h| h.title == "Old finished task").unwrap();
        let range = heading_utils::subtree_line_range(&flat, idx);

        let count = archive_ranges(&path, &lines, vec![range], &index, &db).unwrap();
        assert_eq!(count, 1);

        let remaining = std::fs::read_to_string(&path).unwrap();
        assert!(!remaining.contains("Old finished task"), "archived heading removed from the source file");
        assert!(remaining.contains("Still open task"), "unrelated open task untouched");
        assert!(remaining.contains("Some notes here."), "its own body content untouched");
        assert!(remaining.contains("Another open task"), "unrelated second open task untouched");

        let archive_path = format!("{}_archive", path.display());
        let archived = std::fs::read_to_string(&archive_path).unwrap();
        assert!(archived.contains("Old finished task"), "archived heading actually landed in the _archive file");
        assert!(!archived.contains("Still open task"), "the archive file only got the archived range, not the rest of the source file");

        let conn = db.lock().unwrap();
        let entries = history::list_for_path(&conn, &path.to_string_lossy(), 10).unwrap();
        assert_eq!(entries.len(), 1, "exactly one history entry for the archive action");
    }

    /// The order of the two writes, pinned.
    ///
    /// Archiving is one move made of two writes, and one of them can fail
    /// on its own -- a full disk, a permission revoked mid-run, a vault on
    /// a drive that just went away. This makes the archive side fail (it
    /// is a directory, so it can be neither read nor replaced) and asserts
    /// the source still has everything.
    ///
    /// Written the other way round -- source first, archive second, which
    /// is how it used to be -- the source is already shortened by the time
    /// the archive write fails, and the tasks exist nowhere. The `?` on
    /// that first write also skipped `record_best_effort`, so there was no
    /// history entry to undo either.
    #[test]
    fn a_failing_archive_write_leaves_every_task_in_the_source_file() {
        let dir = TempDir::new().unwrap();
        let index = IndexManager::new();
        let conn = Connection::open_in_memory().unwrap();
        history::store::init_schema(&conn).unwrap();
        let db = Mutex::new(conn);

        let path = dir.path().join("actions.org");
        let original = "* DONE Old finished task\n:PROPERTIES:\n:CLOSED: [2020-01-01 Wed]\n:END:\n\
             * TODO Still open task\n";
        std::fs::write(&path, original).unwrap();
        std::fs::create_dir(format!("{}_archive", path.display())).unwrap();

        let (lines, flat) = heading_utils::read_and_parse(&path).unwrap();
        let idx = flat.iter().position(|h| h.title == "Old finished task").unwrap();
        let range = heading_utils::subtree_line_range(&flat, idx);

        let result = archive_ranges(&path, &lines, vec![range], &index, &db);

        assert!(result.is_err(), "a failed archive write must be reported, not swallowed");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            original,
            "the source file must still hold the task the archive never received"
        );
    }

    /// Baut ein minimales Heading für Tests, mit sinnvollen Defaults für
    /// alle Felder, die für die jeweiligen Tests hier nicht relevant sind.
    fn make_heading(line_no: usize, last_line: usize, level: usize, state: Option<&str>, closed: Option<&str>) -> Heading {
        Heading {
            id: format!("test:{}", line_no),
            file_path: "test.org".to_string(),
            line_no,
            last_line,
            level,
            state: state.map(String::from),
            title: format!("Task {}", line_no),
            closed: closed.map(String::from),
            ..Default::default()
        }
    }

    /* --- is_older_than_n_days --- */

    #[test]
    fn none_closed_counts_as_older() {
        // Keine CLOSED-Angabe -> gilt als archivierbar (siehe else-Zweig).
        assert!(is_older_than_n_days(&None, 7));
    }

    #[test]
    fn recent_date_is_not_older() {
        let today = chrono::Local::now().date_naive().format("%Y-%m-%d").to_string();
        let closed = Some(format!("{} Mon 10:00", today));
        assert!(!is_older_than_n_days(&closed, 7));
    }

    #[test]
    fn date_far_in_the_past_is_older() {
        let closed = Some("2000-01-01 Sa 10:00".to_string());
        assert!(is_older_than_n_days(&closed, 7));
    }

    #[test]
    fn date_exactly_at_threshold_counts() {
        // Der Tag selbst zählt: "nach 5 Tagen" heißt am fünften, nicht am
        // sechsten.
        let n_days = 5;
        let boundary_date = chrono::Local::now().date_naive() - chrono::Duration::days(n_days as i64);
        let closed = Some(format!("{} Mon 10:00", boundary_date.format("%Y-%m-%d")));
        assert!(is_older_than_n_days(&closed, n_days));
    }

    #[test]
    fn day_before_the_threshold_does_not_count() {
        let n_days = 5;
        let date = chrono::Local::now().date_naive() - chrono::Duration::days(n_days as i64 - 1);
        let closed = Some(format!("{} Mon 10:00", date.format("%Y-%m-%d")));
        assert!(!is_older_than_n_days(&closed, n_days));
    }

    #[test]
    fn one_day_past_threshold_is_older() {
        let n_days = 5;
        let past_date = chrono::Local::now().date_naive() - chrono::Duration::days(n_days as i64 + 1);
        let closed = Some(format!("{} Mon 10:00", past_date.format("%Y-%m-%d")));
        assert!(is_older_than_n_days(&closed, n_days));
    }

    #[test]
    fn leading_bracket_is_stripped() {
        let closed = Some("[2000-01-01 Sa 10:00]".to_string());
        assert!(is_older_than_n_days(&closed, 7));
    }

    #[test]
    fn unparseable_date_returns_false() {
        let closed = Some("not-a-date".to_string());
        assert!(!is_older_than_n_days(&closed, 7));
    }

    #[test]
    fn too_short_string_returns_false() {
        let closed = Some("2025".to_string());
        assert!(!is_older_than_n_days(&closed, 7));
    }

    /* --- find_archivable_ranges --- */

    #[test]
    fn empty_input_returns_no_ranges() {
        assert!(find_archivable_ranges(&[], 7).is_empty());
    }

    #[test]
    fn non_done_heading_is_ignored() {
        let headings = vec![make_heading(1, 3, 1, Some("TODO"), Some("2000-01-01 Sa 10:00"))];
        assert!(find_archivable_ranges(&headings, 7).is_empty());
    }

    #[test]
    fn done_but_recent_is_not_archived() {
        let today = chrono::Local::now().date_naive().format("%Y-%m-%d").to_string();
        let headings = vec![make_heading(1, 3, 1, Some("DONE"), Some(&format!("{} Mon 10:00", today)))];
        assert!(find_archivable_ranges(&headings, 7).is_empty());
    }

    #[test]
    fn done_and_old_is_archived_with_own_line_range() {
        let headings = vec![make_heading(1, 3, 1, Some("DONE"), Some("2000-01-01 Sa 10:00"))];
        let ranges = find_archivable_ranges(&headings, 7);
        assert_eq!(ranges, vec![(1, 3)]);
    }

    #[test]
    fn archivable_range_includes_all_child_headings_once_they_are_also_finished() {
        // Parent DONE+alt, Kind (höheres Level) ist ebenfalls fertig (WONT-DO,
        // Teil von FINISHED_STATES) -> ganzer Range wird archiviert.
        let mut headings =
            vec![make_heading(1, 2, 1, Some("DONE"), Some("2000-01-01 Sa 10:00")), make_heading(3, 5, 2, Some("WONT-DO"), None)];
        annotate_progress(&mut headings);
        let ranges = find_archivable_ranges(&headings, 7);
        // end_line kommt vom letzten Kind (last_line = 5), nicht vom Parent selbst (last_line = 2).
        assert_eq!(ranges, vec![(1, 5)]);
    }

    /// A finished top-level heading must not sweep a still-open subtask
    /// into the archive with it.
    #[test]
    fn a_done_parent_with_a_still_open_child_is_not_archived_at_all() {
        let mut headings = vec![
            make_heading(1, 2, 1, Some("DONE"), Some("2000-01-01 Sa 10:00")),
            make_heading(3, 5, 2, Some("TODO"), None), // still open -> blocks the whole subtree
        ];
        annotate_progress(&mut headings);
        let ranges = find_archivable_ranges(&headings, 7);
        assert!(ranges.is_empty());
    }

    #[test]
    fn sibling_of_same_level_is_not_included_in_range() {
        let mut headings = vec![
            make_heading(1, 2, 1, Some("DONE"), Some("2000-01-01 Sa 10:00")),
            make_heading(3, 4, 1, Some("TODO"), None), // gleiches Level -> kein Kind
        ];
        annotate_progress(&mut headings);
        let ranges = find_archivable_ranges(&headings, 7);
        assert_eq!(ranges, vec![(1, 2)]);
    }

    #[test]
    fn multiple_archivable_headings_produce_multiple_ranges() {
        let mut headings = vec![
            make_heading(1, 2, 1, Some("DONE"), Some("2000-01-01 Sa 10:00")),
            make_heading(3, 4, 1, Some("DONE"), Some("2001-01-01 Mo 10:00")),
        ];
        annotate_progress(&mut headings);
        let ranges = find_archivable_ranges(&headings, 7);
        assert_eq!(ranges, vec![(1, 2), (3, 4)]);
    }

    /// A finished subtask inside a still-open parent must stay where it
    /// is -- only top-level headings are independently archivable.
    #[test]
    fn a_done_child_inside_a_still_open_parent_is_never_independently_archived() {
        let mut headings =
            vec![make_heading(1, 4, 1, Some("TODO"), None), make_heading(2, 2, 2, Some("DONE"), Some("2000-01-01 Sa 10:00"))];
        annotate_progress(&mut headings);
        let ranges = find_archivable_ranges(&headings, 7);
        assert!(ranges.is_empty());
    }

    /// Every finished state archives, not just the literal `DONE` -- an
    /// action marked "won't do" is finished too.
    #[test]
    fn a_wont_do_leaf_action_is_archivable_too_not_just_literal_done() {
        let mut headings = vec![make_heading(1, 1, 1, Some("WONT-DO"), Some("2000-01-01 Sa 10:00"))];
        annotate_progress(&mut headings);
        let ranges = find_archivable_ranges(&headings, 7);
        assert_eq!(ranges, vec![(1, 1)]);
    }

    /* --- subtree_progress_with_overlay --- */

    #[test]
    fn overlay_progress_uses_the_pending_new_state_not_the_captured_one() {
        let flat = vec![
            make_heading(1, 3, 1, None, None),
            make_heading(2, 2, 2, Some("TODO"), None), // line_no 2 -- about to become DONE
            make_heading(3, 3, 2, Some("TODO"), None),
        ];
        let mut overlay = std::collections::HashMap::new();
        overlay.insert(2, Some("DONE".to_string()));

        assert_eq!(
            subtree_progress_with_overlay(&flat, 0, &overlay),
            Some(ProgressStats { done: 1, total: NonZero::new(2).unwrap() }),
            "the overlaid heading counts as DONE even though FLAT itself still says TODO"
        );
    }

    #[test]
    fn overlay_progress_none_entry_means_the_pending_change_clears_the_state() {
        let flat = vec![make_heading(1, 2, 1, None, None), make_heading(2, 2, 2, Some("DONE"), None)];
        let mut overlay = std::collections::HashMap::new();
        overlay.insert(2, None); // payload's own "clear the state" sentinel

        assert_eq!(
            subtree_progress_with_overlay(&flat, 0, &overlay),
            None,
            "a heading whose pending state is cleared no longer counts toward total either"
        );
    }

    #[test]
    fn overlay_progress_leaves_untouched_descendants_at_their_captured_state() {
        let flat = vec![
            make_heading(1, 3, 1, None, None),
            make_heading(2, 2, 2, Some("DONE"), None),
            make_heading(3, 3, 2, Some("TODO"), None), // not in the overlay at all
        ];
        let overlay = std::collections::HashMap::new();

        assert_eq!(subtree_progress_with_overlay(&flat, 0, &overlay), Some(ProgressStats { done: 1, total: NonZero::new(2).unwrap() }));
    }

    /* --- subtree_progress_excluding --- */

    #[test]
    fn excluding_progress_skips_a_descendant_about_to_be_deleted() {
        let flat = vec![
            make_heading(1, 3, 1, None, None),
            make_heading(2, 2, 2, Some("DONE"), None), // idx 1 -- about to be removed
            make_heading(3, 3, 2, Some("TODO"), None),
        ];
        let mut removed = std::collections::BTreeSet::new();
        removed.insert(1);

        assert_eq!(
            subtree_progress_excluding(&flat, 0, &removed),
            Some(ProgressStats { done: 0, total: NonZero::new(1).unwrap() }),
            "the DONE descendant about to be deleted no longer counts toward either done or total"
        );
    }

    #[test]
    fn excluding_progress_is_none_once_every_descendant_is_removed() {
        let flat = vec![make_heading(1, 2, 1, None, None), make_heading(2, 2, 2, Some("DONE"), None)];
        let mut removed = std::collections::BTreeSet::new();
        removed.insert(1);

        assert_eq!(subtree_progress_excluding(&flat, 0, &removed), None);
    }

    /* --- annotate_clocked_time --- */

    fn heading_with_clocks(line_no: usize, level: usize, clocks: Vec<&str>) -> Heading {
        let mut h = make_heading(line_no, line_no, level, None, None);
        h.clocks = clocks.into_iter().map(String::from).collect();
        h
    }

    #[test]
    fn own_and_total_match_for_a_leaf_with_no_children() {
        let mut flat = vec![heading_with_clocks(1, 1, vec!["CLOCK: [2026-08-17 Mon 13:00]--[2026-08-17 Mon 14:30] =>  1:30"])];
        annotate_clocked_time(&mut flat);
        assert_eq!(flat[0].own_clocked_minutes, Some(90));
        assert_eq!(flat[0].total_clocked_minutes, Some(90));
    }

    #[test]
    fn sums_multiple_closed_entries_on_the_same_heading() {
        let mut flat = vec![heading_with_clocks(
            1,
            1,
            vec![
                "CLOCK: [2026-08-17 Mon 13:00]--[2026-08-17 Mon 14:30] =>  1:30",
                "CLOCK: [2026-08-16 Sun 09:00]--[2026-08-16 Sun 09:45] =>  0:45",
            ],
        )];
        annotate_clocked_time(&mut flat);
        assert_eq!(flat[0].own_clocked_minutes, Some(135));
    }

    #[test]
    fn an_open_running_entry_does_not_count_toward_own_minutes() {
        let mut flat = vec![heading_with_clocks(
            1,
            1,
            vec!["CLOCK: [2026-08-17 Mon 13:00]--[2026-08-17 Mon 14:30] =>  1:30", "CLOCK: [2026-08-17 Mon 18:00]"],
        )];
        annotate_clocked_time(&mut flat);
        assert_eq!(flat[0].own_clocked_minutes, Some(90), "only the closed entry counts");
    }

    #[test]
    fn own_clocked_minutes_is_none_when_a_heading_has_no_clocks_at_all() {
        let mut flat = vec![heading_with_clocks(1, 1, vec![])];
        annotate_clocked_time(&mut flat);
        assert_eq!(flat[0].own_clocked_minutes, None);
        assert_eq!(flat[0].total_clocked_minutes, None);
    }

    #[test]
    fn total_rolls_up_from_descendants_all_the_way_to_the_root() {
        // Project(L1) > Task A(L2, 30m) > Subtask(L3, 15m), Task B(L2, no clock)
        let mut flat = vec![
            heading_with_clocks(1, 1, vec![]),
            heading_with_clocks(2, 2, vec!["CLOCK: [2026-08-17 Mon 09:00]--[2026-08-17 Mon 09:30] =>  0:30"]),
            heading_with_clocks(3, 3, vec!["CLOCK: [2026-08-17 Mon 10:00]--[2026-08-17 Mon 10:15] =>  0:15"]),
            heading_with_clocks(4, 2, vec![]),
        ];
        annotate_clocked_time(&mut flat);

        assert_eq!(flat[0].total_clocked_minutes, Some(45), "project rolls up both descendants' time");
        assert_eq!(flat[1].total_clocked_minutes, Some(45), "Task A rolls up its own 30m + Subtask's 15m");
        assert_eq!(flat[2].total_clocked_minutes, Some(15), "Subtask is a leaf, own == total");
        assert_eq!(flat[3].total_clocked_minutes, None, "Task B has no clocked time anywhere in its subtree");
    }

    #[test]
    fn siblings_and_unrelated_subtrees_are_left_untouched() {
        // Project A(L1, 10m) has no children; Project B(L1) has Subtask(L2, 20m).
        let mut flat = vec![
            heading_with_clocks(1, 1, vec!["CLOCK: [2026-08-17 Mon 08:00]--[2026-08-17 Mon 08:10] =>  0:10"]),
            heading_with_clocks(2, 1, vec![]),
            heading_with_clocks(3, 2, vec!["CLOCK: [2026-08-17 Mon 09:00]--[2026-08-17 Mon 09:20] =>  0:20"]),
        ];
        annotate_clocked_time(&mut flat);

        assert_eq!(flat[0].total_clocked_minutes, Some(10), "Project A unaffected by Project B's subtree");
        assert_eq!(flat[1].total_clocked_minutes, Some(20), "Project B rolls up only its own descendant");
        assert_eq!(flat[2].total_clocked_minutes, Some(20));
    }

    #[test]
    fn a_malformed_clock_line_is_silently_ignored_not_counted() {
        let mut flat =
            vec![heading_with_clocks(1, 1, vec!["CLOCK: garbage", "CLOCK: [2026-08-17 Mon 09:00]--[2026-08-17 Mon 09:10] =>  0:10"])];
        annotate_clocked_time(&mut flat);
        assert_eq!(flat[0].own_clocked_minutes, Some(10));
    }
}
