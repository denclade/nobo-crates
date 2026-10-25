// SPDX-FileCopyrightText: 2026 Dennis Burgermeister
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::core::models::DiffChunk;
use similar::{ChangeTag, TextDiff};

#[derive(serde::Serialize)]
pub struct LineDiff {
    pub rows: Vec<LineDiffRow>,
    pub added: usize,
    pub removed: usize,
    pub max_line: usize,
}

#[derive(serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LineDiffRow {
    Hunk { old_start: usize, old_count: usize, new_start: usize, new_count: usize },
    Line { tag: &'static str, text: String, old_line: Option<usize>, new_line: Option<usize>, missing_newline: bool },
}

/// Unified line diff, with three lines of context unless all unchanged
/// lines were requested. The UI virtualises the flat rows, so large files
/// with a small edit neither produce a wall of text nor thousands of items.
pub fn compute_line_diff(original: &str, changed: &str, show_unchanged: bool) -> LineDiff {
    let diff = TextDiff::from_lines(original, changed);
    let mut result = LineDiff { rows: Vec::new(), added: 0, removed: 0, max_line: original.lines().count().max(changed.lines().count()) };
    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Insert => result.added += 1,
            ChangeTag::Delete => result.removed += 1,
            ChangeTag::Equal => {}
        }
    }
    let groups = if show_unchanged { vec![diff.ops().to_vec()] } else { diff.grouped_ops(3) };
    for group in groups {
        let (Some(first), Some(last)) = (group.first(), group.last()) else { continue };
        let old = first.old_range().start..last.old_range().end;
        let new = first.new_range().start..last.new_range().end;
        result.rows.push(LineDiffRow::Hunk {
            old_start: old.start + usize::from(!old.is_empty()),
            old_count: old.len(),
            new_start: new.start + usize::from(!new.is_empty()),
            new_count: new.len(),
        });
        for op in &group {
            for change in diff.iter_changes(op) {
                result.rows.push(LineDiffRow::Line {
                    tag: match change.tag() {
                        ChangeTag::Equal => "equal",
                        ChangeTag::Delete => "delete",
                        ChangeTag::Insert => "insert",
                    },
                    text: change.value().trim_end_matches('\n').trim_end_matches('\r').to_string(),
                    old_line: change.old_index().map(|line| line + 1),
                    new_line: change.new_index().map(|line| line + 1),
                    missing_newline: !change.value().ends_with('\n'),
                });
            }
        }
    }
    result
}

pub fn compute_word_diff(original: &str, changed: &str) -> Vec<DiffChunk> {
    let diff = TextDiff::from_words(original, changed);
    diff.iter_all_changes()
        .map(|change| {
            let tag = match change.tag() {
                ChangeTag::Equal => "Equal",
                ChangeTag::Delete => "Delete",
                ChangeTag::Insert => "Insert",
            };
            DiffChunk { tag: tag.to_string(), value: change.value().to_string() }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_texts_produce_only_equal_chunks() {
        let result = compute_word_diff("hello world", "hello world");
        assert!(result.iter().all(|c| c.tag == "Equal"));
    }

    #[test]
    fn detects_insertion() {
        let result = compute_word_diff("hello", "hello world");
        assert!(result.iter().any(|c| c.tag == "Insert"));
    }

    #[test]
    fn a_large_file_shows_only_the_changed_hunks_with_correct_line_numbers() {
        let lines: Vec<String> = (0..10000).map(|line| format!("line {line}\n")).collect();
        let mut changed = lines.clone();
        changed[2500] = "changed first\n".into();
        changed[7500] = "changed second\n".into();
        let result = compute_line_diff(&lines.concat(), &changed.concat(), false);
        assert_eq!((result.added, result.removed), (2, 2));
        assert_eq!(result.rows.len(), 18);
        assert!(matches!(result.rows[0], LineDiffRow::Hunk { old_start: 2498, old_count: 7, new_start: 2498, new_count: 7 }));
        assert!(
            matches!(&result.rows[4], LineDiffRow::Line { tag: "delete", old_line: Some(2501), new_line: None, text, .. } if text == "line 2500")
        );
        assert!(
            matches!(&result.rows[5], LineDiffRow::Line { tag: "insert", old_line: None, new_line: Some(2501), text, .. } if text == "changed first")
        );
    }

    #[test]
    fn new_and_deleted_files_keep_empty_sides_and_blank_lines() {
        for (old, new, added, removed) in [("", "one\n\nthree\n", 3, 0), ("one\n\nthree\n", "", 0, 3)] {
            let result = compute_line_diff(old, new, false);
            assert_eq!((result.added, result.removed), (added, removed));
            assert_eq!(result.rows.len(), 4);
            assert!(matches!(&result.rows[2], LineDiffRow::Line { text, .. } if text.is_empty()));
        }
    }

    #[test]
    fn unchanged_lines_are_optional_and_never_count_as_edits() {
        assert!(compute_line_diff("one\ntwo\n", "one\ntwo\n", false).rows.is_empty());
        let full = compute_line_diff("one\ntwo\n", "one\ntwo\n", true);
        assert_eq!((full.added, full.removed, full.rows.len()), (0, 0, 3));
        assert!(compute_line_diff("", "", true).rows.is_empty());
    }

    #[test]
    fn a_missing_final_newline_remains_visible_and_text_is_not_html() {
        let result = compute_line_diff("<b>literal</b>", "<b>literal</b>\n", false);
        assert!(
            matches!(&result.rows[1], LineDiffRow::Line { tag: "delete", text, missing_newline: true, .. } if text == "<b>literal</b>")
        );
        assert!(matches!(&result.rows[2], LineDiffRow::Line { tag: "insert", missing_newline: false, .. }));
    }
}
