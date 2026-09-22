use crate::core::errors::not_found_at_line;
use crate::core::heading_utils;
use crate::core::models::{Heading, RefilePosition};
use crate::core::org_parser::{ensure_heading_state, parse_headings_from_lines};
use std::path::Path;

/// Ensures TARGET_PARENT_LINE isn't inside the subtree that's about to be
/// moved (would corrupt the file, since the parent would be extracted along
/// with its own child).
pub fn validate_target_not_within_source(
    same_file: bool,
    target_parent_line: Option<usize>,
    start_line: usize,
    end_line: usize,
) -> Result<(), String> {
    if let Some(p_line) = target_parent_line {
        if same_file && heading_utils::is_within_range(p_line, start_line, end_line) {
            return Err(crate::t!("Ziel liegt innerhalb der zu verschiebenden Überschrift", "The target is inside the heading being moved"));
        }
    }
    Ok(())
}

/// Resolves the heading level the moved subtree's root should end up at,
/// based on the target parent (if any) and where relative to it it's
/// inserted.
pub fn resolve_target_level(
    tgt_flat: &[Heading],
    target_parent_line: Option<usize>,
    position: RefilePosition,
) -> Result<usize, String> {
    match target_parent_line {
        None => Ok(1),
        Some(p_line) => {
            let p_idx = tgt_flat
                .iter()
                .position(|h| h.line_no == p_line)
                .ok_or_else(|| not_found_at_line("Ziel-Überschrift", p_line))?;
            Ok(match position {
                RefilePosition::Into => tgt_flat[p_idx].level + 1,
                RefilePosition::Before | RefilePosition::After => tgt_flat[p_idx].level,
            })
        }
    }
}

pub struct RefileParams {
    pub start_line: usize,
    pub end_line: usize,
    pub src_level: usize,
    pub target_level: usize,
    pub default_state: Option<String>,
}

/// Core of the refile operation: removes the subtree [START_LINE, END_LINE]
/// from SRC_LINES, applies DEFAULT_STATE and the heading-level shift needed
/// to fit at TARGET_LEVEL, and splices the result into TGT_LINES at the
/// position implied by TARGET_PARENT_LINE/POSITION.
///
/// Pure, with no file I/O, so the line accounting can be unit tested
/// directly.
pub fn splice_refile(
    mut src_lines: Vec<String>,
    params: &RefileParams,
    target_parent_line: Option<usize>,
    position: RefilePosition,
    same_file: bool,
    tgt_path: &Path,
    mut tgt_lines: Vec<String>,
) -> Result<(Vec<String>, Vec<String>), String> {
    let mut extracted_lines: Vec<String> =
        src_lines.drain((params.start_line - 1)..params.end_line).collect();

    if let Some(state) = params.default_state.as_deref() {
        ensure_heading_state(&mut extracted_lines, state);
    }

    let level_diff = params.target_level as isize - params.src_level as isize;
    if level_diff != 0 {
        heading_utils::adjust_heading_levels(&mut extracted_lines, level_diff);
    }

    if same_file {
        tgt_lines = src_lines.clone();
    }

    let removed_count = params.end_line - params.start_line + 1;
    let shifted_parent_line = target_parent_line.map(|p_line| {
        if same_file {
            heading_utils::shift_line_no(p_line, params.end_line, removed_count)
        } else {
            p_line
        }
    });

    let tgt_flat_fresh = parse_headings_from_lines(tgt_path, &tgt_lines);
    let insert_idx = heading_utils::compute_insert_index(
        &tgt_flat_fresh,
        shifted_parent_line,
        position,
        tgt_lines.len(),
    )?;
    let actual_insert_idx = std::cmp::min(insert_idx, tgt_lines.len());

    tgt_lines.splice(actual_insert_idx..actual_insert_idx, extracted_lines);

    Ok((src_lines, tgt_lines))
}

/// Batched core of commands_headings::bulk_refile_headings -- refiles every
/// one of SOURCES' own subtree (all from the SAME source file, all going
/// "into" the SAME target parent -- the only position useRefile.ts's bulk
/// modal offers) in one pass each, instead of one
/// source-read/target-read/source-write/target-write/reindex cycle per
/// selected item (see bulk_refile_headings' own doc comment on the Rust
/// side for the "why").
///
/// If a selected source is itself a descendant of ANOTHER selected source
/// (e.g. a project and one of its own subtasks both selected -- the UI
/// allows this, same hazard apply_bulk_delete guards against), only the
/// ancestor's own subtree move is kept: the descendant is already carried
/// along with it, and extracting both independently would double-remove /
/// corrupt line offsets.
///
/// Extraction happens bottom-up (descending line_no, same safety invariant
/// as apply_bulk_delete/apply_bulk_update: an already-processed range never
/// invalidates a not-yet-processed one's captured position), but the
/// extracted subtrees are re-inserted at the target in ASCENDING original
/// line_no order, one right after another starting at the target's own
/// insertion point -- so a multi-select refile preserves the moved items'
/// original relative order instead of reversing it.
///
/// SAME_FILE (source file == target file) is handled by accumulating, for
/// TARGET_PARENT_LINE, how many lines were removed by every kept source
/// range strictly BEFORE it in the file (a range after it doesn't shift it
/// at all) -- the multi-item generalization of splice_refile's own single
/// `shift_line_no` call above.
///
/// Returns the new source/target lines plus how many of SOURCES actually
/// resolved to a still-existing, kept (non-covered-by-another-selection)
/// heading -- 0 means both buffers were left untouched, so callers can skip
/// the write/reindex/history entirely.
pub fn splice_bulk_refile(
    flat: &[Heading],
    sources: &[(usize, Option<String>)],
    mut src_lines: Vec<String>,
    target_parent_line: Option<usize>,
    same_file: bool,
    target_level: usize,
    tgt_path: &Path,
    mut tgt_lines: Vec<String>,
) -> Result<(Vec<String>, Vec<String>, u32), String> {
    let mut idx_state: Vec<(usize, Option<String>)> = sources
        .iter()
        .filter_map(|(line_no, state)| {
            flat.iter()
                .position(|h| h.line_no == *line_no)
                .map(|idx| (idx, state.clone()))
        })
        .collect();
    idx_state.sort_unstable_by_key(|&(idx, _)| idx);
    idx_state.dedup_by_key(|&mut (idx, _)| idx);

    let selected: std::collections::BTreeSet<usize> = idx_state.iter().map(|&(idx, _)| idx).collect();
    idx_state.retain(|&(idx, _)| {
        !heading_utils::ancestors_of(flat, idx).iter().any(|a| {
            flat.iter()
                .position(|h| h.line_no == a.line_no)
                .is_some_and(|a_idx| selected.contains(&a_idx))
        })
    });

    let mut ranges: Vec<(usize, usize, usize, Option<String>)> = idx_state
        .iter()
        .map(|&(idx, ref state)| {
            let (start_line, end_line) = heading_utils::subtree_line_range(flat, idx);
            (start_line, end_line, flat[idx].level, state.clone())
        })
        .collect();

    for &(start_line, end_line, ..) in &ranges {
        validate_target_not_within_source(same_file, target_parent_line, start_line, end_line)?;
    }

    ranges.sort_unstable_by(|a, b| b.0.cmp(&a.0));

    let mut extracted: Vec<(usize, Vec<String>)> = Vec::new();
    for (start_line, end_line, src_level, default_state) in &ranges {
        let mut block: Vec<String> = src_lines.drain((*start_line - 1)..*end_line).collect();
        if let Some(state) = default_state.as_deref() {
            ensure_heading_state(&mut block, state);
        }
        let level_diff = target_level as isize - *src_level as isize;
        if level_diff != 0 {
            heading_utils::adjust_heading_levels(&mut block, level_diff);
        }
        extracted.push((*start_line, block));
    }
    extracted.sort_unstable_by_key(|&(start_line, _)| start_line);

    if same_file {
        tgt_lines = src_lines.clone();
    }

    let shifted_parent_line = target_parent_line.map(|p_line| {
        if !same_file {
            return p_line;
        }
        let removed_before: usize = ranges
            .iter()
            .filter(|&&(_, end_line, ..)| end_line < p_line)
            .map(|&(start_line, end_line, ..)| end_line - start_line + 1)
            .sum();
        p_line - removed_before
    });

    let tgt_flat_fresh = parse_headings_from_lines(tgt_path, &tgt_lines);
    let mut insert_idx = heading_utils::compute_insert_index(
        &tgt_flat_fresh,
        shifted_parent_line,
        RefilePosition::Into,
        tgt_lines.len(),
    )?;

    for (_, block) in extracted {
        let at = std::cmp::min(insert_idx, tgt_lines.len());
        let inserted_len = block.len();
        tgt_lines.splice(at..at, block);
        insert_idx = at + inserted_len;
    }

    Ok((src_lines, tgt_lines, idx_state.len() as u32))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn lines(s: &str) -> Vec<String> {
        s.lines().map(String::from).collect()
    }

    #[test]
    fn moving_to_deeper_parent_shifts_level_exactly_once() {
        // level-1 heading moved under a level-1 parent "Into" -> should
        // become level 2, not level 3 (regression test for the duplicated
        // level_diff bug).
        let src = lines("* Task A\nSome body\n* Task B\nOther body");
        let tgt = lines("* Project\nIntro");

        let params = RefileParams {
            start_line: 1,
            end_line: 2,
            src_level: 1,
            target_level: 2, // Project (level 1) + Into => level 2
            default_state: None,
        };

        let (new_src, new_tgt) = splice_refile(
            src,
            &params,
            Some(1),
            RefilePosition::Into,
            false,
            &PathBuf::from("target.org"),
            tgt,
        )
        .unwrap();

        assert_eq!(new_src, vec!["* Task B".to_string(), "Other body".to_string()]);
        assert_eq!(new_tgt[0], "* Project");
        assert_eq!(new_tgt[2], "** Task A"); // exactly one level deeper, not two
        assert_eq!(new_tgt[3], "Some body");
    }

    #[test]
    fn same_level_move_leaves_stars_untouched() {
        let src = lines("* Task A\nBody");
        let tgt = lines("* Other");

        let params = RefileParams {
            start_line: 1,
            end_line: 2,
            src_level: 1,
            target_level: 1,
            default_state: None,
        };

        let (_, new_tgt) = splice_refile(
            src,
            &params,
            None,
            RefilePosition::Into,
            false,
            &PathBuf::from("target.org"),
            tgt,
        )
        .unwrap();

        assert!(new_tgt.contains(&"* Task A".to_string()));
    }

    /// Dragging the second-to-last entry onto the line above the last one
    /// says "stay where you are". Reported as landing two places higher.
    #[test]
    fn dropping_an_entry_just_above_the_one_it_already_precedes_changes_nothing() {
        let src = lines("* TODO A\n* TODO B\n* TODO C\n* TODO D");

        let params =
            RefileParams { start_line: 3, end_line: 3, src_level: 1, target_level: 1, default_state: None };

        let (_, out) = splice_refile(
            src,
            &params,
            Some(4),
            RefilePosition::Before,
            true,
            &PathBuf::from("same.org"),
            Vec::new(),
        )
        .unwrap();

        assert_eq!(out, vec!["* TODO A", "* TODO B", "* TODO C", "* TODO D"]);
    }

    /// ...and dropping it below the last one puts it last.
    #[test]
    fn dropping_an_entry_below_the_last_one_puts_it_last() {
        let src = lines("* TODO A\n* TODO B\n* TODO C\n* TODO D");

        let params =
            RefileParams { start_line: 3, end_line: 3, src_level: 1, target_level: 1, default_state: None };

        let (_, out) = splice_refile(
            src,
            &params,
            Some(4),
            RefilePosition::After,
            true,
            &PathBuf::from("same.org"),
            Vec::new(),
        )
        .unwrap();

        assert_eq!(out, vec!["* TODO A", "* TODO B", "* TODO D", "* TODO C"]);
    }

    #[test]
    fn same_file_refile_removes_from_and_reinserts_into_single_buffer() {
        // "Task C" needs a real state (not just a bare "* Task C") --
        // otherwise it isn't real structure per org_parser's own rule
        // (state or :project: tag required once something ELSE already
        // opened the heading before it), and would be absorbed as
        // Parent's own content instead of staying a separate heading,
        // which is exactly what this test's insert-position math relies
        // on computing correctly.
        let src = lines("* Parent\n* Task A\nBody\n* TODO Task C");

        let params = RefileParams {
            start_line: 2,
            end_line: 3,
            src_level: 1,
            target_level: 2,
            default_state: None,
        };

        let (_, new_tgt) = splice_refile(
            src,
            &params,
            Some(1),
            RefilePosition::Into,
            true,
            &PathBuf::from("same.org"),
            Vec::new(),
        )
        .unwrap();

        assert_eq!(
            new_tgt,
            vec![
                "* Parent".to_string(),
                "** Task A".to_string(),
                "Body".to_string(),
                "* TODO Task C".to_string(),
            ]
        );
    }

    /// Same-file, Before/After refile is exactly what the Projects sidebar's
    /// drag-and-drop reorder feature is built on (see Projects.tsx's
    /// handleMoveTask) -- this pins the invariant that actually matters to a
    /// user dragging a subtask: after moving Task C to right after Task A,
    /// the FULL sibling order must read A, C, B, not just "C ended up
    /// somewhere near A."
    #[test]
    fn same_file_reorder_after_a_specific_sibling_produces_the_expected_full_order() {
        // All three subtasks need a real state -- otherwise only the first
        // ("Task A") stays real structure and B/C get absorbed as its own
        // content instead of remaining separate siblings (org_parser's
        // rule: state or :project: tag required once something else
        // already opened the heading before them).
        let src = lines("* Project\n** TODO Task A\n** TODO Task B\n** TODO Task C");

        let params = RefileParams {
            start_line: 4,
            end_line: 4,
            src_level: 2,
            target_level: 2,
            default_state: None,
        };

        let (_, new_tgt) = splice_refile(
            src,
            &params,
            Some(2), // Task A's own line_no
            RefilePosition::After,
            true,
            &PathBuf::from("same.org"),
            Vec::new(),
        )
        .unwrap();

        assert_eq!(
            new_tgt,
            vec![
                "* Project".to_string(),
                "** TODO Task A".to_string(),
                "** TODO Task C".to_string(),
                "** TODO Task B".to_string(),
            ]
        );
    }

    /* --- splice_bulk_refile --- */

    fn parse(path: &str, text: &str) -> Vec<Heading> {
        parse_headings_from_lines(&PathBuf::from(path), text.lines())
    }

    #[test]
    fn bulk_refile_moves_several_items_into_a_different_file_preserving_original_order() {
        // Every sibling needs a real state -- otherwise only the first
        // ("Task A") stays real structure and the rest get absorbed as
        // its own content instead of remaining separate, addressable
        // headings (org_parser's rule: state or :project: tag required
        // once something else already opened the heading before them).
        let src_text = "* TODO Task A\n* TODO Task B\n* TODO Task C\n";
        let flat = parse("src.org", src_text);
        let line_of = |title: &str| flat.iter().find(|h| h.title == title).unwrap().line_no;

        // Selected "out of order" (C before A) -- the result at the target
        // must still reflect the ORIGINAL file order (A, then C), not
        // selection order.
        let sources = vec![(line_of("Task C"), None), (line_of("Task A"), None)];

        let tgt = lines("* Project");
        let (new_src, new_tgt, count) = splice_bulk_refile(
            &flat, &sources, lines(src_text), Some(1), false, 2, &PathBuf::from("tgt.org"), tgt,
        ).unwrap();

        assert_eq!(count, 2);
        assert_eq!(new_src, vec!["* TODO Task B".to_string()], "only B remains in the source file");
        assert_eq!(
            new_tgt,
            vec![
                "* Project".to_string(),
                "** TODO Task A".to_string(),
                "** TODO Task C".to_string(),
            ],
            "A before C at the target, matching their original relative order in the source file"
        );
    }

    #[test]
    fn bulk_refile_dedups_a_selected_project_and_its_own_selected_subtask() {
        let src_text = "* Project A :project:\n** TODO Subtask\n* Project B :project:\n";
        let flat = parse("src.org", src_text);
        let line_of = |title: &str| flat.iter().find(|h| h.title == title).unwrap().line_no;

        // Selected in "wrong" (parent-first) order on purpose -- dedup must
        // not depend on selection order (same hazard as bulk delete's own
        // test for this).
        let sources = vec![(line_of("Project A"), None), (line_of("Subtask"), None)];

        let tgt = lines("* Target");
        let (new_src, new_tgt, count) = splice_bulk_refile(
            &flat, &sources, lines(src_text), Some(1), false, 2, &PathBuf::from("tgt.org"), tgt,
        ).unwrap();

        assert_eq!(count, 1, "the subtask doesn't count separately -- it's covered by Project A's own subtree move");
        assert_eq!(new_src, vec!["* Project B :project:".to_string()]);
        assert_eq!(
            new_tgt,
            vec![
                "* Target".to_string(),
                "** Project A :project:".to_string(),
                "*** TODO Subtask".to_string(),
            ],
            "the subtask travels along with its parent, both level-shifted"
        );
    }

    #[test]
    fn bulk_refile_within_the_same_file_shifts_a_target_parent_positioned_after_the_moved_items() {
        // Target parent ("Later Project") sits AFTER both moved items in
        // the file -- its line_no must be adjusted down by the total
        // number of lines removed before it, or the insert would land in
        // the wrong place (or panic on an out-of-range index). Task A/B
        // need a real state for the same reason as the sibling-order test
        // above (each moved item resolved by title via `flat`).
        let src_text = "* TODO Task A\n* TODO Task B\n* Later Project :project:\nIntro\n";
        let flat = parse("src.org", src_text);
        let line_of = |title: &str| flat.iter().find(|h| h.title == title).unwrap().line_no;
        let target_line = line_of("Later Project");

        let sources = vec![(line_of("Task A"), None), (line_of("Task B"), None)];

        let (new_src, new_tgt, count) = splice_bulk_refile(
            &flat, &sources, lines(src_text), Some(target_line), true, 2, &PathBuf::from("src.org"), Vec::new(),
        ).unwrap();

        assert_eq!(count, 2);
        assert_eq!(
            new_src,
            vec![
                "* Later Project :project:".to_string(),
                "Intro".to_string(),
            ]
        );
        assert_eq!(
            new_tgt,
            vec![
                "* Later Project :project:".to_string(),
                "Intro".to_string(),
                "** TODO Task A".to_string(),
                "** TODO Task B".to_string(),
            ],
            "both tasks appended at the end of Later Project's own content (after Intro), in original order"
        );
    }

    #[test]
    fn bulk_refile_applies_default_state_independently_per_item() {
        let src_text = "* Task A\n* TODO Task B\n";
        let flat = parse("src.org", src_text);
        let line_of = |title: &str| flat.iter().find(|h| h.title == title).unwrap().line_no;

        // Mirrors useRefile.ts's own per-item rule (source.state ? null :
        // "TODO"): only the stateless item gets a default state.
        let sources = vec![
            (line_of("Task A"), Some("TODO".to_string())),
            (line_of("Task B"), None),
        ];

        let (_, new_tgt, count) = splice_bulk_refile(
            &flat, &sources, lines(src_text), None, false, 1, &PathBuf::from("tgt.org"), Vec::new(),
        ).unwrap();

        assert_eq!(count, 2);
        assert_eq!(new_tgt, vec!["* TODO Task A".to_string(), "* TODO Task B".to_string()]);
    }

    #[test]
    fn description_with_an_auto_demoted_note_star_line_round_trips_correctly_through_a_refile() {
        // "Task" is a level-2 heading whose description contains a note
        // the user typed as "* Notiz" (see
        // heading_utils::apply_content_heading_offset) -- on disk that's
        // demoted to 2+1=3 stars ("*** Notiz"). adjust_heading_levels
        // doesn't distinguish a heading's own line from a content
        // star-line, so refiling Task to level 3 shifts BOTH by the same
        // diff -- re-parsing after the move must still strip back down to
        // the exact same short form the user originally typed, at Task's
        // NEW level.
        let src = lines("* TODO Project A :project:\n** TODO Task\nSome text.\n*** Notiz\n* TODO Project B :project:\n");

        let params = RefileParams {
            start_line: 2,
            end_line: 4,
            src_level: 2,
            target_level: 3, // moving one level deeper
            default_state: None,
        };

        // Same-file refile: the actual final result lives in the SECOND
        // return value (tgt_lines starts as a clone of the post-removal
        // source, then gets the extracted block spliced back in) -- the
        // first value is just the intermediate "file minus the moved
        // block" state, same pattern as
        // same_file_refile_removes_from_and_reinserts_into_single_buffer.
        let (_, new_tgt) = splice_refile(
            src,
            &params,
            Some(5), // Project B's own line_no (moving "Into" it)
            RefilePosition::Into,
            true,
            &PathBuf::from("same.org"),
            Vec::new(),
        )
        .unwrap();

        let text = new_tgt.join("\n");
        let flat = parse_headings_from_lines(&PathBuf::from("same.org"), text.lines());
        let task = flat.iter().find(|h| h.title == "Task").unwrap();
        assert_eq!(task.level, 3, "Task itself landed one level deeper");
        assert_eq!(task.content.trim(), "Some text.\n* Notiz", "the note's short form survives the refile unchanged");
    }

    #[test]
    fn bulk_refile_skips_a_stale_line_no_that_no_longer_resolves_to_any_heading() {
        let src_text = "* Task A\n";
        let flat = parse("src.org", src_text);
        let line_of = |title: &str| flat.iter().find(|h| h.title == title).unwrap().line_no;

        let sources = vec![(line_of("Task A"), None), (9999, None)];

        let (_, new_tgt, count) = splice_bulk_refile(
            &flat, &sources, lines(src_text), None, false, 1, &PathBuf::from("tgt.org"), Vec::new(),
        ).unwrap();

        assert_eq!(count, 1, "the stale entry is silently skipped, not an error");
        assert_eq!(new_tgt, vec!["* Task A".to_string()]);
    }
}
