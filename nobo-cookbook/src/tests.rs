//! Parsed against the sample vault's own cookbook files (`testdata/`), not
//! against fixtures written next to the assertions: those files are what
//! the devcontainer serves and what anyone tries the feature on, so a
//! shape this parser stops handling has to fail here rather than in
//! someone's kitchen.

use super::*;
use std::path::PathBuf;

fn sample(name: &str) -> Vec<Recipe> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../testdata").join(name);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("sample file {}: {e}", path.display()));
    recipes_of(&path.to_string_lossy(), &text)
}

fn simple() -> Vec<Recipe> {
    sample("20240402T181500--rezepte-schnelle-kueche__kochen_rezepte.org")
}
fn by_category() -> Vec<Recipe> {
    sample("20240403T094500--kochbuch-nach-kategorien__kochen_rezepte.org")
}
fn org_chef() -> Vec<Recipe> {
    sample("20240404T113000--org-chef-recipes__kochen_rezepte.org")
}
fn nested() -> Vec<Recipe> {
    sample("20240405T160000--rezepte-mit-teilschritten__kochen_rezepte.org")
}

fn named<'a>(recipes: &'a [Recipe], title: &str) -> &'a Recipe {
    recipes
        .iter()
        .find(|r| r.title == title)
        .unwrap_or_else(|| panic!("no recipe {title:?} in {:?}", recipes.iter().map(|r| &r.title).collect::<Vec<_>>()))
}

fn flat_items(recipe: &Recipe) -> Vec<&str> {
    recipe.sections.as_ref().unwrap().ingredients.iter()
        .flat_map(|group| group.items.iter().map(|item| item.text.as_str()))
        .collect()
}

/* --- what counts as a recipe --- */

#[test]
fn reads_recipes_that_sit_on_the_top_level() {
    let recipes = simple();
    assert_eq!(
        recipes.iter().map(|r| r.title.as_str()).collect::<Vec<_>>(),
        vec!["Spaghetti Carbonara", "Tomatensuppe", "Omas Kartoffelsalat"],
    );
}

/// Why a recipe is not defined as "a level-1 heading": grouped by course,
/// the recipes are on level 2 and their sections on level 3.
#[test]
fn reads_recipes_grouped_under_a_course() {
    let recipes = by_category();
    assert_eq!(
        recipes.iter().map(|r| r.title.as_str()).collect::<Vec<_>>(),
        vec!["Tiramisu", "Apfelkuchen", "Linsencurry"],
    );
    assert_eq!(named(&recipes, "Tiramisu").categories, vec!["Desserts"]);
    assert_eq!(named(&recipes, "Linsencurry").categories, vec!["Hauptgerichte"]);
}

#[test]
fn a_grouping_heading_is_not_itself_a_recipe() {
    let recipes = by_category();
    let titles: Vec<&str> = recipes.iter().map(|r| r.title.as_str()).collect();
    assert!(!titles.contains(&"Desserts"), "{titles:?}");
    assert!(!titles.contains(&"Hauptgerichte"), "{titles:?}");
}

/// A heading with no sections at all is still a recipe -- just an
/// unstructured one. The reading view shows its text as it stands rather
/// than treating the file as broken.
#[test]
fn an_unstructured_entry_is_a_recipe_without_sections() {
    let recipes = simple();
    let recipe = named(&recipes, "Omas Kartoffelsalat");
    assert!(recipe.sections.is_none());
    assert!(recipe.intro.contains("Festkochende Kartoffeln"), "{}", recipe.intro);
}

/// The prose section in the category file has no sections and no children
/// either -- it would slip in as an "unstructured recipe" if the rule were
/// only about sections. It sits at the same level as the courses, so its
/// siblings decide: a heading whose peers are grouping headings is one too.
#[test]
fn a_prose_section_between_courses_does_not_become_a_recipe() {
    let recipes = by_category();
    let titles: Vec<&str> = recipes.iter().map(|r| r.title.as_str()).collect();
    assert!(!titles.contains(&"Notizen zur Vorratshaltung"), "{titles:?}");
}

/* --- the org-chef standard --- */

#[test]
fn reads_the_english_section_headings_org_chef_itself_writes() {
    let recipes = org_chef();
    let pancakes = named(&recipes, "Pancakes");
    let sections = pancakes.sections.as_ref().expect("Ingredients/Directions recognised");
    assert_eq!(sections.ingredients[0].items.len(), 5);
    assert_eq!(sections.steps[0].steps.len(), 3);
}

#[test]
fn preparation_counts_as_directions_too() {
    let recipes = org_chef();
    let guacamole = named(&recipes, "Guacamole");
    let sections = guacamole.sections.as_ref().unwrap();
    assert_eq!(sections.steps[0].steps.len(), 3);
}

#[test]
fn reads_properties_whatever_case_they_were_written_in() {
    let recipes = org_chef();
    let pancakes = named(&recipes, "Pancakes");
    assert_eq!(pancakes.servings.as_deref(), Some("4"));
    assert_eq!(pancakes.prep_time.as_deref(), Some("5"));
    assert_eq!(pancakes.source_url.as_deref(), Some("https://example.com/pancakes"));

    let recipes = simple();
    let carbonara = named(&recipes, "Spaghetti Carbonara");
    assert_eq!(carbonara.servings.as_deref(), Some("2"));
    assert_eq!(carbonara.cook_time.as_deref(), Some("15"));
}

#[test]
fn keeps_tags_and_the_identity_the_reading_state_is_keyed_on() {
    let recipes = simple();
    let carbonara = named(&recipes, "Spaghetti Carbonara");
    assert_eq!(carbonara.tags, vec!["pasta", "fleisch", "schnell"]);
    assert_eq!(carbonara.custom_id.as_deref(), Some("h:rec-carbonara"));
}

/* --- sub-headings inside a section --- */

#[test]
fn a_split_ingredient_list_becomes_groups_rather_than_one_flat_list() {
    let recipes = nested();
    let kuchen = named(&recipes, "Zwiebelkuchen");
    let sections = kuchen.sections.as_ref().unwrap();
    let titles: Vec<Option<&str>> = sections.ingredients.iter().map(|g| g.title.as_deref()).collect();
    assert_eq!(titles, vec![Some("Für den Teig"), Some("Für den Belag")]);
    assert_eq!(sections.ingredients[0].items[0].text, "300 g Mehl");
}

#[test]
fn split_directions_keep_their_own_step_numbering_per_group() {
    let sections = named(&nested(), "Zwiebelkuchen").sections.as_ref().unwrap().clone();
    let titles: Vec<Option<&str>> = sections.steps.iter().map(|g| g.title.as_deref()).collect();
    assert_eq!(titles, vec![Some("Teig"), Some("Belag"), Some("Backen")]);
    assert_eq!(sections.steps[0].steps[0], "Hefe in der lauwarmen Milch auflösen.");
    assert_eq!(sections.steps[2].steps.len(), 2);
}

/// The sub-headings of a section must not be mistaken for recipes -- they
/// have no sections of their own and no children, which is exactly the
/// shape an unstructured recipe has.
#[test]
fn section_sub_headings_are_not_recipes() {
    let recipes = nested();
    let titles: Vec<&str> = recipes.iter().map(|r| r.title.as_str()).collect();
    assert_eq!(titles, vec!["Zwiebelkuchen", "Bananenbrot"]);
}

#[test]
fn an_ungrouped_list_keeps_no_group_title() {
    let recipes = simple();
    let suppe = named(&recipes, "Tomatensuppe");
    let sections = suppe.sections.as_ref().unwrap();
    assert_eq!(sections.ingredients.len(), 1);
    assert!(sections.ingredients[0].title.is_none());
}

#[test]
fn list_markers_are_stripped_from_both_halves() {
    let recipes = simple();
    let carbonara = named(&recipes, "Spaghetti Carbonara");
    assert_eq!(flat_items(carbonara)[0], "200 g Spaghetti");
    let steps = &carbonara.sections.as_ref().unwrap().steps[0].steps;
    assert_eq!(steps[0], "Nudelwasser aufsetzen und kräftig salzen.");
}

/* --- files that are not cookbooks --- */

#[test]
fn a_note_that_merely_mentions_ingredients_is_not_parsed_here() {
    // recipes_of runs on files the user declared; this only pins that the
    // function itself needs the structure, not a file name or a path.
    let recipes = recipes_of("notiz.org", "* Einkaufen\n- 2 Eier\n- Milch\n");
    assert!(recipes[0].sections.is_none());
}
