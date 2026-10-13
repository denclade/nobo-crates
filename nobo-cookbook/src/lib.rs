//! Recipes in the org-chef shape, read out of an ordinary org file.
//!
//! A cookbook file is a hand-written notes file that declares itself (see
//! file_kind: `#+NOBO_TYPE: cookbook`). Nothing here is generated or owned
//! by the app -- this module only READS, so that a reading view can lay a
//! recipe out for someone standing at the stove instead of showing them
//! `:PROPERTIES:` drawers.
//!
//! ```org
//! * Spaghetti Carbonara                                :pasta:schnell:
//! :PROPERTIES:
//! :SERVINGS: 2
//! :PREP-TIME: 10
//! :END:
//!
//! ** Zutaten
//! - 200 g Spaghetti
//!
//! ** Zubereitung
//! 1. Nudelwasser aufsetzen.
//! ```
//!
//! Three decisions worth knowing:
//!
//! - **A recipe is any heading with an ingredients or directions child**,
//!   not "a level-1 heading". A collection grouped by course puts its
//!   recipes on level 2 (`* Desserts` / `** Tiramisu` / `*** Zutaten`),
//!   and one recipe per file puts it on level 1. Both are normal.
//! - **English and German are both first class.** org-chef itself writes
//!   `** Ingredients` / `** Directions`; a German user writes `Zutaten` /
//!   `Zubereitung`. Reading only one of them would mean the format's own
//!   generator produces files this cannot show.
//! - **A heading without those sections is still a recipe** when it sits
//!   in a cookbook file (`sections: None`). The view renders it as plain
//!   text rather than pretending the file is broken -- most collections
//!   have a few notes-shaped entries from before anyone structured them.
//!
//! And one thing this does NOT use: `org_parser`'s heading list. That
//! parser answers "what are the tasks in this file", and to keep prose
//! that happens to start with `*` from becoming one, it only accepts a
//! `*`-line as structure when it carries a TODO keyword, a `:project:`
//! tag or a CUSTOM_ID (see models::is_real_structure). A recipe file has
//! none of those -- `** Zutaten` would be swallowed as text. In a cookbook
//! the outline IS the structure, exactly as org itself reads it, so this
//! module reads the file that way (see `outline`).

pub mod ingredients;
pub mod parse;
pub mod repair;
pub mod check;
pub mod import;
pub use parse::recipes_of;

pub mod commands;
pub mod model;
#[cfg(feature = "tauri")]
pub mod tauri_commands;

use nobo_core::core::file_kind::{FileKind, FileKindSpec, StateSupport};

/// A recipe collection in the org-chef shape.
///
/// An ordinary, hand-written org file that also gets a cooking-oriented
/// reading view. Declaring it is what puts its recipes in the Cookbook
/// section; nothing about the file is generated or owned by the app.
pub const KIND: FileKind = FileKind::new("cookbook");

inventory::submit! {
    FileKindSpec {
        kind: KIND,
        // Always in the build -- reading recipes out of an org file needs
        // nothing a lite variant would leave out (see docs/architecture.md).
        served_by: Some("cookbook"),
        editable: true,
        // A recipe is not a task. Leaving states off keeps TODO badges out
        // of the cookbook and, more importantly, keeps recipes out of Next
        // Actions -- a heading only counts as an action once it has a
        // state at all (see IndexManager::all_headings_with_state).
        states: StateSupport::None,
        // The order of recipes in the file is the user's own; nothing in
        // the app rearranges it, and nothing depends on it either.
        reorderable: false,
        can_add_headings: true,
    }
}
mod outline;

pub use model::{Amount, Ingredient, IngredientGroup, Recipe, Sections, StepGroup};
pub use repair::CollapsedProperties;
pub use check::{FileCheck, Finding};

// Recipes, as a search anyone can ask for by name.
//
// A recipe is a heading like any other and turns up in an ordinary
// search already -- what this adds is asking *for recipes*: only files
// that declare themselves cookbooks, only the headings that are actually
// recipes, matched on title and tags (see core::search on why the hits
// are plain headings).
inventory::submit! {
    nobo_core::core::search::SearchContributor {
        source: "cookbook",
        search: |query| {
            query
                .vault
                .files_of_kind(KIND)
                .into_iter()
                .flat_map(|(path, text)| {
                    let hits: Vec<_> = parse::recipes_of(&path.to_string_lossy(), &text)
                        .into_iter()
                        .filter(|recipe| {
                            query.matches(&recipe.title) || recipe.tags.iter().any(|tag| query.matches(tag))
                        })
                        .collect();
                    // Only now: a cookbook with nothing matching in it
                    // costs the title scan and no more.
                    if hits.is_empty() {
                        return Vec::new();
                    }
                    // The recipe knows where it is; the heading is what
                    // the search page opens.
                    let headings = query.vault.headings(&path);
                    hits.into_iter()
                        .filter_map(|recipe| {
                            headings.iter().find(|heading| heading.line_no == recipe.line_no).cloned()
                        })
                        .collect::<Vec<_>>()
                })
                .collect()
        },
    }
}

// This feature, as the app knows it: in the build or not, and switchable
// on this device (see core::features).
inventory::submit! {
    nobo_core::core::features::FeatureSpec {
        id: "cookbook",
        // No `cfg!`: linked means in the build (see crates/nobo-babel).
        compiled_in: true,
        needs: &[],
        // The shared HTTP door, for the web import -- and only in a build
        // that has the import at all: a list naming a library the build
        // left out would be as wrong as one leaving out a library it has.
        built_on: if cfg!(feature = "import") { &["nobo-http"] } else { &[] },
    }
}

/// What every command answers when this build has no cookbook.
pub fn unavailable() -> String {
    nobo_core::t!("Diese Version von nobo hat keine Rezeptansicht.", "This build of nobo has no recipe view.")
}

/// What the import answers in a build that has no networking.
#[cfg(not(feature = "import"))]
pub fn import_unavailable() -> String {
    nobo_core::t!("Diese Version von nobo kann keine Rezepte aus dem Netz holen.", "This build of nobo cannot fetch recipes from the web.")
}

#[cfg(test)]
mod search_tests {
    use nobo_core::core::indexer::IndexManager;
    use nobo_core::core::search::{self, SearchQuery};
    use nobo_core::core::vault::Vault;

    fn vault_with(file: &str, text: &str) -> (tempfile::TempDir, IndexManager) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(file), text).unwrap();
        let index = IndexManager::new();
        index.scan_directory(dir.path());
        (dir, index)
    }

    const COOKBOOK: &str = "#+NOBO_TYPE: cookbook\n\n* Linsensuppe\n** Zutaten\n- 200 g rote Linsen\n** Zubereitung\n1. Kochen.\n";

    /// The point of the scope: only recipes, and only out of files that
    /// declare themselves cookbooks.
    #[test]
    fn it_finds_a_recipe_by_title() {
        let (dir, index) = vault_with("rezepte.org", COOKBOOK);
        let terms = vec!["linsensuppe".to_string()];
        let query = SearchQuery { raw: "linsensuppe", terms: &terms, vault: &Vault::new(&index, dir.path()) };

        let hits = search::search("cookbook", &query);

        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].title, "Linsensuppe");
    }

    /// A heading in an ordinary note is not a recipe, however well it
    /// matches -- that is what the notes scope is for.
    #[test]
    fn it_ignores_files_that_are_not_cookbooks() {
        let (dir, index) = vault_with("notizen.org", "* Linsensuppe\nirgendwas\n");
        let terms = vec!["linsensuppe".to_string()];
        let query = SearchQuery { raw: "linsensuppe", terms: &terms, vault: &Vault::new(&index, dir.path()) };

        assert!(search::search("cookbook", &query).is_empty());
    }

    /// Sections are not recipes: "Zutaten" matches nothing on its own.
    #[test]
    fn a_section_heading_is_not_a_hit() {
        let (dir, index) = vault_with("rezepte.org", COOKBOOK);
        let terms = vec!["zutaten".to_string()];
        let query = SearchQuery { raw: "zutaten", terms: &terms, vault: &Vault::new(&index, dir.path()) };

        assert!(search::search("cookbook", &query).is_empty());
    }
}

/// Everything this plugin brings, in one call.
///
/// The app adds it with `.plugin(nobo_cookbook::init())` and knows nothing
/// else about it -- the file kinds, the settings section and the feature
/// registration all announce themselves from in here (see
/// nobo_core::core's inventory collections).
/// Concrete in the runtime rather than generic: its commands take an
/// `AppHandle`, and the app runs Wry. Staying generic would mean a type
/// parameter on every command for a second runtime nobody has.
#[cfg(feature = "tauri")]
pub fn init() -> tauri::plugin::TauriPlugin<tauri::Wry> {
    tauri::plugin::Builder::new("nobo-cookbook")
        .invoke_handler(specta_builder().invoke_handler())
        .build()
}

/// This plugin's command surface, in the one place both the runtime and
/// the bindings read it from.
#[cfg(feature = "tauri")]
fn specta_builder() -> tauri_specta::Builder<tauri::Wry> {
    tauri_specta::Builder::<tauri::Wry>::new()
        .plugin_name("nobo-cookbook")
        .commands(command_list())
}

/// Every command, in every build: the import is registered without the
/// `import` feature too, and says why it cannot run (see
/// tauri_commands) -- so there is one list, which the app's build.rs
/// reads for the ACL.
#[cfg(feature = "tauri")]
fn command_list() -> tauri_specta::Commands<tauri::Wry> {
    tauri_specta::collect_commands![
        tauri_commands::cookbook_recipes,
        tauri_commands::cookbook_files,
        tauri_commands::cookbook_set_file,
        tauri_commands::cookbook_create,
        tauri_commands::cookbook_collapsed_properties,
        tauri_commands::cookbook_repair_collapsed_properties,
        tauri_commands::cookbook_import_preview,
        tauri_commands::cookbook_import_save,
    ]
}

/// Writes this plugin's `bindings.ts` next to the frontend half that
/// imports it.
///
/// A test, like the app's own export: running the tests is what
/// regenerates them, so a command whose signature changed without the
/// frontend being regenerated fails the build rather than the device.
#[cfg(all(test, feature = "tauri"))]
mod bindings_export {
    #[test]
    fn export_bindings() {
        super::specta_builder()
            .export(
                specta_typescript::Typescript::default(),
                "../../src/features/cookbook/bindings.ts",
            )
            .expect("failed to write the bindings");
    }
}

