//! What one site, or one recipe plugin, does differently.
//!
//! The generic reader (see `schema_org`) gets every page with schema.org
//! data most of the way. A refiner here takes it the rest of the way for
//! the pages it recognises -- the groups a plugin keeps in its markup
//! but flattens in its JSON-LD, the steps a site keeps behind a login.
//!
//! A refiner only ever improves a recipe the generic reader already
//! found, and every one of them checks its own assumption before acting
//! on it: a site redesign then means a recipe without the improvement,
//! never a wrong one.
//!
//! Adding a site is one file and one line in `REFINERS`.

mod chefkoch;
mod cookidoo;
mod wprm;

use super::{Group, ImportedRecipe, Page};

pub trait Refiner: Sync {
    /// For the log: which refiner touched an import.
    fn name(&self) -> &'static str;

    /// Whether this page is one it knows. By host for a site, by markup
    /// for a plugin many sites share.
    fn recognises(&self, page: &Page) -> bool;

    fn refine(&self, page: &Page, recipe: &mut ImportedRecipe);
}

/// Every refiner, tried in order; all that recognise a page get to refine
/// it.
pub const REFINERS: &[&dyn Refiner] = &[&wprm::WpRecipeMaker, &cookidoo::Cookidoo, &chefkoch::Chefkoch];

/// The flat ingredient list split into the groups SHAPE describes --
/// (title, how many lines) in page order.
///
/// What a site does that keeps its groups in the markup but flattens them
/// in its JSON-LD (WP Recipe Maker, Chefkoch): the groups come from the
/// one, the lines from the other, which has them as clean text already.
/// Only when the counts agree -- if they do not, the two are not
/// describing the same list -- and only when there is more than one
/// group, since one group is the flat list with a heading over it.
fn regroup_ingredients(recipe: &mut ImportedRecipe, shape: Vec<(Option<String>, usize)>) {
    let lines: Vec<String> = recipe.ingredients.iter().flat_map(|g| g.lines.iter().cloned()).collect();
    if shape.len() < 2 || shape.iter().map(|(_, count)| count).sum::<usize>() != lines.len() {
        return;
    }
    let mut rest = lines.into_iter();
    recipe.ingredients = shape.into_iter().map(|(title, count)| Group { title, lines: rest.by_ref().take(count).collect() }).collect();
}
