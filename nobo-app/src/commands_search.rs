// SPDX-FileCopyrightText: 2026 Dennis Burgermeister
// SPDX-License-Identifier: GPL-3.0-or-later

use nobo_core::core::indexer::IndexManager;
use nobo_core::core::models::{FullTextSearchResult, Note, SearchResult};
use nobo_core::utils::search_utils;

/// An empty `query` falls through rather than short-circuiting: the
/// pickers open with one to list the whole vault before anything is typed.
/// The inline search field's "empty means nothing" behaviour is guarded on
/// the frontend and never reaches here.
///
/// `include_unpromoted` also scores plain `.org` files with no Denote
/// identity, via a placeholder note. Only the Search page wants those --
/// every other caller builds an id-based link off the result, which an
/// empty identifier would break.
pub fn search_files(base_dir: String, query: String, include_unpromoted: bool, index_state: &IndexManager) -> Vec<SearchResult> {
    let terms = search_utils::parse_query_terms(&query);
    let mut notes = index_state.all_notes();
    if include_unpromoted {
        notes.extend(index_state.unpromoted_org_files(&base_dir).iter().map(search_utils::note_for_unpromoted_file));
    }

    search_utils::search_notes(notes.iter(), &base_dir, &terms)
}

/// Full-text variant of `search_files` -- matches against the index's
/// cached note bodies (parallelized via rayon), so it lives in Rust rather
/// than the frontend purely for performance, without touching disk. See
/// search_files' own doc comment for INCLUDE_UNPROMOTED.
pub fn search_files_fulltext(
    base_dir: String,
    query: String,
    include_unpromoted: bool,
    index_state: &IndexManager,
) -> Vec<FullTextSearchResult> {
    if query.trim().is_empty() {
        return vec![];
    }

    let unpromoted: Vec<Note> = if include_unpromoted {
        index_state.unpromoted_org_files(&base_dir).iter().map(search_utils::note_for_unpromoted_file).collect()
    } else {
        Vec::new()
    };

    index_state.search_notes_fulltext(&base_dir, query.trim(), &unpromoted)
}

/// What one feature has to say about a query.
///
/// The scopes on the search page are not a fixed list: the core searches
/// notes and headings, a feature answers for its own corner (see
/// core::search). An unknown source -- a feature this build was compiled
/// without -- answers empty, because the scope it belongs to is not
/// offered either.
/// `async` deliberately, though nothing in it awaits: a sync command runs
/// on the main thread, and this one runs per keystroke over as much of the
/// vault as the feature asks for. On the async runtime it keeps the window
/// responsive while it works.
pub fn search_contributed(
    source: String,
    query: String,
    index_state: &IndexManager,
) -> Result<Vec<nobo_core::core::models::Heading>, String> {
    let vault_root = nobo_core::config::settings::Config::require_vault_root()?;
    let terms = search_utils::parse_query_terms(&query);
    let vault = nobo_core::core::vault::Vault::new(index_state, &vault_root);
    let search_query = nobo_core::core::search::SearchQuery { raw: query.trim(), terms: &terms, vault: &vault };
    Ok(nobo_core::core::search::search(&source, &search_query))
}

/// Which of those a build offers at all -- the frontend turns them into
/// scope tabs (see src/features).
pub fn search_sources() -> Vec<String> {
    nobo_core::core::search::sources().into_iter().map(str::to_string).collect()
}
