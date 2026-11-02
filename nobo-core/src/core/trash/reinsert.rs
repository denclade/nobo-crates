// SPDX-FileCopyrightText: 2026 Dennis Burgermeister
// SPDX-License-Identifier: GPL-3.0-or-later

//! Reinsert deleted subtrees by identity, preserving today's neighbours
//! and ancestor metadata. File-level reverse patches are only a fallback
//! for edits that did not remove a complete heading.

use crate::core::models::Heading;
use crate::core::{heading_identity, heading_utils, org_parser};
use crate::utils::fs_utils;
use std::collections::HashSet;
use std::path::PathBuf;

/// Restore removed attachment links in their original heading, without
/// rolling back other text typed during the same editing session.
pub(super) fn restore_links(path: &Path, before: &str, current: &str, saved: &HashSet<PathBuf>) -> Result<String, String> {
    let old = org_parser::parse_headings_from_lines(path, before.lines());
    let mut lines: Vec<String> = current.lines().map(str::to_string).collect();
    let mut present = crate::core::attachments::references(path, current);
    for (line_no, line) in before.lines().enumerate() {
        let mut links: Vec<String> = crate::core::org_ast::inline_spans(line)
            .into_iter()
            .filter_map(|span| {
                let target = span.target?;
                Some(match span.description {
                    Some(description) => format!("[[{target}][{description}]]"),
                    None => format!("[[{target}]]"),
                })
            })
            .collect();
        // Markdown and bare file: links are also uses. Preserve the whole
        // original line when there is no Org span that can restore them.
        links.push(line.to_string());
        for raw in links {
            let wanted = crate::core::attachments::references(path, &raw);
            if !wanted.iter().any(|p| saved.contains(p) && !present.contains(p)) {
                continue;
            }
            let flat = org_parser::parse_headings_from_lines(path, &lines);
            let owner = old.iter().rev().find(|h| h.line_no <= line_no + 1);
            let at = if let Some(owner) = owner {
                locate(&flat, owner)?
                    .ok_or_else(|| {
                        crate::t!(
                            "Die Überschrift „{}“ muss zuerst wiederhergestellt werden.",
                            "Restore the heading “{}” first.",
                            owner.title
                        )
                    })?
                    .last_line
            } else {
                lines.len()
            };
            lines.insert(at.min(lines.len()), raw);
            present.extend(wanted);
        }
    }
    let result = fs_utils::lines_to_content(&lines);
    let restored = crate::core::attachments::references(path, &result);
    if crate::core::attachments::references(path, before).intersection(saved).any(|path| !restored.contains(path)) {
        return Err(crate::t!(
            "Die ursprünglichen Anhang-Verweise konnten nicht sicher wiederhergestellt werden.",
            "The original attachment links could not be restored safely."
        ));
    }
    Ok(result)
}
use std::path::Path;

/// Missing and ambiguous identities are different: only the former is
/// evidence that a subtree was deleted or a sibling anchor disappeared.
fn locate<'a>(flat: &'a [Heading], heading: &Heading) -> Result<Option<&'a Heading>, String> {
    let mut matches = flat.iter().filter(|candidate| {
        if let Some(id) = heading_identity::custom_id_of(heading) {
            heading_identity::custom_id_of(candidate) == Some(id)
        } else {
            candidate.title == heading.title
        }
    });
    let found = matches.next();
    if matches.next().is_some() {
        return Err(crate::t!(
            "Die Überschrift „{}“ ist nicht eindeutig; Wiederherstellen wurde abgebrochen.",
            "The heading “{}” is ambiguous; restoring was aborted.",
            heading.title
        ));
    }
    Ok(found)
}

pub(super) fn deleted_title(path: &Path, before: &str, after: &str) -> Option<String> {
    let before = org_parser::parse_headings_from_lines(path, before.lines());
    let after = org_parser::parse_headings_from_lines(path, after.lines());
    before.iter().find(|h| matches!(locate(&after, h), Ok(None))).map(|h| h.title.clone())
}

pub(super) fn reinsert(path: &Path, before: &str, after: &str, current: &str) -> Result<Option<String>, String> {
    let old_lines: Vec<String> = before.lines().map(str::to_string).collect();
    let old = org_parser::parse_headings_from_lines(path, &old_lines);
    let after = org_parser::parse_headings_from_lines(path, after.lines());
    let mut removed = Vec::new();
    let mut end_of_removed = 0;
    for (idx, h) in old.iter().enumerate() {
        if h.line_no <= end_of_removed {
            continue;
        }
        locate(&old, h)?;
        if locate(&after, h)?.is_some() {
            continue;
        }
        let (_, end) = heading_utils::subtree_line_range(&old, idx);
        removed.push((idx, end));
        end_of_removed = end;
    }
    if removed.is_empty() {
        return Ok(None);
    }
    let mut lines: Vec<String> = current.lines().map(str::to_string).collect();
    // Reverse order lets an earlier deleted sibling use the sibling just
    // restored behind it as an anchor, including a fully deleted file body.
    for (idx, end) in removed.into_iter().rev() {
        let h = &old[idx];
        let flat = org_parser::parse_headings_from_lines(path, &lines);
        if let Some(existing) = locate(&flat, h)? {
            let existing_idx = flat.iter().position(|item| item.line_no == existing.line_no).unwrap();
            let (_, existing_end) = heading_utils::subtree_line_range(&flat, existing_idx);
            let mut original = old_lines[h.line_no - 1..end].to_vec();
            shift_levels(&mut original, &old, h.line_no, existing.level as isize - h.level as isize);
            if original != lines[existing.line_no - 1..existing_end] {
                return Err(crate::t!(
                    "Die Überschrift „{}“ existiert bereits mit anderem Inhalt; Wiederherstellen wurde abgebrochen.",
                    "The heading “{}” already exists with different content; restoring was aborted.",
                    h.title
                ));
            }
            continue;
        }
        // The root is missing, so the whole block is inserted. Its
        // descendants must not duplicate an identity that lives on today --
        // a child restored or moved separately in the meantime, or one of
        // the blocks restored before this one (already part of LINES).
        for descendant in old.iter().filter(|d| d.line_no > h.line_no && d.line_no <= end) {
            let Some(id) = heading_identity::custom_id_of(descendant) else { continue };
            let same_id = |candidate: &&Heading| heading_identity::custom_id_of(candidate) == Some(id);
            if old.iter().filter(same_id).count() > 1 || flat.iter().any(|c| same_id(&c)) {
                return Err(crate::t!(
                    "Die Überschrift „{}“ existiert bereits; Wiederherstellen wurde abgebrochen.",
                    "The heading “{}” already exists; restoring was aborted.",
                    descendant.title
                ));
            }
        }
        let parent = heading_utils::ancestors_of(&old, idx).into_iter().next();
        let current_parent = parent
            .as_ref()
            .map(|p| {
                locate(&flat, p)?.ok_or_else(|| {
                    crate::t!(
                        "Die übergeordnete Überschrift „{}“ muss zuerst wiederhergestellt werden.",
                        "Restore the parent heading “{}” first.",
                        p.title
                    )
                })
            })
            .transpose()?;
        let scope = current_parent.map(|parent| {
            let idx = flat.iter().position(|h| h.line_no == parent.line_no).unwrap();
            heading_utils::subtree_line_range(&flat, idx)
        });
        let mut sibling = None;
        for candidate in old.iter().filter(|s| s.line_no > end).take_while(|s| s.level >= h.level).filter(|s| s.level == h.level) {
            if let Some(found) = locate(&flat, candidate)? {
                if scope.is_none_or(|(start, end)| found.line_no > start && found.line_no <= end) {
                    sibling = Some(found);
                    break;
                }
            }
        }
        let at = sibling.map(|s| s.line_no - 1).or_else(|| scope.map(|(_, end)| end)).unwrap_or(lines.len());
        let mut block = old_lines[h.line_no - 1..end].to_vec();
        if let (Some(original), Some(parent)) = (parent, current_parent) {
            let shift = parent.level as isize - original.level as isize;
            shift_levels(&mut block, &old, h.line_no, shift);
        }
        lines.splice(at..at, block);
    }
    Ok(Some(fs_utils::lines_to_content(&lines)))
}

fn shift_levels(block: &mut [String], headings: &[Heading], start_line: usize, shift: isize) {
    if shift != 0 {
        for (offset, line) in block.iter_mut().enumerate() {
            if let Some(heading) = headings.iter().find(|heading| heading.line_no == start_line + offset) {
                let level = (heading.level as isize + shift).max(1) as usize;
                *line = format!("{}{}", "*".repeat(level), &line[heading.level..]);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reused_identity_with_different_content_is_a_conflict() {
        let before = "* TODO Original\n:PROPERTIES:\n:CUSTOM_ID: same-id\n:END:\nOriginal description\n* TODO Keep\n";
        let current = "* TODO Renamed\n:PROPERTIES:\n:CUSTOM_ID: same-id\n:END:\nNew description\n* TODO Keep\n";
        assert!(reinsert(Path::new("a.org"), before, "* TODO Keep\n", current).is_err());
    }

    #[test]
    fn an_already_restored_identical_subtree_is_idempotent() {
        let before = "* TODO Original\n:PROPERTIES:\n:CUSTOM_ID: same-id\n:END:\nOriginal description\n* TODO Keep\n";
        assert_eq!(reinsert(Path::new("a.org"), before, "* TODO Keep\n", before).unwrap().unwrap(), before);
    }

    #[test]
    fn a_descendant_id_living_on_elsewhere_is_a_conflict() {
        let before = "* Parent\n:PROPERTIES:\n:CUSTOM_ID: parent\n:END:\n** Child\n:PROPERTIES:\n:CUSTOM_ID: child\n:END:\n* TODO Keep\n";
        let current = "* TODO Keep\n* Moved child\n:PROPERTIES:\n:CUSTOM_ID: child\n:END:\nNew description\n";
        assert!(reinsert(Path::new("a.org"), before, "* TODO Keep\n", current).is_err());
    }

    #[test]
    fn a_descendant_id_duplicated_in_the_deleted_text_is_a_conflict() {
        let before = "* TODO A\n** X\n:PROPERTIES:\n:CUSTOM_ID: same\n:END:\n* TODO B\n** Y\n:PROPERTIES:\n:CUSTOM_ID: same\n:END:\n";
        assert!(reinsert(Path::new("a.org"), before, "", "").is_err());
    }

    #[test]
    fn a_deleted_parent_with_identified_children_is_restored() {
        let before = "* Parent\n:PROPERTIES:\n:CUSTOM_ID: parent\n:END:\n** Child\n:PROPERTIES:\n:CUSTOM_ID: child\n:END:\n* TODO Keep\n";
        assert_eq!(reinsert(Path::new("a.org"), before, "* TODO Keep\n", "* TODO Keep\n").unwrap().unwrap(), before);
    }

    #[test]
    fn duplicate_title_or_id_never_becomes_an_insertion_guess() {
        for before in [
            "* TODO Duplicate\n* TODO Duplicate\n",
            "* A\n:PROPERTIES:\n:CUSTOM_ID: same\n:END:\n* B\n:PROPERTIES:\n:CUSTOM_ID: same\n:END:\n",
        ] {
            assert!(reinsert(Path::new("a.org"), before, "", "").is_err());
        }
    }
    #[test]
    fn context_restore_recovers_bare_file_and_markdown_links() {
        let path = Path::new("/vault/a.org");
        for link in ["file:.attachments/x.png", "![image](.attachments/x.png)"] {
            let before = format!("* Owner\n{link}\n");
            let saved = HashSet::from([PathBuf::from("/vault/.attachments/x.png")]);
            let result = restore_links(path, &before, "* Owner\nNew description\n", &saved).unwrap();
            assert!(result.contains(link));
            assert!(result.contains("New description"));
        }
    }
}
