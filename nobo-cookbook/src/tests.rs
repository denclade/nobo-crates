//! Parsed against the sample vault's own cookbook files (`testdata/`), not
//! against fixtures written next to the assertions: those files are what
//! the devcontainer serves and what anyone tries the feature on, so a
//! shape this parser stops handling has to fail here rather than in
//! someone's kitchen.

use super::*;
use crate::model::Amount;
use crate::repair;
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
/// The messy one: recipes as they come back from an import, which is
/// where every assertion below it was learned.
fn imported() -> Vec<Recipe> {
    sample("20240406T101500--rezepte-aus-dem-import__kochen_rezepte.org")
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
    // The fifth key org-chef writes, and the one that used to be dropped:
    // a recipe that says how long it takes end to end said it in vain.
    assert_eq!(pancakes.ready_in.as_deref(), Some("20"));
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

/* --- what an imported file looks like --- */

/// The one that sent us looking: a step that names a machine setting on
/// the next line is one step, not two. A recipe of three was numbered to
/// seven, and the numbers are what a cook follows.
#[test]
fn a_step_that_runs_over_several_lines_stays_one_step() {
    let recipes = imported();
    let recipe = named(&recipes, "Protein-Kartoffelgratin");
    let steps = &recipe.sections.as_ref().unwrap().steps[0].steps;

    assert_eq!(steps.len(), 3, "{steps:#?}");
    assert!(steps[1].starts_with("*Zwiebel hacken"), "{:?}", steps[1]);
    assert!(steps[1].contains("3 Min / Varoma / Stufe 1"), "the settings belong to their step: {:?}", steps[1]);
}

/// Indentation decides, not the bullet: a list nested inside a step is
/// part of that step, however it is marked.
#[test]
fn a_sub_list_inside_a_step_stays_inside_it() {
    let recipes = imported();
    let recipe = named(&recipes, "Porridge");
    let steps = &recipe.sections.as_ref().unwrap().steps[0].steps;

    assert_eq!(steps.len(), 4, "{steps:#?}");
    assert!(steps[2].contains("Kakaopulver"), "{:?}", steps[2]);
    assert!(steps[2].contains("Hafermilch auffüllen"), "{:?}", steps[2]);
}

/// Nutrition, tips, meal prep: somebody wrote them under their recipe, so
/// they come back with it. Dropping them showed half a recipe.
#[test]
fn a_section_that_is_neither_ingredients_nor_steps_is_kept_with_its_heading() {
    let recipes = imported();
    let recipe = named(&recipes, "Protein-Kartoffelgratin");
    let extras = &recipe.sections.as_ref().unwrap().extras;

    assert_eq!(
        extras.iter().map(|g| g.title.clone().unwrap_or_default()).collect::<Vec<_>>(),
        vec!["Nährwerte", "📦 Mealprep", "🔁 Varianten"],
    );
    assert_eq!(extras[0].steps[0], "Kalorien: ca. 580 kcal");
}

/// A qualifier after the heading is still that section's own name, and
/// still not a recipe.
#[test]
fn an_extra_section_keeps_the_heading_as_written() {
    let recipes = imported();
    let recipe = named(&recipes, "Grüner Salat mit Joghurt-Dressing");
    let extras = &recipe.sections.as_ref().unwrap().extras;

    assert_eq!(extras[0].title.as_deref(), Some("Nährwerte (pro Portion, ca.)"));
    assert!(recipes.iter().all(|r| r.title != "Nährwerte (pro Portion, ca.)"));
}

/// An import that lost the drawers leaves most recipes with no properties
/// at all. They are still recipes, and everything but the metadata is
/// still there.
#[test]
fn a_recipe_without_a_property_drawer_is_still_read_in_full() {
    let recipes = imported();
    let recipe = named(&recipes, "Kartoffelgulasch");

    assert_eq!(recipe.servings, None);
    assert_eq!(recipe.custom_id, None);
    assert_eq!(flat_items(recipe).len(), 8);
    assert_eq!(recipe.sections.as_ref().unwrap().steps[0].steps.len(), 2);
}

/// Repeated keys in one drawer -- the other half of that import. What
/// matters here is that a reader gets one value and not a crash; putting
/// them back where they belong is `repair`, below.
#[test]
fn a_drawer_with_a_key_repeated_answers_with_one_value() {
    let recipes = imported();
    let recipe = named(&recipes, "Vegetarisches Erdnuss-Gemüse");

    assert_eq!(recipe.servings.as_deref(), Some("4"));
    assert_eq!(recipe.cook_time.as_deref(), Some("0"));
}

/* --- giving a collapsed drawer back --- */

fn import_text() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../testdata/20240406T101500--rezepte-aus-dem-import__kochen_rezepte.org");
    std::fs::read_to_string(path).unwrap()
}

/// Six recipes, six values per key -- except `:source-url:`, which has
/// four because two recipes never had a source. One is arithmetic, the
/// other is a guess, and only the first is offered.
#[test]
fn a_collapsed_drawer_is_recognised_and_says_what_it_can_give_back() {
    let report = repair::find("k.org", &import_text()).expect("nothing found");

    assert_eq!(report.recipe_count, 6);
    assert_eq!(report.title, "Vegetarisches Erdnuss-Gemüse");
    assert_eq!(report.keys, vec!["cook-time", "prep-time", "servings"]);
    assert_eq!(report.left_alone, vec!["source-url"]);
}

/// The Nth value to the Nth recipe -- checked against a recipe in the
/// middle, where an off-by-one would show.
#[test]
fn every_recipe_gets_its_own_values_back() {
    let (fixed, _) = repair::repair("k.org", &import_text()).unwrap();
    let recipes = recipes_of("k.org", &fixed);

    let got: Vec<(&str, Option<&str>, Option<&str>)> = recipes
        .iter()
        .map(|r| (r.title.as_str(), r.servings.as_deref(), r.cook_time.as_deref()))
        .collect();
    assert_eq!(
        got,
        vec![
            ("Vegetarisches Erdnuss-Gemüse", Some("4"), Some("30min")),
            ("Poke Bowl mit Tahini-Sauce", Some("2"), None),
            ("Kartoffelgulasch", Some("3"), Some("20min")),
            ("Porridge", Some("1"), Some("10min")),
            ("Protein-Kartoffelgratin", Some("2"), Some("40")),
            ("Grüner Salat mit Joghurt-Dressing", Some("4"), Some("0")),
        ],
    );
}

/// What it does not touch: the id the first recipe is known by, the key
/// it could not place, and the recipes themselves.
#[test]
fn a_repair_leaves_alone_what_it_cannot_place() {
    let before = import_text();
    let (fixed, _) = repair::repair("k.org", &before).unwrap();

    // Counted as drawer lines: the file's own header mentions the key in
    // prose, and that is not a property.
    let sources = fixed.lines().filter(|line| line.starts_with(":source-url:")).count();
    assert_eq!(sources, 4, "all four stay where they were");
    assert_eq!(fixed.matches(":CUSTOM_ID: h:rec-erdnuss").count(), 1);
    assert_eq!(
        recipes_of("k.org", &fixed).iter().map(|r| r.title.clone()).collect::<Vec<_>>(),
        recipes_of("k.org", &before).iter().map(|r| r.title.clone()).collect::<Vec<_>>(),
    );
    assert_eq!(flat_items(named(&recipes_of("k.org", &fixed), "Kartoffelgulasch")).len(), 8);
}

/// Run twice it finds nothing the second time -- which is what lets the
/// app do it without asking every time it opens the cookbook.
#[test]
fn a_repaired_file_is_not_repaired_again() {
    let (fixed, _) = repair::repair("k.org", &import_text()).unwrap();

    assert!(repair::find("k.org", &fixed).is_none());
    assert!(repair::repair("k.org", &fixed).is_none());
}

/// An ordinary cookbook has nothing to give back.
#[test]
fn a_file_with_one_drawer_per_recipe_is_left_alone() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../testdata/20240404T113000--org-chef-recipes__kochen_rezepte.org");
    let text = std::fs::read_to_string(path).unwrap();

    assert!(repair::find("k.org", &text).is_none());
}

/// Ingredient lines set in columns with runs of spaces, which is what a
/// site that renders a table pastes in.
#[test]
fn an_amount_survives_being_set_in_columns() {
    let recipes = imported();
    let recipe = named(&recipes, "Poke Bowl mit Tahini-Sauce");
    let items = &recipe.sections.as_ref().unwrap().ingredients[0].items;

    let rice = items[0].amount.as_ref().unwrap();
    assert_eq!((rice.from, rice.unit.as_str()), (180.0, "g"));
    assert!(rice.item.starts_with("Jasminreis"), "{:?}", rice.item);

    let salt = items[2].amount.as_ref().unwrap();
    assert_eq!((salt.from, salt.unit.as_str(), salt.item.as_str()), (0.5, "TL", "Salz"));
}

/// Every spelling of an amount these files actually contain, read off the
/// sample rather than made up next to the assertion.
#[test]
fn reads_the_amounts_an_imported_file_actually_holds() {
    let recipes = imported();
    let recipe = named(&recipes, "Kartoffelgulasch");
    let items = &recipe.sections.as_ref().unwrap().ingredients[0].items;
    let amounts: Vec<Option<f64>> = items.iter().map(|i| i.amount.as_ref().map(|a| a.from)).collect();

    assert_eq!(amounts, vec![Some(700.0), None, Some(1.5), Some(0.5), Some(0.5), Some(1.0), Some(2.0), Some(2.0)]);
    assert_eq!(items[5].amount.as_ref().unwrap().unit, "kg", "a unit glued to the number");
    assert_eq!(items[6].amount.as_ref().unwrap().to, Some(3.0), "a range written org's own way");
}

/// A range, whichever dash was written between its ends: an en dash from
/// a recipe site, org's own "--" from Emacs. Both ends are kept, so
/// doubling the recipe moves them together.
#[test]
fn reads_a_range_whichever_dash_was_used() {
    let recipes = imported();
    let cheese = named_item(named(&recipes, "Protein-Kartoffelgratin"), "körniger Frischkäse");
    assert_eq!(((cheese.from, cheese.to), cheese.unit.as_str()), ((150.0, Some(200.0)), "g"));

    let onions = named_item(named(&recipes, "Kartoffelgulasch"), "Zwiebeln");
    assert_eq!((onions.from, onions.to), (2.0, Some(3.0)));
}

fn named_item<'a>(recipe: &'a Recipe, item: &str) -> &'a Amount {
    recipe.sections.as_ref().unwrap().ingredients.iter()
        .flat_map(|group| group.items.iter())
        .find_map(|i| i.amount.as_ref().filter(|a| a.item == item))
        .unwrap_or_else(|| panic!("no ingredient {item:?}"))
}
