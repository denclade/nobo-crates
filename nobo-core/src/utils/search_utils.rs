// SPDX-FileCopyrightText: 2026 Dennis Burgermeister
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::core::models::{DirEntry, FullTextSearchResult, Note, SearchResult, UnpromotedOrgFile};
use crate::utils::fs_utils;
use rayon::prelude::*;
use std::collections::HashMap;
use std::path::Path;

const TITLE_WEIGHT: f32 = 100.0;
const PATH_WEIGHT: f32 = 30.0;
const TAG_WEIGHT: f32 = 20.0;
// Kept well below TITLE_WEIGHT/TAG_WEIGHT so a title/tag hit always ranks
// above a content-only match, no matter how many times the term occurs in
// the body -- occurrence count only breaks ties among content matches.
const OCCURRENCE_WEIGHT: f32 = 2.0;

pub fn parse_query_terms(query: &str) -> Vec<String> {
    query.to_lowercase().split_whitespace().map(String::from).collect()
}

pub fn score_note(query_terms: &[String], note: &Note) -> f32 {
    let mut score = 0.0;
    let title_lower = note.title.to_lowercase();
    let path_lower = note.path.to_lowercase();

    for term in query_terms {
        if title_lower.contains(term.as_str()) {
            score += TITLE_WEIGHT;
        }

        if path_lower.contains(term.as_str()) {
            score += PATH_WEIGHT;
        }

        let has_tag = note.tags.iter().any(|t| t.to_lowercase().contains(term.as_str()));
        if has_tag {
            let tag_count = note.tags.len() as f32;
            score += TAG_WEIGHT / tag_count.max(1.0);
        }
    }

    score
}

/// `base_dir` must already be canonicalized the same way `note.path` is
/// (see search_notes/search_notes_fulltext, both of which canonicalize
/// their own base_dir before calling this) -- otherwise strip_prefix below
/// falls through to its own graceful fallback (see display_path's comment)
/// instead of stripping anything.
pub fn build_search_result(note: &Note, score: f32, base_dir: &str) -> SearchResult {
    let path_obj = Path::new(&note.path);
    let name = path_obj.file_name().unwrap_or_default().to_string_lossy().into_owned();
    let parent = path_obj.parent().unwrap_or_else(|| Path::new(""));

    // Vault-relative, "/" meaning the vault root -- the same convention
    // the frontend uses. An absolute parent path would leak the platform's
    // storage prefix into the UI. Falls back to the untouched parent if
    // the note somehow is not under base_dir, which callers already
    // filter for, but a display field should degrade rather than panic.
    let relative_parent = parent.strip_prefix(base_dir).unwrap_or(parent);
    let parent_str = relative_parent.to_string_lossy().into_owned();

    SearchResult {
        file: DirEntry {
            path: note.path.clone(),
            name,
            is_dir: false,
            note: Some(note.clone()),
            // Search results are about matching text, and the star belongs
            // to the notes sidebar -- nothing here reads it.
            favorite: false,
        },
        score,
        // The `starts_with('/')` branch only fires in the strip_prefix
        // fallback case above (note.path wasn't under base_dir) -- there
        // parent_str is already an untouched absolute path, so prepending
        // another "/" would double it up.
        display_path: if parent_str.is_empty() {
            "/".to_string()
        } else if parent_str.starts_with('/') {
            parent_str
        } else {
            format!("/{parent_str}")
        },
    }
}

/// A placeholder `Note` for a plain, not-yet-promoted `.org` file, so it
/// can flow through the same scoring machinery real notes do. Identifier,
/// date and tags are empty -- there is no Denote filename to derive them
/// from.
///
/// Callers that build links off search results must never include these:
/// an empty identifier produces a broken link.
pub fn note_for_unpromoted_file(file: &UnpromotedOrgFile) -> Note {
    Note { path: file.path.clone(), identifier: String::new(), title: file.title.clone(), date_str: String::new(), tags: Vec::new() }
}

/// Canonicalizes `base_dir` itself: note paths come from the
/// canonical-keyed index, so a raw caller-supplied one matches nothing.
///
/// Empty `query_terms` lists the whole vault -- every note matches
/// unscored, rather than scoring to 0.0 and being filtered out by the
/// score check a real query goes through.
pub fn search_notes<'a>(notes: impl Iterator<Item = &'a Note>, base_dir: &str, query_terms: &[String]) -> Vec<SearchResult> {
    let base_dir = fs_utils::canonicalize_or_self(Path::new(base_dir)).to_string_lossy().into_owned();
    let mut results: Vec<SearchResult> = notes
        .filter(|note| note.path.starts_with(&base_dir))
        .filter_map(|note| {
            if query_terms.is_empty() {
                return Some(build_search_result(note, 0.0, &base_dir));
            }
            let score = score_note(query_terms, note);
            if score > 0.0 {
                Some(build_search_result(note, score, &base_dir))
            } else {
                None
            }
        })
        .collect();

    // Title tie-break (not just base_dir-relative path) -- keeps the
    // "list everything" empty-query case in a stable, sensible order
    // instead of HashMap iteration order (see IndexManager::all_notes),
    // and does the same for any real query where two notes score equally.
    results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal).then_with(|| a.file.name.cmp(&b.file.name)));
    results
}

fn build_fulltext_result(note: &Note, score: f32, occurrences: usize, base_dir: &str) -> FullTextSearchResult {
    let base = build_search_result(note, score, base_dir);
    FullTextSearchResult { file: base.file, score, display_path: base.display_path, occurrences }
}

/// One character as the matcher compares it.
///
/// `char::to_lowercase` is context-free, which `str::to_lowercase` is not:
/// the latter knows a `Σ` at the end of a word lowercases to final sigma
/// `ς`, and a character on its own never can. So the two forms of sigma
/// are folded together here.
///
/// That makes the search MORE forgiving than lowercasing the haystack
/// was, not less -- typing `σοφοσ` used to find nothing in `ΣΟΦΟΣ`,
/// because the haystack had become `σοφος` and the query had not. Both
/// spellings now find it, which is what someone typing into a search box
/// means either way.
fn folded(ch: char) -> char {
    if ch == 'ς' {
        'σ'
    } else {
        ch
    }
}

/// How far into a haystack a case-insensitive match starting here reaches,
/// or `None` if there is no match at this position.
///
/// Lowercases one character at a time out of the ITERATOR rather than
/// lowercasing the haystack first, because the haystack is the whole
/// vault: `to_lowercase()` on it allocated a second copy of every note
/// body on every keystroke, for every note, whether or not it matched.
///
/// NEEDLE_LOWER must already be lowercase -- it is the query, lowercased
/// once per search instead of once per note.
///
/// Case folding is not length-preserving (`İ` lowercases to two chars),
/// which is why this reports a byte length instead of assuming the
/// needle's own. It is also why nothing here may be used for highlighting
/// offsets: a match found at byte N of the original is not at byte N of
/// its lowercased form.
fn match_len_at(haystack: &str, needle_lower: &str) -> Option<usize> {
    let mut needle = needle_lower.chars().map(folded);
    let mut consumed = 0;
    for (offset, ch) in haystack.char_indices() {
        for lowered in ch.to_lowercase().map(folded) {
            match needle.next() {
                None => return Some(consumed),
                Some(wanted) if wanted == lowered => {}
                Some(_) => return None,
            }
        }
        consumed = offset + ch.len_utf8();
        // Checked after the whole character, so a needle that ends inside
        // one character's expansion still consumes it. That differs from
        // `to_lowercase().matches()` only in where the NEXT search starts
        // in such a case, never in whether a hit is found.
        if needle.clone().next().is_none() {
            return Some(consumed);
        }
    }
    needle.next().is_none().then_some(consumed)
}

/// Non-overlapping case-insensitive occurrences, the count
/// `haystack.to_lowercase().matches(needle_lower).count()` gives -- see
/// `match_len_at` for why it is not written that way.
///
/// An empty needle answers 0 rather than "one per position"; the only
/// caller rejects an empty query before it gets here (see
/// commands_search::search_files_fulltext).
fn count_matches_ci(haystack: &str, needle_lower: &str) -> usize {
    if needle_lower.is_empty() {
        return 0;
    }
    let mut count = 0;
    let mut rest = haystack;
    while !rest.is_empty() {
        match match_len_at(rest, needle_lower) {
            Some(len) if len > 0 => {
                count += 1;
                rest = &rest[len..];
            }
            // No match here: on to the next character boundary.
            _ => {
                let step = rest.chars().next().map_or(rest.len(), char::len_utf8);
                rest = &rest[step..];
            }
        }
    }
    count
}

/// Whether HAYSTACK contains NEEDLE_LOWER case-insensitively, without
/// lowercasing HAYSTACK -- see `match_len_at`.
fn contains_ci(haystack: &str, needle_lower: &str) -> bool {
    if needle_lower.is_empty() {
        return true;
    }
    let mut rest = haystack;
    while !rest.is_empty() {
        if match_len_at(rest, needle_lower).is_some_and(|len| len > 0) {
            return true;
        }
        let step = rest.chars().next().map_or(rest.len(), char::len_utf8);
        rest = &rest[step..];
    }
    false
}

/// Full-body search over every note, run in Rust (parallelized via rayon)
/// since it's still one pass over every note's content per keystroke --
/// BODIES is the index's cached file content (see
/// core::indexer::AppIndex::note_bodies), so this never touches disk.
/// Title/tag hits always outrank content-only hits (see OCCURRENCE_WEIGHT);
/// among content matches, more occurrences ranks higher.
///
/// NOTES is an iterator rather than a slice so the index can hand over
/// references to what it already holds, chained with the caller's extra
/// notes, instead of cloning the whole vault into a Vec first.
pub fn search_notes_fulltext<'a, I>(notes: I, bodies: &HashMap<String, String>, base_dir: &str, query: &str) -> Vec<FullTextSearchResult>
where
    I: IntoIterator<Item = &'a Note>,
{
    let query_lower = query.to_lowercase();

    // Collected first because rayon needs an indexable source; these are
    // references, so it is one pointer per note, not one note per note.
    let candidates: Vec<&Note> = notes.into_iter().filter(|note| note.path.starts_with(base_dir)).collect();

    let mut results: Vec<FullTextSearchResult> = candidates
        .par_iter()
        .filter_map(|note| {
            let title_hit = contains_ci(&note.title, &query_lower);
            let tag_hit = note.tags.iter().any(|t| contains_ci(t, &query_lower));
            let content = bodies.get(&note.path).map(String::as_str).unwrap_or("");
            let occurrences = count_matches_ci(content, &query_lower);

            if !title_hit && !tag_hit && occurrences == 0 {
                return None;
            }

            let mut score = occurrences as f32 * OCCURRENCE_WEIGHT;
            if title_hit {
                score += TITLE_WEIGHT;
            }
            if tag_hit {
                score += TAG_WEIGHT;
            }

            Some(build_fulltext_result(note, score, occurrences, base_dir))
        })
        .collect();

    results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    results
}

/* ------ Test ------- */

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::models::Note;

    fn make_note(title: &str, path: &str, tags: Vec<&str>) -> Note {
        Note { title: title.to_string(), path: path.to_string(), tags: tags.into_iter().map(String::from).collect(), ..Default::default() }
    }

    #[test]
    fn display_path_has_single_leading_slash() {
        let note = make_note("Test", "/home/user/notes/sub/a.org", vec![]);
        let result = build_search_result(&note, 0.0, "/home/user/notes");
        assert_eq!(result.display_path, "/sub");
    }

    /// `display_path` must be vault-relative, "/" meaning the vault root,
    /// rather than an absolute path leaking the platform storage prefix
    /// into the UI.
    #[test]
    fn display_path_is_vault_relative_not_the_full_absolute_path() {
        let note = make_note("Test", "/data/data/com.nobo.app/files/notes/projekte/kunde-x.org", vec![]);
        let result = build_search_result(&note, 0.0, "/data/data/com.nobo.app/files/notes");
        assert_eq!(result.display_path, "/projekte");
    }

    #[test]
    fn display_path_is_the_vault_root_itself_for_a_top_level_note() {
        let note = make_note("Test", "/home/user/notes/a.org", vec![]);
        let result = build_search_result(&note, 0.0, "/home/user/notes");
        assert_eq!(result.display_path, "/");
    }

    #[test]
    fn display_path_falls_back_to_the_untouched_parent_when_the_note_is_not_under_base_dir() {
        let note = make_note("Test", "/elsewhere/a.org", vec![]);
        let result = build_search_result(&note, 0.0, "/home/user/notes");
        assert_eq!(result.display_path, "/elsewhere");
    }

    #[test]
    fn empty_query_terms_score_zero() {
        let note = make_note("Test", "/a/b.org", vec![]);
        assert_eq!(score_note(&[], &note), 0.0);
    }

    #[test]
    fn title_match_scores_higher_than_path_match() {
        let title_hit = make_note("Rust Notes", "/foo/bar.org", vec![]);
        let path_hit = make_note("Bar", "/rust/bar.org", vec![]);
        let terms = vec!["rust".to_string()];

        assert!(score_note(&terms, &title_hit) > score_note(&terms, &path_hit));
    }

    #[test]
    fn tag_score_splits_across_multiple_tags() {
        let one_tag = make_note("X", "/x.org", vec!["rust"]);
        let two_tags = make_note("Y", "/y.org", vec!["rust", "other"]);
        let terms = vec!["rust".to_string()];
        assert!(score_note(&terms, &one_tag) > score_note(&terms, &two_tags));
    }

    #[test]
    fn search_notes_filters_by_base_dir() {
        let notes = vec![make_note("A", "/00_tests/a.org", vec![]), make_note("B", "/other/b.org", vec![])];
        let terms = vec!["a".to_string()];
        let results = search_notes(notes.iter(), "/00_tests", &terms);

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].file.path, "/00_tests/a.org");
    }

    /// Real bug: BufferSwitcherModal/LinkInsertModal's note step/
    /// HeadingLinkModal all open by calling search_files with an empty
    /// query, expecting the whole vault back to browse before typing
    /// anything -- score_note alone would score every note 0.0 for zero
    /// terms and the old `score > 0.0` filter dropped them all, so those
    /// modals silently showed nothing until the user typed a character.
    #[test]
    fn search_notes_with_empty_terms_returns_every_note_in_base_dir_unscored() {
        let notes = vec![
            make_note("Zebra", "/00_tests/zebra.org", vec![]),
            make_note("Apple", "/00_tests/apple.org", vec![]),
            make_note("Elsewhere", "/other/elsewhere.org", vec![]),
        ];
        let results = search_notes(notes.iter(), "/00_tests", &[]);

        assert_eq!(results.len(), 2, "only notes under base_dir, but ALL of them");
        assert!(results.iter().all(|r| r.score == 0.0));
        // Tie-broken by filename, not left in arbitrary input order.
        assert_eq!(results[0].file.name, "apple.org");
        assert_eq!(results[1].file.name, "zebra.org");
    }

    /* --- note_for_unpromoted_file / include_unpromoted search --- */

    #[test]
    fn note_for_unpromoted_file_has_no_identifier_but_scores_and_displays_like_a_real_note() {
        let file = UnpromotedOrgFile { path: "/00_tests/install.org".to_string(), title: "Moodle PMHS Notizen".to_string() };
        let note = note_for_unpromoted_file(&file);

        assert_eq!(note.identifier, "", "no Denote filename to derive one from");
        assert_eq!(note.path, file.path);
        assert_eq!(note.title, file.title);

        let results = search_notes(std::iter::once(&note), "/00_tests", &["moodle".to_string()]);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].file.note.as_ref().unwrap().identifier, "");
    }

    /* --- fulltext search --- */

    use tempfile::TempDir;

    /// Builds a `Note` and registers its body in `bodies`, mirroring how
    /// the index populates note bodies at scan time. Touches no disk --
    /// fulltext search reads from that map.
    fn make_note_with_content(
        bodies: &mut HashMap<String, String>,
        dir: &Path,
        title: &str,
        filename: &str,
        tags: Vec<&str>,
        content: &str,
    ) -> Note {
        let path_str = dir.join(filename).to_string_lossy().into_owned();
        bodies.insert(path_str.clone(), content.to_string());
        Note { title: title.to_string(), path: path_str, tags: tags.into_iter().map(String::from).collect(), ..Default::default() }
    }

    #[test]
    fn fulltext_counts_case_insensitive_occurrences() {
        let tmp = TempDir::new().unwrap();
        let mut bodies = HashMap::new();
        let note = make_note_with_content(&mut bodies, tmp.path(), "Untitled", "a.org", vec![], "Rust is great. RUST rocks. rust!");

        let results = search_notes_fulltext(&[note], &bodies, tmp.path().to_str().unwrap(), "rust");

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].occurrences, 3);
    }

    #[test]
    fn fulltext_title_hit_outranks_many_content_occurrences() {
        let tmp = TempDir::new().unwrap();
        let mut bodies = HashMap::new();
        let title_hit = make_note_with_content(&mut bodies, tmp.path(), "Rust Notes", "a.org", vec![], "no matches here");
        let content_hit = make_note_with_content(&mut bodies, tmp.path(), "Other", "b.org", vec![], &"rust ".repeat(50));

        let results = search_notes_fulltext(&[title_hit, content_hit], &bodies, tmp.path().to_str().unwrap(), "rust");

        assert_eq!(results[0].file.path, tmp.path().join("a.org").to_string_lossy());
    }

    #[test]
    fn fulltext_excludes_notes_without_any_hit() {
        let tmp = TempDir::new().unwrap();
        let mut bodies = HashMap::new();
        let note = make_note_with_content(&mut bodies, tmp.path(), "Unrelated", "a.org", vec![], "nothing to see here");

        let results = search_notes_fulltext(&[note], &bodies, tmp.path().to_str().unwrap(), "rust");

        assert!(results.is_empty());
    }

    #[test]
    fn fulltext_filters_by_base_dir() {
        let tmp = TempDir::new().unwrap();
        let other = TempDir::new().unwrap();
        let mut bodies = HashMap::new();
        let inside = make_note_with_content(&mut bodies, tmp.path(), "A", "a.org", vec![], "rust");
        let outside = make_note_with_content(&mut bodies, other.path(), "B", "b.org", vec![], "rust");

        let results = search_notes_fulltext(&[inside, outside], &bodies, tmp.path().to_str().unwrap(), "rust");

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].file.path, tmp.path().join("a.org").to_string_lossy());
    }

    #[test]
    fn fulltext_missing_body_treated_as_empty_content() {
        let tmp = TempDir::new().unwrap();
        let mut bodies = HashMap::new();
        // Title matches but no body was ever cached for this path.
        let note = make_note_with_content(&mut bodies, tmp.path(), "Rust Notes", "a.org", vec![], "irrelevant");
        bodies.remove(&note.path);

        let results = search_notes_fulltext(&[note], &bodies, tmp.path().to_str().unwrap(), "rust");

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].occurrences, 0);
    }

    /// The matcher replaced `to_lowercase().matches().count()`, so what it
    /// has to answer is what that answered -- including for text where
    /// lowercasing changes the length, which is the reason the
    /// straightforward "lowercase and search" version had to go.
    #[test]
    fn counting_matches_agrees_with_lowercasing_the_haystack_first() {
        let cases = [
            ("Rust and rust and RUST", "rust"),
            ("nothing here", "rust"),
            ("aaaa", "aa"), // non-overlapping: 2, not 3
            ("Grüße aus Zürich", "grüße"),
            ("STRASSE", "strasse"),
            ("İstanbul im Winter", "winter"), // dotted capital I nearby
            ("", "rust"),
            ("rust", "rust and more"), // needle longer than haystack
        ];

        for (haystack, needle) in cases {
            let needle_lower = needle.to_lowercase();
            let expected = haystack.to_lowercase().matches(&needle_lower).count();
            assert_eq!(count_matches_ci(haystack, &needle_lower), expected, "counting {needle:?} in {haystack:?}");
            assert_eq!(
                contains_ci(haystack, &needle_lower),
                haystack.to_lowercase().contains(&needle_lower),
                "containment of {needle:?} in {haystack:?}"
            );
        }
    }

    /// The one place the context-free fold is deliberately not identical
    /// to lowercasing the haystack -- see `folded`.
    #[test]
    fn either_spelling_of_sigma_finds_the_other() {
        for needle in ["σοφος", "σοφοσ"] {
            assert_eq!(count_matches_ci("ΣΟΦΟΣ", needle), 1, "searching {needle:?}");
            assert!(contains_ci("ΣΟΦΟΣ", needle));
        }
    }

    /// An empty query never reaches the search (the command rejects it),
    /// and answering "one match per position" for one would rank every
    /// note by its length. Pinned because the replaced expression did
    /// exactly that.
    #[test]
    fn an_empty_query_matches_nothing() {
        assert_eq!(count_matches_ci("anything at all", ""), 0);
    }
}
