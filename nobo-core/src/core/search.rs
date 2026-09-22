//! Search results a feature knows about and the core does not.
//!
//! The core searches notes and headings, which covers most of a vault --
//! a recipe and a synced task are headings like any other and turn up in
//! an ordinary search already. What it cannot do is answer *as* a
//! feature: "the recipes matching X", "the tasks in your lists matching
//! X", each with the wording and the ordering that makes sense there.
//!
//! So a feature registers what it can answer:
//!
//! ```ignore
//! // features/cookbook/mod.rs
//! inventory::submit! {
//!     SearchContributor {
//!         source: "cookbook",
//!         search: |query| { /* … */ },
//!     }
//! }
//! ```
//!
//! # Why the hits are plain headings
//!
//! Because they are. A recipe is a heading in an org file; so is a task
//! in a mirrored list. Returning `Heading` means the search page opens
//! them, previews them and navigates them with the code it already has --
//! a feature contributes *which* headings and under what name, not a new
//! kind of result to render.
//!
//! # What a contributor may assume
//!
//! It is handed the query and the index, and it runs on the caller's
//! thread while a user waits: no IO, no network, and no reparsing the
//! vault -- everything it needs is in `SearchQuery::index` already.

use crate::core::models::Heading;
use crate::core::vault::Vault;

/// What is being searched for, and what to search in.
pub struct SearchQuery<'a> {
    /// As typed, for a contributor that wants to match the whole phrase.
    pub raw: &'a str,
    /// Lowercased words, the way `search_utils` splits them -- what most
    /// contributors want.
    pub terms: &'a [String],
    /// The vault, as this build lets a feature read it (see
    /// `core::vault`). Read it; do not walk the disk.
    pub vault: &'a Vault<'a>,
}

impl SearchQuery<'_> {
    /// Whether every term appears somewhere in TEXT. The default rule, so
    /// two features do not disagree about what "matching" means.
    ///
    /// One lowercase copy of TEXT per call, whatever the number of terms
    /// -- this runs once per candidate per keystroke, so the cost that
    /// matters is how many candidates a contributor hands it (see
    /// `Vault::files_of_kind`, which is where that is decided).
    pub fn matches(&self, text: &str) -> bool {
        let haystack = text.to_lowercase();
        !self.terms.is_empty() && self.terms.iter().all(|term| haystack.contains(term))
    }
}

/// One answerable question, registered by whoever can answer it.
pub struct SearchContributor {
    /// Its own name, used as the scope id on the frontend (see
    /// `src/features` -- the manifest supplies the label).
    pub source: &'static str,
    pub search: fn(&SearchQuery) -> Vec<Heading>,
}

inventory::collect!(SearchContributor);

/// Every source this build can search, sorted so a list reads the same
/// way twice.
pub fn sources() -> Vec<&'static str> {
    let mut sources: Vec<&'static str> = inventory::iter::<SearchContributor>.into_iter().map(|c| c.source).collect();
    sources.sort_unstable();
    sources
}

/// What SOURCE has to say about QUERY, or nothing when this build has no
/// such source -- which is a normal answer for a feature that was left
/// out, not an error to show.
pub fn search(source: &str, query: &SearchQuery) -> Vec<Heading> {
    inventory::iter::<SearchContributor>
        .into_iter()
        .find(|contributor| contributor.source == source)
        .map(|contributor| (contributor.search)(query))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_query_matches_only_when_every_word_does() {
        let terms = vec!["rote".to_string(), "linsen".to_string()];
        let index = crate::core::indexer::IndexManager::new();
        let vault = Vault::new(&index, std::path::Path::new("/vault"));
        let query = SearchQuery { raw: "rote linsen", terms: &terms, vault: &vault };

        assert!(query.matches("Rote Linsen mit Reis"));
        assert!(!query.matches("Rote Bete"));
    }

    /// An empty query matches nothing rather than everything: the search
    /// page asks on every keystroke, and "everything" as a first answer
    /// is a screenful of noise.
    #[test]
    fn an_empty_query_matches_nothing() {
        let index = crate::core::indexer::IndexManager::new();
        let vault = Vault::new(&index, std::path::Path::new("/vault"));
        let query = SearchQuery { raw: "", terms: &[], vault: &vault };

        assert!(!query.matches("irgendwas"));
    }

    #[test]
    fn an_unknown_source_answers_empty_rather_than_failing() {
        let index = crate::core::indexer::IndexManager::new();
        let vault = Vault::new(&index, std::path::Path::new("/vault"));
        let query = SearchQuery { raw: "x", terms: &[], vault: &vault };

        assert!(search("no-such-feature", &query).is_empty());
    }
}
