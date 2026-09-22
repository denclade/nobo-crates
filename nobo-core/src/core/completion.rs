//! What could come next where the caret is.
//!
//! One place decides *that* something can be completed and *what* is
//! being completed -- a tag, a link, a language, an org-tempo trigger --
//! and what range a chosen item replaces. The vocabulary for the kinds
//! that need the vault (tags, notes, files) is not here: those are index
//! questions, and the host already knows how to ask them. That split is
//! what lets this stay a pure function on the synchronous door while the
//! lists themselves are fetched the normal way.

use serde::{Deserialize, Serialize};

use crate::core::expansions;
use crate::core::models::TODO_STATES;
use crate::core::org_ast::{heading_level, indent_of};
use crate::core::org_highlight::known_languages;

/// What is being completed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    /// An org-tempo trigger; `items` carries them.
    Expansion,
    /// A TODO keyword at the start of a heading; `items` carries them.
    TodoState,
    /// A language after `#+BEGIN_SRC`; `items` carries them.
    Language,
    /// A tag, after a `:` in a heading or in `#+FILETAGS:`. The host
    /// fills these from the index.
    Tag,
    /// A note, inside a `[[` or after `[[id:`. Host-filled.
    Note,
    /// A file, after `[[file:`. Host-filled.
    File,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Item {
    /// What the strip shows.
    pub label: String,
    /// What kind of thing it is, for a host that wants to name it in its
    /// own language (an expansion's "src", "quote", …).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub detail: String,
    /// What replaces `[from, to)`. May carry org-tempo's `%?`.
    pub insert: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Completion {
    pub kind: Kind,
    /// The range an accepted item replaces -- UTF-16, like everything
    /// else a host is handed.
    pub from: u32,
    pub to: u32,
    /// What has been typed of it so far, for the kinds the host fills.
    pub prefix: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<Item>,
}

fn utf16_len(text: &str) -> u32 {
    text.encode_utf16().count() as u32
}

/// What can be completed at CURSOR, or `None`.
///
/// `author` is the configured default author, for `<author` (see
/// [`expansions::entries`]).
pub fn at(text: &str, cursor: u32, author: &str) -> Option<Completion> {
    // The line the caret is on, and how far into it it is.
    let mut start = 0u32;
    let mut line = "";
    for candidate in text.split('\n') {
        let end = start + utf16_len(candidate);
        if cursor <= end {
            line = candidate;
            break;
        }
        start = end + 1;
    }
    let column = cursor.saturating_sub(start);
    let units: Vec<u16> = line.encode_utf16().collect();
    let before = String::from_utf16_lossy(&units[..(column as usize).min(units.len())]);

    // `<s`, `<date`, … -- only where org-tempo would take it.
    if expansions::is_trigger_position(&before) {
        let trigger = before.trim_start();
        let items: Vec<Item> = expansions::matching(trigger, author)
            .into_iter()
            .map(|expansion| Item {
                label: expansion.trigger,
                detail: expansion.kind,
                insert: expansion.insert,
            })
            .collect();
        if items.is_empty() {
            return None;
        }
        return Some(Completion {
            kind: Kind::Expansion,
            from: start + indent_of(line) as u32,
            to: cursor,
            prefix: trigger.to_string(),
            items,
        });
    }

    // A language, right after `#+BEGIN_SRC `.
    if let Some(typed) = source_language_prefix(&before) {
        let items: Vec<Item> = known_languages()
            .into_iter()
            .filter(|language| language.starts_with(&typed))
            .map(|language| Item { label: language.clone(), detail: String::new(), insert: language })
            .collect();
        if items.is_empty() {
            return None;
        }
        return Some(Completion {
            kind: Kind::Language,
            from: cursor - utf16_len(&typed),
            to: cursor,
            prefix: typed,
            items,
        });
    }

    // A keyword, where a heading's keyword goes.
    if let Some(typed) = todo_prefix(line, &before) {
        let items: Vec<Item> = TODO_STATES
            .iter()
            .filter(|state| state.starts_with(&typed))
            .map(|state| Item {
                label: state.to_string(),
                detail: String::new(),
                insert: format!("{state} "),
            })
            .collect();
        if items.is_empty() {
            return None;
        }
        return Some(Completion {
            kind: Kind::TodoState,
            from: cursor - utf16_len(&typed),
            to: cursor,
            prefix: typed,
            items,
        });
    }

    // A link. `[[` alone means "a note", and the two prefixes org uses
    // say which kind of thing is being pointed at.
    if let Some((kind, typed)) = link_prefix(&before) {
        return Some(Completion {
            kind,
            from: cursor - utf16_len(&typed),
            to: cursor,
            prefix: typed,
            items: Vec::new(),
        });
    }

    // A tag, in a heading's tag run or in `#+FILETAGS:`.
    if let Some(typed) = tag_prefix(line, &before) {
        return Some(Completion {
            kind: Kind::Tag,
            from: cursor - utf16_len(&typed),
            to: cursor,
            prefix: typed,
            items: Vec::new(),
        });
    }

    None
}

fn source_language_prefix(before: &str) -> Option<String> {
    let trimmed = before.trim_start();
    let rest = trimmed.get(..11).filter(|start| start.eq_ignore_ascii_case("#+BEGIN_SRC")).map(|_| &trimmed[11..])?;
    let typed = rest.strip_prefix(' ')?;
    (!typed.contains(char::is_whitespace)).then(|| typed.to_lowercase())
}

/// The keyword being typed on a heading line, which is only the first
/// word after the stars.
fn todo_prefix(line: &str, before: &str) -> Option<String> {
    let level = heading_level(line)?;
    let after_stars = before.get(level..)?;
    let typed = after_stars.strip_prefix(' ')?;
    if typed.is_empty() || typed.contains(char::is_whitespace) {
        return None;
    }
    // Only an upper-case run looks like a keyword; anything else is a
    // title, and offering DONE while somebody types "Dinner" is noise.
    typed.chars().all(|ch| ch.is_uppercase() || ch == '-').then(|| typed.to_string())
}

fn link_prefix(before: &str) -> Option<(Kind, String)> {
    let open = before.rfind("[[")?;
    let inside = &before[open + 2..];
    if inside.contains(']') {
        return None;
    }
    if let Some(typed) = inside.strip_prefix("file:") {
        return Some((Kind::File, typed.to_string()));
    }
    for prefix in ["id:", "denote:"] {
        if let Some(typed) = inside.strip_prefix(prefix) {
            return Some((Kind::Note, typed.to_string()));
        }
    }
    (!inside.contains(':')).then(|| (Kind::Note, inside.to_string()))
}

/// The tag being typed: after a `:` in the tag run at the end of a
/// heading, or anywhere in `#+FILETAGS:`.
fn tag_prefix(line: &str, before: &str) -> Option<String> {
    let in_filetags = line.trim_start().to_uppercase().starts_with("#+FILETAGS:");
    if !in_filetags && heading_level(line).is_none() {
        return None;
    }
    let colon = before.rfind(':')?;
    // On a heading, the tag run is the tail: a colon in the title is not
    // a tag (`Kaffee: kochen`), which is why the rest has to be one.
    let typed = &before[colon + 1..];
    if typed.contains(char::is_whitespace) {
        return None;
    }
    if !in_filetags {
        let before_colon = &before[..colon];
        let starts_run = before_colon.ends_with(' ') || before_colon.ends_with(':');
        if !starts_run {
            return None;
        }
    }
    Some(typed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind_at(text: &str, cursor: u32) -> Option<Kind> {
        at(text, cursor, "").map(|completion| completion.kind)
    }

    #[test]
    fn a_trigger_offers_its_expansions() {
        let completion = at("<s", 2, "").expect("a trigger");
        assert_eq!(completion.kind, Kind::Expansion);
        assert_eq!(completion.from, 0);
        assert_eq!(completion.to, 2);
        assert_eq!(completion.items[0].insert, "#+BEGIN_SRC %?\n#+END_SRC");
    }

    #[test]
    fn a_trigger_in_a_sentence_is_just_a_less_than_sign() {
        assert!(kind_at("2 <s 3", 4).is_none());
    }

    #[test]
    fn a_keyword_is_offered_only_where_one_can_stand() {
        assert_eq!(kind_at("* TO", 4), Some(Kind::TodoState));
        // A title, not a keyword.
        assert!(kind_at("* Dinner", 8).is_none());
        // Not a heading at all.
        assert!(kind_at("TO", 2).is_none());
    }

    #[test]
    fn a_language_is_offered_after_begin_src() {
        let completion = at("#+BEGIN_SRC py", 14, "").expect("a language");
        assert_eq!(completion.kind, Kind::Language);
        assert_eq!(completion.items[0].label, "python");
        assert_eq!(completion.from, 12);
    }

    #[test]
    fn a_link_says_what_it_points_at() {
        assert_eq!(kind_at("see [[", 6), Some(Kind::Note));
        assert_eq!(kind_at("see [[id:2026", 13), Some(Kind::Note));
        assert_eq!(kind_at("see [[file:a", 12), Some(Kind::File));
        // Already closed.
        assert!(kind_at("see [[a]] and", 13).is_none());
    }

    #[test]
    fn a_tag_is_offered_in_the_tag_run_and_in_filetags() {
        assert_eq!(kind_at("* Task :sh", 10), Some(Kind::Tag));
        assert_eq!(kind_at("#+FILETAGS: :de", 15), Some(Kind::Tag));
        // A colon inside a title is not a tag run.
        assert!(kind_at("* Kaffee:kochen", 15).is_none());
    }

    #[test]
    fn the_range_is_measured_in_utf16() {
        let completion = at("Grüße\n<s", 8, "").expect("a trigger");
        // "Grüße\n" is six code units even though it is seven bytes.
        assert_eq!(completion.from, 6);
    }
}
