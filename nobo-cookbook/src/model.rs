//! What the cookbook says to the frontend.
//!
//! Separate from the parsing on purpose: these types appear in command
//! signatures, so they exist in every build -- including one compiled
//! without the `cookbook` feature, where the commands are still there and
//! answer that they are not (see this feature's module doc).

/// One ingredient line: always the original text, plus the amount when
/// this could make sense of it.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[cfg_attr(feature = "tauri", derive(specta::Type))]
pub struct Ingredient {
    /// Exactly as it stands in the file, minus the list bullet.
    pub text: String,
    #[cfg_attr(feature = "tauri", specta(optional))]
    pub amount: Option<Amount>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[cfg_attr(feature = "tauri", derive(specta::Type))]
pub struct Amount {
    pub from: f64,
    /// The upper end of a range (`2-3`), `None` for a single value. Kept
    /// so scaling can move both ends together.
    #[cfg_attr(feature = "tauri", specta(optional))]
    pub to: Option<f64>,
    /// `g`, `EL`, `tbsp`, ... -- whatever stood between the number and the
    /// ingredient, empty when the number counts the thing itself
    /// ("2 Eier").
    pub unit: String,
    /// The rest of the line, i.e. what is being measured.
    pub item: String,
}

/// One recipe, ready to render.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[cfg_attr(feature = "tauri", derive(specta::Type))]
pub struct Recipe {
    /// Where it lives, so the view can open the file and the reading state
    /// can be keyed on something that survives a rename (see custom_id).
    pub file_path: String,
    #[cfg_attr(feature = "tauri", specta(type = specta_typescript::Number))]
    pub line_no: usize,
    #[cfg_attr(feature = "tauri", specta(optional))]
    pub custom_id: Option<String>,
    pub title: String,
    pub tags: Vec<String>,
    /// The heading path above it -- `["Desserts"]` for a recipe grouped
    /// under a course, empty for a top-level one. The sidebar groups by
    /// this before falling back to tags.
    pub categories: Vec<String>,
    #[cfg_attr(feature = "tauri", specta(optional))]
    pub servings: Option<String>,
    #[cfg_attr(feature = "tauri", specta(optional))]
    pub prep_time: Option<String>,
    #[cfg_attr(feature = "tauri", specta(optional))]
    pub cook_time: Option<String>,
    /// How long it takes from start to finish -- org-chef's `:ready-in:`.
    /// Its own field rather than prep + cook: the hour a dough rests is in
    /// neither of those, which is exactly why the key exists.
    #[cfg_attr(feature = "tauri", specta(optional))]
    pub ready_in: Option<String>,
    #[cfg_attr(feature = "tauri", specta(optional))]
    pub source_url: Option<String>,
    /// The recipe's picture, as an absolute path: a link to an image file
    /// on a line of its own, first thing under the heading -- where an
    /// import puts it. Taken out of `intro`, which is prose, so a view
    /// shows the picture as a picture rather than as link syntax.
    #[cfg_attr(feature = "tauri", specta(optional))]
    #[serde(default)]
    pub image: Option<String>,
    /// The prose between the heading and its first section.
    pub intro: String,
    /// `None` for a heading with neither section -- the reading view falls
    /// back to showing `intro` as it stands (see the module comment).
    #[cfg_attr(feature = "tauri", specta(optional))]
    pub sections: Option<Sections>,
}

/// The structured parts of a recipe, each already flattened to lines.
/// Sub-headings inside them (`*** Für den Teig`, `*** Belag`) become
/// groups rather than being lost or being mistaken for recipes of their
/// own.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[cfg_attr(feature = "tauri", derive(specta::Type))]
pub struct Sections {
    pub ingredients: Vec<IngredientGroup>,
    pub steps: Vec<StepGroup>,
    /// Every other section the recipe has -- the nutrition table, the
    /// tips, the "meal prep" note, whatever else somebody wrote under
    /// their recipe. Kept because it is theirs: a reading view that only
    /// knew ingredients and steps showed half of such a recipe and said
    /// nothing about the other half.
    ///
    /// A `StepGroup` because that is what one is: a titled list of lines.
    /// Its `title` is always set here, since an untitled section is not a
    /// section at all.
    #[cfg_attr(feature = "tauri", specta(optional))]
    #[serde(default)]
    pub extras: Vec<StepGroup>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[cfg_attr(feature = "tauri", derive(specta::Type))]
pub struct IngredientGroup {
    /// `None` for the ungrouped list directly under `** Zutaten`.
    #[cfg_attr(feature = "tauri", specta(optional))]
    pub title: Option<String>,
    pub items: Vec<Ingredient>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[cfg_attr(feature = "tauri", derive(specta::Type))]
pub struct StepGroup {
    #[cfg_attr(feature = "tauri", specta(optional))]
    pub title: Option<String>,
    /// One entry per step, with the list marker (`1.`, `-`) stripped.
    pub steps: Vec<String>,
}
