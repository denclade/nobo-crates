// SPDX-FileCopyrightText: 2026 Dennis Burgermeister
// SPDX-License-Identifier: GPL-3.0-or-later

//! Chefkoch, the largest German recipe community.
//!
//! Its JSON-LD is complete, but written for search engines rather than
//! for a cookbook:
//!
//! - **The title names the user** ("Bihun-Suppe von Delphinella"). The
//!   page's own heading says just "Bihun-Suppe", so that is taken -- when
//!   the JSON-LD title starts with it, which is the check that both name
//!   the same recipe.
//! - **The description is an advert** ("Über 275 Bewertungen und für
//!   schmackhaft befunden. Mit ► Portionsrechner ► Kochbuch ..."),
//!   generated from a template and the same on every recipe. Recognised
//!   by the template's arrows and left out: it says nothing about the dish.
//! - **Ingredient groups** ("Für die Sauce:") exist only in the markup,
//!   one `ds-ingredients-table` each with its headline; the JSON-LD
//!   flattens them (see `regroup_ingredients`).
//!
//! The picture is a reference into the graph, which the generic reader
//! resolves; and the single "Zubereitung" section it would otherwise
//! keep is dropped there too.

use super::super::html::{elements, first_tag_text};
use super::super::{ImportedRecipe, Page};
use super::Refiner;

pub struct Chefkoch;

impl Refiner for Chefkoch {
    fn name(&self) -> &'static str {
        "chefkoch"
    }

    fn recognises(&self, page: &Page) -> bool {
        page.host_is("chefkoch")
    }

    fn refine(&self, page: &Page, recipe: &mut ImportedRecipe) {
        if let Some(heading) = first_tag_text(page.html, "h1") {
            if heading.len() < recipe.title.len() && recipe.title.starts_with(&heading) {
                recipe.title = heading;
            }
        }

        if recipe.description.as_deref().is_some_and(|text| text.contains('►')) {
            recipe.description = None;
        }

        let shape: Vec<(Option<String>, usize)> = elements(page.html, "ds-ingredients-table")
            .iter()
            .map(|table| {
                let title = table.text_of_first("ds-ingredients-table__headline").map(|t| t.trim_end_matches(':').trim().to_string());
                (title, table.find("ds-ingredients-table__tr").len())
            })
            .collect();
        super::regroup_ingredients(recipe, shape);
    }
}
