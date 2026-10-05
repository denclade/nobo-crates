//! Cookidoo, Vorwerk's Thermomix recipe platform.
//!
//! Two things differ from the generic case:
//!
//! - **The steps are not on the page.** Cookidoo shows them only to a
//!   signed-in subscriber, and its JSON-LD leaves `recipeInstructions`
//!   out entirely. There is nothing to read, and signing in on the
//!   user's behalf is not this app's business -- so the import says so,
//!   in a note that links back, instead of arriving as a recipe that
//!   silently has no steps.
//! - **The ingredient lines lose half of what they say.** The page shows
//!   "Hähnchenbrust -- in Würfeln (2 cm)"; the JSON-LD has only "200 g
//!   Hähnchenbrust". How a thing is cut is part of the ingredient, so the
//!   lines are rebuilt from the page's own ingredient elements -- when
//!   there are exactly as many as the JSON-LD has, which is the check
//!   that both are the same list.

use nobo_core::t;

use super::super::html::elements;
use super::super::{Group, ImportedRecipe, Page};
use super::Refiner;

pub struct Cookidoo;

impl Refiner for Cookidoo {
    fn name(&self) -> &'static str {
        "cookidoo"
    }

    fn recognises(&self, page: &Page) -> bool {
        page.host_is("cookidoo")
    }

    fn refine(&self, page: &Page, recipe: &mut ImportedRecipe) {
        let lines: Vec<String> = elements(page.html, "recipe-ingredient__wrapper")
            .iter()
            .filter_map(|item| {
                let name = item.text_of_first("recipe-ingredient__name")?;
                let mut line = match item.text_of_first("recipe-ingredient__amount") {
                    Some(amount) => format!("{amount} {name}"),
                    None => name,
                };
                if let Some(description) = item.text_of_first("recipe-ingredient__description") {
                    line.push_str(", ");
                    line.push_str(&description);
                }
                Some(line)
            })
            .collect();
        let known: usize = recipe.ingredients.iter().map(|group| group.lines.len()).sum();
        if !lines.is_empty() && lines.len() == known {
            recipe.ingredients = vec![Group { title: None, lines }];
        }

        if recipe.steps.is_empty() {
            let url = &recipe.source_url;
            recipe.notes.push(t!(
                "Die Zubereitungsschritte zeigt Cookidoo nur angemeldet: [[{url}][auf Cookidoo öffnen]]",
                "Cookidoo shows the steps only when signed in: [[{url}][open on Cookidoo]]"
            ));
        }
    }
}
