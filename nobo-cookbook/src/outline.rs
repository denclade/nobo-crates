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

    for (index, line) in text.lines().enumerate() {
        let trimmed = line.trim();

        if let Some(heading) = vault::parse_heading_line(line) {
            let (level, title, tags) = (heading.level, heading.title, heading.tags);
            nodes.push(Node {
                level,
                line_no: index + 1,
                title,
                tags,
                properties: HashMap::new(),
                content: String::new(),
            });
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
