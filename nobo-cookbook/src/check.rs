// SPDX-FileCopyrightText: 2026 Dennis Burgermeister
// SPDX-License-Identifier: GPL-3.0-or-later

//! What is wrong with a cookbook file, as far as it can be told from the
//! file alone -- and which of it can be put right without guessing.
//!
//! Only what changes what the reader sees. A property spelled `:Servings:`
//! is not here, because every key is matched case-insensitively; a drawer
//! a blank line below its heading is not here, because the outline reader
//! takes a drawer anywhere under the heading it belongs to. A check that
//! lists harmless things teaches people to stop reading it.
//!
//! What is here, and why each one matters:
//!
//! - **Collapsed properties.** Every recipe's servings and times stacked in
//!   the first recipe's drawer, which is how some imports hand a file
//!   back. The reader keeps the last value it sees for a key, so the first
//!   recipe shows the last recipe's servings, and every other recipe shows
//!   none -- which is what "only the first recipe can be scaled" was.
//!   Fixable: see `repair`.
//! - **A repeated property** that the repair cannot assign: more values
//!   than one for a key, but not one per recipe. Org reads one; the rest
//!   are invisible. Not fixable -- the repair refuses to guess which value
//!   belongs to whom, and so does this.
//! - **No servings.** The scaling needs a number to scale from. Reported
//!   once per file, with the recipes it concerns; not fixable, because the
//!   number is not in the file.
//! - **No ingredients and no steps.** A recipe that is a title and some
//!   text. The recipe page has nothing to show but the text.
//!
//! Damage inside a line -- an ingredient with letters missing, which one
//! real import did to fourteen lines -- cannot be told from an ingredient
//! nobody has heard of, and is not attempted.

use std::collections::HashMap;

use nobo_core::core::models::{ORG_PROPERTIES_END, ORG_PROPERTIES_START};
use nobo_core::core::vault;

use crate::parse::recipes_of;
use crate::repair;

/// The four things this can say about a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[cfg_attr(feature = "tauri", derive(specta::Type))]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    CollapsedProperties,
    RepeatedProperty,
    NoServings,
    NoSections,
}

/// One problem, and what it concerns.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[cfg_attr(feature = "tauri", derive(specta::Type))]
pub struct Finding {
    pub kind: Kind,
    /// The recipes it concerns, by title, in file order. For collapsed
    /// properties that is the one recipe holding everybody's values.
    pub recipes: Vec<String>,
    /// For the two property kinds: which keys.
    pub keys: Vec<String>,
    /// For collapsed properties, how many recipes the drawer holds values
    /// for; for a repeated property, how many values the key has; for the
    /// other two, how many recipes it concerns.
    #[cfg_attr(feature = "tauri", specta(type = specta_typescript::Number))]
    pub count: usize,
    /// Whether the fix button can put it right.
    pub fixable: bool,
}

/// One cookbook file's findings.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[cfg_attr(feature = "tauri", derive(specta::Type))]
pub struct FileCheck {
    pub file_path: String,
    #[cfg_attr(feature = "tauri", specta(type = specta_typescript::Number))]
    pub recipe_count: usize,
    pub findings: Vec<Finding>,
}

impl FileCheck {
    pub fn is_fixable(&self) -> bool {
        self.findings.iter().any(|finding| finding.fixable)
    }
}

pub fn check(file_path: &str, text: &str) -> FileCheck {
    let recipes = recipes_of(file_path, text);
    let collapsed = repair::find(file_path, text);
    let mut findings = Vec::new();

    // The keys the repair will hand back. Everything they would cause --
    // the repeats in the first drawer, the recipes with no servings -- is
    // the one collapsed finding, not three.
    let handed_back: Vec<String> =
        collapsed.as_ref().map(|report| report.keys.iter().map(|key| key.to_lowercase()).collect()).unwrap_or_default();

    if let Some(report) = &collapsed {
        findings.push(Finding {
            kind: Kind::CollapsedProperties,
            recipes: vec![report.title.clone()],
            keys: report.keys.clone(),
            count: report.recipe_count,
            fixable: true,
        });
    }

    let lines: Vec<&str> = text.lines().collect();
    for recipe in &recipes {
        for (key, values) in repeated_keys(&lines, recipe.line_no) {
            if handed_back.contains(&key.to_lowercase()) {
                continue;
            }
            findings.push(Finding {
                kind: Kind::RepeatedProperty,
                recipes: vec![recipe.title.clone()],
                keys: vec![key],
                count: values,
                fixable: false,
            });
        }
    }

    let servings_handed_back = handed_back.iter().any(|key| key == "servings");
    let without_servings: Vec<String> =
        recipes.iter().filter(|recipe| recipe.servings.is_none() && recipe.sections.is_some()).map(|recipe| recipe.title.clone()).collect();
    if !without_servings.is_empty() && !servings_handed_back {
        findings.push(Finding {
            kind: Kind::NoServings,
            count: without_servings.len(),
            recipes: without_servings,
            keys: Vec::new(),
            fixable: false,
        });
    }

    let without_sections: Vec<String> =
        recipes.iter().filter(|recipe| recipe.sections.is_none()).map(|recipe| recipe.title.clone()).collect();
    if !without_sections.is_empty() {
        findings.push(Finding {
            kind: Kind::NoSections,
            count: without_sections.len(),
            recipes: without_sections,
            keys: Vec::new(),
            fixable: false,
        });
    }

    FileCheck { file_path: file_path.to_string(), recipe_count: recipes.len(), findings }
}

/// Keys with more than one value in the drawer under the heading on
/// 1-based LINE_NO, in the order the drawer first names them.
///
/// Read from the text, not from the outline: the outline keeps one value
/// per key, which is exactly the loss this is looking for.
fn repeated_keys(lines: &[&str], line_no: usize) -> Vec<(String, usize)> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    let mut in_drawer = false;

    for line in lines.iter().skip(line_no) {
        if vault::parse_heading_line(line).is_some() {
            break;
        }
        let trimmed = line.trim();
        if trimmed.eq_ignore_ascii_case(ORG_PROPERTIES_START) {
            in_drawer = true;
            continue;
        }
        if !in_drawer {
            continue;
        }
        if trimmed.eq_ignore_ascii_case(ORG_PROPERTIES_END) {
            break;
        }
        if let Some((key, _)) = vault::parse_property(trimmed) {
            let folded = key.to_lowercase();
            if !counts.contains_key(&folded) {
                order.push(key);
            }
            *counts.entry(folded).or_insert(0) += 1;
        }
    }

    order
        .into_iter()
        .filter_map(|key| {
            let n = counts[&key.to_lowercase()];
            (n > 1).then_some((key, n))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(check: &FileCheck) -> Vec<Kind> {
        check.findings.iter().map(|finding| finding.kind).collect()
    }

    const CLEAN: &str = "\
#+NOBO_TYPE: cookbook

* Linsensuppe
:PROPERTIES:
:servings: 4
:END:
** Ingredients
- 200 g Linsen
** Directions
1. Kochen.
* Brot
:PROPERTIES:
:servings: 1
:END:
** Ingredients
- 500 g Mehl
** Directions
1. Backen.
";

    #[test]
    fn a_file_in_order_has_nothing_to_say() {
        let check = check("a.org", CLEAN);
        assert_eq!(check.recipe_count, 2);
        assert!(check.findings.is_empty(), "{:?}", check.findings);
        assert!(!check.is_fixable());
    }

    // The shape a real import handed back: the first drawer holds every
    // recipe's values, one key has a value for only some of them.
    const COLLAPSED: &str = "\
#+NOBO_TYPE: cookbook

* Suppe
:PROPERTIES:
:source-url: https://example.org/suppe
:servings: 4
:servings: 1
:servings: 2
:END:
** Ingredients
- 1 Topf
** Directions
1. Kochen.
* Brot
** Ingredients
- 500 g Mehl
** Directions
1. Backen.
* Salat
** Ingredients
- 1 Kopf
** Directions
1. Waschen.
";

    #[test]
    fn a_collapsed_drawer_is_one_fixable_finding_not_three() {
        let check = check("a.org", COLLAPSED);
        // Not also "servings repeated" and "two recipes without servings":
        // those are what the collapse causes, and the fix takes them all.
        assert_eq!(kinds(&check), vec![Kind::CollapsedProperties]);
        let finding = &check.findings[0];
        assert!(finding.fixable);
        assert_eq!(finding.recipes, vec!["Suppe".to_string()]);
        assert_eq!(finding.count, 3);
        assert_eq!(finding.keys, vec!["servings".to_string()]);
    }

    #[test]
    fn what_the_repair_leaves_behind_is_still_reported() {
        let mut text = COLLAPSED.replace(
            ":source-url: https://example.org/suppe\n",
            ":source-url: https://example.org/suppe\n:source-url: https://example.org/brot\n",
        );
        // Two sources for three recipes: no arithmetic says whose.
        let (fixed, _) = repair::repair("a.org", &text).expect("collapsed");
        text = fixed;

        let check = check("a.org", &text);
        assert_eq!(kinds(&check), vec![Kind::RepeatedProperty]);
        let finding = &check.findings[0];
        assert!(!finding.fixable);
        assert_eq!(finding.keys, vec!["source-url".to_string()]);
        assert_eq!(finding.count, 2);
    }

    #[test]
    fn recipes_without_servings_are_named_once_per_file() {
        let text = CLEAN.replace(":servings: 1\n", "");
        let check = check("a.org", &text);
        assert_eq!(kinds(&check), vec![Kind::NoServings]);
        assert_eq!(check.findings[0].recipes, vec!["Brot".to_string()]);
        assert!(!check.findings[0].fixable);
    }

    #[test]
    fn a_recipe_that_is_only_a_title_and_text_is_named() {
        let text = format!("{CLEAN}* Omas Kuchen\nSteht im roten Heft.\n");
        let check = check("a.org", &text);
        assert!(kinds(&check).contains(&Kind::NoSections), "{:?}", check.findings);
        let finding = check.findings.iter().find(|finding| finding.kind == Kind::NoSections).unwrap();
        assert_eq!(finding.recipes, vec!["Omas Kuchen".to_string()]);
        // And not ALSO "no servings": a recipe with nothing to scale is
        // not missing a number to scale it by.
        assert!(!kinds(&check).contains(&Kind::NoServings), "{:?}", check.findings);
    }

    // Three spellings, one key. Three values for two recipes is not one
    // per recipe, so this is a repeat the repair leaves alone -- with two
    // it would have been a collapse, and handed back.
    #[test]
    fn a_key_spelled_differently_is_the_same_key() {
        let text = CLEAN.replace(":servings: 4\n", ":Servings: 4\n:SERVINGS: 6\n:servings: 8\n");
        let check = check("a.org", &text);
        assert_eq!(kinds(&check), vec![Kind::RepeatedProperty], "{:?}", check.findings);
        assert_eq!(check.findings[0].count, 3);
    }
}
