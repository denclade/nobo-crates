// SPDX-FileCopyrightText: 2026 Dennis Burgermeister
// SPDX-License-Identifier: GPL-3.0-or-later

//! Adding an entry under a heading path, without disturbing the rest.
//!
//! What a feature does when it files something into an org file the user
//! also writes by hand: a recipe under "Hauptgerichte", a bookmark under
//! "Rust". The text around it -- other entries, prose, code blocks, the
//! blank lines between sections, the file's line endings -- stays as it
//! was; the entry goes at the end of the heading it belongs under.

use crate::core::org_parser::{closed_block_lines, parse_heading_line};

/// One heading line, as far as placing an entry needs it.
struct Node {
    level: usize,
    /// 1-based.
    line_no: usize,
    title: String,
}

/// Every heading of TEXT, the way org reads them: a `*`-line inside a
/// closed block is text, not a heading.
fn outline(text: &str) -> Vec<Node> {
    let lines: Vec<&str> = text.lines().collect();
    let in_block = closed_block_lines(&lines);
    lines
        .iter()
        .enumerate()
        .filter(|(index, _)| !in_block[*index])
        .filter_map(|(index, line)| {
            parse_heading_line(line).map(|heading| Node { level: heading.level, line_no: index + 1, title: heading.title })
        })
        .collect()
}

/// TEXT with ENTRY added: at the end of the heading CATEGORY names (a
/// path of titles, outermost first),
/// or at the end of the file at level 1 when CATEGORY is empty or not
/// there. ENTRY is built for the level it lands on.
///
/// Returns the new text and the line the entry starts on (1-based), so
/// the caller can open it.
pub fn insert_under(text: &str, category: &[String], entry: impl Fn(usize) -> String) -> (String, usize) {
    let nodes = outline(text);
    let heading = category_heading(&nodes, category).map(|index| nodes[index].line_no);
    insert_under_heading(text, heading, entry)
}

/// The same for a heading the caller has already found, by its line
/// (1-based) -- a feature that tells its own headings apart from the
/// others with the same title (a bookmark called "Rust" next to the
/// collection "Rust"). `None`, or a line that holds no heading: the end
/// of the file, at level 1.
pub fn insert_under_heading(text: &str, heading_line: Option<usize>, entry: impl Fn(usize) -> String) -> (String, usize) {
    let nodes = outline(text);
    // Each line with its own ending, and new ones in the file's: a file
    // written with CRLF (Emacs on Windows, Orgzly via some sync clients)
    // came back entirely LF after an import -- every line changed, as far
    // as a diff or the other device's sync is concerned.
    let eol = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut lines: Vec<String> = text.split_inclusive('\n').map(str::to_string).collect();
    if let Some(last) = lines.last_mut().filter(|last| !last.ends_with('\n')) {
        last.push_str(eol);
    }

    let target = heading_line.and_then(|line| nodes.iter().position(|node| node.line_no == line));
    let (at, level) = match target {
        Some(index) => {
            let node = &nodes[index];
            // The end of its subtree: the next heading at its level or
            // above, or the end of the file.
            let end = nodes[index + 1..].iter().find(|next| next.level <= node.level).map(|next| next.line_no - 1).unwrap_or(lines.len());
            (end, node.level + 1)
        }
        None => (lines.len(), 1),
    };

    // Trailing blank lines of the place it goes stay after it, so
    // whatever spacing the file had between sections survives.
    let mut at = at;
    while at > 0 && lines[at - 1].trim().is_empty() {
        at -= 1;
    }
    let block = entry(level);
    let mut spaced: Vec<String> = Vec::new();
    if at > 0 {
        spaced.push(eol.to_string());
    }
    spaced.extend(block.lines().map(|line| format!("{line}{eol}")));
    let first_line = at + 1 + usize::from(at > 0);
    lines.splice(at..at, spaced);

    (lines.concat(), first_line)
}

/// Whether TEXT has the heading PATH names (titles, outermost first) --
/// for a caller that creates the missing ones before filing under them.
pub fn has_heading_path(text: &str, path: &[String]) -> bool {
    path.is_empty() || category_heading(&outline(text), path).is_some()
}

fn category_heading(nodes: &[Node], category: &[String]) -> Option<usize> {
    let mut found: Option<usize> = None;
    for title in category {
        let (from, level, until) = match found {
            None => (0, 1, nodes.len()),
            Some(parent) => {
                let parent_level = nodes[parent].level;
                let until = nodes[parent + 1..]
                    .iter()
                    .position(|next| next.level <= parent_level)
                    .map(|offset| parent + 1 + offset)
                    .unwrap_or(nodes.len());
                (parent + 1, parent_level + 1, until)
            }
        };
        found = Some((from..until).find(|&index| nodes[index].level == level && nodes[index].title.trim() == title.trim())?);
    }
    found
}

#[cfg(test)]
mod tests {
    use super::insert_under;

    fn entry(level: usize) -> String {
        format!("{} Neu\n", "*".repeat(level))
    }

    #[test]
    fn an_entry_goes_at_the_end_of_its_heading_one_level_down() {
        let text = "* Rust\n** alt\n* Reisen\n";
        let (out, line) = insert_under(text, &["Rust".to_string()], entry);
        assert_eq!(out, "* Rust\n** alt\n\n** Neu\n* Reisen\n");
        assert_eq!(line, 4);
    }

    #[test]
    fn a_heading_path_is_found_level_by_level() {
        let text = "* Rust\n** Async\n* Reisen\n";
        assert!(super::has_heading_path(text, &["Rust".into(), "Async".into()]));
        assert!(!super::has_heading_path(text, &["Reisen".into(), "Async".into()]));
        assert!(super::has_heading_path(text, &[]));
    }

    #[test]
    fn without_a_path_it_goes_at_the_end_of_the_file() {
        let (out, _) = insert_under("* A\n", &[], entry);
        assert_eq!(out, "* A\n\n* Neu\n");
    }

    #[test]
    fn a_star_line_in_a_code_block_is_no_heading_to_file_under() {
        let text = "#+begin_src org\n,* Rust\n#+end_src\n* Rust\n";
        let (out, _) = insert_under(text, &["Rust".to_string()], entry);
        assert!(out.ends_with("* Rust\n\n** Neu\n"), "{out}");
        assert!(out.starts_with("#+begin_src org\n,* Rust\n#+end_src"), "{out}");
    }
}
