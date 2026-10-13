//! Keeping a vault tidy: CUSTOM_IDs backfilled or removed, empty property
//! drawers, org files promoted to notes -- Settings' maintenance section.

use nobo_core::config::settings::Config;
use nobo_core::core::history::record_best_effort;
use nobo_core::core::indexer::IndexManager;
use nobo_core::core::models::{
    CustomIdBackfillPreview, EmptyPropertiesDrawer, FileSnapshot, Heading, HeadingLocation, HeadingTarget, HistoryEntryKind, Note,
    OrphanedCustomId, UnpromotedOrgFile,
};
use nobo_core::core::vault_write;
use nobo_core::core::{heading_identity, heading_utils};
use nobo_core::utils::fs_utils;
use nobo_core::utils::fs_utils::filename_of;
use nobo_core::utils::note_utils;
use nobo_core::utils::ToCmdResult;
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Defines how many titles should be loaded when headings are found without a CUSTOM_ID.
pub(super) const EXAMPLE_TITLE_LIMIT: usize = 5;

/// Whether `path` is one of the four configured GTD files, compared by
/// filename only.
///
/// The configured paths are stored as the directory picker produced them,
/// often relative, while `path` comes from the vault scan, which
/// canonicalizes its root -- comparing the two strings directly is the
/// path mismatch this codebase has hit repeatedly. Safe here, since a GTD
/// file's basename cannot collide with a note by construction.
pub(super) fn is_configured_gtd_file(path: &Path, config: &Config) -> bool {
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
