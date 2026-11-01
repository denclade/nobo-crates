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
        for span in crate::core::org_ast::inline_spans(line) {
            let Some(target) = span.target else { continue };
            let raw = match span.description {
                Some(description) => format!("[[{target}][{description}]]"),
                None => format!("[[{target}]]"),
            };
            let wanted = crate::core::attachments::references(path, &raw);
            if !wanted.iter().any(|p| saved.contains(p) && !present.contains(p)) {
                continue;
            }
            let flat = org_parser::parse_headings_from_lines(path, &lines);
            let owner = old.iter().rev().find(|h| h.line_no <= line_no + 1);
            let at = if let Some(owner) = owner {
                locate(&flat, owner)
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
    Ok(fs_utils::lines_to_content(&lines))
}
use std::path::Path;

fn locate<'a>(flat: &'a [Heading], heading: &Heading) -> Option<&'a Heading> {
    let target = heading_identity::target_of(heading);
    heading_identity::resolve_target(flat, &target, "Papierkorb").ok().map(|idx| &flat[idx])
}

pub(super) fn deleted_title(path: &Path, before: &str, after: &str) -> Option<String> {
    let before = org_parser::parse_headings_from_lines(path, before.lines());
    let after = org_parser::parse_headings_from_lines(path, after.lines());
    before.iter().find(|h| locate(&after, h).is_none()).map(|h| h.title.clone())
}

pub(super) fn reinsert(path: &Path, before: &str, after: &str, current: &str) -> Result<Option<String>, String> {
    let old_lines: Vec<String> = before.lines().map(str::to_string).collect();
    let old = org_parser::parse_headings_from_lines(path, &old_lines);
    let after = org_parser::parse_headings_from_lines(path, after.lines());
    let mut removed = Vec::new();
    let mut end_of_removed = 0;
    for (idx, h) in old.iter().enumerate() {
        if h.line_no <= end_of_removed || locate(&after, h).is_some() {
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
        if locate(&flat, h).is_some() {
            continue;
        }
        let parent = heading_utils::ancestors_of(&old, idx).into_iter().next();
        let current_parent = parent
            .as_ref()
            .map(|p| {
                locate(&flat, p).ok_or_else(|| {
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
        let sibling = old
            .iter()
            .filter(|s| s.line_no > end)
            .take_while(|s| s.level >= h.level)
            .filter(|s| s.level == h.level)
            .filter_map(|s| locate(&flat, s))
            .find(|s| scope.is_none_or(|(start, end)| s.line_no > start && s.line_no <= end));
        let at = sibling.map(|s| s.line_no - 1).or_else(|| scope.map(|(_, end)| end)).unwrap_or(lines.len());
        let mut block = old_lines[h.line_no - 1..end].to_vec();
        if let (Some(original), Some(parent)) = (parent, current_parent) {
            let shift = parent.level as isize - original.level as isize;
            if shift != 0 {
                for (offset, line) in block.iter_mut().enumerate() {
                    if let Some(heading) = old.iter().find(|heading| heading.line_no == h.line_no + offset) {
                        let level = (heading.level as isize + shift).max(1) as usize;
                        *line = format!("{}{}", "*".repeat(level), &line[heading.level..]);
                    }
                }
            }
        }
        lines.splice(at..at, block);
    }
    Ok(Some(fs_utils::lines_to_content(&lines)))
}
