// SPDX-FileCopyrightText: 2026 Dennis Burgermeister
// SPDX-License-Identifier: GPL-3.0-or-later

//! org-tempo-style text expansions: `<s` and a tab becomes a source block.
//!
//! Lifted from nobo's `features/expansions`, where it is a feature the
//! editor asks for completions from. Here it is core, for the reason the
//! rest of the grammar is: the triggers and what they expand to are part
//! of how org is written, and a second front end inventing its own set
//! would mean two editors that disagree about what `<q` does.
//!
//! What each one *means* is not here. An entry carries a `kind`, and
//! whoever shows it names it in the reader's own language -- the core has
//! no business holding an English string that a German UI then has to
//! translate back.

use serde::{Deserialize, Serialize};

use crate::core::org_syntax;

/// One trigger and what it leaves behind.
///
/// `insert` may contain org-tempo's `%?`, which is where the caret lands
/// (see [`crate::core::org_edit::insert_text`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Expansion {
    /// What is typed, `<` included.
    pub trigger: String,
    /// What this is, for a host to name: "src", "example", "quote",
    /// "date", "datetime", "author".
    pub kind: String,
    pub insert: String,
}

fn entry(trigger: &str, kind: &str, insert: String) -> Expansion {
    Expansion { trigger: trigger.to_string(), kind: kind.to_string(), insert }
}

/// Every expansion, in the order a picker should offer them.
///
/// `author` is the configured default author; the core does not read the
/// config, because that would be a file read on a path that has to stay a
/// pure function (see nobo-capi's synchronous door).
pub fn entries(author: &str) -> Vec<Expansion> {
    vec![
        entry("<s", "src", "#+BEGIN_SRC %?\n#+END_SRC".to_string()),
        entry("<e", "example", "#+BEGIN_EXAMPLE\n%?\n#+END_EXAMPLE".to_string()),
        entry("<q", "quote", "#+BEGIN_QUOTE\n%?\n#+END_QUOTE".to_string()),
        entry("<date", "date", org_syntax::timestamp_now(false)),
        entry("<datetime", "datetime", org_syntax::timestamp_now(true)),
        entry("<author", "author", if author.is_empty() { "%?".to_string() } else { author.to_string() }),
    ]
}

/// The expansions a half-typed trigger could still become.
///
/// Matching is on what has been typed so far, so `<d` offers both dates
/// and `<` offers everything -- which is what makes this worth a
/// suggestion strip rather than a key nobody knows about.
pub fn matching(prefix: &str, author: &str) -> Vec<Expansion> {
    entries(author).into_iter().filter(|entry| entry.trigger.starts_with(prefix)).collect()
}

/// Whether a trigger may expand here: it has to be the only thing on the
/// line so far, apart from indentation.
///
/// The same constraint org-tempo enforces, and the reason `<` in the
/// middle of a sentence never turns into a block.
pub fn is_trigger_position(line_before_cursor: &str) -> bool {
    let trimmed = line_before_cursor.trim_start();
    trimmed.starts_with('<') && !trimmed[1..].contains(char::is_whitespace)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_prefix_narrows_the_list() {
        assert_eq!(matching("<", "").len(), 6);
        let dates = matching("<dat", "");
        assert_eq!(dates.iter().map(|e| e.trigger.as_str()).collect::<Vec<_>>(), vec!["<date", "<datetime"]);
        assert_eq!(matching("<s", "")[0].insert, "#+BEGIN_SRC %?\n#+END_SRC");
    }

    #[test]
    fn the_author_comes_from_the_host() {
        assert_eq!(matching("<author", "Jo")[0].insert, "Jo");
        // Nobody configured: the caret goes where the name would be.
        assert_eq!(matching("<author", "")[0].insert, "%?");
    }

    #[test]
    fn a_trigger_has_to_be_alone_on_its_line() {
        assert!(is_trigger_position("<s"));
        assert!(is_trigger_position("    <q"));
        assert!(!is_trigger_position("see <s"));
        assert!(!is_trigger_position("<s and more"));
        assert!(!is_trigger_position("text"));
    }
}
