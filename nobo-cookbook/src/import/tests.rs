//! The import, on real pages.
//!
//! The fixtures are three live recipe pages cut down to what is read from
//! them (see the comment at the top of each): one WP Recipe Maker blog,
//! Cookidoo, and a site with nothing but plain schema.org data.

use super::*;
use crate::parse::recipes_of;

const BIANCA: &str = include_str!("fixtures/biancazapatka.html");
const COOKIDOO: &str = include_str!("fixtures/cookidoo.html");
const ZJW: &str = include_str!("fixtures/zuckerjagdwurst.html");
const CHEFKOCH: &str = include_str!("fixtures/chefkoch.html");
const CHEFKOCH_GROUPED: &str = include_str!("fixtures/chefkoch-lasagne.html");

fn titles(groups: &[Group]) -> Vec<Option<&str>> {
    groups.iter().map(|group| group.title.as_deref()).collect()
}

#[test]
fn a_wprm_page_gets_its_ingredient_groups_back() {
    let recipe = parse_page("https://biancazapatka.com/de/vegane-poke-bowl-mit-tahini-sauce/", BIANCA).unwrap();

    assert_eq!(recipe.title, "Vegane Poke Bowl mit Tahini-Sauce");
    assert_eq!(recipe.servings.as_deref(), Some("4 Portionen"));
    assert_eq!((recipe.prep_time.as_deref(), recipe.cook_time.as_deref(), recipe.ready_in.as_deref()), (Some("20"), Some("25"), Some("45")));
    assert_eq!(
        titles(&recipe.ingredients),
        [Some("Reis"), Some("Tofu"), Some("Gemüse (nach Belieben)"), Some("Tahini-Sauce"), Some("Zum Garnieren (optional)")]
    );
    // The salt goes into the rice, which is the whole point of the groups.
    assert_eq!(recipe.ingredients[0].lines.last().map(String::as_str), Some("½ TL Salz"));
    assert_eq!(recipe.ingredients[4].lines, ["Sesam", "frische Zitronen (oder Limetten)", "Jalapeños (in Scheiben geschnitten)", "frische Kräuter (z.B. Koriander oder Thai-Basilikum)"]);
    // The sections of steps come from the JSON-LD itself.
    assert!(titles(&recipe.steps).contains(&Some("Tahini-Sauce")));
    assert_eq!(
        recipe.image_url.as_deref(),
        Some("https://biancazapatka.com/wp-content/uploads/2020/07/vegane-poke-bowl-mit-tofu-und-tahini-sauce.jpg")
    );
}

#[test]
fn cookidoo_keeps_how_things_are_cut_and_says_where_the_steps_are() {
    let url = "https://cookidoo.de/recipes/recipe/de-DE/r449784";
    let recipe = parse_page(url, COOKIDOO).unwrap();

    assert_eq!(recipe.title, "Bihunsuppe");
    let lines = &recipe.ingredients[0].lines;
    assert_eq!(lines[0], "200 g Hähnchenbrust, in Würfeln (2 cm)");
    // The entity Cookidoo writes into its own data, decoded.
    assert_eq!(lines[9], "½ - 1 TL Sambal Oelek, nach Geschmack");
    assert!(recipe.steps.is_empty());
    assert_eq!(recipe.notes.len(), 1);
    assert!(recipe.notes[0].contains(url), "{}", recipe.notes[0]);
}

#[test]
fn plain_schema_org_data_is_enough_and_colon_lines_become_groups() {
    let recipe = parse_page("https://www.zuckerjagdwurst.com/de/rezepte/vegane-blaetterteig-hasen-mit-zwiebel-pilz-fuellung", ZJW).unwrap();

    assert_eq!(recipe.title, "Vegane Blätterteig-Hasen mit Zwiebel-Pilz-Füllung");
    assert_eq!(recipe.servings.as_deref(), Some("15–20 Stück"));
    assert_eq!(recipe.ready_in.as_deref(), Some("50"));
    assert_eq!(titles(&recipe.ingredients), [Some("Für die Blätterteig-Hasen"), Some("Für die Zwiebel-Pilz-Füllung"), Some("Zum Servieren")]);
    assert_eq!(recipe.steps.len(), 1);
    assert!(recipe.steps[0].lines.len() >= 5);
    assert!(recipe.image_url.as_deref().unwrap_or("").contains("zuckerjagdwurst"));
}

#[test]
fn chefkoch_without_the_user_the_advert_or_a_section_called_zubereitung() {
    let recipe = parse_page("https://www.chefkoch.de/rezepte/519331148201779/Bihun-Suppe.html", CHEFKOCH).unwrap();

    assert_eq!(recipe.title, "Bihun-Suppe");
    assert_eq!(recipe.description, None, "the template advert is not a description");
    // A reference into the graph, resolved.
    assert_eq!(
        recipe.image_url.as_deref(),
        Some("https://img.chefkoch-cdn.de/rezepte/519331148201779/bilder/1457319/crop-960x540/bihun-suppe.jpg")
    );
    assert_eq!(recipe.servings.as_deref(), Some("4 Portionen"));
    assert_eq!((recipe.prep_time.as_deref(), recipe.ready_in.as_deref()), (Some("30"), Some("60")));
    assert_eq!(recipe.ingredients.len(), 1, "one table: no groups to make");
    assert_eq!(recipe.ingredients[0].lines.len(), 12);
    assert_eq!(recipe.steps.len(), 1);
    assert_eq!(recipe.steps[0].title, None);
    assert_eq!(recipe.steps[0].lines.len(), 5);
}

#[test]
fn chefkoch_ingredient_groups_come_back_from_its_tables() {
    let recipe = parse_page("https://www.chefkoch.de/rezepte/1143511220703880/Julies-feine-Gemueselasagne.html", CHEFKOCH_GROUPED).unwrap();

    assert_eq!(recipe.title, "Julies feine Gemüselasagne");
    assert_eq!(titles(&recipe.ingredients), [Some("Für die Lasagne"), Some("Für die Sauce")]);
    assert_eq!(recipe.ingredients[0].lines.last().map(String::as_str), Some("9 Lasagneplatte(n)"));
    assert_eq!(recipe.ingredients[1].lines[0], "2 Knoblauchzehe(n)");
}

#[test]
fn a_page_without_recipe_data_says_so() {
    assert!(parse_page("https://example.org", "<html><body>Kuchen</body></html>").is_err());
}

/// The round trip that matters: what the import writes, the cookbook
/// reads back as the same recipe -- groups, properties, picture and all.
#[test]
fn an_imported_recipe_reads_back_as_the_recipe_it_was() {
    let recipe = parse_page("https://biancazapatka.com/de/vegane-poke-bowl-mit-tahini-sauce/", BIANCA).unwrap();
    let text = to_org(&recipe, 1, Some(".attachments/poke.jpg"));

    let read = recipes_of("/vault/kochbuch.org", &text);
    assert_eq!(read.len(), 1);
    let back = &read[0];
    assert_eq!(back.title, recipe.title);
    assert_eq!(back.servings, recipe.servings);
    assert_eq!(back.ready_in.as_deref(), Some("45"));
    assert_eq!(back.source_url.as_deref(), Some(recipe.source_url.as_str()));
    assert_eq!(back.image.as_deref(), Some("/vault/.attachments/poke.jpg"));
    assert!(!back.intro.contains("[[file:"), "the picture is not prose: {}", back.intro);
    let sections = back.sections.as_ref().expect("structured");
    assert_eq!(sections.ingredients.len(), 5);
    assert_eq!(sections.ingredients[0].items[0].text, "180 g Jasminreis (ungekocht, oder eine andere Reissorte oder Reisnudeln oder Quinoa (*siehe Anmerkungen))");
    assert_eq!(sections.steps.iter().map(|g| g.steps.len()).sum::<usize>(), recipe.steps.iter().map(|g| g.lines.len()).sum::<usize>());
}

#[test]
fn notes_become_their_own_section_not_steps() {
    let recipe = parse_page("https://cookidoo.de/recipes/recipe/de-DE/r449784", COOKIDOO).unwrap();
    let read = recipes_of("/k.org", &to_org(&recipe, 1, None));
    let sections = read[0].sections.as_ref().unwrap();
    assert!(sections.steps.is_empty());
    assert_eq!(sections.extras.len(), 1);
}

#[test]
fn an_entry_goes_under_its_category_one_level_down() {
    let file = "#+NOBO_TYPE: cookbook\n* Suppen\n** Linsensuppe\n*** Zutaten\n- Linsen\n\n* Desserts\n** Tiramisu\n";
    let (text, line) = insert(file, &["Suppen".to_string()], |level| format!("{} Neu\n", "*".repeat(level)));

    assert_eq!(text, "#+NOBO_TYPE: cookbook\n* Suppen\n** Linsensuppe\n*** Zutaten\n- Linsen\n\n** Neu\n\n* Desserts\n** Tiramisu\n");
    assert_eq!(text.lines().nth(line - 1), Some("** Neu"));
}

#[test]
fn without_a_category_an_entry_goes_to_the_end_at_the_top() {
    let (text, line) = insert("* A\ntext\n\n", &[], |level| format!("{} Neu\n", "*".repeat(level)));
    assert_eq!(text, "* A\ntext\n\n* Neu\n\n");
    assert_eq!(text.lines().nth(line - 1), Some("* Neu"));

    let (text, line) = insert("", &[], |level| format!("{} Neu\n", "*".repeat(level)));
    assert_eq!((text.as_str(), line), ("* Neu\n", 1));
}

#[test]
fn a_missing_category_falls_back_to_the_top_level() {
    let (text, _) = insert("* Suppen\n", &["Kuchen".to_string()], |level| format!("{} Neu\n", "*".repeat(level)));
    assert_eq!(text, "* Suppen\n\n* Neu\n");
}

#[test]
fn a_description_cannot_turn_into_a_heading() {
    let recipe = ImportedRecipe {
        title: "T".into(),
        source_url: "https://x".into(),
        description: Some("* Tipp: vorher einweichen".into()),
        ..Default::default()
    };
    let read = recipes_of("/k.org", &to_org(&recipe, 1, None));
    assert_eq!(read.len(), 1);
    assert_eq!(read[0].intro, "* Tipp: vorher einweichen");
}

#[test]
fn image_names_are_safe_and_readable() {
    assert_eq!(image_filename("Vegane Blätterteig-Hasen mit Zwiebel-Pilz-Füllung", "jpg"), "vegane-blaetterteig-hasen-mit-zwiebel-pilz-fuellung.jpg");
    assert_eq!(image_filename("??", "png"), "recipe.png");
}

/// A cookbook quoting org in a code block: a `*`-line in there is text.
/// The outline used to read it as a heading, so an import into
/// "Hauptgerichte" was written INTO the code block, under the quoted
/// heading, and the real category never got it.
#[test]
fn an_import_never_lands_inside_a_code_block() {
    let text = "#+NOBO_TYPE: cookbook\n* Desserts\n** Tiramisu\nVorlage:\n#+BEGIN_SRC org\n* Hauptgerichte\n* Suppen\n#+END_SRC\n* Hauptgerichte\n** Linsencurry\n";

    let (out, line) = super::insert(text, &["Hauptgerichte".to_string()], |level| format!("{} Neues Rezept\n", "*".repeat(level)));

    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[line - 1], "** Neues Rezept", "{out}");
    let block_end = lines.iter().position(|l| *l == "#+END_SRC").unwrap();
    assert!(line - 1 > block_end, "after the code block, under the real category:\n{out}");
    assert!(out.contains("#+BEGIN_SRC org\n* Hauptgerichte\n* Suppen\n#+END_SRC"), "the quote is untouched:\n{out}");
}

/// A CRLF file stays CRLF: the import adds lines, it does not rewrite
/// every other one.
#[test]
fn an_import_keeps_the_files_line_endings() {
    let text = "#+NOBO_TYPE: cookbook\r\n* Suppen\r\n** Linsen\r\nText\r\n";

    let (out, line) = super::insert(text, &["Suppen".to_string()], |level| format!("{} Neu\nZeile\n", "*".repeat(level)));

    assert!(!out.replace("\r\n", "").contains('\n'), "no bare LF anywhere:\n{out:?}");
    assert!(out.starts_with(text.trim_end_matches("\r\n")), "{out:?}");
    assert_eq!(out.lines().nth(line - 1), Some("** Neu"));
    assert!(out.ends_with("Zeile\r\n"), "{out:?}");
}

#[test]
fn an_lf_file_without_a_final_newline_gets_one_before_the_entry() {
    let (out, _) = super::insert("* A\ntext", &[], |level| format!("{} Neu\n", "*".repeat(level)));
    assert_eq!(out, "* A\ntext\n\n* Neu\n");
}
