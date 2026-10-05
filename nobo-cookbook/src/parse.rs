//! Reading recipes out of an org file.
//!
//! Only compiled with the `cookbook` feature: without it the app still
//! knows what a cookbook file IS (see the module next door, which keeps
//! the file kind registered so a shared vault stays safe), it just does
//! not read recipes out of one.

use nobo_core::core::models::property_value;

use super::model::{IngredientGroup, Recipe, Sections, StepGroup};
use super::outline::{self, Node};

/// Section headings that introduce the ingredient list, lowercased. A
/// trailing colon is tolerated (`** Zutaten:`).
///
/// "Ingredients" and "Directions" are what org-chef itself writes, and
/// nothing else: `org-chef-recipe-to-org-element` builds both headlines
/// with the title fixed and `:level 2`. Everything beyond those two words
/// -- the German ones, the other English ones below -- is this app
/// reading what a person typed, not what org-chef produced.
const INGREDIENT_HEADINGS: &[&str] = &["zutaten", "ingredients"];

/// Section headings that introduce the steps, lowercased.
const DIRECTION_HEADINGS: &[&str] = &[
    "zubereitung", "schritte", "anleitung",
    "directions", "preparation", "instructions", "steps", "method",
];

use super::ingredients;

/// Property names per field, in the order they are tried. The first is the
/// one org-chef writes; the rest are what a German user would type.
/// Matching is case-insensitive (see models::property_value), so
/// `:servings:` and `:SERVINGS:` are the same key.
const SERVINGS_KEYS: &[&str] = &["SERVINGS", "PORTIONEN"];
const PREP_TIME_KEYS: &[&str] = &["PREP-TIME", "VORBEREITUNGSZEIT", "ZUBEREITUNGSZEIT"];
const COOK_TIME_KEYS: &[&str] = &["COOK-TIME", "KOCHZEIT", "BACKZEIT"];
/// The fifth of the five org-chef writes, and the one that was missing:
/// how long the recipe takes end to end. Neither the sum of the other two
/// nor derivable from them -- dough that rests for an hour is in neither.
const READY_IN_KEYS: &[&str] = &["READY-IN", "FERTIG-IN", "GESAMTZEIT", "GESAMTDAUER"];
const SOURCE_KEYS: &[&str] = &["SOURCE-URL", "QUELLE", "SOURCE"];

/// Every recipe in one cookbook file, in file order.
pub fn recipes_of(file_path: &str, text: &str) -> Vec<Recipe> {
    let nodes = outline::read(text);
    // Once, not once per heading: recipe_level walks the whole outline and
    // asks children_of about every node, so computing it inside the filter
    // made a long cookbook quadratically slower for an answer that is the
    // same every time.
    let level = recipe_level(&nodes);
    nodes
        .iter()
        .enumerate()
        .filter(|(index, node)| is_recipe(&nodes, *index, node, level))
        .map(|(index, node)| build(file_path, &nodes, index, node))
        .collect()
}

/// A heading is a recipe when it owns an ingredients or directions
/// section -- that is the unambiguous case and needs no guessing.
///
/// The awkward one is a heading with nothing under it, because an
/// unstructured recipe (`* Omas Kartoffelsalat`, just prose) and a stray
/// note in the same file have exactly the same shape. Nothing in the text
/// tells them apart, so the file's own layout decides: a childless
/// heading counts as a recipe when it sits at the level the file's real
/// recipes sit at. In a flat collection that is level 1, so the loose
/// entry is a recipe; in one grouped by course the recipes are level 2, so
/// `* Notizen zur Vorratshaltung` next to `* Desserts` is not.
///
/// Section headings themselves are never recipes, and neither are the
/// sub-headings inside them (`*** Für den Teig`) -- those are deeper than
/// the recipe level.
fn is_recipe(flat: &[Node], index: usize, heading: &Node, recipe_level: usize) -> bool {
    if section_of(heading).is_some() {
        return false;
    }
    // One pass over the children, answering both questions it is asked:
    // does it own a section (then it is a recipe), and does it have any
    // children at all (a childless one is judged by its level instead).
    let mut childless = true;
    for (_, child) in children_of(flat, index, heading.level) {
        childless = false;
        if section_of(child).is_some() {
            return true;
        }
    }
    childless && heading.level == recipe_level
}

/// The level this file keeps its recipes at: the shallowest heading that
/// owns a section. A file with no structured recipe at all has none to go
/// by, and falls back to its shallowest heading.
fn recipe_level(flat: &[Node]) -> usize {
    flat.iter()
        .enumerate()
        .filter(|(index, node)| children_of(flat, *index, node.level).any(|(_, child)| section_of(child).is_some()))
        .map(|(_, node)| node.level)
        .min()
        .or_else(|| flat.iter().map(|node| node.level).min())
        .unwrap_or(1)
}

fn build(file_path: &str, flat: &[Node], index: usize, heading: &Node) -> Recipe {
    let mut ingredients = Vec::new();
    let mut steps = Vec::new();
    let mut extras = Vec::new();
    let mut structured = false;

    for (child_index, child) in children_of(flat, index, heading.level) {
        match section_of(child) {
            Some(Section::Ingredients) => {
                structured = true;
                ingredients = groups_of(flat, child_index, child, |lines| {
                    lines.iter().map(|line| ingredients::parse(line)).collect()
                })
                .into_iter()
                .map(|(title, items)| IngredientGroup { title, items })
                .collect();
            }
            Some(Section::Directions) => {
                structured = true;
                steps = groups_of(flat, child_index, child, |lines| lines.to_vec())
                    .into_iter()
                    .map(|(title, steps)| StepGroup { title, steps })
                    .collect();
            }
            // Neither, and therefore somebody's own section: the
            // nutrition table, the tips, the "meal prep" note. Kept
            // whole rather than dropped -- see Sections::extras.
            None => {
                let lines = list_lines(&child.content);
                if !lines.is_empty() {
                    extras.push(StepGroup { title: Some(child.title.clone()), steps: lines });
                }
            }
        }
    }

    let (image, intro) = picture_of(file_path, heading.content.trim());
    Recipe {
        file_path: file_path.to_string(),
        line_no: heading.line_no,
        custom_id: property_value(&heading.properties, "CUSTOM_ID").map(str::to_string),
        title: heading.title.clone(),
        tags: heading.tags.clone(),
        categories: ancestor_titles(flat, index, heading.level),
        servings: first_property(heading, SERVINGS_KEYS),
        prep_time: first_property(heading, PREP_TIME_KEYS),
        cook_time: first_property(heading, COOK_TIME_KEYS),
        ready_in: first_property(heading, READY_IN_KEYS),
        source_url: first_property(heading, SOURCE_KEYS),
        image,
        intro,
        sections: structured.then_some(Sections { ingredients, steps, extras }),
    }
}

/// The picture leading the intro, and the intro without it.
///
/// Only a link that stands alone on the first line counts: one in the
/// middle of somebody's prose is part of what they wrote.
fn picture_of(file_path: &str, intro: &str) -> (Option<String>, String) {
    let (first, rest) = intro.split_once('\n').unwrap_or((intro, ""));
    let target = first
        .trim()
        .strip_prefix("[[file:")
        .and_then(|link| link.strip_suffix("]]"))
        .map(|link| link.split("][").next().unwrap_or(link));
    let is_image = |path: &str| {
        let lower = path.to_lowercase();
        IMAGE_EXTENSIONS.iter().any(|extension| lower.ends_with(extension))
    };
    match target.filter(|path| is_image(path)) {
        Some(path) => {
            let absolute = match std::path::Path::new(path).is_absolute() {
                true => std::path::PathBuf::from(path),
                false => std::path::Path::new(file_path).parent().unwrap_or(std::path::Path::new("")).join(path),
            };
            (Some(absolute.to_string_lossy().into_owned()), rest.trim().to_string())
        }
        None => (None, intro.to_string()),
    }
}

const IMAGE_EXTENSIONS: &[&str] = &[".jpg", ".jpeg", ".png", ".webp", ".gif"];

enum Section {
    Ingredients,
    Directions,
}

fn section_of(heading: &Node) -> Option<Section> {
    let title = heading.title.trim().trim_end_matches(':').to_lowercase();
    if INGREDIENT_HEADINGS.contains(&title.as_str()) {
        Some(Section::Ingredients)
    } else if DIRECTION_HEADINGS.contains(&title.as_str()) {
        Some(Section::Directions)
    } else {
        None
    }
}

/// A section's content, split into the ungrouped part and one group per
/// sub-heading (`*** Für den Teig`).
fn groups_of<T>(
    flat: &[Node],
    index: usize,
    section: &Node,
    convert: impl Fn(&[String]) -> Vec<T>,
) -> Vec<(Option<String>, Vec<T>)> {
    let mut groups = Vec::new();
    let direct = list_lines(&section.content);
    if !direct.is_empty() {
        groups.push((None, convert(&direct)));
    }
    for (_, child) in children_of(flat, index, section.level) {
        let lines = list_lines(&child.content);
        if !lines.is_empty() {
            groups.push((Some(child.title.clone()), convert(&lines)));
        }
    }
    groups
}

/// The list items in a section body, with their markers removed.
///
/// An item is not a line. Org lets one run over as many lines as it
/// likes, and recipes use that constantly -- a step names what to do and
/// then the machine settings under it, a sub-list spells out a sauce
/// inside the step that calls for it. Indentation is what says so, and
/// it is the only thing that does: a line indented past the item above it
/// belongs to that item, whether or not it carries a bullet of its own.
///
/// Splitting on newlines instead turned a recipe of six steps into
/// eighteen, each continuation line numbered as a step in its own right.
///
/// Anything that is not a list item and has nothing to continue -- a
/// stray sentence before the first one -- is kept as its own entry:
/// dropping content is worse than showing an unnumbered line.
fn list_lines(content: &str) -> Vec<String> {
    let mut items: Vec<String> = Vec::new();
    // How far the item being built is indented, so its continuations can
    // be told from the item that follows it.
    let mut indent = 0;

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let at = line.len() - line.trim_start().len();
        let marker = strip_marker(trimmed);

        match marker {
            Some(rest) if at <= indent || items.is_empty() => {
                items.push(rest.to_string());
                indent = at;
            }
            // Either a deeper bullet or a plain wrapped line: both
            // continue the item above.
            _ => match items.last_mut() {
                Some(item) => {
                    item.push('\n');
                    item.push_str(trimmed);
                }
                None => items.push(trimmed.to_string()),
            },
        }
    }
    items
}

/// The line without its list marker, or `None` when it carries none.
fn strip_marker(line: &str) -> Option<&str> {
    if let Some(rest) = line.strip_prefix("- ").or_else(|| line.strip_prefix("+ ")) {
        return Some(rest.trim_start());
    }
    // "1." / "1)" -- an ordered list, which is what steps normally are.
    let digits: String = line.chars().take_while(char::is_ascii_digit).collect();
    if !digits.is_empty() {
        let rest = &line[digits.len()..];
        if let Some(rest) = rest.strip_prefix('.').or_else(|| rest.strip_prefix(')')) {
            return Some(rest.trim_start());
        }
    }
    None
}

/// The headings directly below INDEX, i.e. its own children -- not its
/// grandchildren, and stopping where the subtree ends.
fn children_of(
    flat: &[Node],
    index: usize,
    level: usize,
) -> impl Iterator<Item = (usize, &Node)> {
    flat.iter()
        .enumerate()
        .skip(index + 1)
        .take_while(move |(_, heading)| heading.level > level)
        .filter(move |(_, heading)| heading.level == level + 1)
}

/// The titles of the headings this one sits under, outermost first.
fn ancestor_titles(flat: &[Node], index: usize, level: usize) -> Vec<String> {
    let mut wanted = level;
    let mut titles: Vec<String> = flat[..index]
        .iter()
        .rev()
        .filter(|heading| {
            let is_ancestor = heading.level < wanted;
            if is_ancestor {
                wanted = heading.level;
            }
            is_ancestor
        })
        .map(|heading| heading.title.clone())
        .collect();
    titles.reverse();
    titles
}

fn first_property(heading: &Node, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| property_value(&heading.properties, key))
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
