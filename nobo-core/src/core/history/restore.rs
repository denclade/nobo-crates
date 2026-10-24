// SPDX-FileCopyrightText: 2026 Dennis Burgermeister
// SPDX-License-Identifier: GPL-3.0-or-later

use rusqlite::Connection;
use similar::{DiffTag, TextDiff};
use std::path::PathBuf;

use crate::core::heading_identity::{self, HeadingIdentity};
use crate::core::heading_utils;
use crate::core::history;
use crate::core::history::store;
use crate::core::indexer::IndexManager;
use crate::core::models::{FileSnapshot, Heading, HeadingRestoreCheck, HeadingUpdatePayload, HistoryEntryKind};
use crate::core::org_parser;
use crate::utils::fs_utils;
use crate::utils::ToCmdResult;

/// Everything recovered from a `HeadingDeleted` entry's before/after
/// snapshot pair -- the exact original org-text of the deleted subtree
/// (heading line + every descendant, verbatim, not reconstructed), its own
/// title, and its immediate parent's title/identity AT THE TIME OF
/// DELETION (`None` for both if it was a top-level heading). `parent_title`
/// is kept separately from `parent_identity` purely for the human-readable
/// "not feasible" reason -- capture_identity may have preferred the
/// parent's CUSTOM_ID over its title, but the title is still what a person
/// reading the reason recognizes.
struct RemovedHeading {
    raw_lines: Vec<String>,
    title: String,
    parent_title: Option<String>,
    parent_identity: Option<HeadingIdentity>,
    /// The heading that directly followed the deleted one at the SAME
    /// level, at the time of deletion (i.e. its original next sibling --
    /// `None` if it was the last child of its parent, or the last
    /// top-level heading in the file). Preferred over `parent_identity`
    /// when resolving where to reinsert on restore (see
    /// resolve_restore_delete) specifically so a restored heading reappears
    /// in its ORIGINAL relative position among its siblings, not always
    /// appended at the end -- see this feature's own history for the
    /// "restore always drops it at the end" gap this closes.
    next_sibling_identity: Option<HeadingIdentity>,
}

/// The heading immediately after IDX's own subtree, at the SAME level --
/// i.e. its next sibling, `None` if IDX was the last child of its parent (or
/// the last top-level heading). Stops looking the moment a SHALLOWER
/// heading is hit first (that means the parent's own scope ended with no
/// same-level sibling in between), so this never mistakes a cousin in some
/// later, unrelated branch for a real sibling.
fn next_sibling_of(flat: &[Heading], idx: usize) -> Option<&Heading> {
    let level = flat[idx].level;
    let (_, end_line) = heading_utils::subtree_line_range(flat, idx);
    let next = flat.iter().find(|h| h.line_no > end_line && h.level <= level)?;
    (next.level == level).then_some(next)
}

/// Isolates the block a deletion removed, by diffing `before` and `after`
/// line-by-line.
///
/// Not simply "one clean Delete op": a deletion also touches the removed
/// heading's ancestors, rewriting each of their blocks to bump
/// LAST_ACTIVITY -- an unrelated change earlier in the file, showing up as
/// its own Replace or Insert op.
///
/// Only pure Delete ops can be the deleted heading; a rewritten ancestor
/// block always has a replacement on the `after` side. So this collects
/// every Delete op whose old range exactly matches some heading's subtree
/// range in `before`. Exactly one match is trustworthy; zero or several
/// return `None` rather than a guess.
fn find_removed_heading(before: &str, after: &str) -> Option<RemovedHeading> {
    let diff = TextDiff::from_lines(before, after);
    let before_lines: Vec<String> = before.lines().map(str::to_string).collect();
    let flat = org_parser::parse_headings_from_lines(&PathBuf::new(), &before_lines);

    let mut found: Option<usize> = None;
    for op in diff.ops() {
        if op.tag() != DiffTag::Delete {
            continue;
        }
        let range = op.old_range();
        let matched = flat.iter().enumerate().find_map(|(i, _)| {
            let (start_line, end_line) = heading_utils::subtree_line_range(&flat, i);
            (start_line - 1 == range.start && end_line == range.end).then_some(i)
        });
        if let Some(idx) = matched {
            if found.is_some() {
                return None;
            }
            found = Some(idx);
        }
    }
    let idx = found?;

    let (start_line, end_line) = heading_utils::subtree_line_range(&flat, idx);
    if end_line > before_lines.len() {
        return None;
    }
    let raw_lines = before_lines[start_line - 1..end_line].to_vec();
    let parent = heading_utils::ancestors_of(&flat, idx).into_iter().next();
    let parent_title = parent.as_ref().map(|p| p.title.clone());
    let parent_identity = parent.as_ref().map(heading_identity::capture_identity);
    let next_sibling_identity = next_sibling_of(&flat, idx).map(heading_identity::capture_identity);

    Some(RemovedHeading { raw_lines, title: flat[idx].title.clone(), parent_title, parent_identity, next_sibling_identity })
}

/// What a targeted field revert needs from a `HeadingUpdated` entry:
/// `identity` to re-find the heading, and `payload` carrying only the
/// fields this edit actually changed -- unrelated ones stay `None`, so a
/// revert cannot clobber a later change to the same heading.
struct UpdateRevert {
    identity: HeadingIdentity,
    payload: HeadingUpdatePayload,
}

/// The update counterpart to `find_removed_heading`, and deliberately not
/// diff-based.
///
/// A line diff finds the minimal-edit alignment between two texts, which
/// degrades badly once the file is full of near-identical structure --
/// every heading's `:PROPERTIES:` drawer looks alike, so among many
/// equally minimal alignments the diff can report one giant changed region
/// spanning hundreds of unrelated lines.
///
/// An update only ever changes existing headings' fields, never adds or
/// removes one, so a simple edit leaves both flat heading lists the same
/// length and order -- the headings at the same index are the same
/// heading. A length mismatch means this cannot be trusted, returning
/// `None` like the delete case does.
///
/// `old_title_hint` is the heading's title before this edit, used to
/// locate it in `before`'s parse; first match wins.
fn find_update_revert(before: &str, after: &str, old_title_hint: &str) -> Option<UpdateRevert> {
    let before_flat = org_parser::parse_headings_from_lines(&PathBuf::new(), before.lines());
    let after_flat = org_parser::parse_headings_from_lines(&PathBuf::new(), after.lines());
    if before_flat.len() != after_flat.len() {
        return None;
    }

    let idx = before_flat.iter().position(|h| h.title == old_title_hint)?;
    let before_heading = &before_flat[idx];
    let after_heading = &after_flat[idx];
    if before_heading.level != after_heading.level {
        return None;
    }

    let mut payload = HeadingUpdatePayload::default();
    if before_heading.title != after_heading.title {
        payload.title = Some(before_heading.title.clone());
    }
    if before_heading.state != after_heading.state {
        // Some("") is this payload's own "clear the state" sentinel (see
        // build_org_heading_block_inner: `new_state.is_empty()` -> no
        // state) -- unwrap_or_default() already produces exactly that when
        // the heading had no state at all.
        payload.state = Some(before_heading.state.clone().unwrap_or_default());
    }
    if before_heading.content != after_heading.content {
        payload.content = Some(before_heading.content.clone());
    }

    if payload.title.is_none() && payload.state.is_none() && payload.content.is_none() {
        return None;
    }

    // From `after`: when there is no CUSTOM_ID and the fallback is a
    // title, the current file has the newest one -- using the old title
    // fails for exactly the title-change case. Identical either way when a
    // CUSTOM_ID is present, which this revert never changes.
    Some(UpdateRevert { identity: heading_identity::capture_identity(after_heading), payload })
}

/// Where a restored heading's lines get spliced back in: before its
/// original next sibling, so it reappears where it sat. Appending under
/// the parent is the fallback for when that sibling is gone or never
/// existed.
enum ReinsertAnchor {
    /// No parent -- a top-level heading, appended at the very end of the file.
    TopLevel,
    /// Inserted as the last child of this (current) parent line.
    AppendToParent { parent_line_no: usize },
    /// Inserted directly before this sibling's line, restoring the
    /// heading's original relative position.
    BeforeSibling { sibling_line_no: usize },
}

/// What applying a feasible restore actually does -- `Reinsert` (the
/// HeadingDeleted case, see ReinsertAnchor) or `PatchFields` (the
/// HeadingUpdated case: only the differing fields captured by
/// find_update_revert get applied, onto whichever CURRENT line the target
/// heading resolved to).
enum RestoreAction {
    Reinsert { anchor: ReinsertAnchor, raw_lines: Vec<String> },
    PatchFields { target_line_no: usize, payload: HeadingUpdatePayload },
}

enum HeadingRestoreOutcome {
    Feasible(RestoreAction),
    NotFeasible { reason: String },
}

fn resolve_restore_delete(removed: RemovedHeading, current_flat: &[Heading]) -> HeadingRestoreOutcome {
    // `Some(None)` = top-level (no parent to resolve at all), `Some(Some(line_no))` =
    // parent still exists at `line_no`, `None` = had a parent but it's gone.
    let resolved_parent: Option<Option<usize>> = match &removed.parent_identity {
        None => Some(None),
        Some(identity) => heading_identity::resolve(current_flat, identity).map(|h| Some(h.line_no)),
    };

    // The next-sibling anchor is only trusted if it resolves to a heading
    // that's STILL a direct child of that same (resolved) parent -- a
    // same-titled/same-CUSTOM_ID heading that got refiled somewhere else in
    // the meantime must never redirect the restore into the wrong file/
    // project, even if it happens to still exist somewhere.
    if let Some(expected_parent) = resolved_parent {
        if let Some(sibling_identity) = &removed.next_sibling_identity {
            if let Some(sibling) = heading_identity::resolve(current_flat, sibling_identity) {
                let sibling_idx = current_flat.iter().position(|h| h.line_no == sibling.line_no).unwrap();
                let sibling_parent = heading_utils::ancestors_of(current_flat, sibling_idx).into_iter().next().map(|p| p.line_no);
                if sibling_parent == expected_parent {
                    let anchor = ReinsertAnchor::BeforeSibling { sibling_line_no: sibling.line_no };
                    return HeadingRestoreOutcome::Feasible(RestoreAction::Reinsert { anchor, raw_lines: removed.raw_lines });
                }
            }
        }
    }

    match resolved_parent {
        Some(None) => {
            HeadingRestoreOutcome::Feasible(RestoreAction::Reinsert { anchor: ReinsertAnchor::TopLevel, raw_lines: removed.raw_lines })
        }
        Some(Some(parent_line_no)) => {
            let anchor = ReinsertAnchor::AppendToParent { parent_line_no };
            HeadingRestoreOutcome::Feasible(RestoreAction::Reinsert { anchor, raw_lines: removed.raw_lines })
        }
        None => HeadingRestoreOutcome::NotFeasible {
            reason: crate::t!(
                "Die übergeordnete Überschrift \"{}\" existiert nicht mehr.",
                "The parent heading \"{}\" no longer exists.",
                removed.parent_title.as_deref().unwrap_or("")
            ),
        },
    }
}

fn resolve_restore_update(revert: UpdateRevert, current_flat: &[Heading]) -> HeadingRestoreOutcome {
    match heading_identity::resolve(current_flat, &revert.identity) {
        Some(h) => {
            let target_line_no = h.line_no;
            HeadingRestoreOutcome::Feasible(RestoreAction::PatchFields { target_line_no, payload: revert.payload })
        }
        None => HeadingRestoreOutcome::NotFeasible {
            reason: crate::t!(
                "Die Überschrift konnte im aktuellen Dokument nicht mehr eindeutig gefunden werden.",
                "The heading could not be identified unambiguously in the current document."
            ),
        },
    }
}

/// Pure, in-memory splice -- mirrors commands_headings::create_heading_in_file's
/// insertion logic (find_subtree_end_line + insert-after, or append at file
/// end) but against a COPY of CURRENT_LINES, no disk I/O. Deliberately does
/// NOT reflect the touch_ancestors LAST_ACTIVITY bump `apply()` below also
/// performs -- a cosmetic simplification (see this feature's own plan doc),
/// the actual restored CONTENT here is exactly what gets written.
fn build_preview_reinsert(
    current_lines: &[String],
    current_flat: &[Heading],
    anchor: &ReinsertAnchor,
    raw_lines: &[String],
) -> Vec<String> {
    let mut lines = current_lines.to_vec();
    match anchor {
        ReinsertAnchor::BeforeSibling { sibling_line_no } => {
            if current_flat.iter().any(|h| h.line_no == *sibling_line_no) {
                let insert_at = sibling_line_no - 1;
                lines.splice(insert_at..insert_at, raw_lines.iter().cloned());
            }
        }
        ReinsertAnchor::AppendToParent { parent_line_no } => {
            if let Some(parent_idx) = current_flat.iter().position(|h| h.line_no == *parent_line_no) {
                let insert_after = heading_utils::find_subtree_end_line(current_flat, parent_idx);
                lines.splice(insert_after..insert_after, raw_lines.iter().cloned());
            }
        }
        ReinsertAnchor::TopLevel => lines.extend(raw_lines.iter().cloned()),
    }
    lines
}

/// Same in-memory-only spirit as build_preview_reinsert above, but a
/// REPLACE at the target's own current position instead of an insert.
fn build_preview_patch(
    current_lines: &[String],
    current_flat: &[Heading],
    target_line_no: usize,
    payload: &HeadingUpdatePayload,
) -> Vec<String> {
    let mut lines = current_lines.to_vec();
    if let Some(idx) = current_flat.iter().position(|h| h.line_no == target_line_no) {
        let heading = &current_flat[idx];
        let new_block = heading_utils::build_org_heading_block(heading, payload);
        let end = heading.last_line.min(lines.len());
        let start = (heading.line_no - 1).min(end);
        lines.splice(start..end, new_block);
    }
    lines
}

enum OutcomeInput {
    Delete(RemovedHeading),
    Update(UpdateRevert),
}

/// What check() and apply() both need, recomputed each time rather than
/// trusting a previously shown preview -- the file may have changed in
/// between. `title`, `current_lines` and `current_flat` are only
/// meaningful when `outcome` is `Feasible`.
struct Computation {
    path: PathBuf,
    outcome: HeadingRestoreOutcome,
    title: String,
    current_lines: Vec<String>,
    current_flat: Vec<Heading>,
}

fn not_feasible(reason: impl Into<String>) -> Computation {
    Computation {
        path: PathBuf::new(),
        outcome: HeadingRestoreOutcome::NotFeasible { reason: reason.into() },
        title: String::new(),
        current_lines: Vec::new(),
        current_flat: Vec::new(),
    }
}

/// Entry kinds that only support the whole-file restore below.
///
/// A bulk action records one whole-file snapshot pair per touched file and
/// nothing per heading, so there is no way to re-diff N individual
/// headings back out of it. The only well-defined undo is writing every
/// touched file back to its `before` text -- a bigger hammer, since it
/// also discards unrelated edits made since, which is why it stays a
/// separate path.
fn is_bulk_snapshot_kind(kind: &HistoryEntryKind) -> bool {
    matches!(
        kind,
        HistoryEntryKind::HeadingsBulkDeleted { .. }
            | HistoryEntryKind::HeadingsBulkUpdated { .. }
            | HistoryEntryKind::HeadingsBulkRefiled { .. }
    )
}

/// The COUNT carried over into the new HeadingsBulkRestored entry apply_bulk
/// records -- read back off the ORIGINAL entry's own kind so the restore's
/// own history label reads symmetrically ("7 Einträge ... wiederhergestellt"
/// for a "7 Einträge ... gelöscht" entry). Only ever called after
/// is_bulk_snapshot_kind has already confirmed KIND is one of these three.
fn bulk_count(kind: &HistoryEntryKind) -> u32 {
    match kind {
        HistoryEntryKind::HeadingsBulkDeleted { count, .. }
        | HistoryEntryKind::HeadingsBulkUpdated { count, .. }
        | HistoryEntryKind::HeadingsBulkRefiled { count, .. } => *count,
        _ => 0,
    }
}

/// check()'s whole-file counterpart. Infeasible only when nothing was
/// stored to restore from; a missing current file is fine, since restoring
/// recreates it.
///
/// The first snapshot's file is what the preview diffs against; any other
/// touched files are surfaced via `additional_filenames`, so the user
/// knows the restore is not scoped to the previewed one.
fn check_bulk(entry: &crate::core::models::HistoryEntry) -> HeadingRestoreCheck {
    let Some(primary) = entry.snapshots.first() else {
        return HeadingRestoreCheck {
            feasible: false,
            reason: Some(crate::t!(
                "Kein Inhalt gespeichert -- nicht wiederherstellbar.",
                "No content was recorded -- nothing to restore."
            )),
            raw_block: None,
            current_text: None,
            preview_text: None,
            additional_filenames: Vec::new(),
        };
    };

    let current_text = std::fs::read_to_string(&primary.path).unwrap_or_default();
    let preview_text = primary.before.clone().unwrap_or_default();
    let additional_filenames = entry.snapshots[1..]
        .iter()
        .map(|s| PathBuf::from(&s.path).file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_else(|| s.path.clone()))
        .collect();

    HeadingRestoreCheck {
        feasible: true,
        reason: None,
        raw_block: None,
        current_text: Some(current_text),
        preview_text: Some(preview_text),
        additional_filenames,
    }
}

/// apply()'s whole-file counterpart: writes every touched file back to its
/// `before` text, recreating it if missing and removing it if `before` is
/// `None`.
///
/// Stops on the first failure rather than leaving partial state, with a
/// message naming which file -- `io::Error`'s own Display never includes
/// the path, which is why every write is wrapped here.
fn apply_bulk(conn: &Connection, index_manager: &IndexManager, entry: &crate::core::models::HistoryEntry) -> Result<(), String> {
    if entry.snapshots.is_empty() {
        return Err(crate::t!("Kein Inhalt gespeichert -- nicht wiederherstellbar.", "No content was recorded -- nothing to restore."));
    }

    // Pre-flight: verify every file this would touch is actually writable
    // (its parent directory still exists) BEFORE changing anything -- a
    // partial multi-file apply (file 1 reverted, file 2 fails) would leave
    // file 1's already-applied change with no history entry recorded to
    // undo it via Ctrl+Z (record() below only runs once, after every file
    // succeeds), a genuinely confusing half-applied state for exactly the
    // multi-file case (a cross-file HeadingsBulkRefiled) this guards
    // against -- named per-file so the user knows exactly which one.
    for snap in &entry.snapshots {
        if snap.before.is_some() {
            let path = PathBuf::from(&snap.path);
            if path.parent().is_some_and(|p| !p.exists()) {
                return Err(crate::t!(
                    "Der Ordner für \"{}\" existiert nicht mehr -- Wiederherstellen nicht möglich.",
                    "The folder for \"{}\" no longer exists -- restoring is not possible.",
                    snap.path
                ));
            }
        }
    }

    let mut restored_snapshots = Vec::with_capacity(entry.snapshots.len());
    for snap in &entry.snapshots {
        let path = PathBuf::from(&snap.path);
        match &snap.before {
            Some(content) => {
                // Unguarded on purpose: this puts back exactly what was
                // snapshotted, including a generated mirror that was
                // legitimately rewritten by its generator. Atomic all the
                // same -- a half-written restore is the one failure this
                // path must not have.
                fs_utils::write_atomic(&path, content)
                    .map_err(|e| crate::t!("Wiederherstellen von \"{}\" fehlgeschlagen: {e}", "Restoring \"{}\" failed: {e}", snap.path))?;
                index_manager.update_file(&path);
            }
            None => {
                let _ = std::fs::remove_file(&path);
                index_manager.remove_file(&path);
            }
        }
        // Swapped before/after -- undoing THIS new entry (via the normal
        // global undo stack) re-applies the original bulk action, same
        // convention Reinsert/PatchFields already use below.
        restored_snapshots.push(FileSnapshot { path: snap.path.clone(), before: snap.after.clone(), after: snap.before.clone() });
    }

    let primary_filename =
        PathBuf::from(&entry.snapshots[0].path).file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
    let _ = history::record(
        conn,
        HistoryEntryKind::HeadingsBulkRestored { filename: primary_filename, count: bulk_count(&entry.kind) },
        false,
        restored_snapshots,
    );

    Ok(())
}

fn compute(entry: &crate::core::models::HistoryEntry) -> Result<Computation, String> {
    let Some(snapshot) = entry.snapshots.first() else {
        return Ok(not_feasible(&crate::t!(
            "Kein Inhalt gespeichert -- nicht wiederherstellbar.",
            "No content was recorded -- nothing to restore."
        )));
    };
    let Some(before) = snapshot.before.as_deref() else {
        return Ok(not_feasible(&crate::t!(
            "Kein Inhalt gespeichert -- nicht wiederherstellbar.",
            "No content was recorded -- nothing to restore."
        )));
    };
    let after = snapshot.after.as_deref().unwrap_or("");
    let path = PathBuf::from(&snapshot.path);

    let input = match &entry.kind {
        HistoryEntryKind::HeadingDeleted { .. } => {
            let Some(removed) = find_removed_heading(before, after) else {
                return Ok(not_feasible(&crate::t!(
                    "Der gelöschte Bereich konnte nicht eindeutig bestimmt werden.",
                    "The deleted range could not be determined unambiguously."
                )));
            };
            OutcomeInput::Delete(removed)
        }
        HistoryEntryKind::HeadingUpdated { title, .. } => {
            let Some(revert) = find_update_revert(before, after, title) else {
                return Ok(not_feasible(&crate::t!(
                    "Die Änderung konnte nicht eindeutig bestimmt werden.",
                    "The change could not be determined unambiguously."
                )));
            };
            OutcomeInput::Update(revert)
        }
        _ => {
            return Err(crate::t!(
                "Dieser Eintrags-Typ unterstützt kein gezieltes Wiederherstellen.",
                "This kind of history entry cannot be restored selectively."
            ))
        }
    };

    let Ok(current_lines) = fs_utils::read_lines(&path) else {
        return Ok(not_feasible(&crate::t!("Datei existiert nicht mehr.", "The file no longer exists.")));
    };
    // Parsed from the very lines just read, not from the index cache: the
    // previews below splice INTO current_lines using line numbers out of
    // this parse, so the two have to be one snapshot. A cache that the
    // watcher has not caught up with yet (or a sync pull mid-session)
    // would put the restored block at the wrong place in the file.
    let current_flat = org_parser::parse_headings_from_lines(&path, &current_lines);

    let (outcome, title) = match input {
        OutcomeInput::Delete(removed) => {
            let title = removed.title.clone();
            (resolve_restore_delete(removed, &current_flat), title)
        }
        OutcomeInput::Update(revert) => {
            // The CURRENT (pre-revert) title -- same convention
            // update_heading_in_file itself uses for its own HeadingUpdated
            // entries, so a revert's own history label reads the same way
            // any other edit's does.
            let title = heading_identity::resolve(&current_flat, &revert.identity).map(|h| h.title.clone()).unwrap_or_default();
            (resolve_restore_update(revert, &current_flat), title)
        }
    };

    Ok(Computation { path, outcome, title, current_lines, current_flat })
}

/// Whether `entry_id` can be restored, with a preview of the result.
/// Read-only; apply() reruns this rather than trusting what was shown.
///
/// A deleted or updated heading gets the targeted single-heading restore,
/// re-finding that heading in the current AST. A bulk entry gets the
/// whole-file restore, since no per-heading identity was stored for it.
/// Any other kind never recorded enough to restore anything.
pub fn check(conn: &Connection, entry_id: i64) -> Result<HeadingRestoreCheck, String> {
    let entry =
        store::get(conn, entry_id).to_cmd()?.ok_or_else(|| crate::t!("History-Eintrag nicht gefunden.", "History entry not found."))?;
    if !matches!(entry.kind, HistoryEntryKind::HeadingDeleted { .. } | HistoryEntryKind::HeadingUpdated { .. }) {
        if is_bulk_snapshot_kind(&entry.kind) {
            return Ok(check_bulk(&entry));
        }
        return Err(crate::t!(
            "Dieser Eintrags-Typ unterstützt kein gezieltes Wiederherstellen.",
            "This kind of history entry cannot be restored selectively."
        ));
    }

    let computation = compute(&entry)?;
    match computation.outcome {
        HeadingRestoreOutcome::NotFeasible { reason } => Ok(HeadingRestoreCheck {
            feasible: false,
            reason: Some(reason),
            raw_block: None,
            current_text: None,
            preview_text: None,
            additional_filenames: Vec::new(),
        }),
        HeadingRestoreOutcome::Feasible(action) => {
            let (raw_block, preview) = match &action {
                RestoreAction::Reinsert { anchor, raw_lines } => {
                    let preview = build_preview_reinsert(&computation.current_lines, &computation.current_flat, anchor, raw_lines);
                    (raw_lines.join("\n"), preview)
                }
                RestoreAction::PatchFields { target_line_no, payload } => {
                    let raw = computation
                        .current_flat
                        .iter()
                        .find(|h| h.line_no == *target_line_no)
                        .map(|h| heading_utils::build_org_heading_block(h, payload).join("\n"))
                        .unwrap_or_default();
                    let preview = build_preview_patch(&computation.current_lines, &computation.current_flat, *target_line_no, payload);
                    (raw, preview)
                }
            };
            Ok(HeadingRestoreCheck {
                feasible: true,
                reason: None,
                raw_block: Some(raw_block),
                current_text: Some(computation.current_lines.join("\n")),
                preview_text: Some(preview.join("\n")),
                additional_filenames: Vec::new(),
            })
        }
    }
}

/// Applies a feasible restore or revert:
/// - `Reinsert`: the same parent-insertion path as creating a heading, and
///   recorded as an independently undoable `HeadingRestored` entry.
/// - `PatchFields`: the same write path as updating a heading, minus the
///   trigger cascade -- a revert undoes a prior action rather than being a
///   new one that should re-fire automation. Recorded as a normal
///   `HeadingUpdated`, since a revert is an edit.
///
/// History goes through `record` directly rather than `record_best_effort`,
/// whose re-lock would deadlock on the guard the caller already holds. A
/// failed recording is swallowed either way, so it never fails an
/// otherwise-successful restore.
pub fn apply(conn: &Connection, index_manager: &IndexManager, entry_id: i64) -> Result<(), String> {
    let entry =
        store::get(conn, entry_id).to_cmd()?.ok_or_else(|| crate::t!("History-Eintrag nicht gefunden.", "History entry not found."))?;
    if !matches!(entry.kind, HistoryEntryKind::HeadingDeleted { .. } | HistoryEntryKind::HeadingUpdated { .. }) {
        if is_bulk_snapshot_kind(&entry.kind) {
            return apply_bulk(conn, index_manager, &entry);
        }
        return Err(crate::t!(
            "Dieser Eintrags-Typ unterstützt kein gezieltes Wiederherstellen.",
            "This kind of history entry cannot be restored selectively."
        ));
    }

    let computation = compute(&entry)?;
    let HeadingRestoreOutcome::Feasible(action) = computation.outcome else {
        let HeadingRestoreOutcome::NotFeasible { reason } = computation.outcome else { unreachable!() };
        return Err(reason);
    };

    let path = computation.path;
    let title = computation.title;
    let filename = path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();

    match action {
        RestoreAction::Reinsert { anchor, raw_lines } => {
            let (before, result, after) = history::snapshot_around(&path, || {
                match anchor {
                    ReinsertAnchor::BeforeSibling { sibling_line_no } => {
                        let mut edit =
                            heading_utils::open_edit(&path, &heading_utils::target_at_line(sibling_line_no), "Nachbar-Überschrift")?;
                        edit.insert_before(raw_lines.clone())?;

                        // The sibling's ancestor chain is the restored
                        // heading's too, being inserted as the same
                        // parent's child right before it.
                        edit.touch_ancestors()?;
                        edit.commit()?;
                    }
                    ReinsertAnchor::AppendToParent { parent_line_no } => {
                        let mut edit = heading_utils::open_edit(&path, &heading_utils::target_at_line(parent_line_no), "Ziel-Überschrift")?;
                        edit.insert_after_subtree(raw_lines.clone())?;
                        edit.touch_self_and_ancestors()?;
                        edit.commit()?;
                    }
                    ReinsertAnchor::TopLevel => {
                        fs_utils::append_lines(&path, raw_lines.clone()).to_cmd()?;
                    }
                }
                index_manager.update_file(&path);
                Ok(())
            });

            if result.is_ok() {
                let _ = history::record(
                    conn,
                    HistoryEntryKind::HeadingRestored { filename, title },
                    false,
                    vec![FileSnapshot { path: path.to_string_lossy().into_owned(), before, after }],
                );
            }
            result
        }
        RestoreAction::PatchFields { target_line_no, payload } => {
            let (before, result, after) = history::snapshot_around(&path, || {
                // Addressed by line rather than by identity: this puts
                // back a recorded position, and the heading that sits
                // there is the one the snapshot describes. Refuses when
                // nothing does, with the same message as before.
                let mut edit = heading_utils::open_edit(&path, &heading_utils::target_at_line(target_line_no), "Ziel-Überschrift")?;
                edit.write(&payload)?;
                edit.touch_ancestors()?;
                edit.commit()?;
                index_manager.update_file(&path);
                Ok(())
            });

            if result.is_ok() {
                let _ = history::record(
                    conn,
                    HistoryEntryKind::HeadingUpdated { filename, title },
                    false,
                    vec![FileSnapshot { path: path.to_string_lossy().into_owned(), before, after }],
                );
            }
            result
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::history::store;
    use tempfile::TempDir;

    fn conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        store::init_schema(&conn).unwrap();
        conn
    }

    fn record_delete(conn: &Connection, path: &std::path::Path, before: &str, after: &str, title: &str) -> i64 {
        history::record(
            conn,
            HistoryEntryKind::HeadingDeleted {
                filename: path.file_name().unwrap().to_string_lossy().into_owned(),
                title: title.to_string(),
            },
            false,
            vec![FileSnapshot {
                path: path.to_string_lossy().into_owned(),
                before: Some(before.to_string()),
                after: Some(after.to_string()),
            }],
        )
        .unwrap();
        store::list(conn, 1).unwrap()[0].id
    }

    fn record_update(conn: &Connection, path: &std::path::Path, before: &str, after: &str, old_title: &str) -> i64 {
        history::record(
            conn,
            HistoryEntryKind::HeadingUpdated {
                filename: path.file_name().unwrap().to_string_lossy().into_owned(),
                title: old_title.to_string(),
            },
            false,
            vec![FileSnapshot {
                path: path.to_string_lossy().into_owned(),
                before: Some(before.to_string()),
                after: Some(after.to_string()),
            }],
        )
        .unwrap();
        store::list(conn, 1).unwrap()[0].id
    }

    /// Undo splices into the file it has just read, at line numbers worked
    /// out from the headings in it -- so those headings have to come from
    /// that same read. They used to come from the index cache, and an
    /// index the watcher had not caught up with (a sync pull, an external
    /// edit) put the restored block wherever those stale numbers pointed.
    #[test]
    fn restoring_into_a_file_that_changed_behind_the_index_still_lands_in_the_right_place() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("projects.org");
        let before =
            "* Project A                                                        :project:\n** TODO Subtask One\n** TODO Subtask Two\n";
        let after = "* Project A                                                        :project:\n** TODO Subtask One\n";
        std::fs::write(&path, after).unwrap();
        let conn = conn();
        let index = IndexManager::new();
        index.update_file(&path);
        let id = record_delete(&conn, &path, before, after, "Subtask Two");

        // A sync pull drops another project in above it. The index still
        // holds the pre-pull parse, where Project A started at line 1.
        std::fs::write(
            &path,
            "* Project Z                                                        :project:\n** TODO Fremde Aufgabe\n* Project A                                                        :project:\n** TODO Subtask One\n",
        ).unwrap();

        apply(&conn, &index, id).unwrap();

        let result = std::fs::read_to_string(&path).unwrap();
        assert!(result.contains("** TODO Subtask One\n** TODO Subtask Two"), "restored under Project A: {result}");
        assert!(result.contains("* Project Z"), "{result}");
        assert!(result.contains("** TODO Fremde Aufgabe\n* Project A"), "the pulled-in project kept its own subtask: {result}");
    }

    #[test]
    fn restores_deleted_subtask_as_last_child_of_its_still_existing_project() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("projects.org");
        let before =
            "* Project A                                                        :project:\n** TODO Subtask One\n** TODO Subtask Two\n";
        let after = "* Project A                                                        :project:\n** TODO Subtask One\n";
        std::fs::write(&path, after).unwrap();
        let conn = conn();
        let index = IndexManager::new();
        let id = record_delete(&conn, &path, before, after, "Subtask Two");

        let check = check(&conn, id).unwrap();
        assert!(check.feasible);
        assert!(check.preview_text.unwrap().contains("Subtask Two"));

        apply(&conn, &index, id).unwrap();
        let result = std::fs::read_to_string(&path).unwrap();
        assert!(result.contains("Subtask Two"), "restored heading present: {result}");
        assert!(result.find("Subtask One").unwrap() < result.find("Subtask Two").unwrap());
    }

    /// The core order-preservation guarantee: deleting the MIDDLE of three
    /// subtasks and restoring it must put it back exactly where it was --
    /// between its original neighbors -- not appended after the last one
    /// (which is what a naive "always insert as the parent's last child"
    /// restore would do, silently reordering Three ahead of Two forever).
    #[test]
    fn restoring_a_deleted_middle_subtask_reinserts_it_between_its_original_neighbors() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("projects.org");
        let before =
            "* Project A                                                        :project:\n** TODO One\n** TODO Two\n** TODO Three\n";
        let after = "* Project A                                                        :project:\n** TODO One\n** TODO Three\n";
        std::fs::write(&path, after).unwrap();
        let conn = conn();
        let index = IndexManager::new();
        let id = record_delete(&conn, &path, before, after, "Two");

        apply(&conn, &index, id).unwrap();
        let result = std::fs::read_to_string(&path).unwrap();
        let titles: Vec<&str> = ["One", "Two", "Three"].iter().map(|t| result.find(t).map(|_| *t).unwrap()).collect();
        let mut positions: Vec<(usize, &str)> = titles.iter().map(|t| (result.find(t).unwrap(), *t)).collect();
        positions.sort();
        assert_eq!(
            positions.iter().map(|(_, t)| *t).collect::<Vec<_>>(),
            vec!["One", "Two", "Three"],
            "restored order must exactly match the original document order"
        );
    }

    /// A deleted first child must go back before the sibling that
    /// followed it, not after every surviving one.
    #[test]
    fn restoring_a_deleted_first_subtask_reinserts_it_before_its_original_next_sibling() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("projects.org");
        let before =
            "* Project A                                                        :project:\n** TODO One\n** TODO Two\n** TODO Three\n";
        let after = "* Project A                                                        :project:\n** TODO Two\n** TODO Three\n";
        std::fs::write(&path, after).unwrap();
        let conn = conn();
        let index = IndexManager::new();
        let id = record_delete(&conn, &path, before, after, "One");

        apply(&conn, &index, id).unwrap();
        let result = std::fs::read_to_string(&path).unwrap();
        let mut positions: Vec<(usize, &str)> = ["One", "Two", "Three"].iter().map(|t| (result.find(t).unwrap(), *t)).collect();
        positions.sort();
        assert_eq!(positions.iter().map(|(_, t)| *t).collect::<Vec<_>>(), vec!["One", "Two", "Three"]);
    }

    /// Graceful degradation: if the original next sibling is ALSO gone by
    /// the time of the restore (its own separate deletion, a refile
    /// elsewhere, ...), there's no original position left to anchor to --
    /// falls back to appending as the parent's last child, same as this
    /// feature's very first (pre-order-preservation) behavior, rather than
    /// failing the restore entirely.
    #[test]
    fn restore_falls_back_to_append_when_the_original_next_sibling_no_longer_exists() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("projects.org");
        // The history entry's own before/after snapshot pair represents
        // ONLY "Two"'s own deletion (a clean, isolatable single-heading
        // removal) -- "Three" is still present in both.
        let before =
            "* Project A                                                        :project:\n** TODO One\n** TODO Two\n** TODO Three\n";
        let after = "* Project A                                                        :project:\n** TODO One\n** TODO Three\n";
        // But the file ON DISK, right now, has since moved on further --
        // "Three" was ALSO independently removed at some later point, so by
        // the time THIS restore runs there's no original next-sibling left
        // to anchor to at all.
        std::fs::write(&path, "* Project A                                                        :project:\n** TODO One\n").unwrap();
        let conn = conn();
        let index = IndexManager::new();
        let id = record_delete(&conn, &path, before, after, "Two");

        let check = check(&conn, id).unwrap();
        assert!(check.feasible);
        apply(&conn, &index, id).unwrap();
        let result = std::fs::read_to_string(&path).unwrap();
        assert!(result.find("One").unwrap() < result.find("Two").unwrap(), "falls back to appended-after-surviving-siblings");
    }

    /// Safety rail on the order-preservation anchor: a heading sharing the
    /// deleted subtask's original next-sibling's title, but which has since
    /// been refiled into a DIFFERENT project, must never be mistaken for
    /// that original sibling -- anchoring to it would silently restore the
    /// deleted heading into the wrong project entirely, worse than the
    /// plain append-at-end fallback ever was.
    #[test]
    fn restore_does_not_anchor_to_a_same_titled_heading_that_moved_to_a_different_project() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("projects.org");
        let before =
            "* Project A                                                        :project:\n** TODO One\n** TODO Two\n** TODO Three\n";
        let after = "* Project A                                                        :project:\n** TODO One\n** TODO Three\n";
        // "Two" (no CUSTOM_ID, so title-fallback identity) now ALSO exists
        // under an unrelated Project B -- must not be used as the restore
        // anchor for Project A's own deleted "Two".
        std::fs::write(
            &path,
            format!("{after}* Project B                                                        :project:\n** TODO Two\n"),
        )
        .unwrap();
        let conn = conn();
        let index = IndexManager::new();
        let id = record_delete(&conn, &path, before, after, "Two");

        apply(&conn, &index, id).unwrap();
        let result = std::fs::read_to_string(&path).unwrap();
        let project_a_end = result.find("Project B").unwrap();
        let restored_two_pos = result[..project_a_end].find("Two");
        assert!(restored_two_pos.is_some(), "restored under Project A, not left missing entirely");
        assert!(
            result[..project_a_end].find("One").unwrap() < restored_two_pos.unwrap(),
            "appended after Project A's own surviving sibling, not before Project B's unrelated \"Two\""
        );
    }

    #[test]
    fn not_feasible_when_the_parent_project_no_longer_exists() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("projects.org");
        let before = "* Project A                                                        :project:\n** TODO Subtask One\n";
        let after = "* Project A                                                        :project:\n";
        std::fs::write(&path, "* Project B                                                        :project:\n").unwrap();
        let conn = conn();
        let index = IndexManager::new();
        let id = record_delete(&conn, &path, before, after, "Subtask One");

        let check = check(&conn, id).unwrap();
        assert!(!check.feasible);
        assert!(check.reason.unwrap().contains("Project A"));
        assert!(apply(&conn, &index, id).is_err());
    }

    #[test]
    fn deleted_top_level_heading_is_always_feasible_appended_at_file_end() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("projects.org");
        let before = "* Project A                                                        :project:\n* Project B                                                        :project:\n";
        let after = "* Project A                                                        :project:\n";
        std::fs::write(&path, after).unwrap();
        let conn = conn();
        let index = IndexManager::new();
        let id = record_delete(&conn, &path, before, after, "Project B");

        let check = check(&conn, id).unwrap();
        assert!(check.feasible);

        apply(&conn, &index, id).unwrap();
        assert!(std::fs::read_to_string(&path).unwrap().contains("Project B"));
    }

    #[test]
    fn oversized_snapshot_reports_not_feasible_instead_of_restoring() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("projects.org");
        std::fs::write(&path, "* Project A                                                        :project:\n").unwrap();
        let conn = conn();
        // No snapshot content at all -- same shape core::history::record
        // stores for an over-MAX_SNAPSHOT_BYTES action.
        history::record(&conn, HistoryEntryKind::HeadingDeleted { filename: "projects.org".into(), title: "X".into() }, false, vec![])
            .unwrap();
        let id = store::list(&conn, 1).unwrap()[0].id;

        let check = check(&conn, id).unwrap();
        assert!(!check.feasible);
        assert!(check.reason.is_some());
    }

    /// Real-world edge case a hand-written before/after pair doesn't
    /// exercise: delete_heading_in_file ALSO calls touch_ancestors on the
    /// deleted heading's ancestors (see that command's own body), which
    /// rewrites the parent's PROPERTIES drawer (LAST_ACTIVITY bump) -- a
    /// second, unrelated change elsewhere in the file. This reproduces that
    /// exact real code path (not a synthetic string) to prove
    /// find_removed_heading still isolates the actual deletion correctly
    /// instead of bailing out as "ambiguous" the moment a second diff hunk
    /// shows up.
    #[test]
    fn restore_works_when_the_real_delete_also_touched_the_parents_last_activity() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("projects.org");
        std::fs::write(
            &path,
            concat!(
                "* Project                                                          :project:\n",
                ":PROPERTIES:\n",
                ":LAST_ACTIVITY: 2020-01-01 Wed 00:00\n",
                ":END:\n",
                "** TODO Keep me\n",
                "** TODO Delete me\n",
            ),
        )
        .unwrap();
        let index = IndexManager::new();

        // Exact body of commands_headings::delete_heading_in_file, minus the
        // tauri::State wrapper (same convention as
        // commands_headings::tests::creating_a_subtask_after_a_state_change...).
        let flat = index.get_headings_or_parse(&path);
        let idx = flat.iter().position(|h| h.title == "Delete me").unwrap();
        let before = std::fs::read_to_string(&path).ok();
        let mut edit =
            heading_utils::open_edit(&path, &heading_utils::target_at_line(flat[idx].line_no), &crate::core::errors::subject::heading())
                .unwrap();
        edit.delete_subtree().unwrap();
        edit.touch_ancestors().unwrap();
        edit.commit().unwrap();
        index.update_file(&path);
        let after = std::fs::read_to_string(&path).ok();

        assert_ne!(before, after, "sanity check the fixture actually changed");
        let removed = find_removed_heading(before.as_deref().unwrap(), after.as_deref().unwrap());
        assert!(removed.is_some(), "must isolate the deletion despite the ancestor's LAST_ACTIVITY also changing");
        let removed = removed.unwrap();
        assert_eq!(removed.title, "Delete me");
        assert_eq!(removed.parent_title.as_deref(), Some("Project"));
        assert!(!std::fs::read_to_string(&path).unwrap().contains("Delete me"));

        let conn = conn();
        let id = record_delete(&conn, &path, before.as_ref().unwrap(), after.as_ref().unwrap(), "Delete me");
        let checked = check(&conn, id).unwrap();
        assert!(checked.feasible);
        apply(&conn, &index, id).unwrap();
        assert!(std::fs::read_to_string(&path).unwrap().contains("Delete me"), "restored despite the ancestor property touch");
    }

    #[test]
    fn check_and_apply_error_for_a_nonexistent_entry_id() {
        let conn = conn();
        let index = IndexManager::new();
        assert!(check(&conn, 999).is_err());
        assert!(apply(&conn, &index, 999).is_err());
    }

    #[test]
    fn check_and_apply_error_for_an_entry_kind_that_supports_no_targeted_restore() {
        let conn = conn();
        let index = IndexManager::new();
        history::record(
            &conn,
            HistoryEntryKind::FileEdited { filename: "a.org".into() },
            false,
            vec![FileSnapshot { path: "/a.org".into(), before: Some("v1".into()), after: Some("v2".into()) }],
        )
        .unwrap();
        let id = store::list(&conn, 1).unwrap()[0].id;

        assert!(check(&conn, id).is_err(), "only HeadingDeleted/HeadingUpdated entries support a targeted restore");
        assert!(apply(&conn, &index, id).is_err());
    }

    #[test]
    fn not_feasible_when_the_before_snapshot_is_missing() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("projects.org");
        std::fs::write(&path, "* Project A                                                        :project:\n").unwrap();
        let conn = conn();
        // A snapshot row can exist with `before: None` (e.g. if the file
        // itself didn't exist before the action) -- not a shape a real
        // HeadingDeleted entry produces, but check()/apply() must still
        // degrade cleanly rather than panic on the missing text.
        history::record(
            &conn,
            HistoryEntryKind::HeadingDeleted { filename: "projects.org".into(), title: "X".into() },
            false,
            vec![FileSnapshot { path: path.to_string_lossy().into_owned(), before: None, after: Some("* Project A :project:\n".into()) }],
        )
        .unwrap();
        let id = store::list(&conn, 1).unwrap()[0].id;

        let check = check(&conn, id).unwrap();
        assert!(!check.feasible);
        assert!(check.reason.is_some());
    }

    #[test]
    fn not_feasible_when_more_than_one_heading_was_removed_at_once() {
        // A HeadingDeleted entry's own before/after pair should never look
        // like this in practice (delete_heading_in_file always removes
        // exactly one subtree) -- but find_removed_heading must not
        // misattribute the restore to either heading if it ever does, so
        // this defends against a corrupted/unexpected snapshot instead of
        // guessing which one was "really" meant.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("projects.org");
        let before =
            "* Project A                                                        :project:\n** TODO Subtask One\n** TODO Subtask Two\n";
        let after = "* Project A                                                        :project:\n";
        std::fs::write(&path, after).unwrap();
        let conn = conn();
        let id = record_delete(&conn, &path, before, after, "Subtask One");

        let check = check(&conn, id).unwrap();
        assert!(!check.feasible);
    }

    #[test]
    fn not_feasible_when_the_removed_lines_dont_align_with_a_whole_heading() {
        // Same defensive reasoning as the test above -- a removed content
        // LINE (not a whole heading's own subtree range) can't be
        // attributed to any single heading, so this must report "not
        // computable" instead of restoring a bogus fragment.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("projects.org");
        let before = "* Project A                                                        :project:\n** TODO Subtask\nSome content line\n";
        let after = "* Project A                                                        :project:\n** TODO Subtask\n";
        std::fs::write(&path, after).unwrap();
        let conn = conn();
        let id = record_delete(&conn, &path, before, after, "Subtask");

        let check = check(&conn, id).unwrap();
        assert!(!check.feasible);
    }

    #[test]
    fn not_feasible_when_the_file_no_longer_exists_on_disk() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("projects.org");
        let before = "* Project A                                                        :project:\n** TODO Subtask One\n";
        let after = "* Project A                                                        :project:\n";
        // Deliberately never write AFTER to disk at all -- simulates the
        // file itself having been deleted/moved since.
        let conn = conn();
        let index = IndexManager::new();
        let id = record_delete(&conn, &path, before, after, "Subtask One");

        let check = check(&conn, id).unwrap();
        assert!(!check.feasible);
        assert!(check.reason.unwrap().contains("existiert nicht mehr"));
        assert!(apply(&conn, &index, id).is_err());
    }

    #[test]
    fn restore_reinserts_a_deleted_subtree_including_its_own_children() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("projects.org");
        let before = concat!(
            "* Project A                                                        :project:\n",
            "** TODO Parent Task\n",
            "*** TODO Child One\n",
            "*** TODO Child Two\n",
        );
        let after = "* Project A                                                        :project:\n";
        std::fs::write(&path, after).unwrap();
        let conn = conn();
        let index = IndexManager::new();
        let id = record_delete(&conn, &path, before, after, "Parent Task");

        let check = check(&conn, id).unwrap();
        assert!(check.feasible);
        let raw = check.raw_block.unwrap();
        assert!(raw.contains("Parent Task") && raw.contains("Child One") && raw.contains("Child Two"), "whole subtree captured: {raw}");

        apply(&conn, &index, id).unwrap();
        let result = std::fs::read_to_string(&path).unwrap();
        assert!(result.contains("Child One") && result.contains("Child Two"), "descendants restored along with their parent: {result}");
    }

    #[test]
    fn restore_resolves_a_deeply_nested_immediate_parent_not_the_root_project() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("projects.org");
        let before = concat!(
            "* Project A                                                        :project:\n",
            "** Sub Parent\n",
            "*** TODO Grandchild\n",
        );
        let after = "* Project A                                                        :project:\n** Sub Parent\n";
        std::fs::write(&path, after).unwrap();
        let conn = conn();
        let index = IndexManager::new();
        let id = record_delete(&conn, &path, before, after, "Grandchild");

        let check = check(&conn, id).unwrap();
        assert!(check.feasible);

        apply(&conn, &index, id).unwrap();
        let result = std::fs::read_to_string(&path).unwrap();
        // Restored as a child of "Sub Parent" (its own immediate parent),
        // not appended after "Project A" as if it were top-level.
        assert!(result.find("Sub Parent").unwrap() < result.find("Grandchild").unwrap());
    }

    #[test]
    fn restore_picks_the_first_matching_parent_when_titles_are_duplicated() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("projects.org");
        let before = "* Project A                                                        :project:\n** TODO Subtask One\n";
        let after = "* Project A                                                        :project:\n";
        // The CURRENT file has two headings sharing the exact same title --
        // resolve_restore_delete's documented "first match wins" behavior.
        std::fs::write(
            &path,
            concat!(
                "* Project A                                                        :project:\n",
                "* Project A                                                        :project:\n",
            ),
        )
        .unwrap();
        let conn = conn();
        let index = IndexManager::new();
        let id = record_delete(&conn, &path, before, after, "Subtask One");

        apply(&conn, &index, id).unwrap();
        let result = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = result.lines().collect();
        // Restored right after the FIRST "Project A" line, before the second one.
        let first_project = lines.iter().position(|l| l.contains("Project A")).unwrap();
        let subtask = lines.iter().position(|l| l.contains("Subtask One")).unwrap();
        let second_project = lines.iter().rposition(|l| l.contains("Project A")).unwrap();
        assert!(first_project < subtask && subtask < second_project, "restored under the first occurrence: {result}");
    }

    #[test]
    fn restore_targets_an_older_entry_independently_of_the_undo_redo_stack_position() {
        // The whole point of this feature over the existing jump_to/undo
        // stack (see this module's own doc comments): restoring one
        // specific entry must work regardless of whether OTHER, newer
        // actions have since been recorded on top of it -- no sequential
        // replay involved.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("projects.org");
        let before = "* Project A                                                        :project:\n** TODO Subtask One\n";
        let after = "* Project A                                                        :project:\n";
        std::fs::write(&path, after).unwrap();
        let conn = conn();
        let index = IndexManager::new();
        let older_id = record_delete(&conn, &path, before, after, "Subtask One");

        // A newer, unrelated action recorded afterwards -- normal undo()
        // would only ever reach THIS one, never older_id directly.
        let other_path = dir.path().join("other.org");
        std::fs::write(&other_path, "newer content").unwrap();
        history::record(
            &conn,
            HistoryEntryKind::FileEdited { filename: "other.org".into() },
            false,
            vec![FileSnapshot {
                path: other_path.to_string_lossy().into_owned(),
                before: Some("older content".into()),
                after: Some("newer content".into()),
            }],
        )
        .unwrap();

        let check = check(&conn, older_id).unwrap();
        assert!(check.feasible, "still checkable despite not being the most recent entry");
        apply(&conn, &index, older_id).unwrap();
        assert!(std::fs::read_to_string(&path).unwrap().contains("Subtask One"));
        // The unrelated newer action's own file is untouched by this.
        assert_eq!(std::fs::read_to_string(&other_path).unwrap(), "newer content");
    }

    #[test]
    fn a_successful_restore_is_itself_a_new_undoable_history_entry() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("projects.org");
        let before = "* Project A                                                        :project:\n** TODO Subtask One\n";
        let after = "* Project A                                                        :project:\n";
        std::fs::write(&path, after).unwrap();
        let conn = conn();
        let index = IndexManager::new();
        let id = record_delete(&conn, &path, before, after, "Subtask One");

        apply(&conn, &index, id).unwrap();
        assert!(std::fs::read_to_string(&path).unwrap().contains("Subtask One"));

        history::undo(&conn, &index).unwrap();
        assert!(!std::fs::read_to_string(&path).unwrap().contains("Subtask One"), "undo reverts the restore itself");
    }

    #[test]
    fn parent_resolution_prefers_custom_id_over_a_since_renamed_title() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("projects.org");
        let before = concat!(
            "* Project A                                                        :project:\n",
            ":PROPERTIES:\n",
            ":CUSTOM_ID: proj-a-stable\n",
            ":END:\n",
            "** TODO Subtask One\n",
        );
        // The project was RENAMED since the deletion (but its CUSTOM_ID is
        // unchanged) -- title-only matching would call this not feasible;
        // CUSTOM_ID matching must still find it.
        let after = concat!(
            "* Project A Renamed                                                :project:\n",
            ":PROPERTIES:\n",
            ":CUSTOM_ID: proj-a-stable\n",
            ":END:\n",
        );
        std::fs::write(&path, after).unwrap();
        let conn = conn();
        let index = IndexManager::new();
        let id = record_delete(&conn, &path, before, after, "Subtask One");

        let check = check(&conn, id).unwrap();
        assert!(check.feasible, "resolved via CUSTOM_ID despite the title change");

        apply(&conn, &index, id).unwrap();
        let result = std::fs::read_to_string(&path).unwrap();
        assert!(result.find("Project A Renamed").unwrap() < result.find("Subtask One").unwrap());
    }

    #[test]
    fn revert_title_change_restores_the_old_title_and_leaves_everything_else() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("actions.org");
        let before = "* TODO Old Title\nSCHEDULED: <2026-08-20 Thu>\n";
        let after = "* TODO New Title\nSCHEDULED: <2026-08-20 Thu>\n";
        std::fs::write(&path, after).unwrap();
        let conn = conn();
        let index = IndexManager::new();
        let id = record_update(&conn, &path, before, after, "Old Title");

        let check = check(&conn, id).unwrap();
        assert!(check.feasible);
        assert!(check.raw_block.unwrap().contains("Old Title"));

        apply(&conn, &index, id).unwrap();
        let result = std::fs::read_to_string(&path).unwrap();
        assert!(result.contains("Old Title") && !result.contains("New Title"), "{result}");
        assert!(result.contains("SCHEDULED: <2026-08-20 Thu>"), "unrelated field untouched: {result}");
    }

    #[test]
    fn revert_state_change_restores_the_old_state() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("actions.org");
        let before = "* TODO Some Task\n";
        let after = "* DONE Some Task\nCLOSED: [2026-08-13 Thu 12:00]\n";
        std::fs::write(&path, after).unwrap();
        let conn = conn();
        let index = IndexManager::new();
        let id = record_update(&conn, &path, before, after, "Some Task");

        apply(&conn, &index, id).unwrap();
        let result = std::fs::read_to_string(&path).unwrap();
        assert!(result.contains("* TODO Some Task"), "{result}");
        assert!(!result.contains("CLOSED:"), "CLOSED line removed along with reverting away from DONE: {result}");
    }

    #[test]
    fn revert_description_change_restores_the_old_content_only() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("actions.org");
        let before = "* TODO Some Task\nOld description text\n";
        let after = "* TODO Some Task\nNew description text\n";
        std::fs::write(&path, after).unwrap();
        let conn = conn();
        let index = IndexManager::new();
        let id = record_update(&conn, &path, before, after, "Some Task");

        apply(&conn, &index, id).unwrap();
        let result = std::fs::read_to_string(&path).unwrap();
        assert!(result.contains("Old description text") && !result.contains("New description text"), "{result}");
    }

    #[test]
    fn revert_does_not_clobber_a_later_unrelated_change_to_the_same_heading() {
        // The title changed (entry under test), and SEPARATELY -- later --
        // the description also changed. Reverting the title must not also
        // roll back the description, which this entry never touched.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("actions.org");
        let title_before = "* TODO Old Title\nOriginal description\n";
        let title_after = "* TODO New Title\nOriginal description\n";
        let final_state = "* TODO New Title\nEdited later, unrelated to the title revert\n";
        std::fs::write(&path, final_state).unwrap();
        let conn = conn();
        let index = IndexManager::new();
        let id = record_update(&conn, &path, title_before, title_after, "Old Title");

        apply(&conn, &index, id).unwrap();
        let result = std::fs::read_to_string(&path).unwrap();
        assert!(result.contains("Old Title"), "title reverted: {result}");
        assert!(
            result.contains("Edited later, unrelated to the title revert"),
            "later description edit preserved, not rolled back: {result}"
        );
    }

    #[test]
    fn revert_is_not_feasible_when_the_heading_no_longer_exists() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("actions.org");
        let before = "* TODO Old Title\n";
        let after = "* TODO New Title\n";
        // The heading has since been deleted entirely.
        std::fs::write(&path, "").unwrap();
        let conn = conn();
        let index = IndexManager::new();
        let id = record_update(&conn, &path, before, after, "Old Title");

        let check = check(&conn, id).unwrap();
        assert!(!check.feasible);
        assert!(apply(&conn, &index, id).is_err());
    }

    #[test]
    fn a_reverted_update_is_itself_a_new_undoable_history_entry() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("actions.org");
        let before = "* TODO Old Title\n";
        let after = "* TODO New Title\n";
        std::fs::write(&path, after).unwrap();
        let conn = conn();
        let index = IndexManager::new();
        let id = record_update(&conn, &path, before, after, "Old Title");

        apply(&conn, &index, id).unwrap();
        assert!(std::fs::read_to_string(&path).unwrap().contains("Old Title"));

        history::undo(&conn, &index).unwrap();
        assert!(std::fs::read_to_string(&path).unwrap().contains("New Title"), "undo reverts the revert itself");
    }

    /* --- whole-file bulk restore (HeadingsBulkDeleted/BulkUpdated/BulkRefiled) --- */

    fn record_bulk_delete(conn: &Connection, path: &std::path::Path, before: &str, after: &str, count: u32) -> i64 {
        history::record(
            conn,
            HistoryEntryKind::HeadingsBulkDeleted { filename: path.file_name().unwrap().to_string_lossy().into_owned(), count },
            false,
            vec![FileSnapshot {
                path: path.to_string_lossy().into_owned(),
                before: Some(before.to_string()),
                after: Some(after.to_string()),
            }],
        )
        .unwrap();
        store::list(conn, 1).unwrap()[0].id
    }

    #[test]
    fn bulk_delete_restore_reverts_the_whole_file_to_its_before_snapshot() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("projects.org");
        let before = "* Task A\n* Task B\n* Task C\n";
        let after = "* Task C\n";
        std::fs::write(&path, after).unwrap();
        let conn = conn();
        let index = IndexManager::new();
        let id = record_bulk_delete(&conn, &path, before, after, 2);

        let check = check(&conn, id).unwrap();
        assert!(check.feasible);
        assert_eq!(check.current_text.as_deref(), Some(after));
        assert_eq!(check.preview_text.as_deref(), Some(before));
        assert!(check.additional_filenames.is_empty(), "only one file involved");

        apply(&conn, &index, id).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    #[test]
    fn bulk_restore_recreates_a_file_that_no_longer_exists_on_disk() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("projects.org");
        let before = "* Task A\n* Task B\n";
        let after = "* Task B\n";
        // Never written to disk at all -- simulates the whole file having
        // been deleted/moved since the bulk action (the exact "informieren"
        // scenario) -- restoring must still work, recreating it.
        let conn = conn();
        let index = IndexManager::new();
        let id = record_bulk_delete(&conn, &path, before, after, 1);

        let check = check(&conn, id).unwrap();
        assert!(check.feasible, "a currently-missing file doesn't block restoring -- it just gets recreated");
        assert_eq!(check.current_text.as_deref(), Some(""), "no current content to show, not an error");

        apply(&conn, &index, id).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before, "file recreated with its before content");
    }

    #[test]
    fn bulk_restore_removes_a_file_that_didnt_exist_before_the_action() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("new-target.org");
        std::fs::write(&path, "* Task A\n").unwrap();
        let conn = conn();
        let index = IndexManager::new();
        // before: None -- e.g. a HeadingsBulkRefiled entry whose target was
        // a brand-new file that didn't exist before the refile.
        history::record(
            &conn,
            HistoryEntryKind::HeadingsBulkRefiled { count: 1, target_filename: "new-target.org".into() },
            false,
            vec![FileSnapshot { path: path.to_string_lossy().into_owned(), before: None, after: Some("* Task A\n".into()) }],
        )
        .unwrap();
        let id = store::list(&conn, 1).unwrap()[0].id;

        apply(&conn, &index, id).unwrap();
        assert!(!path.exists(), "restoring removes the file that didn't exist before the bulk action created it");
    }

    #[test]
    fn bulk_refile_restore_reverts_both_source_and_target_and_lists_the_additional_file() {
        let dir = TempDir::new().unwrap();
        let src_path = dir.path().join("actions.org");
        let tgt_path = dir.path().join("projects.org");
        let src_before = "* Task A\n* Task B\n";
        let src_after = "* Task B\n";
        let tgt_before = "* Project\n";
        let tgt_after = "* Project\n** Task A\n";
        std::fs::write(&src_path, src_after).unwrap();
        std::fs::write(&tgt_path, tgt_after).unwrap();
        let conn = conn();
        let index = IndexManager::new();
        // Target snapshot listed first (mirrors bulk_refile_headings' own
        // snapshot order: target always first, source appended when it
        // differs from the target).
        history::record(
            &conn,
            HistoryEntryKind::HeadingsBulkRefiled { count: 1, target_filename: "projects.org".into() },
            false,
            vec![
                FileSnapshot {
                    path: tgt_path.to_string_lossy().into_owned(),
                    before: Some(tgt_before.into()),
                    after: Some(tgt_after.into()),
                },
                FileSnapshot {
                    path: src_path.to_string_lossy().into_owned(),
                    before: Some(src_before.into()),
                    after: Some(src_after.into()),
                },
            ],
        )
        .unwrap();
        let id = store::list(&conn, 1).unwrap()[0].id;

        let check = check(&conn, id).unwrap();
        assert!(check.feasible);
        assert_eq!(
            check.additional_filenames,
            vec!["actions.org".to_string()],
            "the source file is named, even though only the target's diff is shown"
        );

        apply(&conn, &index, id).unwrap();
        assert_eq!(std::fs::read_to_string(&tgt_path).unwrap(), tgt_before, "target reverted");
        assert_eq!(std::fs::read_to_string(&src_path).unwrap(), src_before, "source reverted too, not just the previewed file");
    }

    #[test]
    fn bulk_restore_is_not_feasible_when_no_snapshot_content_was_stored() {
        let conn = conn();
        let index = IndexManager::new();
        // Same shape core::history::record stores for an over-MAX_SNAPSHOT_BYTES action.
        history::record(&conn, HistoryEntryKind::HeadingsBulkDeleted { filename: "big.org".into(), count: 500 }, false, vec![]).unwrap();
        let id = store::list(&conn, 1).unwrap()[0].id;

        let check = check(&conn, id).unwrap();
        assert!(!check.feasible);
        assert!(check.reason.is_some());
        assert!(apply(&conn, &index, id).is_err());
    }

    #[test]
    fn bulk_restore_fails_cleanly_without_touching_either_file_when_one_folder_is_gone() {
        let dir = TempDir::new().unwrap();
        let ok_path = dir.path().join("actions.org");
        let missing_dir_path = dir.path().join("deleted-folder").join("projects.org");
        std::fs::write(&ok_path, "current content").unwrap();
        // deleted-folder/ deliberately never created -- simulates the whole
        // containing folder having been removed since this bulk action.
        let conn = conn();
        let index = IndexManager::new();
        history::record(
            &conn,
            HistoryEntryKind::HeadingsBulkRefiled { count: 1, target_filename: "projects.org".into() },
            false,
            vec![
                FileSnapshot {
                    path: ok_path.to_string_lossy().into_owned(),
                    before: Some("before content".into()),
                    after: Some("current content".into()),
                },
                FileSnapshot {
                    path: missing_dir_path.to_string_lossy().into_owned(),
                    before: Some("also before".into()),
                    after: Some("also after".into()),
                },
            ],
        )
        .unwrap();
        let id = store::list(&conn, 1).unwrap()[0].id;

        let err = apply(&conn, &index, id);
        assert!(err.is_err());
        assert!(err.unwrap_err().contains("projects.org"), "names the specific problem file");
        assert_eq!(
            std::fs::read_to_string(&ok_path).unwrap(),
            "current content",
            "the OTHER, writable file is left untouched -- no partial apply"
        );
    }

    #[test]
    fn a_bulk_restore_is_itself_a_new_undoable_history_entry() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("projects.org");
        let before = "* Task A\n* Task B\n";
        let after = "* Task B\n";
        std::fs::write(&path, after).unwrap();
        let conn = conn();
        let index = IndexManager::new();
        let id = record_bulk_delete(&conn, &path, before, after, 1);

        apply(&conn, &index, id).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);

        history::undo(&conn, &index).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), after, "undo reverts the restore itself, back to the bulk action's own result");
    }

    fn record_bulk_refile(conn: &Connection, snapshots: Vec<FileSnapshot>, count: u32, target_filename: &str) -> i64 {
        history::record(
            conn,
            HistoryEntryKind::HeadingsBulkRefiled { count, target_filename: target_filename.to_string() },
            false,
            snapshots,
        )
        .unwrap();
        store::list(conn, 1).unwrap()[0].id
    }

    /// "mehrere refile restores aus einem projekt" -- several ITEMS refiled
    /// together in ONE bulk_refile_headings call (same source file, same
    /// target), not just the count=1 case every other test above uses.
    /// Restoring must bring back every one of them, not just the first/last.
    #[test]
    fn bulk_refile_restore_with_several_items_from_one_project_reverts_them_all() {
        let dir = TempDir::new().unwrap();
        let src_path = dir.path().join("actions.org");
        let tgt_path = dir.path().join("projects.org");
        let src_before = "* Task A\n* Task B\n* Task C\n* Task D\n";
        let src_after = "* Task D\n";
        let tgt_before = "* Project\n";
        let tgt_after = "* Project\n** Task A\n** Task B\n** Task C\n";
        std::fs::write(&src_path, src_after).unwrap();
        std::fs::write(&tgt_path, tgt_after).unwrap();
        let conn = conn();
        let index = IndexManager::new();
        let id = record_bulk_refile(
            &conn,
            vec![
                FileSnapshot {
                    path: tgt_path.to_string_lossy().into_owned(),
                    before: Some(tgt_before.into()),
                    after: Some(tgt_after.into()),
                },
                FileSnapshot {
                    path: src_path.to_string_lossy().into_owned(),
                    before: Some(src_before.into()),
                    after: Some(src_after.into()),
                },
            ],
            3,
            "projects.org",
        );

        apply(&conn, &index, id).unwrap();
        assert_eq!(std::fs::read_to_string(&tgt_path).unwrap(), tgt_before, "all three moved tasks gone from the target again");
        assert_eq!(std::fs::read_to_string(&src_path).unwrap(), src_before, "all three back in the source, D (never moved) undisturbed");
    }

    /// "restore_targets_an_older_entry_independently..." above proves this
    /// for a single-heading restore -- this is the bulk equivalent: TWO
    /// separate bulk refiles out of the SAME source file/project (each its
    /// own bulk_refile_headings call, see useRefile.ts's own
    /// groupByFileDescendingLine grouping -- one call and one history entry
    /// per source file, not one for the whole multi-file selection).
    /// Restoring the OLDER one must not require or disturb the newer one.
    #[test]
    fn two_separate_bulk_refiles_out_of_the_same_project_restore_independently() {
        let dir = TempDir::new().unwrap();
        let src_path = dir.path().join("actions.org");
        let tgt_path = dir.path().join("projects.org");

        // First bulk refile: Task A moves out.
        let src_v1 = "* Task A\n* Task B\n* Task C\n";
        let src_v2 = "* Task B\n* Task C\n";
        let tgt_v1 = "* Project\n";
        let tgt_v2 = "* Project\n** Task A\n";

        let conn = conn();
        let index = IndexManager::new();
        let older_id = record_bulk_refile(
            &conn,
            vec![
                FileSnapshot { path: tgt_path.to_string_lossy().into_owned(), before: Some(tgt_v1.into()), after: Some(tgt_v2.into()) },
                FileSnapshot { path: src_path.to_string_lossy().into_owned(), before: Some(src_v1.into()), after: Some(src_v2.into()) },
            ],
            1,
            "projects.org",
        );

        // Second, later bulk refile: Task B ALSO moves out, independently.
        let src_v3 = "* Task C\n";
        let tgt_v3 = "* Project\n** Task A\n** Task B\n";
        std::fs::write(&src_path, src_v3).unwrap();
        std::fs::write(&tgt_path, tgt_v3).unwrap();
        record_bulk_refile(
            &conn,
            vec![
                FileSnapshot { path: tgt_path.to_string_lossy().into_owned(), before: Some(tgt_v2.into()), after: Some(tgt_v3.into()) },
                FileSnapshot { path: src_path.to_string_lossy().into_owned(), before: Some(src_v2.into()), after: Some(src_v3.into()) },
            ],
            1,
            "projects.org",
        );

        // Restoring the OLDER entry directly (not via the linear stack) --
        // brings the files back to right after the FIRST refile, i.e. Task
        // B still shows up in the target (from the second refile, which
        // this restore neither knows nor needs to know about) alongside
        // Task A moving back to the source.
        apply(&conn, &index, older_id).unwrap();
        assert_eq!(std::fs::read_to_string(&tgt_path).unwrap(), tgt_v1, "reverts the WHOLE file to the older entry's own before state (see this module's own whole-file-restore doc comment) -- including undoing the second refile's contribution too, since there's no per-heading tracking to separate them");
        assert_eq!(std::fs::read_to_string(&src_path).unwrap(), src_v1, "source likewise reverted all the way back");
    }

    /// "mehrere dateien aus mehreren projekten refiled" -- bulk refiles out
    /// of TWO DIFFERENT source files/projects into two DIFFERENT targets
    /// (no shared file between them) restore independently, each only
    /// touching its own files.
    #[test]
    fn bulk_refiles_from_different_projects_restore_independently_without_cross_contamination() {
        let dir = TempDir::new().unwrap();
        let src_a = dir.path().join("project-a").join("actions.org");
        let tgt_a = dir.path().join("project-a").join("projects.org");
        let src_b = dir.path().join("project-b").join("actions.org");
        let tgt_b = dir.path().join("project-b").join("projects.org");
        std::fs::create_dir_all(src_a.parent().unwrap()).unwrap();
        std::fs::create_dir_all(src_b.parent().unwrap()).unwrap();

        std::fs::write(&src_a, "* Task Y\n").unwrap();
        std::fs::write(&tgt_a, "* Project A\n** Task X\n").unwrap();
        std::fs::write(&src_b, "* Task W\n").unwrap();
        std::fs::write(&tgt_b, "* Project B\n** Task Z\n").unwrap();

        let conn = conn();
        let index = IndexManager::new();
        let entry_a = record_bulk_refile(
            &conn,
            vec![
                FileSnapshot {
                    path: tgt_a.to_string_lossy().into_owned(),
                    before: Some("* Project A\n".into()),
                    after: Some("* Project A\n** Task X\n".into()),
                },
                FileSnapshot {
                    path: src_a.to_string_lossy().into_owned(),
                    before: Some("* Task X\n* Task Y\n".into()),
                    after: Some("* Task Y\n".into()),
                },
            ],
            1,
            "projects.org",
        );
        let entry_b = record_bulk_refile(
            &conn,
            vec![
                FileSnapshot {
                    path: tgt_b.to_string_lossy().into_owned(),
                    before: Some("* Project B\n".into()),
                    after: Some("* Project B\n** Task Z\n".into()),
                },
                FileSnapshot {
                    path: src_b.to_string_lossy().into_owned(),
                    before: Some("* Task Z\n* Task W\n".into()),
                    after: Some("* Task W\n".into()),
                },
            ],
            1,
            "projects.org",
        );

        apply(&conn, &index, entry_a).unwrap();
        assert_eq!(std::fs::read_to_string(&tgt_a).unwrap(), "* Project A\n", "project A's target reverted");
        assert_eq!(std::fs::read_to_string(&src_a).unwrap(), "* Task X\n* Task Y\n", "project A's source reverted");
        assert_eq!(
            std::fs::read_to_string(&tgt_b).unwrap(),
            "* Project B\n** Task Z\n",
            "project B completely untouched by restoring project A's entry"
        );
        assert_eq!(std::fs::read_to_string(&src_b).unwrap(), "* Task W\n", "project B's source likewise untouched");

        apply(&conn, &index, entry_b).unwrap();
        assert_eq!(std::fs::read_to_string(&tgt_b).unwrap(), "* Project B\n", "project B's own restore now applies independently");
        assert_eq!(std::fs::read_to_string(&src_b).unwrap(), "* Task Z\n* Task W\n");
    }

    /// "... und inzwischen ein projekt davon gelöscht" -- of two bulk
    /// refiles out of different projects, one project's SOURCE FILE has
    /// since been deleted entirely (not just its folder -- see the
    /// dedicated folder-gone test above for that stricter failure case).
    /// Restoring must still work stably for BOTH: the untouched project
    /// restores normally, and the one whose file vanished gets it
    /// recreated rather than failing or silently doing nothing.
    #[test]
    fn restoring_bulk_refiles_stays_stable_when_one_involved_projects_file_was_since_deleted() {
        let dir = TempDir::new().unwrap();
        let src_a = dir.path().join("project-a-actions.org");
        let tgt_a = dir.path().join("project-a-projects.org");
        let src_b = dir.path().join("project-b-actions.org");
        let tgt_b = dir.path().join("project-b-projects.org");
        std::fs::write(&tgt_a, "* Project A\n** Task X\n").unwrap();
        std::fs::write(&src_b, "* Task W\n").unwrap();
        std::fs::write(&tgt_b, "* Project B\n** Task Z\n").unwrap();
        // src_a deliberately never (re)written to disk -- simulates the
        // file having been deleted since project A's own bulk refile.

        let conn = conn();
        let index = IndexManager::new();
        let entry_a = record_bulk_refile(
            &conn,
            vec![
                FileSnapshot {
                    path: tgt_a.to_string_lossy().into_owned(),
                    before: Some("* Project A\n".into()),
                    after: Some("* Project A\n** Task X\n".into()),
                },
                FileSnapshot {
                    path: src_a.to_string_lossy().into_owned(),
                    before: Some("* Task X\n* Task Y\n".into()),
                    after: Some("* Task Y\n".into()),
                },
            ],
            1,
            "project-a-projects.org",
        );
        let entry_b = record_bulk_refile(
            &conn,
            vec![
                FileSnapshot {
                    path: tgt_b.to_string_lossy().into_owned(),
                    before: Some("* Project B\n".into()),
                    after: Some("* Project B\n** Task Z\n".into()),
                },
                FileSnapshot {
                    path: src_b.to_string_lossy().into_owned(),
                    before: Some("* Task Z\n* Task W\n".into()),
                    after: Some("* Task W\n".into()),
                },
            ],
            1,
            "project-b-projects.org",
        );

        // Informing the user: the check for project A's entry still comes
        // back feasible (a missing file doesn't block restoring -- see
        // check_bulk's own doc comment), and still names the source file
        // via additional_filenames even though it currently doesn't exist.
        let check_a = check(&conn, entry_a).unwrap();
        assert!(check_a.feasible);
        assert_eq!(check_a.additional_filenames, vec!["project-a-actions.org".to_string()]);

        apply(&conn, &index, entry_a).unwrap();
        assert!(src_a.exists(), "the deleted file is recreated by restoring");
        assert_eq!(std::fs::read_to_string(&src_a).unwrap(), "* Task X\n* Task Y\n");
        assert_eq!(std::fs::read_to_string(&tgt_a).unwrap(), "* Project A\n");

        // Project B, entirely unrelated to A's missing file, restores
        // completely normally.
        apply(&conn, &index, entry_b).unwrap();
        assert_eq!(std::fs::read_to_string(&tgt_b).unwrap(), "* Project B\n");
        assert_eq!(std::fs::read_to_string(&src_b).unwrap(), "* Task Z\n* Task W\n");
    }
}
