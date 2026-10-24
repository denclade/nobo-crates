// SPDX-FileCopyrightText: 2026 Dennis Burgermeister
// SPDX-License-Identifier: GPL-3.0-or-later

//! The plain org outline of a file: every `*`-line is a heading, the way
//! org itself reads one.
//!
//! Deliberately separate from `org_parser`, which answers a different
//! question -- see the module comment in `mod.rs`. Line parsing itself is
//! not duplicated: the heading line and the property drawer go through
//! `org_parser`'s own readers, so tag syntax and case-insensitive property
//! keys behave identically here and everywhere else.

use std::collections::HashMap;

use nobo_core::core::models::{ORG_PROPERTIES_END, ORG_PROPERTIES_START};
use nobo_core::core::vault;

#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    pub level: usize,
    /// 1-based, so a recipe can be opened at its line in the editor.
    pub line_no: usize,
    pub title: String,
    pub tags: Vec<String>,
    pub properties: HashMap<String, String>,
    /// Everything between this heading and the next one, drawer excluded.
    pub content: String,
}

pub fn read(text: &str) -> Vec<Node> {
    let mut nodes: Vec<Node> = Vec::new();
    let mut in_drawer = false;
    // A `*`-line inside a code block is text, as everywhere else in the
    // app (see vault::closed_block_lines). Read as a heading here, a
    // quoted org outline became recipes in the cookbook view -- and an
    // import, which places itself by this outline, was written into the
    // middle of the quote.
    let lines: Vec<&str> = text.lines().collect();
    let in_block = vault::closed_block_lines(&lines);

    for (index, line) in lines.iter().copied().enumerate() {
        let trimmed = line.trim();

        let heading_here = if in_block[index] { None } else { vault::parse_heading_line(line) };
        if let Some(heading) = heading_here {
            let (level, title, tags) = (heading.level, heading.title, heading.tags);
            nodes.push(Node { level, line_no: index + 1, title, tags, properties: HashMap::new(), content: String::new() });
            in_drawer = false;
            continue;
        }

        let Some(current) = nodes.last_mut() else { continue };

        if trimmed.eq_ignore_ascii_case(ORG_PROPERTIES_START) {
            in_drawer = true;
            continue;
        }
        if in_drawer {
            if trimmed.eq_ignore_ascii_case(ORG_PROPERTIES_END) {
                in_drawer = false;
            } else if let Some((key, value)) = vault::parse_property(trimmed) {
                current.properties.insert(key, value);
            }
            continue;
        }

        current.content.push_str(line);
        current.content.push('\n');
    }

    for node in &mut nodes {
        while node.content.ends_with('\n') {
            node.content.pop();
        }
    }
    nodes
}
