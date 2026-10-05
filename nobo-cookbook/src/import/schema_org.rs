//! The generic reader: a page's schema.org `Recipe`, as JSON-LD.
//!
//! This is what makes most sites work without anyone writing a line for
//! them. Search engines show recipe cards from exactly this data, so
//! nearly every recipe site publishes it -- WordPress plugins, Cookidoo,
//! the big portals. What varies is the shape: a `Recipe` at the top, in
//! an `@graph`, in an array; instructions as one string, as steps, as
//! sections of steps; an image as a URL, a list of them or an
//! `ImageObject`. All of that is absorbed here, so a site refiner only
//! deals with what a site does differently, never with schema.org's
//! many spellings of the same thing.

use serde_json::Value;

use super::html::{collapse_whitespace, decode_entities, text_of};
use super::{Group, ImportedRecipe};

/// The first `Recipe` in any of the page's JSON-LD blocks, read into the
/// import model. `None` when the page has none.
pub fn read(url: &str, blocks: &[&str]) -> Option<ImportedRecipe> {
    let recipe = blocks.iter().filter_map(|block| parse_json(block)).find_map(|value| find_recipe(&value).cloned())?;

    let title = text(recipe.get("name"))?;
    Some(ImportedRecipe {
        title,
        source_url: text(recipe.get("url")).filter(|u| u.starts_with("http")).unwrap_or_else(|| url.to_string()),
        description: text(recipe.get("description")),
        image_url: image(recipe.get("image")),
        servings: servings(recipe.get("recipeYield")),
        prep_time: minutes(recipe.get("prepTime")),
        cook_time: minutes(recipe.get("cookTime")),
        ready_in: minutes(recipe.get("totalTime")),
        ingredients: ingredients(recipe.get("recipeIngredient").or_else(|| recipe.get("ingredients"))),
        steps: instructions(recipe.get("recipeInstructions")),
        notes: Vec::new(),
    })
}

/// Pages ship JSON-LD that a strict parser refuses: a raw newline or tab
/// inside a string is the usual offence (hand-edited templates). A second
/// attempt with those turned into spaces costs nothing and saves the
/// page.
fn parse_json(block: &str) -> Option<Value> {
    serde_json::from_str(block).ok().or_else(|| {
        let cleaned: String = block.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
        serde_json::from_str(&cleaned).ok()
    })
}

fn find_recipe(value: &Value) -> Option<&Value> {
    match value {
        Value::Object(map) => {
            if is_type(map.get("@type"), "Recipe") {
                return Some(value);
            }
            // `@graph` is the usual place, but a recipe nested anywhere
            // (a `mainEntity`, an `ItemList`) is still the recipe.
            map.values().find_map(find_recipe)
        }
        Value::Array(items) => items.iter().find_map(find_recipe),
        _ => None,
    }
}

fn is_type(value: Option<&Value>, wanted: &str) -> bool {
    match value {
        Some(Value::String(t)) => t == wanted,
        Some(Value::Array(types)) => types.iter().any(|t| t.as_str() == Some(wanted)),
        _ => false,
    }
}

/// A plain-text field: entities decoded, markup stripped, whitespace
/// collapsed. Empty counts as missing.
fn text(value: Option<&Value>) -> Option<String> {
    let raw = match value? {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Array(items) => return items.iter().find_map(|item| text(Some(item))),
        Value::Object(map) => return text(map.get("text").or_else(|| map.get("name"))),
        _ => return None,
    };
    Some(clean(&raw)).filter(|s| !s.is_empty())
}

/// Text from JSON may still carry HTML -- entities (Cookidoo writes
/// `&frac12;`) or tags (some plugins keep the editor's `<strong>`).
fn clean(raw: &str) -> String {
    if raw.contains('<') {
        text_of(raw)
    } else {
        collapse_whitespace(&decode_entities(raw))
    }
}

/// The first image the page offers. Sites list the same picture in
/// several crops, largest first by convention, and the first is the one
/// they chose to lead with.
fn image(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(url) => Some(url.trim().to_string()).filter(|u| u.starts_with("http")),
        Value::Array(items) => items.iter().find_map(|item| image(Some(item))),
        Value::Object(map) => image(map.get("url").or_else(|| map.get("contentUrl"))),
        _ => None,
    }
}

/// `recipeYield` says it several ways at once (`["4", "4 Portionen"]`).
/// The longest that has a number in it says the most: "4 Portionen" over
/// "4", and the recipe view reads the number out of either.
fn servings(value: Option<&Value>) -> Option<String> {
    let candidates: Vec<String> = match value? {
        Value::Array(items) => items.iter().filter_map(|item| text(Some(item))).collect(),
        other => text(Some(other)).into_iter().collect(),
    };
    candidates.into_iter().filter(|c| c.chars().any(|ch| ch.is_ascii_digit())).max_by_key(|c| c.chars().count())
}

/// An ISO 8601 duration (`PT1H30M`) as a bare number of minutes -- what
/// org-chef writes and what the recipe view shows as minutes. Anything
/// that is not one is kept as the site wrote it; zero is no time at all.
fn minutes(value: Option<&Value>) -> Option<String> {
    let raw = text(value)?;
    let Some(spec) = raw.strip_prefix('P') else { return Some(raw) };
    let mut total = 0.0;
    let mut number = String::new();
    let mut in_time = false;
    for c in spec.chars() {
        match c {
            'T' => in_time = true,
            '0'..='9' | '.' => number.push(c),
            unit => {
                let n: f64 = number.parse().unwrap_or(0.0);
                number.clear();
                total += match (unit, in_time) {
                    ('D', false) => n * 24.0 * 60.0,
                    ('W', false) => n * 7.0 * 24.0 * 60.0,
                    ('H', true) => n * 60.0,
                    ('M', true) => n,
                    ('S', true) => n / 60.0,
                    // Months and years: no recipe takes that long, and a
                    // guess at their length would be wrong anyway.
                    _ => return Some(raw),
                };
            }
        }
    }
    let total = total.round() as i64;
    (total > 0).then(|| total.to_string())
}

/// The ingredient list, grouped where the page groups it.
///
/// schema.org has no ingredient groups, so sites that have them write the
/// group name as an ingredient of its own -- "Für die Füllung:" between
/// the lines it heads (zuckerjagdwurst.com does). Such a line ends in a
/// colon and has no number in it, which no real ingredient line does.
fn ingredients(value: Option<&Value>) -> Vec<Group> {
    let lines: Vec<String> = match value {
        Some(Value::Array(items)) => items.iter().filter_map(|item| text(Some(item))).collect(),
        Some(other) => text(Some(other)).into_iter().collect(),
        None => Vec::new(),
    };
    let mut groups: Vec<Group> = Vec::new();
    for line in lines {
        if line.ends_with(':') && !line.chars().any(|c| c.is_ascii_digit()) {
            groups.push(Group { title: Some(line.trim_end_matches(':').trim().to_string()), lines: Vec::new() });
            continue;
        }
        match groups.last_mut() {
            Some(group) => group.lines.push(line),
            None => groups.push(Group { title: None, lines: vec![line] }),
        }
    }
    groups.retain(|group| !group.lines.is_empty());
    groups
}

/// The steps, in every shape schema.org allows: one block of text, a list
/// of strings, a list of `HowToStep`s, or `HowToSection`s holding steps.
fn instructions(value: Option<&Value>) -> Vec<Group> {
    let mut groups: Vec<Group> = Vec::new();
    let mut loose = Group { title: None, lines: Vec::new() };
    match value {
        Some(Value::String(block)) => loose.lines = split_block(block),
        Some(Value::Array(items)) => {
            for item in items {
                if is_type(item.get("@type"), "HowToSection") {
                    let title = text(item.get("name")).map(|t| t.trim_end_matches(':').trim().to_string());
                    let lines = item
                        .get("itemListElement")
                        .and_then(Value::as_array)
                        .map(|steps| steps.iter().flat_map(step_lines).collect())
                        .unwrap_or_default();
                    groups.push(Group { title, lines });
                } else {
                    loose.lines.extend(step_lines(item));
                }
            }
        }
        Some(other) => loose.lines.extend(step_lines(other)),
        None => {}
    }
    if !loose.lines.is_empty() {
        groups.insert(0, loose);
    }
    groups.retain(|group| !group.lines.is_empty());
    groups
}

fn step_lines(step: &Value) -> Vec<String> {
    match step {
        // `text` is the step; `name` is often the same text again, or a
        // short label for it -- only a fallback.
        Value::Object(map) => text(map.get("text").or_else(|| map.get("name"))).into_iter().collect(),
        Value::String(s) => split_block(s),
        _ => Vec::new(),
    }
}

/// One string holding all the steps: split where the page split them --
/// paragraphs, list items, line breaks.
fn split_block(block: &str) -> Vec<String> {
    let marked = ["</p>", "</li>", "<br>", "<br/>", "<br />"]
        .iter()
        .fold(block.to_string(), |text, tag| text.replace(tag, "\n"));
    marked.lines().map(clean).filter(|line| !line.is_empty()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn durations_become_minutes() {
        assert_eq!(minutes(Some(&json!("PT20M"))).as_deref(), Some("20"));
        assert_eq!(minutes(Some(&json!("PT1H30M"))).as_deref(), Some("90"));
        assert_eq!(minutes(Some(&json!("P0DT0H45M"))).as_deref(), Some("45"));
        assert_eq!(minutes(Some(&json!("PT0M"))), None);
        assert_eq!(minutes(Some(&json!("20 Min."))).as_deref(), Some("20 Min."));
    }

    #[test]
    fn the_most_telling_yield_wins() {
        assert_eq!(servings(Some(&json!(["4", "4 Portionen"]))).as_deref(), Some("4 Portionen"));
        assert_eq!(servings(Some(&json!(6))).as_deref(), Some("6"));
    }

    #[test]
    fn a_colon_line_without_a_number_heads_a_group() {
        let groups = ingredients(Some(&json!(["Für den Teig:", "200 g Mehl", "Zum Servieren:", "Kresse"])));
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].title.as_deref(), Some("Für den Teig"));
        assert_eq!(groups[1].lines, ["Kresse"]);
    }

    #[test]
    fn a_recipe_is_found_inside_a_graph_with_a_list_type() {
        let block = r#"{"@graph":[{"@type":"WebPage"},{"@type":["Recipe"],"name":"Suppe","recipeInstructions":"<p>Kochen.</p><p>Essen.</p>"}]}"#;
        let recipe = read("https://example.org/suppe", &[block]).expect("a recipe");
        assert_eq!(recipe.title, "Suppe");
        assert_eq!(recipe.steps[0].lines, ["Kochen.", "Essen."]);
        assert_eq!(recipe.source_url, "https://example.org/suppe");
    }

    #[test]
    fn a_raw_newline_inside_a_string_does_not_lose_the_page() {
        let block = "{\"@type\":\"Recipe\",\"name\":\"Brot\nmit Kruste\"}";
        assert_eq!(read("https://x", &[block]).map(|r| r.title).as_deref(), Some("Brot mit Kruste"));
    }
}
