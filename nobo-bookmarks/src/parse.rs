//! Bookmarks out of a file's text, the way org reads an outline.
//!
//! Not the core's heading list: that one takes a `*`-line for structure
//! only with a TODO keyword, a `:project:` tag or a CUSTOM_ID (see
//! nobo_core's models::is_real_structure), and a bookmarks file is a plain
//! outline -- `* Rust` / `** reqwest`. Like the cookbook, this reads every
//! `*`-line outside a code block as a heading, as org itself does.
//!
//! One pass gives both halves of a file: its bookmarks, each with the
//! line ranges it is made of (so a write can change exactly those lines
//! and nothing else, see `write`), and its collections, each with its
//! heading's line (so a write files under THAT heading, not the first one
//! that happens to carry the same title).

use std::hash::{Hash, Hasher};

use nobo_core::core::org_syntax::{self, property_drawer};
use nobo_core::core::vault::{self, closed_block_lines, parse_heading_line};

use crate::model::Bookmark;
use crate::url;

/// The tag that means "read later".
pub const READ_LATER: &str = "readlater";

/// One bookmark and where its parts are, as 0-based line indexes.
#[derive(Debug, Clone)]
pub struct Entry {
    pub bookmark: Bookmark,
    pub heading: usize,
    /// The heading line as org reads it -- what an edit writes back, with
    /// its TODO keyword, priority and cookie intact.
    pub heading_line: vault::HeadingLine,
    /// The `:URL:` line.
    pub url_line: usize,
    /// The drawer's `:END:` line.
    pub drawer_end: usize,
    /// Where the body starts: after the property drawer.
    pub body_start: usize,
    /// Where the note ends: the description's quote block, or the end of
    /// the body.
    pub note_end: usize,
    /// The description's quote block, `#+begin_quote` to `#+end_quote`.
    pub quote: Option<(usize, usize)>,
    /// The next heading, of any level: where this entry's own lines end.
    pub own_end: usize,
    /// The first line after this bookmark's subtree.
    pub subtree_end: usize,
}

/// One collection: a heading that is no bookmark, nor inside one.
#[derive(Debug, Clone, PartialEq)]
pub struct CollectionAt {
    /// Its titles, outermost first.
    pub path: Vec<String>,
    /// Its heading (0-based).
    pub heading: usize,
}

/// A bookmarks file, read.
#[derive(Debug, Clone, Default)]
pub struct Outline {
    pub entries: Vec<Entry>,
    pub collections: Vec<CollectionAt>,
}

impl Outline {
    /// The collection PATH names. The first of several with the same
    /// path -- what a write files under, and what the list shows.
    pub fn collection(&self, path: &[String]) -> Option<&CollectionAt> {
        self.collections.iter().find(|collection| collection.path == path)
    }
}

/// Every collection in TEXT, as its path of titles (outermost first) with
/// its line (1-based), in file order -- the empty ones too, so a
/// collection made by hand or by an import can be filed into before it
/// holds anything.
pub fn collections_of(text: &str) -> Vec<(Vec<String>, usize)> {
    outline("", "", text).collections.into_iter().map(|collection| (collection.path, collection.heading + 1)).collect()
}

/// What a bookmarks file is called as a category: its `#+TITLE:`, else
/// its file name without `bookmarks-` and `.org` ("work" for
/// `bookmarks-work.org`).
pub fn category_title(path: &str, text: &str) -> String {
    let lines: Vec<&str> = text.lines().take(40).collect();
    if let Some(title) = nobo_core::core::file_kind::leading_keyword(&lines, "#+TITLE:").filter(|title| !title.is_empty()) {
        return title;
    }
    let name = std::path::Path::new(path).file_stem().and_then(|stem| stem.to_str()).unwrap_or(path);
    name.strip_prefix("bookmarks-").unwrap_or(name).to_string()
}

/// Every bookmark in TEXT (the file PATH, called CATEGORY).
pub fn bookmarks_of(path: &str, category: &str, text: &str) -> Vec<Bookmark> {
    entries(path, category, text).into_iter().map(|entry| entry.bookmark).collect()
}

/// Every bookmark in TEXT, with its line ranges.
pub fn entries(path: &str, category: &str, text: &str) -> Vec<Entry> {
    outline(path, category, text).entries
}

/// The address a `:URL:` value names: the value itself, or the target of
/// an org link written there (`[[https://x][X]]`). `None` for an empty
/// one -- a heading with an empty `:URL:` is no bookmark yet.
pub fn url_of(value: &str) -> Option<String> {
    let value = value.trim();
    let value = match value.strip_prefix("[[").and_then(|rest| rest.strip_suffix("]]")) {
        Some(link) => link.split("][").next().unwrap_or(link).trim(),
        None => value,
    };
    (!value.is_empty()).then(|| value.to_string())
}

/// A title as it is shown: org links by what they say (`[[url][Name]]` is
/// "Name", `[[url]]` the address). The heading itself keeps them.
pub fn display_title(title: &str) -> String {
    let mut out = String::new();
    let mut rest = title;
    while let Some(start) = rest.find("[[") {
        let Some(end) = rest[start..].find("]]") else { break };
        out.push_str(&rest[..start]);
        let link = &rest[start + 2..start + end];
        out.push_str(link.split_once("][").map_or(link, |(_, description)| description));
        rest = &rest[start + end + 2..];
    }
    out.push_str(rest);
    out
}

/// TEXT read as a bookmarks file.
pub fn outline(path: &str, category: &str, text: &str) -> Outline {
    let lines: Vec<&str> = text.lines().collect();
    let in_block = closed_block_lines(&lines);
    let headings: Vec<(usize, vault::HeadingLine)> = lines
        .iter()
        .enumerate()
        .filter(|(index, _)| !in_block[*index])
        .filter_map(|(index, line)| parse_heading_line(line).map(|heading| (index, heading)))
        .collect();

    let mut out = Outline::default();
    // The headings above the current one: (level, title, tags, is a bookmark).
    let mut stack: Vec<(usize, String, Vec<String>, bool)> = Vec::new();
    // How often each address has come up so far -- a second bookmark to
    // the same page in one file needs a key of its own.
    let mut seen: Vec<String> = Vec::new();

    for (position, (index, heading)) in headings.iter().enumerate() {
        while stack.last().is_some_and(|(level, ..)| *level >= heading.level) {
            stack.pop();
        }
        let own_end = headings.get(position + 1).map(|(next, _)| *next).unwrap_or(lines.len());
        let subtree_end = headings[position + 1..]
            .iter()
            .find(|(_, next)| next.level <= heading.level)
            .map(|(next, _)| *next)
            .unwrap_or(lines.len());

        let drawer = property_drawer(&lines, *index).filter(|drawer| drawer.end < own_end);
        let url = drawer.as_ref().and_then(|drawer| drawer.get("URL")).and_then(|property| url_of(&property.value).map(|url| (property.line, url)));
        let inside_bookmark = stack.iter().any(|(.., bookmark)| *bookmark);

        match (url, drawer) {
            (Some((url_line, url_value)), Some(drawer)) => {
                let body_start = drawer.end + 1;
                let quote = description_block(&lines, body_start, own_end);
                let note_end = quote.map_or(own_end, |(start, _)| start);
                let own_tags: Vec<String> = heading.tags.iter().filter(|tag| *tag != READ_LATER).cloned().collect();
                let inherited_tags: Vec<String> = stack
                    .iter()
                    .flat_map(|(_, _, tags, _)| tags.iter().cloned())
                    .filter(|tag| tag != READ_LATER && !own_tags.contains(tag))
                    .collect();
                let occurrence = seen.iter().filter(|earlier| **earlier == url_value).count();
                seen.push(url_value.clone());
                let key = match occurrence {
                    0 => format!("{path}\n{url_value}"),
                    n => format!("{path}\n{url_value}\n{n}"),
                };
                out.entries.push(Entry {
                    bookmark: Bookmark {
                        key,
                        revision: revision(&lines[*index..own_end]),
                        file_path: path.to_string(),
                        line_no: index + 1,
                        title: display_title(&heading.title),
                        host: url::host(&url_value),
                        url: url_value,
                        tags: own_tags,
                        inherited_tags,
                        read_later: heading.tags.iter().any(|tag| tag == READ_LATER),
                        created: drawer.get("CREATED").map(|property| property.value.clone()),
                        updated: drawer.get("UPDATED").map(|property| property.value.clone()),
                        note: body_text(&lines[body_start.min(note_end)..note_end]),
                        description: quote.map(|(start, end)| org_syntax::one_line(&body_text(&lines[start + 1..end]))).filter(|text| !text.is_empty()),
                        collection: stack.iter().filter(|(.., bookmark)| !bookmark).map(|(_, title, ..)| title.clone()).collect(),
                        category: category.to_string(),
                    },
                    heading: *index,
                    heading_line: heading.clone(),
                    url_line,
                    drawer_end: drawer.end,
                    body_start,
                    note_end,
                    quote,
                    own_end,
                    subtree_end,
                });
            }
            // A heading under a bookmark is part of that bookmark, not a
            // collection of its own.
            _ if !inside_bookmark => {
                let mut path: Vec<String> = stack.iter().map(|(_, title, ..)| title.clone()).collect();
                path.push(heading.title.clone());
                out.collections.push(CollectionAt { path, heading: *index });
            }
            _ => {}
        }
        let is_bookmark = out.entries.last().is_some_and(|entry| entry.heading == *index);
        stack.push((heading.level, heading.title.clone(), heading.tags.clone(), is_bookmark));
    }
    out
}

/// The quote block that ends the body FROM..UNTIL, if one does: that is
/// where a saved page's description goes. A quote the user wrote further
/// up, with their own words after it, is part of the note.
fn description_block(lines: &[&str], from: usize, until: usize) -> Option<(usize, usize)> {
    let mut end = until;
    while end > from && lines[end - 1].trim().is_empty() {
        end -= 1;
    }
    if end <= from || !lines[end - 1].trim().eq_ignore_ascii_case("#+end_quote") {
        return None;
    }
    let close = end - 1;
    let open = (from..close).rev().find(|&at| lines[at].trim().eq_ignore_ascii_case("#+begin_quote"))?;
    Some((open, close))
}

/// Body lines as the text they stand for: what `write` escaped so org
/// would not read it as structure is read back as it was typed.
fn body_text(lines: &[&str]) -> String {
    lines.iter().map(|line| org_syntax::unescape_body_line(line)).collect::<Vec<_>>().join("\n").trim().to_string()
}

/// A fingerprint of an entry's own lines. Sent back with an edit, it tells
/// whether the entry is still what the user saw -- or was changed since,
/// in Emacs or by a sync, and must not be overwritten blindly.
fn revision(lines: &[&str]) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for line in lines {
        line.trim_end().hash(&mut hasher);
    }
    format!("{:016x}", hasher.finish())
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
        assert_eq!(reqwest.key, "/v/arbeit.org\nhttps://docs.rs/reqwest");
        assert!(!reqwest.read_later);
    }

    #[test]
    fn every_collection_is_listed_also_an_empty_one() {
        let text = "* Rust\n** Async\n** reqwest\n:PROPERTIES:\n:URL: https://x\n:END:\n*** Anmerkung\n* Leer\n";
        let paths: Vec<Vec<String>> = collections_of(text).into_iter().map(|(path, _)| path).collect();
        assert_eq!(paths, [vec!["Rust".to_string()], vec!["Rust".into(), "Async".into()], vec!["Leer".into()]]);
        assert_eq!(collections_of(text)[2].1, 8, "where it is, to open it there");
    }

    #[test]
    fn a_category_is_named_by_its_title_or_its_file() {
        assert_eq!(category_title("/v/.nobo/bookmarks-work.org", "#+TITLE: Arbeit\n"), "Arbeit");
        assert_eq!(category_title("/v/.nobo/bookmarks-work.org", "* x\n"), "work");
    }

    #[test]
    fn readlater_is_a_flag_not_a_tag() {
        let later = &bookmarks_of("/v/a.org", "A", FILE)[1];
        assert!(later.read_later);
        assert!(later.tags.is_empty());
    }

    /// `org-adapt-indentation` writes the drawer indented under its heading.
    #[test]
    fn an_indented_drawer_is_read() {
        let text = "* Rust\n** reqwest\n   :PROPERTIES:\n   :URL: https://docs.rs/reqwest\n   :END:\n   Notiz.\n";
        let found = bookmarks_of("/v/a.org", "A", text);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].note, "Notiz.");
    }

    #[test]
    fn an_empty_url_is_no_bookmark_and_a_link_is_its_target() {
        let text = "* Leer\n:PROPERTIES:\n:URL:\n:END:\n* Link\n:PROPERTIES:\n:URL: [[https://x.example/a][X]]\n:END:\n";
        let found = bookmarks_of("/v/a.org", "A", text);
        assert_eq!(found.len(), 1);
        assert_eq!((found[0].url.as_str(), found[0].host.as_str()), ("https://x.example/a", "x.example"));
        assert_eq!(collections_of(text)[0].0, ["Leer"], "with nothing to point to, it is a collection");
    }

    #[test]
    fn a_link_in_a_title_shows_what_it_says() {
        assert_eq!(display_title("Lies [[https://x][das Buch]] heute"), "Lies das Buch heute");
        assert_eq!(display_title("[[https://x]]"), "https://x");
        assert_eq!(display_title("ohne"), "ohne");
    }

    /// A quote the user wrote in the middle of the note stays in the note;
    /// only the one that ends the entry is the page's description.
    #[test]
    fn only_the_closing_quote_is_the_description() {
        let text = "* B\n:PROPERTIES:\n:URL: https://x\n:END:\nVorher.\n#+begin_quote\nZitat.\n#+end_quote\nNachher.\n";
        let bookmark = &bookmarks_of("/v/a.org", "A", text)[0];
        assert_eq!(bookmark.note, "Vorher.\n#+begin_quote\nZitat.\n#+end_quote\nNachher.");
        assert_eq!(bookmark.description, None);
    }

    #[test]
    fn the_same_address_twice_has_two_keys() {
        let text = "* A\n:PROPERTIES:\n:URL: https://x\n:END:\n* B\n:PROPERTIES:\n:URL: https://x\n:END:\n";
        let found = bookmarks_of("/v/a.org", "A", text);
        assert_ne!(found[0].key, found[1].key);
    }

    #[test]
    fn a_skipped_level_still_nests() {
        let text = "* A\n*** B\n**** x\n:PROPERTIES:\n:URL: https://x\n:END:\n";
        assert_eq!(bookmarks_of("/v/a.org", "A", text)[0].collection, ["A", "B"]);
        assert_eq!(collections_of(text)[1].0, ["A", "B"]);
    }

    #[test]
    fn the_revision_changes_with_the_entry() {
        let before = &bookmarks_of("/v/a.org", "A", FILE)[0];
        let after = &bookmarks_of("/v/a.org", "A", &FILE.replace("Für das", "Fürs"))[0];
        assert_ne!(before.revision, after.revision);
        let unrelated = &bookmarks_of("/v/a.org", "A", &FILE.replace("Kein Lesezeichen hier.", "Anders."))[0];
        assert_eq!(before.revision, unrelated.revision, "a change elsewhere is no conflict");
    }
}
