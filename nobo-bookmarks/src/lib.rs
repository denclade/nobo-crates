//! Bookmarks kept as org: a link, a note, tags, read later.
//!
//! A bookmarks file is an ordinary notes file that declares itself (see
//! file_kind: `#+NOBO_TYPE: bookmarks`), and each one is a category --
//! "Arbeit", "Privat". In it, a bookmark is a heading with a `:URL:`; the
//! headings above it are its collection; its tags are org tags, and
//! `:readlater:` marks the ones still to read.
//!
//! ```org
//! * Rust
//! ** reqwest – Rust                                          :http:
//! :PROPERTIES:
//! :URL:      https://docs.rs/reqwest
//! :CREATED:  [2026-09-30 Mi 20:55]
//! :END:
//! Für das Web-Scraping-Modul.
//!
//! #+begin_quote
//! An ergonomic, batteries-included HTTP Client for Rust.
//! #+end_quote
//! ```
//!
//! Read the way org reads an outline (see `parse` on why not the core's
//! heading list), written one bookmark at a time (see `write`), every
//! write recorded in the history. See docs/concept-bookmark-manager.md.

pub mod categories;
pub mod commands;
pub mod import_linkwarden;
pub mod model;
pub mod parse;
pub mod preview;
pub mod url;
pub mod write;
#[cfg(feature = "tauri")]
pub mod tauri_commands;

pub use model::{Bookmark, BookmarkFile, Collection, Draft, Located, Patch, Preview, Target};

use nobo_core::core::file_kind::{FileKind, FileKindSpec, StateSupport};
use nobo_core::core::models::Heading;

/// A bookmarks file -- one category of bookmarks.
pub const KIND: FileKind = FileKind::new("bookmarks");

inventory::submit! {
    FileKindSpec {
        kind: KIND,
        served_by: Some("bookmarks"),
        editable: true,
        // A bookmark is no task: no TODO badges, and above all no
        // bookmarks among the next actions -- a heading counts as an
        // action once it has a state at all. Read later is a tag.
        states: StateSupport::None,
        reorderable: false,
        can_add_headings: true,
    }
}

// Bookmarks, as a search anyone can ask for by name: title, address,
// tags and note of every bookmark, and only of bookmarks.
inventory::submit! {
    nobo_core::core::search::SearchContributor {
        source: "bookmarks",
        search: |query| {
            query
                .vault
                .files_of_kind(KIND)
                .into_iter()
                .flat_map(|(path, text)| {
                    let path = path.to_string_lossy().into_owned();
                    parse::bookmarks_of(&path, "", &text)
                        .into_iter()
                        .filter(|bookmark| {
                            let haystack = format!(
                                "{} {} {} {} {}",
                                bookmark.title,
                                bookmark.url,
                                bookmark.tags.join(" "),
                                bookmark.collection.join(" "),
                                bookmark.note
                            );
                            query.matches(&haystack)
                        })
                        .map(|bookmark| as_heading(&bookmark))
                        .collect::<Vec<_>>()
                })
                .collect()
        },
    }
}

/// A bookmark as the search page shows a hit: a heading, opened in its
/// file at its line.
fn as_heading(bookmark: &Bookmark) -> Heading {
    Heading {
        id: format!("bookmark:{}:{}", bookmark.file_path, bookmark.line_no),
        file_path: bookmark.file_path.clone(),
        line_no: bookmark.line_no,
        last_line: bookmark.line_no,
        level: 1,
        title: bookmark.title.clone(),
        tags: bookmark.tags.clone(),
        properties: [("URL".to_string(), bookmark.url.clone())].into_iter().collect(),
        content: bookmark.note.clone(),
        ..Heading::default()
    }
}

inventory::submit! {
    nobo_core::core::features::FeatureSpec {
        id: "bookmarks",
        compiled_in: true,
        needs: &[],
        // The shared HTTP door, for a page's title -- only in a build that
        // looks pages up at all.
        built_on: if cfg!(feature = "fetch") { &["nobo-http"] } else { &[] },
    }
}

/// What every command answers when this build has no bookmarks.
pub fn unavailable() -> String {
    nobo_core::t!("Diese Version von nobo hat keine Lesezeichen.", "This build of nobo has no bookmarks.")
}

/// Everything this plugin brings, in one call: `.plugin(nobo_bookmarks::init())`.
#[cfg(feature = "tauri")]
pub fn init() -> tauri::plugin::TauriPlugin<tauri::Wry> {
    tauri::plugin::Builder::new("nobo-bookmarks")
        .invoke_handler(specta_builder().invoke_handler())
        .build()
}

#[cfg(feature = "tauri")]
fn specta_builder() -> tauri_specta::Builder<tauri::Wry> {
    tauri_specta::Builder::<tauri::Wry>::new()
        .plugin_name("nobo-bookmarks")
        .commands(command_list())
}

/// The commands, once: `fetch` adds the page lookup to the same list.
#[cfg(feature = "tauri")]
macro_rules! command_list {
    ($($fetch:ident),*) => {
        tauri_specta::collect_commands![
            tauri_commands::bookmarks_list,
            tauri_commands::bookmarks_files,
            tauri_commands::bookmarks_collections,
            tauri_commands::bookmarks_set_file,
            tauri_commands::bookmarks_create_file,
            tauri_commands::bookmarks_find,
            tauri_commands::bookmarks_save,
            tauri_commands::bookmarks_update,
            tauri_commands::bookmarks_delete,
            tauri_commands::bookmarks_move,
            tauri_commands::bookmarks_import_linkwarden,
            $(tauri_commands::$fetch,)*
        ]
    };
}

#[cfg(all(feature = "tauri", feature = "fetch"))]
fn command_list() -> tauri_specta::Commands<tauri::Wry> {
    command_list![bookmarks_preview]
}

/// The same without looking pages up, for a build that leaves networking out.
#[cfg(all(feature = "tauri", not(feature = "fetch")))]
fn command_list() -> tauri_specta::Commands<tauri::Wry> {
    command_list![]
}

/// Writes this plugin's `bindings.ts` next to the frontend half that
/// imports it -- a test, so running the tests is what regenerates them.
#[cfg(all(test, feature = "tauri"))]
mod bindings_export {
    #[test]
    fn export_bindings() {
        std::fs::create_dir_all("../../src/features/bookmarks").expect("the frontend folder");
        super::specta_builder()
            .export(specta_typescript::Typescript::default(), "../../src/features/bookmarks/bindings.ts")
            .expect("failed to write the bindings");
    }
}

#[cfg(test)]
mod search_tests {
    use nobo_core::core::indexer::IndexManager;
    use nobo_core::core::search::{self, SearchQuery};
    use nobo_core::core::vault::Vault;

    const FILE: &str = "#+NOBO_TYPE: bookmarks\n\n* Rust\n** reqwest :http:\n:PROPERTIES:\n:URL: https://docs.rs/reqwest\n:END:\nFür das Scraping.\n";

    fn hits(file: &str, text: &str, words: &str) -> Vec<nobo_core::core::models::Heading> {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(file), text).unwrap();
        let index = IndexManager::new();
        index.scan_directory(dir.path());
        let terms: Vec<String> = words.split_whitespace().map(str::to_string).collect();
        let query = SearchQuery { raw: words, terms: &terms, vault: &Vault::new(&index, dir.path()) };
        search::search("bookmarks", &query)
    }

    #[test]
    fn it_finds_a_bookmark_by_title_address_tag_or_note() {
        for words in ["reqwest", "docs.rs", "http", "scraping", "rust"] {
            assert_eq!(hits("lesezeichen.org", FILE, words).len(), 1, "{words}");
        }
    }

    /// Categories are kept in `.nobo/` -- a hidden folder, which the
    /// index reads all the same.
    #[test]
    fn a_category_in_the_app_folder_is_found() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".nobo")).unwrap();
        std::fs::write(dir.path().join(".nobo/bookmarks-work.org"), FILE).unwrap();
        let index = IndexManager::new();
        index.scan_directory(dir.path());
        let terms = vec!["reqwest".to_string()];
        let query = SearchQuery { raw: "reqwest", terms: &terms, vault: &Vault::new(&index, dir.path()) };
        assert_eq!(search::search("bookmarks", &query).len(), 1);
    }

    /// The sample vault's two categories (crates/testdata/.nobo): what the
    /// app shows when the devcontainer starts.
    #[test]
    fn the_sample_categories_read_as_written() {
        let read = |name: &str| {
            let path = format!("../testdata/.nobo/{name}");
            let text = std::fs::read_to_string(&path).unwrap();
            (crate::parse::category_title(&path, &text), crate::parse::bookmarks_of(&path, "", &text), crate::parse::collections_of(&text))
        };
        let (work, work_bookmarks, work_collections) = read("bookmarks-work.org");
        let (private, private_bookmarks, _) = read("bookmarks-private.org");
        assert_eq!((work.as_str(), private.as_str()), ("Work", "Private"));
        assert_eq!(work_bookmarks.len(), 9);
        assert_eq!(private_bookmarks.len(), 7);
        assert!(work_collections.iter().any(|(path, _)| path == &["Meetings".to_string()]), "an empty collection is one");
        let tokio = work_bookmarks.iter().find(|b| b.url.contains("tokio")).unwrap();
        assert_eq!(tokio.collection, ["Rust", "Async"]);
        assert_eq!(tokio.inherited_tags, ["rust"]);
        assert!(tokio.read_later);
        let read_later = work_bookmarks.iter().chain(&private_bookmarks).filter(|b| b.read_later).count();
        assert_eq!(read_later, 4);
    }

    #[test]
    fn it_ignores_files_that_are_not_bookmarks() {
        assert!(hits("notiz.org", &FILE.replace("#+NOBO_TYPE: bookmarks\n", ""), "reqwest").is_empty());
    }
}
