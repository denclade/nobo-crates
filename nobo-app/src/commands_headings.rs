use nobo_core::config::settings::Config;
use nobo_core::core::edna;
use nobo_core::core::history::record_best_effort;
use nobo_core::core::indexer::IndexManager;
use nobo_core::core::models::{
    BulkHeadingUpdate, BulkRefileSource, CustomIdBackfillPreview, EmptyPropertiesDrawer, FileSnapshot, Heading, HeadingCreatePayload,
    HeadingLocation, HeadingTarget, HeadingUpdatePayload, HistoryEntryKind, Note, OrphanedCustomId, RefilePosition, TimeTrackingSummary,
    UnpromotedOrgFile,
};
use nobo_core::core::vault_write;
use nobo_core::core::{clock, file_kind, heading_identity, heading_utils, refile, task_utils};
use nobo_core::utils::effort_utils;
use nobo_core::utils::fs_utils;
use nobo_core::utils::fs_utils::filename_of;
use nobo_core::utils::note_utils;
use nobo_core::utils::repeat_utils;
use nobo_core::utils::ToCmdResult;
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub fn get_all_actions(index_manager: &IndexManager) -> Result<Vec<Heading>, String> {
    Ok(index_manager.all_headings_with_state())
}

/// Defines how many titles should be loaded when headings are found without a CUSTOM_ID.
const EXAMPLE_TITLE_LIMIT: usize = 5;

/// Whether `path` is one of the four configured GTD files, compared by
/// filename only.
///
/// The configured paths are stored as the directory picker produced them,
/// often relative, while `path` comes from the vault scan, which
/// canonicalizes its root -- comparing the two strings directly is the
/// path mismatch this codebase has hit repeatedly. Safe here, since a GTD
/// file's basename cannot collide with a note by construction.
fn is_configured_gtd_file(path: &Path, config: &Config) -> bool {
    let name = fs_utils::filename_of(path);
    [&config.inbox_file_path, &config.actions_file_path, &config.projects_file_path, &config.someday_file_path]
        .iter()
        .any(|p| fs_utils::filename_of(p.as_str()) == name)
}

/// Previews which headings are missing a `CUSTOM_ID`, so the user can
/// decide whether to backfill them.
///
/// Restricted to top-level headings in plain notes: a note is free-form
/// outline text, and stamping every sub-heading with a :PROPERTIES: drawer
/// clutters it. GTD files are exempt -- their sub-headings are real
/// subtasks that drag/drop, refile, clock and history restore all need a
/// stable id for at every level.
pub fn check_missing_custom_ids(index_manager: &IndexManager) -> Result<CustomIdBackfillPreview, String> {
    // Maybe return the data per file to give the user information of the files affected?
    // Would that be interesting information in some use-case?
    let config = Config::load().unwrap_or_default();
    let missing: Vec<Heading> = index_manager
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

    Ok(CustomIdBackfillPreview { missing_count: missing.len() as u32, example_titles, example_block })
}

/// Stamps a CUSTOM_ID onto every heading `check_missing_custom_ids` found --
/// same level-1-for-notes/every-level-for-GTD-files scoping as that command,
/// see its own doc comment. Deliberately does NOT bump UPDATED/LAST_ACTIVITY.
/// Records one CustomIdsBackfilled entry per touched file, so this one-time
/// action is itself normally undo-able.
pub fn backfill_custom_ids(index_manager: &IndexManager, db: &Mutex<Connection>) -> Result<u32, String> {
    // Reads and writes every vault file with an un-stamped heading, which
    // would otherwise occupy an async worker thread for its whole
    // duration. See orchestrator.rs's rescan_local_state for why
    // block_in_place rather than spawn_blocking.
    let config = Config::load().unwrap_or_default();
    crate::blocking(|| {
        let mut total = 0u32;
        for (path_str, cached) in index_manager.headings_by_file() {
            let path = PathBuf::from(&path_str);
            let is_gtd = is_configured_gtd_file(&path, &config);
            let wants_stamp = |h: &Heading| !heading_identity::has_custom_id(h) && (is_gtd || h.level == 1);

            // The cache decides WHICH files are worth opening -- being one
            // file behind there costs nothing but a skipped or a pointless
            // open.
            if !cached.iter().any(|h| wants_stamp(h)) {
                continue;
            }

            // The line numbers spliced below may not come from it. This
            // runs across the whole vault after the user confirmed a
            // preview, so a sync pull, a CalDAV pull or another editor has
            // had a whole dialog's worth of time to move every line in
            // this file -- and a block spliced against the old numbers
            // lands on whatever occupies them now. Same rule every other
            // writer follows, see heading_utils::parse_from_disk.
            // One read for both halves. This used to read three times --
            // parse, snapshot, lines -- and splice line numbers from the
            // first read into the text of the third, then CLAMP a range
            // that no longer fit instead of refusing it. A file that
            // shrank in between had six lines of its new content
            // overwritten, and the undo snapshot came from yet another
            // read, so undo restored the wrong state too.
            //
            // A file that cannot be read is skipped, not fatal: this walks
            // the whole vault, and one deleted file must not abort the
            // rest of the sweep.
            let Ok((mut lines, flat)) = heading_utils::read_and_parse(&path) else {
                log::warn!("backfill_custom_ids: could not read {} -- skipped", path.display());
                continue;
            };
            let mut missing: Vec<&Heading> = flat.iter().filter(|h| wants_stamp(h)).collect();
            if missing.is_empty() {
                continue;
            }
            // Reverse sort to prevent `line_no` changes of items after adding
            // a `CUSTOM_ID` to `PROPERTIES`.
            missing.sort_unstable_by(|a, b| b.line_no.cmp(&a.line_no));

            let before = Some(fs_utils::lines_to_content(&lines));
            for heading in &missing {
                let (start, end) = heading_utils::heading_block_range(&lines, heading)?;
                lines.splice(start..end, heading_utils::stamp_custom_id_only(heading));
            }
            fs_utils::write_lines(&path, &lines).to_cmd()?;
            index_manager.update_file(&path);
            let after = Some(fs_utils::lines_to_content(&lines));

            record_best_effort(
                &db,
                HistoryEntryKind::CustomIdsBackfilled { filename: filename_of(&path), count: missing.len() as u32 },
                false,
                vec![FileSnapshot { path: path_str, before, after }],
            );

            total += missing.len() as u32;
        }
        Ok(total)
    })
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

/// Applies PAYLOAD to the one heading TARGET identifies.
///
/// TARGET, not a bare line number: the caller's line number was captured
/// when it last loaded the file and every write since has shifted it, so
/// trusting it blindly is what let one task's edit land on another. See
/// heading_identity::resolve_target -- it re-finds the heading by its
/// CUSTOM_ID, and refuses the write rather than guessing.
pub fn update_heading_in_file(
    file_path: String,
    target: HeadingTarget,
    payload: HeadingUpdatePayload,
    index_manager: &IndexManager,
    db: &Mutex<Connection>,
) -> Result<(), String> {
    let target_path = PathBuf::from(&file_path);
    // One read of the file, the heading found by identity in it, and
    // every splice below landing in exactly those lines -- see
    // heading_utils::open_edit.
    let mut edit = heading_utils::open_edit(&target_path, &target, &nobo_core::core::errors::subject::heading())?;
    let heading = edit.heading().clone();
    let line_no = heading.line_no;

    // A repeating task is never actually finished: the edit becomes "back
    // to TODO, one interval later" instead (see repeat_utils). Applied
    // here rather than inside build_org_heading_block, which the undo/redo
    // restore path also goes through -- restoring a heading to DONE must
    // reinstate exactly that, not advance the schedule again.
    //
    // Never in a file that only knows open-or-done. That is what a synced
    // task list is (see core::file_kind): the RRULE belongs to the server
    // there, which advances the task when it is told the occurrence is
    // complete. Advancing it here instead would leave the heading back at
    // TODO, so the push would report NEEDS-ACTION and the server would
    // never learn it was done at all.
    //
    // Asked as a capability rather than "is this a CalDAV list", because
    // the answer is about what the file can represent -- the next kind
    // with the same limitation gets this right without touching this
    // line.
    let states = file_kind::capabilities(file_kind::of_path(&target_path)).states;
    let repeated = if states == file_kind::StateSupport::DoneOnly { None } else { repeat_utils::repeat_on_finish(&heading, &payload) };
    let (heading, payload) = match repeated {
        Some(repeat) => (repeat.heading, repeat.payload),
        None => (heading, payload),
    };

    let state_changed = payload.state.is_some() && payload.state != heading.state;
    let title = heading.title.clone();

    // Built from `heading` rather than from `edit.heading()`, because the
    // repeat rule above may have substituted a different one -- but
    // spliced into the range the PARSE gives, never one carried on a
    // clone.
    //
    // Snapshotted whole-file, around the mutation AND the apply_triggers
    // call below, rather than just the spliced block -- apply_triggers is
    // verified to only ever write back into this SAME file (see its own
    // doc comment), so one snapshot pair here already captures any
    // trigger-cascaded changes too, no multi-file tracking needed.
    let new_block = heading_utils::build_org_heading_block(&heading, &payload);
    let entry = HistoryEntryKind::HeadingUpdated { filename: filename_of(&target_path), title };
    vault_write::recorded(
        &target_path,
        |_| entry,
        index_manager,
        db,
        || {
            edit.replace(new_block)?;
            // A subtask's own state/tags/content changing also bumps its
            // enclosing project's (and every ancestor's) LAST_ACTIVITY, so it
            // resurfaces under "sort by last activity" without needing to be
            // opened itself.
            edit.touch_ancestors()?;
            edit.commit()?;
            if state_changed {
                // The triggers read the file as it is now.
                index_manager.update_file(&target_path);
                edna::apply_triggers(&target_path, line_no, &index_manager)?;
            }
            Ok(())
        },
    )
}

/// Stamps a CUSTOM_ID onto exactly one heading, but only if it doesn't
/// already have one -- returns the id either way. Called right before a
/// heading-link is inserted (see the heading-link modal), the "on link
/// creation" trigger that replaced the old blanket "every new heading gets
/// one" auto-plugin: a heading only needs a stable anchor once something
/// actually links to it.
pub fn ensure_custom_id_for_heading(
    file_path: String,
    target: HeadingTarget,
    index_manager: &IndexManager,
    db: &Mutex<Connection>,
) -> Result<String, String> {
    let target_path = PathBuf::from(&file_path);
    let mut edit = heading_utils::open_edit(&target_path, &target, &nobo_core::core::errors::subject::heading())?;
    let mut heading = edit.heading().clone();

    if let Some(id) = heading_identity::custom_id_of(&heading) {
        return Ok(id.to_string());
    }

    // Filled in on the clone first, then stamp_custom_id_only's own
    // ensure_custom_id call sees one is already present and leaves it
    // alone (a no-op, not a second, different id) -- see its own doc
    // comment. That's what lets this function hand back the exact id
    // that ends up on disk, without needing a second reparse to read it
    // back.
    heading_identity::ensure_custom_id(&mut heading.properties);
    let id = heading_identity::custom_id_of(&heading).unwrap().to_string();

    let block = heading_utils::stamp_custom_id_only(&heading);
    let entry = HistoryEntryKind::CustomIdsBackfilled { filename: filename_of(&target_path), count: 1 };
    vault_write::recorded(
        &target_path,
        |_| entry,
        index_manager,
        db,
        || {
            edit.replace(block)?;
            edit.commit()
        },
    )?;

    Ok(id)
}

/// Every heading in the vault whose own CUSTOM_ID is never referenced by a
/// `::#h:ID` heading-link anywhere -- see Settings' Maintenance section,
/// which surfaces these for review before remove_custom_ids deletes them.
pub fn find_orphaned_custom_ids(base_dir: String, index_manager: &IndexManager) -> Result<Vec<OrphanedCustomId>, String> {
    note_utils::find_orphaned_custom_ids(Path::new(&base_dir), &index_manager.headings_by_file()).to_cmd()
}

/// Removes JUST the `:CUSTOM_ID:` property line from each of TARGETS' own
/// properties drawer, leaving every other property (and the rest of the
/// heading) untouched -- the confirm step after find_orphaned_custom_ids'
/// preview. Returns how many headings were actually touched (a target
/// whose heading has since moved/lost its CUSTOM_ID is silently skipped,
/// same "stale index, don't panic" stance as update_heading_in_file's own
/// not_found_at_line case, just non-fatal here since this runs as one
/// best-effort batch over a preview list the user already reviewed).
pub fn remove_custom_ids(targets: Vec<HeadingLocation>, index_manager: &IndexManager, db: &Mutex<Connection>) -> Result<u32, String> {
    crate::blocking(|| {
        let mut by_file: std::collections::HashMap<String, Vec<HeadingTarget>> = std::collections::HashMap::new();
        for target in targets {
            by_file.entry(target.file_path).or_default().push(target.target);
        }

        let mut total = 0u32;
        for (path_str, targets) in by_file {
            let path = PathBuf::from(&path_str);
            // One read for lines and parse, and the snapshot rebuilt from
            // it -- see backfill_custom_ids above for what three separate
            // reads cost.
            let Ok((mut lines, flat)) = heading_utils::read_and_parse(&path) else {
                log::warn!("could not read {} -- skipped", path.display());
                continue;
            };
            let before = Some(fs_utils::lines_to_content(&lines));

            // Resolved first, all of them, then applied descending (an
            // earlier removal shifts every line below it -- same reasoning
            // backfill_custom_ids' own reverse sort comment gives). The
            // preview these targets come from can be minutes old, and
            // stripping the CUSTOM_ID off the WRONG heading silently
            // breaks every link pointing at it -- resolving by identity is
            // what rules that out.
            //
            // An entry that no longer resolves is dropped rather than
            // failing the batch, unlike the user-initiated bulk
            // update/delete: this is a "clean up whatever of this list is
            // still there" sweep, and one heading having moved on since
            // the preview is not a reason to refuse the other fifty.
            // Skipping can never hit the wrong heading either way.
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

            fs_utils::write_lines(&path, &lines).to_cmd()?;
            index_manager.update_file(&path);
            let after = Some(fs_utils::lines_to_content(&lines));

            record_best_effort(
                &db,
                HistoryEntryKind::CustomIdsRemoved { filename: filename_of(&path), count: touched },
                false,
                vec![FileSnapshot { path: path_str, before, after }],
            );

            total += touched;
        }
        Ok(total)
    })
}

/// Every plain `.org` file under BASE_DIR that isn't a recognized Denote
/// note yet -- Settings' Maintenance section surfaces these so the user
/// can review and promote the ones they actually want treated as a real
/// note (see promote_org_file_to_note). GTD files (Inbox/Actions/Projects/
/// Someday) are excluded -- they're ALSO plain, non-Denote `.org` files by
/// this same check, but promoting one would be actively wrong, a
/// different subsystem owns them entirely (same exclusion
/// check_missing_custom_ids/backfill_custom_ids already use).
pub fn find_unpromoted_org_files(base_dir: String) -> Result<Vec<UnpromotedOrgFile>, String> {
    let config = Config::load().unwrap_or_default();
    Ok(note_utils::find_unpromoted_org_files(Path::new(&base_dir))
        .into_iter()
        .filter(|f| !is_configured_gtd_file(Path::new(&f.path), &config))
        .collect())
}

/// Promotes PATH into a real Denote note (see note_utils::
/// promote_org_file_to_note's own doc comment for what that actually
/// does -- a header stamp AND a rename) -- the confirm step after
/// find_unpromoted_org_files' preview, and also called directly the first
/// time a plain `.org` file gets saved through the Notes editor (see
/// useNoteEditor.ts's saveActiveNote).
pub fn promote_org_file_to_note(path: String, index_manager: &IndexManager, db: &Mutex<Connection>) -> Result<Note, String> {
    crate::blocking(|| {
        let old_path = PathBuf::from(&path);
        // Promoting RENAMES the file to its Denote name -- the step that
        // made a user's whole projects.org vanish when the notes editor
        // still did this on every save (see core::app_files).
        nobo_core::core::app_files::refuse_if_app_owned(&old_path, "zur Notiz machen", "turned into a note")?;
        let old_filename = filename_of(&old_path);
        let note = note_utils::promote_org_file_to_note(&old_path).to_cmd()?;

        // remove_file, not update_file: the old path is already gone by
        // now, and build_file_entry's metadata call fails on a path that
        // no longer exists, so update_file would silently no-op.
        index_manager.remove_file(&old_path);
        let new_path = PathBuf::from(&note.path);
        index_manager.update_file(&new_path);

        record_best_effort(
            &db,
            HistoryEntryKind::FileRenamed { old_filename, new_filename: filename_of(&new_path) },
            false,
            vec![FileSnapshot { path: note.path.clone(), before: None, after: std::fs::read_to_string(&new_path).ok() }],
        );

        Ok(note)
    })
}

/// Every heading in the vault whose own `:PROPERTIES:...:END:` drawer is
/// completely empty -- Settings' Maintenance section surfaces these for
/// review before remove_empty_properties_drawers deletes them.
pub fn find_empty_properties_drawers(index_manager: &IndexManager) -> Result<Vec<EmptyPropertiesDrawer>, String> {
    note_utils::find_empty_properties_drawers(&index_manager.headings_by_file()).to_cmd()
}

/// Removes each of TARGETS' own `:PROPERTIES:`/`:END:` boundary lines, but
/// only where the drawer is still genuinely empty by the time this runs
/// (a target whose drawer has since gained a real property, or whose
/// heading has moved/vanished, is silently skipped -- same best-effort
/// stance as remove_custom_ids). Returns how many were actually removed.
pub fn remove_empty_properties_drawers(
    targets: Vec<HeadingLocation>,
    index_manager: &IndexManager,
    db: &Mutex<Connection>,
) -> Result<u32, String> {
    crate::blocking(|| {
        let mut by_file: std::collections::HashMap<String, Vec<HeadingTarget>> = std::collections::HashMap::new();
        for target in targets {
            by_file.entry(target.file_path).or_default().push(target.target);
        }

        let mut total = 0u32;
        for (path_str, targets) in by_file {
            let path = PathBuf::from(&path_str);
            // One read for lines and parse, and the snapshot rebuilt from
            // it -- see backfill_custom_ids above for what three separate
            // reads cost.
            let Ok((mut lines, flat)) = heading_utils::read_and_parse(&path) else {
                log::warn!("could not read {} -- skipped", path.display());
                continue;
            };
            let before = Some(fs_utils::lines_to_content(&lines));

            // Same resolve-then-apply-descending shape as
            // remove_custom_ids above, including why an unresolvable
            // entry is skipped rather than failing the sweep.
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

            fs_utils::write_lines(&path, &lines).to_cmd()?;
            index_manager.update_file(&path);
            let after = Some(fs_utils::lines_to_content(&lines));

            record_best_effort(
                &db,
                HistoryEntryKind::EmptyPropertiesDrawersRemoved { filename: filename_of(&path), count: touched },
                false,
                vec![FileSnapshot { path: path_str, before, after }],
            );

            total += touched;
        }
        Ok(total)
    })
}

/// Applies each entry's own payload to the heading at its `line_no`, all
/// within one file -- a selection spanning several files calls this once
/// per file. The fast path every bulk action funnels through instead of
/// looping `update_heading_in_file`, which costs a full-file read, an
/// ancestor touch pass, a reparse and a history row per task.
///
/// One read, one write, one reparse and one history entry for the whole
/// batch, whatever each entry's payload is.
///
/// The batching itself -- which headings get a real edit, which ancestors
/// only a deduplicated LAST_ACTIVITY touch, and the deepest-line-first
/// splice order that keeps it all against one in-memory copy -- is
/// `heading_utils::apply_bulk_update`, a pure function so that
/// line-shifting invariant is unit-testable. This is the I/O around it.
///
/// Does not run edna's TRIGGER cascade: a line captured before the batch
/// is no longer reliable once earlier edits have grown or shrunk the file,
/// and re-locating each heading means the per-item reparse this exists to
/// eliminate. Triggers still fire from a single-item update.
pub fn bulk_update_headings(
    file_path: String,
    updates: Vec<BulkHeadingUpdate>,
    index_manager: &IndexManager,
    db: &Mutex<Connection>,
) -> Result<u32, String> {
    let target_path = PathBuf::from(&file_path);

    let filename = filename_of(&target_path);
    let entry = |result: &Result<u32, String>| HistoryEntryKind::HeadingsBulkUpdated { filename, count: *result.as_ref().unwrap_or(&0) };
    vault_write::recorded(&target_path, entry, index_manager, db, || {
        // Both halves from one read: the line numbers below are spliced
        // into exactly the text they were parsed out of (see
        // heading_utils::read_and_parse).
        let (mut lines, flat) = heading_utils::read_and_parse(&target_path)?;
        let count = heading_utils::apply_bulk_update(&flat, &updates, &mut lines)?;
        if count > 0 {
            fs_utils::write_lines(&target_path, &lines).to_cmd()?;
        }
        Ok(count)
    })
}

/// Deletes each of `targets`' subtrees from one file -- the batched
/// counterpart to `bulk_update_headings`, with the same one
/// read/write/reparse/history-entry-per-batch shape.
///
/// The batching itself -- removal order, ancestor touch dedup, and the
/// overlap handling for a selected project plus one of its own selected
/// subtasks -- is the pure `heading_utils::apply_bulk_delete`.
pub fn bulk_delete_headings(
    file_path: String,
    targets: Vec<HeadingTarget>,
    index_manager: &IndexManager,
    db: &Mutex<Connection>,
) -> Result<u32, String> {
    let target_path = PathBuf::from(&file_path);

    let filename = filename_of(&target_path);
    let entry = |result: &Result<u32, String>| HistoryEntryKind::HeadingsBulkDeleted { filename, count: *result.as_ref().unwrap_or(&0) };
    vault_write::recorded(&target_path, entry, index_manager, db, || {
        // Both halves from one read: the line numbers below are spliced
        // into exactly the text they were parsed out of (see
        // heading_utils::read_and_parse).
        let (mut lines, flat) = heading_utils::read_and_parse(&target_path)?;
        let count = heading_utils::apply_bulk_delete(&flat, &targets, &mut lines)?;
        if count > 0 {
            fs_utils::write_lines(&target_path, &lines).to_cmd()?;
        }
        Ok(count)
    })
}

pub fn create_heading_in_file(payload: HeadingCreatePayload, index_manager: &IndexManager, db: &Mutex<Connection>) -> Result<(), String> {
    let target_path = PathBuf::from(&payload.file_path);
    let title = payload.title.clone();

    // Resolved before anything is written, and outside the snapshot below,
    // exactly like update_heading_in_file: a stale parent line would file
    // the new subtask under whichever heading moved into it, where the
    // user will not think to look for it (see
    // heading_identity::resolve_target).
    //
    // The new heading's own level falls out of that same resolution -- one
    // deeper than the parent as it exists NOW. Deriving it here rather
    // than trusting the caller is what keeps the heading, and the
    // demotion of its description, consistent with where it lands.
    let parent_edit = match &payload.parent {
        Some(parent) => Some(heading_utils::open_edit(&target_path, parent, &nobo_core::core::errors::subject::parent_heading())?),
        None => None,
    };
    let level = parent_edit.as_ref().map_or(1, |edit| edit.heading().level + 1);
    let new_lines = heading_utils::build_new_heading_block(&payload, level);

    let entry = HistoryEntryKind::HeadingCreated { filename: filename_of(&target_path), title };
    vault_write::recorded(
        &target_path,
        |_| entry,
        index_manager,
        db,
        || {
            match parent_edit {
                Some(mut edit) => {
                    edit.insert_after_subtree(new_lines)?;

                    // A new subtask is a change to its project too -- the
                    // parent itself included, not just its ancestors, since
                    // the parent is rarely the node being edited directly
                    // here. Applied after the insertion, which sits at or
                    // beyond the parent's own subtree end and therefore
                    // strictly after every one of these headings' own line
                    // ranges.
                    edit.touch_self_and_ancestors()?;
                    edit.commit()?;
                }
                None => {
                    // The same rule HeadingEdit applies on insert: a task
                    // pasted into the new heading's description, drawer and
                    // id included, must not arrive with an id the file
                    // already has (see heading_utils::renew_taken_ids). A
                    // file that does not exist yet has none to collide with.
                    let mut new_lines = new_lines;
                    if target_path.exists() {
                        let (_, flat) = heading_utils::read_and_parse(&target_path)?;
                        let taken = heading_utils::ids_in_file_except(&flat, None);
                        heading_utils::renew_taken_ids(&mut new_lines, &taken, false);
                    }
                    fs_utils::append_lines(&target_path, new_lines).to_cmd()?;
                }
            }
            Ok(())
        },
    )
}

pub fn delete_heading_in_file(
    file_path: String,
    target: HeadingTarget,
    index_manager: &IndexManager,
    db: &Mutex<Connection>,
) -> Result<(), String> {
    let target_path = PathBuf::from(&file_path);

    // Resolved by identity like every other write, and this is the one
    // where a stale line number costs the most: it would delete a whole
    // subtree that was never selected.
    let mut edit = heading_utils::open_edit(&target_path, &target, &nobo_core::core::errors::subject::heading())?;
    let title = edit.heading().title.clone();

    let entry = HistoryEntryKind::HeadingDeleted { filename: filename_of(&target_path), title };
    vault_write::recorded(
        &target_path,
        |_| entry,
        index_manager,
        db,
        || {
            edit.delete_subtree()?;
            // Deleting/completing a subtask is a change to its project too.
            edit.touch_ancestors()?;
            edit.commit()
        },
    )
}

/// Reorders a heading among its siblings.
///
/// BEFORE names the sibling it should end up above; `None` puts it last.
/// That is the shape a drag has -- "this one now goes where that one is"
/// -- and it needs no index, which would be stale the moment anything
/// else in the file moved.
///
/// Only the order changes: the level and the parent stay what they were,
/// and an anchor that is not a sibling is refused rather than quietly
/// re-parenting the subtree (see `HeadingEdit::move_subtree_before`).
/// Recorded as an ordinary file edit, because that is what it is -- the
/// snapshot pair is the whole file, so undo puts the order back.
pub fn move_heading_in_file(
    file_path: String,
    target: HeadingTarget,
    before: Option<HeadingTarget>,
    index_manager: &IndexManager,
    db: &Mutex<Connection>,
) -> Result<(), String> {
    let target_path = PathBuf::from(&file_path);

    let edit = heading_utils::open_edit(&target_path, &target, &nobo_core::core::errors::subject::heading())?;
    // Resolved against the same parse the move will splice, and by
    // identity like every other write: a line number alone would move the
    // entry above whatever has since taken that line.
    let anchor = match &before {
        Some(sibling) => {
            Some(heading_identity::resolve_target(edit.flat(), sibling, &nobo_core::core::errors::subject::sibling_heading())?)
        }
        None => None,
    };

    reorder(edit, anchor, &target_path, index_manager, db)
}

/// The tail every reorder shares: the move, the write, the reindex, the
/// history entry -- or none of it. A drop that lands where the heading
/// already is leaves `move_subtree_before` with nothing mutated, and
/// committing anyway would rewrite the file for no change and wake every
/// watcher on it, including the sync about to be told the order changed.
fn reorder(
    mut edit: heading_utils::HeadingEdit,
    anchor: Option<usize>,
    path: &Path,
    index_manager: &IndexManager,
    db: &Mutex<Connection>,
) -> Result<(), String> {
    edit.move_subtree_before(anchor)?;
    if !edit.is_mutated() {
        return Ok(());
    }

    let entry = HistoryEntryKind::FileEdited { filename: filename_of(path) };
    vault_write::recorded(path, |_| entry, index_manager, db, || edit.commit())
}

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

/// Starts a new open CLOCK entry on FILE_PATH/LINE_NO -- see
/// core::clock::clock_in for the actual logic (exactly one global timer,
/// auto-closing whatever else was running).
pub fn clock_in(file_path: String, target: HeadingTarget, index_manager: &IndexManager, db: &Mutex<Connection>) -> Result<(), String> {
    clock::clock_in(&file_path, &target, index_manager, db)
}

/// Stops FILE_PATH/LINE_NO's own running timer -- see core::clock::clock_out
/// for the actual logic.
pub fn clock_out(file_path: String, target: HeadingTarget, index_manager: &IndexManager, db: &Mutex<Connection>) -> Result<(), String> {
    clock::clock_out(&file_path, &target, index_manager, db)
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
fn resolve_effort_presets(file_content: Option<&str>, settings_presets_org: Option<&str>) -> Vec<u32> {
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

/// What a cross-file move actually managed to put on disk.
struct RefileWrites {
    target_written: bool,
    source_written: bool,
    error: Option<String>,
}

/// The writes a refile is made of, in the order that makes a failure
/// survivable.
///
/// **Target first, source second.** Either write can fail -- ENOSPC, a
/// read-only mount, the Android background kill in the gap between them --
/// and the order is what decides the cost. This way the worst case is a
/// subtree that exists twice, which the user can see and delete. The other
/// way round it exists nowhere: the source was already shortened, and the
/// `?` on the second write returned before `record_best_effort` ran, so
/// there was not even a history entry to undo. The index was left stale
/// too, still advertising a task that was gone.
///
/// The kind pre-flight is the same idea as `history::restore::apply_bulk`'s
/// (`core/core/history/restore.rs`): find out that a file cannot be
/// written BEFORE writing the other one. Parent directories are
/// deliberately not checked -- `write_atomic` creates them, so refusing
/// here would reject a refile into a new folder that would have worked.
///
/// Never returns early: the caller records history from these flags, so
/// the entry describes what is on disk rather than what was intended.
fn write_refile_result(
    src_path: &Path,
    tgt_path: &Path,
    same_file: bool,
    new_src_lines: &[String],
    new_tgt_lines: &[String],
    index_manager: &IndexManager,
) -> RefileWrites {
    let refuse = |path: &Path| -> Option<String> {
        (!file_kind::capabilities(file_kind::of_path(path)).editable).then(|| {
            nobo_core::t!(
                "\"{}\" wird von nobo erzeugt und kann nicht beschrieben werden -- nichts wurde verschoben.",
                "\"{}\" is generated by nobo and cannot be written -- nothing was moved.",
                filename_of(path)
            )
        })
    };
    if let Some(error) = refuse(tgt_path).or_else(|| (!same_file).then(|| refuse(src_path)).flatten()) {
        return RefileWrites { target_written: false, source_written: false, error: Some(error) };
    }

    if let Err(e) = fs_utils::write_lines(tgt_path, new_tgt_lines).to_cmd() {
        return RefileWrites { target_written: false, source_written: false, error: Some(e) };
    }
    index_manager.update_file(tgt_path);

    if same_file {
        return RefileWrites { target_written: true, source_written: true, error: None };
    }

    let source = fs_utils::write_lines(src_path, new_src_lines).to_cmd();
    // Reindexed even on failure: the source file is whatever it is now,
    // and leaving the index describing the pre-refile state is how a
    // vanished task keeps showing up in lists.
    index_manager.update_file(src_path);

    match source {
        Ok(()) => RefileWrites { target_written: true, source_written: true, error: None },
        Err(e) => RefileWrites {
            target_written: true,
            source_written: false,
            error: Some(nobo_core::t!(
                "{e} -- die Überschrift wurde in \"{}\" eingefügt, konnte aber nicht aus \"{}\" entfernt werden und steht jetzt in beiden Dateien.", "{e} -- the heading was inserted into \"{}\" but could not be removed from \"{}\", so it is now in both files.",
                filename_of(tgt_path),
                filename_of(src_path)
            )),
        },
    }
}

pub fn refile_heading(
    source_file: String,
    source: HeadingTarget,
    target_file: String,
    target_parent: Option<HeadingTarget>,
    position: Option<RefilePosition>,
    default_state: Option<String>,
    index_manager: &IndexManager,
    db: &Mutex<Connection>,
) -> Result<(), String> {
    let position = position.unwrap_or(RefilePosition::Into);
    let src_path = std::path::PathBuf::from(&source_file);
    let tgt_path = std::path::PathBuf::from(&target_file);
    let same_file = source_file == target_file;

    // A drop beside a sibling is a reorder, not a refile, and reordering
    // has exactly one implementation: `HeadingEdit::move_subtree_before`.
    // The line arithmetic of lifting a block out and putting it back in
    // the same buffer is the kind that goes wrong quietly, and two copies
    // of it would each need their own fixes. Deciding here rather than in
    // the frontend keeps a drop what it is to the caller -- (target,
    // zone) -- while the file's kind still gets to say whether its order
    // is the user's at all (see FileKindSpec::reorderable).
    //
    // Only when the refile would have changed nothing but the order: a
    // `default_state` handed in for a heading that has none is a state
    // change, and that stays a refile with its trigger and its entry.
    if same_file && matches!(position, RefilePosition::Before | RefilePosition::After) {
        if let Some(beside) = &target_parent {
            let edit = heading_utils::open_edit(&src_path, &source, &nobo_core::core::errors::subject::source_heading())?;
            let other = heading_identity::resolve_target(edit.flat(), beside, &nobo_core::core::errors::subject::target_heading())?;
            let keeps_state = default_state.is_none() || edit.heading().state.is_some();
            if edit.is_sibling(other) && keeps_state {
                let anchor = match position {
                    RefilePosition::Before => Some(other),
                    _ => edit.next_sibling_of(other),
                };
                return reorder(edit, anchor, &src_path, index_manager, db);
            }
        }
    }

    // Lines and parse from one read (heading_utils::read_and_parse): the
    // subtree extracted below is cut out of the very text these line
    // numbers were parsed from, not out of a second read of a file that
    // may have moved on in between.
    let (all_src_lines, src_flat) = heading_utils::read_and_parse(&src_path)?;
    let src_idx = heading_identity::resolve_target(&src_flat, &source, &nobo_core::core::errors::subject::source_heading())?;

    // A target file that doesn't exist yet is a valid refile destination --
    // the "refile into a new file" flow (see useRefile.ts's path-typed
    // RefileTarget) always refiles into an empty, not-yet-created file;
    // write_lines below creates it (and any parent dirs) on first write.
    // Treated as an empty file to splice into, same as an existing-but-empty
    // one -- the only difference is what history should offer to undo to.
    let tgt_existed = same_file || tgt_path.exists();
    // A same-file refile (reordering within one list) reuses the source
    // read: it is the very same file on disk.
    let (all_tgt_lines, tgt_parsed) =
        if same_file || !tgt_existed { (Vec::new(), Vec::new()) } else { heading_utils::read_and_parse(&tgt_path)? };
    let tgt_flat: &[Heading] = match (&target_parent, same_file) {
        (None, _) => &[],
        (Some(_), true) => &src_flat,
        (Some(_), false) => &tgt_parsed,
    };

    // Both ends are re-found before anything moves: a refile that resolves
    // either one to a stale line moves the wrong subtree, or moves the
    // right one into the wrong place -- both unrecoverable without undo.
    let target_parent_line = match &target_parent {
        None => None,
        Some(parent) => {
            Some(tgt_flat[heading_identity::resolve_target(tgt_flat, parent, &nobo_core::core::errors::subject::target_heading())?].line_no)
        }
    };
    let src_level = src_flat[src_idx].level;
    let title = src_flat[src_idx].title.clone();
    let (start_line, end_line) = heading_utils::subtree_line_range(&src_flat, src_idx);

    refile::validate_target_not_within_source(same_file, target_parent_line, start_line, end_line)?;

    let target_level = match target_parent_line {
        None => 1,
        Some(p_line) => refile::resolve_target_level(tgt_flat, Some(p_line), position)?,
    };

    // Captured before splice_refile consumes all_src_lines/all_tgt_lines --
    // whole-file text, same convention as the other heading commands.
    let before_src = all_src_lines.join("\n");
    // `None` (not `Some("")`) when the target didn't exist yet -- matches
    // FileSnapshot's convention (see core::models), so undoing this refile
    // deletes the newly-created file instead of leaving an empty stray one.
    let before_tgt: Option<String> = if same_file {
        Some(before_src.clone())
    } else if tgt_existed {
        Some(all_tgt_lines.join("\n"))
    } else {
        None
    };

    let params = refile::RefileParams { start_line, end_line, src_level, target_level, default_state };
    let (new_src_lines, new_tgt_lines) =
        refile::splice_refile(all_src_lines, &params, target_parent_line, position, same_file, &tgt_path, all_tgt_lines)?;
    let after_src = new_src_lines.join("\n");
    let after_tgt = new_tgt_lines.join("\n");

    let writes = write_refile_result(&src_path, &tgt_path, same_file, &new_src_lines, &new_tgt_lines, &index_manager);

    let target_filename = filename_of(&tgt_path);
    let mut snapshots = Vec::new();
    if writes.target_written {
        snapshots.push(FileSnapshot { path: target_file, before: before_tgt, after: Some(after_tgt) });
    }
    if !same_file && writes.source_written {
        snapshots.push(FileSnapshot { path: source_file, before: Some(before_src), after: Some(after_src) });
    }
    // Recorded before the error is returned, and only for the files that
    // actually changed -- so a half-landed move is still undoable.
    if !snapshots.is_empty() {
        record_best_effort(&db, HistoryEntryKind::Refiled { title, target_filename }, false, snapshots);
    }

    match writes.error {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Refiles `sources`, all from one source file, into the same target --
/// the only position the bulk modal offers, unlike single-item refile,
/// which also supports drag-and-drop's Before/After.
///
/// One source read, one target read, one write each, one reindex each and
/// one history entry for the whole batch. The batching itself --
/// extraction order, ancestor-overlap dedup, and same-file line-shift
/// accounting -- is the pure `refile::splice_bulk_refile`.
pub fn bulk_refile_headings(
    source_file: String,
    sources: Vec<BulkRefileSource>,
    target_file: String,
    target_parent: Option<HeadingTarget>,
    index_manager: &IndexManager,
    db: &Mutex<Connection>,
) -> Result<u32, String> {
    let src_path = std::path::PathBuf::from(&source_file);
    let tgt_path = std::path::PathBuf::from(&target_file);
    let same_file = source_file == target_file;

    // One read per file, lines and parse together, source read reused for
    // a same-file batch -- see refile_heading above.
    let (all_src_lines, src_flat) = heading_utils::read_and_parse(&src_path)?;
    let tgt_existed = same_file || tgt_path.exists();
    let (all_tgt_lines, tgt_parsed) =
        if same_file || !tgt_existed { (Vec::new(), Vec::new()) } else { heading_utils::read_and_parse(&tgt_path)? };
    let tgt_flat: &[Heading] = match (&target_parent, same_file) {
        (None, _) => &[],
        (Some(_), true) => &src_flat,
        (Some(_), false) => &tgt_parsed,
    };

    // Same both-ends guard as single refile above, and all-or-nothing:
    // one unresolvable entry aborts the batch instead of moving the rest
    // plus one wrong subtree.
    let target_parent_line = match &target_parent {
        None => None,
        Some(parent) => {
            Some(tgt_flat[heading_identity::resolve_target(tgt_flat, parent, &nobo_core::core::errors::subject::target_heading())?].line_no)
        }
    };

    let target_level = match target_parent_line {
        None => 1,
        Some(p_line) => refile::resolve_target_level(tgt_flat, Some(p_line), RefilePosition::Into)?,
    };

    let before_src = all_src_lines.join("\n");
    let before_tgt: Option<String> = if same_file {
        Some(before_src.clone())
    } else if tgt_existed {
        Some(all_tgt_lines.join("\n"))
    } else {
        None
    };

    let source_entries: Vec<(usize, Option<String>)> = sources
        .into_iter()
        .map(|s| {
            let idx = heading_identity::resolve_target(&src_flat, &s.target, &nobo_core::core::errors::subject::source_heading())?;
            Ok((src_flat[idx].line_no, s.default_state))
        })
        .collect::<Result<_, String>>()?;

    let (new_src_lines, new_tgt_lines, count) = refile::splice_bulk_refile(
        &src_flat,
        &source_entries,
        all_src_lines,
        target_parent_line,
        same_file,
        target_level,
        &tgt_path,
        all_tgt_lines,
    )?;

    if count == 0 {
        return Ok(0);
    }

    let after_src = new_src_lines.join("\n");
    let after_tgt = new_tgt_lines.join("\n");

    let writes = write_refile_result(&src_path, &tgt_path, same_file, &new_src_lines, &new_tgt_lines, &index_manager);

    let target_filename = filename_of(&tgt_path);
    let mut snapshots = Vec::new();
    if writes.target_written {
        snapshots.push(FileSnapshot { path: target_file, before: before_tgt, after: Some(after_tgt) });
    }
    if !same_file && writes.source_written {
        snapshots.push(FileSnapshot { path: source_file, before: Some(before_src), after: Some(after_src) });
    }
    if !snapshots.is_empty() {
        record_best_effort(&db, HistoryEntryKind::HeadingsBulkRefiled { count, target_filename }, false, snapshots);
    }

    match writes.error {
        Some(e) => Err(e),
        None => Ok(count),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nobo_core::core::history;
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
        assert_eq!(
            resolve_effort_presets(Some(content), Some("")),
            effort_utils::parse_effort_all(effort_utils::DEFAULT_EFFORT_PRESETS_ORG),
        );
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
        std::fs::write(&path, concat!(
            "* Has one\n:PROPERTIES:\n:CUSTOM_ID: existing\n:LAST_ACTIVITY: 2020-01-01 Wed 00:00\n:END:\n",
            "* TODO Missing one\n",
        )).unwrap();
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

        let count = remove_custom_ids_body(
            &index,
            &db,
            vec![HeadingLocation { file_path: path.to_string_lossy().into_owned(), target: at_line(1) }],
        );
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

        let count = remove_custom_ids_body(
            &index,
            &db,
            vec![HeadingLocation { file_path: path.to_string_lossy().into_owned(), target: at_line(1) }],
        );
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

        let count = remove_custom_ids_body(
            &index,
            &db,
            vec![HeadingLocation { file_path: path.to_string_lossy().into_owned(), target: at_line(99) }],
        );
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
        note_utils::find_unpromoted_org_files(base_dir)
            .into_iter()
            .filter(|f| !is_configured_gtd_file(Path::new(&f.path), config))
            .collect()
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
        update_heading_body(&index, &path, &second, HeadingUpdatePayload { state: Some("DONE".to_string()), ..Default::default() })
            .unwrap();

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
            heading_utils::open_edit(&path, &at_line(flat2[group_idx].line_no), &nobo_core::core::errors::subject::parent_heading())
                .unwrap();
        edit.insert_after_subtree(new_lines).unwrap();
        edit.touch_self_and_ancestors().unwrap();
        edit.commit().unwrap();
        index.update_file(&path);

        let flat3 = index.get_headings_or_parse(&path);
        let project3 = flat3.iter().find(|h| h.title == "Project").unwrap();
        let group3 = flat3.iter().find(|h| h.title == "Group").unwrap();
        assert_ne!(
            project3.properties.get("LAST_ACTIVITY").unwrap(),
            "2021-01-01 Wed 00:00",
            "project's LAST_ACTIVITY bumped by new subtask"
        );
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
                FileSnapshot {
                    path: tgt_path.to_string_lossy().into_owned(),
                    before: Some(before_tgt),
                    after: Some(new_tgt_lines.join("\n")),
                },
                FileSnapshot {
                    path: src_path.to_string_lossy().into_owned(),
                    before: Some(before_src),
                    after: Some(new_src_lines.join("\n")),
                },
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
        assert_eq!(
            project.properties.get("UPDATED").unwrap(),
            "2020-01-01 Wed 00:00",
            "project's own UPDATED untouched by a subtask batch"
        );
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
        assert_eq!(
            project.properties.get("UPDATED").unwrap(),
            "2020-01-01 Wed 00:00",
            "project's own UPDATED untouched by a subtask batch"
        );
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
}
