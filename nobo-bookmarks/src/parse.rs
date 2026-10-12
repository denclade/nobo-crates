//! Bookmarks out of a file's text, the way org reads an outline.
//!
//! Not the core's heading list: that one takes a `*`-line for structure
//! only with a TODO keyword, a `:project:` tag or a CUSTOM_ID (see
//! nobo_core's models::is_real_structure), and a bookmarks file is a plain
//! outline -- `* Rust` / `** reqwest`. Like the cookbook, this reads every
//! `*`-line outside a code block as a heading, as org itself does.
//!
//! Each bookmark comes with the line ranges it is made of, so a write can
//! change exactly those lines and nothing else (see `write`).

use nobo_core::core::vault::{self, closed_block_lines, parse_heading_line};

use crate::model::Bookmark;
use crate::url;

/// The tag that means "read later".
pub const READ_LATER: &str = "readlater";

/// One bookmark and where its parts are, as 0-based line indexes.
#[derive(Debug, Clone)]
pub struct Entry {
    pub bookmark: Bookmark,
    pub level: usize,
    pub heading: usize,
    /// The `:URL:` line.
    pub url_line: usize,
    /// Where the body starts: after the property drawer.
    pub body_start: usize,
    /// Where the note ends: the quote block, a child heading, or the end
    /// of the body.
    pub note_end: usize,
    /// The first line after this bookmark's subtree.
    pub subtree_end: usize,
}

/// Every bookmark in TEXT (the file PATH, called CATEGORY).
pub fn bookmarks_of(path: &str, category: &str, text: &str) -> Vec<Bookmark> {
    entries(path, category, text).into_iter().map(|entry| entry.bookmark).collect()
}

/// Every bookmark in TEXT, with its line ranges.
pub fn entries(path: &str, category: &str, text: &str) -> Vec<Entry> {
    let lines: Vec<&str> = text.lines().collect();
    let in_block = closed_block_lines(&lines);
    let headings: Vec<(usize, vault::HeadingLine)> = lines
        .iter()
        .enumerate()
        .filter(|(index, _)| !in_block[*index])
        .filter_map(|(index, line)| parse_heading_line(line).map(|heading| (index, heading)))
        .collect();

    let mut entries = Vec::new();
    // The headings above the current one: (level, title, tags, is a bookmark).
    let mut stack: Vec<(usize, String, Vec<String>, bool)> = Vec::new();

    for (position, (index, heading)) in headings.iter().enumerate() {
        while stack.last().is_some_and(|(level, ..)| *level >= heading.level) {
            stack.pop();
        }
        let next_heading = headings.get(position + 1).map(|(next, _)| *next).unwrap_or(lines.len());
        let subtree_end = headings[position + 1..]
            .iter()
            .find(|(_, next)| next.level <= heading.level)
            .map(|(next, _)| *next)
            .unwrap_or(lines.len());

        let drawer = property_drawer(&lines, *index + 1, next_heading);
        let url = drawer.as_ref().and_then(|drawer| drawer.get("URL").map(|(line, value)| (*line, value.clone())));
        let is_bookmark = url.is_some();

        if let (Some((url_line, url_value)), Some(drawer)) = (url, drawer.as_ref()) {
            let body_start = drawer.end + 1;
            let quote = (body_start..next_heading).find(|&at| lines[at].trim().eq_ignore_ascii_case("#+begin_quote"));
            let note_end = quote.unwrap_or(next_heading);
            let description = quote.map(|start| {
                let end = (start + 1..next_heading)
                    .find(|&at| lines[at].trim().eq_ignore_ascii_case("#+end_quote"))
                    .unwrap_or(next_heading);
                lines[start + 1..end].iter().map(|line| line.trim()).collect::<Vec<_>>().join(" ").trim().to_string()
            });
            let own_tags: Vec<String> = heading.tags.iter().filter(|tag| *tag != READ_LATER).cloned().collect();
            let inherited_tags: Vec<String> = stack
                .iter()
                .flat_map(|(_, _, tags, _)| tags.iter().cloned())
                .filter(|tag| tag != READ_LATER && !own_tags.contains(tag))
                .collect();
            entries.push(Entry {
                bookmark: Bookmark {
                    file_path: path.to_string(),
                    line_no: index + 1,
                    title: heading.title.clone(),
                    host: url::host(&url_value),
                    url: url_value,
                    tags: own_tags,
                    inherited_tags,
                    read_later: heading.tags.iter().any(|tag| tag == READ_LATER),
                    created: drawer.get("CREATED").map(|(_, value)| value.clone()),
                    note: lines[body_start.min(note_end)..note_end].join("\n").trim().to_string(),
                    description: description.filter(|text| !text.is_empty()),
                    collection: stack.iter().filter(|(.., bookmark)| !bookmark).map(|(_, title, ..)| title.clone()).collect(),
                    category: category.to_string(),
                },
                level: heading.level,
                heading: *index,
                url_line,
                body_start,
                note_end,
                subtree_end,
            });
        }
        stack.push((heading.level, heading.title.clone(), heading.tags.clone(), is_bookmark));
    }
    entries
}

/// The `:PROPERTIES:` drawer right under the heading at FROM - 1, if it
/// has one: each key (upper case) with its line and value, and the line
/// of its `:END:`.
struct Drawer {
    values: Vec<(String, usize, String)>,
    end: usize,
}

impl Drawer {
    fn get(&self, key: &str) -> Option<(&usize, &String)> {
        self.values.iter().find(|(name, ..)| name == key).map(|(_, line, value)| (line, value))
    }
}

fn property_drawer(lines: &[&str], from: usize, until: usize) -> Option<Drawer> {
    // Org puts a planning line (SCHEDULED:...) between heading and drawer.
    let mut at = from;
    while at < until && is_planning(lines[at]) {
        at += 1;
    }
    if at >= until || !lines[at].trim().eq_ignore_ascii_case(":PROPERTIES:") {
        return None;
    }
    let mut values = Vec::new();
    for (line, text) in lines.iter().enumerate().take(until).skip(at + 1) {
        if text.trim().eq_ignore_ascii_case(":END:") {
            return Some(Drawer { values, end: line });
        }
        if let Some((key, value)) = vault::parse_property(text) {
            values.push((key.to_ascii_uppercase(), line, value));
        }
    }
    None
}

fn is_planning(line: &str) -> bool {
    let line = line.trim_start();
    line.starts_with("SCHEDULED:") || line.starts_with("DEADLINE:") || line.starts_with("CLOSED:")
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: &str = "#+TITLE: Arbeit\n#+NOBO_TYPE: bookmarks\n\n* Rust :dev:\n** reqwest – Rust :http:\n:PROPERTIES:\n:URL:      https://docs.rs/reqwest\n:CREATED:  [2026-09-30 Mi 20:55]\n:END:\nFür das Web-Scraping-Modul.\n\n#+begin_quote\nAn ergonomic HTTP Client.\n#+end_quote\n** Async in der Praxis :readlater:\n:PROPERTIES:\n:URL: https://blog.example.org/async\n:END:\n* Notizen\nKein Lesezeichen hier.\n#+begin_src org\n** Kein Lesezeichen\n:PROPERTIES:\n:URL: https://nope.example\n:END:\n#+end_src\n";

    #[test]
    fn a_heading_with_a_url_is_a_bookmark_and_the_rest_is_not() {
        let found = bookmarks_of("/v/arbeit.org", "Arbeit", FILE);
        assert_eq!(found.iter().map(|b| b.title.as_str()).collect::<Vec<_>>(), ["reqwest – Rust", "Async in der Praxis"]);
    }

    #[test]
    fn a_bookmark_knows_its_parts() {
        let reqwest = &bookmarks_of("/v/arbeit.org", "Arbeit", FILE)[0];
        assert_eq!(reqwest.url, "https://docs.rs/reqwest");
        assert_eq!(reqwest.host, "docs.rs");
        assert_eq!(reqwest.tags, ["http"]);
        assert_eq!(reqwest.inherited_tags, ["dev"]);
        assert_eq!(reqwest.collection, ["Rust"]);
        assert_eq!(reqwest.category, "Arbeit");
        assert_eq!(reqwest.created.as_deref(), Some("[2026-09-30 Mi 20:55]"));
        assert_eq!(reqwest.note, "Für das Web-Scraping-Modul.");
        assert_eq!(reqwest.description.as_deref(), Some("An ergonomic HTTP Client."));
        assert_eq!(reqwest.line_no, 5);
        assert!(!reqwest.read_later);
    }

    #[test]
    fn readlater_is_a_flag_not_a_tag() {
        let later = &bookmarks_of("/v/a.org", "A", FILE)[1];
        assert!(later.read_later);
        assert!(later.tags.is_empty());
    }
}
