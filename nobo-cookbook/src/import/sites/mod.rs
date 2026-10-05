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

mod cookidoo;
mod wprm;

use super::{ImportedRecipe, Page};

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
pub const REFINERS: &[&dyn Refiner] = &[&wprm::WpRecipeMaker, &cookidoo::Cookidoo];
