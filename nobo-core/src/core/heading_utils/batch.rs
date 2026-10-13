use super::serialize::{build_org_heading_block, touch_last_activity};
use super::tree::{ancestors_of, subtree_line_range};
use crate::core::heading_identity;
use crate::core::models::{BulkHeadingUpdate, Heading, HeadingTarget, HeadingUpdatePayload};
use crate::core::task_utils;
use crate::utils::repeat_utils::{self, RepeatedEdit};
use std::collections::HashMap;

/// The 0-based half-open range a heading's 1-based `line_no`/`last_line`
/// name, checked against the LINES actually in hand.
///
/// Not trimmed to fit. Both callers below take their FLAT and their LINES
/// from one read of one file (`heading_utils::read_and_parse`), so a range
/// that does not fit is not a range to salvage -- it means the two have
/// drifted apart, and splicing a clamped version of it writes the block
/// over whatever occupies those lines instead, or cuts the file off at the
/// range's start. `fs_utils::splice_lines` stopped clamping for the same
/// reason; these are the in-memory twin of it.
pub(super) fn block_range(lines: &[String], line_no: usize, last_line: usize, title: &str) -> Result<(usize, usize), String> {
    if line_no == 0 || last_line < line_no || last_line > lines.len() {
        return Err(crate::t!(
            "\"{title}\" liegt nicht mehr auf den Zeilen {line_no}..{last_line} (die Datei hat {}) -- sie wurde inzwischen geändert, bitte neu laden.", "\"{title}\" is no longer at lines {line_no}..{last_line} (the file has {}) -- it has changed since, please reload.",
            lines.len()
        ));
    }
    Ok((line_no - 1, last_line))
}

/// The 0-based half-open range HEADING's own block occupies in LINES.
///
/// For callers outside this module that splice by hand -- the maintenance
/// sweeps, which stamp or strip a drawer on many headings in one file.
/// They used to clamp with `.min(lines.len())`, which turns "these line
/// numbers are stale" into "overwrite whatever is there now"; this
/// refuses, like everything else on the write path.
pub fn heading_block_range(lines: &[String], heading: &Heading) -> Result<(usize, usize), String> {
    block_range(lines, heading.line_no, heading.last_line, &heading.title)
}

/// Batch apples the updated properties in UPDATES to all headings in FLAT.
///
/// All-or-nothing on identity: an entry whose heading cannot be resolved
/// with certainty (see heading_identity::resolve_target) aborts the whole
/// batch before LINES is touched, rather than being skipped or -- far
/// worse -- applied to whatever moved into its line meanwhile.
pub fn apply_bulk_update(flat: &[Heading], updates: &[BulkHeadingUpdate], lines: &mut Vec<String>) -> Result<u32, String> {
    let mut edits: Vec<(usize, &HeadingUpdatePayload)> = Vec::new();
    let mut ancestor_idxs: std::collections::BTreeSet<usize> = std::collections::BTreeSet::new();
    // Bulk-finishing a selection that happens to contain a repeating task
    // has to behave exactly like finishing it on its own -- see
    // repeat_utils::repeat_on_finish.
    let mut repeats: HashMap<usize, RepeatedEdit> = HashMap::new();

    for update in updates {
        let idx = heading_identity::resolve_target(flat, &update.target, &crate::core::errors::subject::heading())?;
        // Two entries resolving to the SAME heading means the caller's view
        // of the file is stale (two of its headings are one by now). Each
        // carries its own payload, so there is no safe pick -- and applying
        // both would splice the same, by then outdated, line range twice
        // and mangle the block. Aborts like any other unresolvable entry.
        //
        // apply_bulk_delete below merely dedups the same situation: there
        // both entries ask for the identical outcome, so collapsing them
        // loses nothing.
        if edits.iter().any(|&(existing, _)| existing == idx) {
            return Err(crate::t!(
                "\"{}\" ist in dieser Auswahl doppelt -- die Änderungen wurden nicht gespeichert, bitte neu laden",
                "\"{}\" appears twice in this selection -- the changes were not saved, please reload",
                flat[idx].title
            ));
        }
        if let Some(edit) = repeat_utils::repeat_on_finish(&flat[idx], &update.payload) {
            repeats.insert(idx, edit);
        }
        edits.push((idx, &update.payload));
        for ancestor in ancestors_of(flat, idx) {
            if let Some(a_idx) = flat.iter().position(|h| h.line_no == ancestor.line_no) {
                ancestor_idxs.insert(a_idx);
            }
        }
    }
    // An ancestor that's ALSO a direct target gets its real edit (which
    // already bumps its own LAST_ACTIVITY, see build_org_heading_block) --
    // touching it a second time via touch_last_activity would just be
    // redundant.
    for (i, _) in &edits {
        ancestor_idxs.remove(i);
    }

    // What each edited heading's state becomes once the batch lands, keyed
    // by line_no. The whole batch runs against one unmodified pre-edit
    // parse, so an ancestor's block is built before its descendants' edits
    // are spliced in and its captured progress stats are stale.
    // `Some("")` is the payload's clear-the-state sentinel, turned into
    // `None` in the overlay here.
    let state_overlay: std::collections::HashMap<usize, Option<String>> = edits
        .iter()
        .filter_map(|&(i, p)| {
            let state = repeats.get(&i).map_or(p.state.as_ref(), |edit| edit.payload.state.as_ref());
            state.map(|s| (flat[i].line_no, if s.is_empty() { None } else { Some(s.clone()) }))
        })
        .collect();

    let mut all: Vec<(usize, Option<&HeadingUpdatePayload>)> =
        edits.iter().map(|&(i, p)| (i, Some(p))).chain(ancestor_idxs.iter().map(|&i| (i, None))).collect();
    all.sort_unstable_by(|a, b| flat[b.0].line_no.cmp(&flat[a.0].line_no));

    for (idx, payload) in &all {
        let mut heading = flat[*idx].clone();
        heading.progress_stats = task_utils::subtree_progress_with_overlay(flat, *idx, &state_overlay);
        let block = match (payload, repeats.get(idx)) {
            (Some(_), Some(edit)) => {
                heading.state = edit.heading.state.clone();
                build_org_heading_block(&heading, &edit.payload)
            }
            (Some(p), None) => build_org_heading_block(&heading, p),
            (None, _) => touch_last_activity(&heading),
        };
        let (start, end) = block_range(lines, heading.line_no, heading.last_line, &heading.title)?;
        lines.splice(start..end, block);
    }

    Ok(edits.len() as u32)
}

/// The batched core of `bulk_delete_headings`: removes each heading's
/// subtree from `lines` in place, plus a deduplicated LAST_ACTIVITY touch
/// for every remaining ancestor -- one pass over one in-memory copy rather
/// than a file read and write per heading.
///
/// Where a selected heading is a descendant of another selected one, only
/// the ancestor's delete is kept: the descendant's lines are covered by
/// it, and deleting both would double-remove and corrupt offsets.
///
/// Ranges are removed deepest first, so a still-to-be-processed range's
/// position is never invalidated. Ancestor touches follow in a second
/// pass, against corrected line numbers: an ancestor's own line precedes
/// the descendant deleted under it, but not the deletions under an
/// ancestor earlier in the file -- see `shift_of` below.
///
/// Returns how many subtrees were actually removed -- fewer than
/// `targets` when a selection contained both a heading and one of its own
/// descendants (see the overlap handling below); 0 means `lines` is
/// untouched and the caller can skip the write.
pub fn apply_bulk_delete(flat: &[Heading], targets: &[HeadingTarget], lines: &mut Vec<String>) -> Result<u32, String> {
    // Same all-or-nothing identity rule as apply_bulk_update above, and
    // the stakes are higher here: a deletion resolved to the wrong
    // heading takes a whole subtree with it.
    let mut idxs: Vec<usize> = targets
        .iter()
        .map(|target| heading_identity::resolve_target(flat, target, &crate::core::errors::subject::heading()))
        .collect::<Result<_, _>>()?;
    idxs.sort_unstable();
    idxs.dedup();

    let selected: std::collections::BTreeSet<usize> = idxs.iter().copied().collect();
    idxs.retain(|&idx| {
        !ancestors_of(flat, idx)
            .iter()
            .any(|a| flat.iter().position(|h| h.line_no == a.line_no).is_some_and(|a_idx| selected.contains(&a_idx)))
    });

    let mut ancestor_idxs: std::collections::BTreeSet<usize> = std::collections::BTreeSet::new();
    for &idx in &idxs {
        for ancestor in ancestors_of(flat, idx) {
            if let Some(a_idx) = flat.iter().position(|h| h.line_no == ancestor.line_no) {
                ancestor_idxs.insert(a_idx);
            }
        }
    }

    // Each range keeps the index it came from, so the drain below can name
    // the heading it was about to remove if the range no longer fits.
    let mut ranges: Vec<(usize, usize, usize)> = idxs
        .iter()
        .map(|&idx| {
            let (start_line, end_line) = subtree_line_range(flat, idx);
            (start_line, end_line, idx)
        })
        .collect();

    // Every heading (by its own index into FLAT) that's about to disappear
    // -- not just the top-level IDXS themselves, but every descendant
    // covered by their own subtree ranges too -- so an ancestor's touch
    // below can exclude them all from its progress count. Built from FLAT
    // (still the pre-delete parse) before LINES is touched, same staleness
    // reasoning as apply_bulk_update's own state_overlay: this whole batch
    // runs in one pass with no reparse in between.
    let removed_indices: std::collections::BTreeSet<usize> = flat
        .iter()
        .enumerate()
        .filter(|(_, h)| ranges.iter().any(|&(start_line, end_line, _)| h.line_no >= start_line && h.line_no <= end_line))
        .map(|(i, _)| i)
        .collect();

    ranges.sort_unstable_by(|a, b| b.0.cmp(&a.0));
    for &(start_line, end_line, idx) in &ranges {
        let (start, end) = block_range(lines, start_line, end_line, &flat[idx].title)?;
        lines.drain(start..end);
    }

    // How far LINE_NO has moved up by the time the deletions are done.
    //
    // An ancestor's own line always precedes the descendant deleted under
    // it -- but not the deletions under a DIFFERENT ancestor earlier in
    // the file, and those move it just the same. Its captured line_no is
    // from FLAT, the pre-delete parse, so without this the second
    // ancestor's block is spliced over whatever moved into those
    // positions: its heading written a second time, the lines really
    // there lost.
    //
    // A range is always wholly before or wholly after an ancestor, never
    // across it: a selected heading inside another selected heading's
    // subtree was dropped above, so no ancestor sits in a removed range.
    let shift_of = |line_no: usize| -> usize {
        ranges.iter().filter(|&&(_, end_line, _)| end_line < line_no).map(|&(start_line, end_line, _)| end_line + 1 - start_line).sum()
    };

    let mut touches: Vec<usize> = ancestor_idxs.into_iter().collect();
    touches.sort_unstable_by(|a, b| flat[*b].line_no.cmp(&flat[*a].line_no));
    for idx in touches {
        let mut heading = flat[idx].clone();
        heading.progress_stats = task_utils::subtree_progress_excluding(flat, idx, &removed_indices);
        let block = touch_last_activity(&heading);
        let shift = shift_of(heading.line_no);
        let (start, end) =
            block_range(lines, heading.line_no.saturating_sub(shift), heading.last_line.saturating_sub(shift), &heading.title)?;
        lines.splice(start..end, block);
    }

    Ok(idxs.len() as u32)
}
