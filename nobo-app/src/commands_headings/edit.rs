// SPDX-FileCopyrightText: 2026 Dennis Burgermeister
// SPDX-License-Identifier: GPL-3.0-or-later

//! Changing headings: create, update, delete, reorder, the bulk edits, the
//! clock. Every write goes through core::vault_write::recorded -- locked,
//! indexed, one undo away.

use nobo_core::core::edna;
use nobo_core::core::indexer::IndexManager;
use nobo_core::core::models::{BulkHeadingUpdate, HeadingCreatePayload, HeadingTarget, HeadingUpdatePayload, HistoryEntryKind};
use nobo_core::core::vault_write;
use nobo_core::core::{clock, file_kind, heading_identity, heading_utils};
use nobo_core::utils::fs_utils;
use nobo_core::utils::fs_utils::filename_of;
use nobo_core::utils::repeat_utils;
use nobo_core::utils::ToCmdResult;
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

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
pub(super) fn reorder(
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
