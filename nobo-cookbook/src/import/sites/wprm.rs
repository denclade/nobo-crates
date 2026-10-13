//! WP Recipe Maker, the WordPress plugin behind a large share of food
//! blogs (biancazapatka.com among them).
//!
//! Its JSON-LD keeps the instruction sections but flattens the
//! ingredients into one list -- "Reis:", "Tofu:", "Tahini-Sauce:" are
//! gone, and with them which salt goes into the rice and which into the
//! sauce. The page's own markup still has them: one
//! `wprm-recipe-ingredient-group` per group, a group name, one
//! `wprm-recipe-ingredient` per line, in the JSON-LD's order.
//!
//! So the groups are taken from the markup and the lines from the
//! JSON-LD, which has them as clean text already (see
//! `regroup_ingredients`).

use super::super::html::elements;
use super::super::{ImportedRecipe, Page};
use super::Refiner;

pub struct WpRecipeMaker;

impl Refiner for WpRecipeMaker {
    fn name(&self) -> &'static str {
        "wp-recipe-maker"
    }

    fn recognises(&self, page: &Page) -> bool {
        page.html.contains("wprm-recipe-ingredient-group")
    }

    fn refine(&self, page: &Page, recipe: &mut ImportedRecipe) {
        // The first recipe card only: a page can embed a second one
        // (a sauce, a "you might also like"), and its groups are not this
        // recipe's.
        let Some(card) = elements(page.html, "wprm-recipe-ingredients-container").into_iter().next() else {
            return;
        };
        let shape: Vec<(Option<String>, usize)> = card
            .find("wprm-recipe-ingredient-group")
            .iter()
            .map(|group| {
                let title = group.text_of_first("wprm-recipe-group-name").map(|t| t.trim_end_matches(':').trim().to_string());
                (title, group.find("wprm-recipe-ingredient").len())
            })
            .collect();

        super::regroup_ingredients(recipe, shape);
    }
}
