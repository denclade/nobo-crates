//! A recipe from a web page, into a cookbook.
//!
//! Three steps, and each has its own module so that each can be tested
//! without the others:
//!
//! 1. **Reading** (`parse_page`): a page's HTML into an `ImportedRecipe`.
//!    Pure -- no network, no files -- so it is tested on saved pages. The
//!    generic reader (`schema_org`) does most of it; refiners
//!    (`sites`) fix what one site or plugin does differently.
//! 2. **Writing** (`to_org`, `insert`): an `ImportedRecipe` as an
//!    org-chef entry, placed in a cookbook file. Also pure.
//! 3. **Fetching** (`fetch`, behind the `import` feature): the page and
//!    its picture, over HTTP. The only part that talks to anything, and
//!    the only part a build without networking leaves out.
//!
//! The format written is org-chef's own, keys and section names
//! included, so a file filled from this app reads the same in Emacs.

// The page helpers live in the core, shared with the bookmarks; under
// their old name here so the readers below did not have to change.
pub use nobo_core::utils::html;
#[cfg(feature = "import")]
pub mod fetch;
mod schema_org;
mod sites;

use nobo_core::core::org_syntax::one_line;
use nobo_core::t;

/// A recipe as a page described it -- before it is anybody's org text.
///
/// Serialisable both ways: the preview hands it to the frontend, and the
/// frontend hands it back to be saved, so the page is fetched once and
/// what is saved is exactly what was shown.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "tauri", derive(specta::Type))]
pub struct ImportedRecipe {
    pub title: String,
    pub source_url: String,
    #[serde(default)]
    pub description: Option<String>,
    /// The picture's address on the web; it becomes a file in the vault
    /// only when the recipe is saved.
    #[serde(default)]
    pub image_url: Option<String>,
    #[serde(default)]
    pub servings: Option<String>,
    /// Minutes, as a bare number where the page gave a duration.
    #[serde(default)]
    pub prep_time: Option<String>,
    #[serde(default)]
    pub cook_time: Option<String>,
    #[serde(default)]
    pub ready_in: Option<String>,
    #[serde(default)]
    pub ingredients: Vec<Group>,
    #[serde(default)]
    pub steps: Vec<Group>,
    /// What the import itself has to say about the recipe -- that a site
    /// keeps its steps behind a login, say. Written as a section of its
    /// own so it reads as a remark, not as part of the recipe.
    #[serde(default)]
    pub notes: Vec<String>,
}

/// A titled run of lines: an ingredient group, a section of steps.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "tauri", derive(specta::Type))]
pub struct Group {
    #[serde(default)]
    pub title: Option<String>,
    pub lines: Vec<String>,
}

/// The page being read, as the refiners see it.
pub struct Page<'a> {
    pub url: &'a str,
    pub html: &'a str,
    host: String,
}

impl<'a> Page<'a> {
    pub fn new(url: &'a str, html: &'a str) -> Self {
        let host = url.split("://").nth(1).unwrap_or(url).split(['/', '?', '#', ':']).next().unwrap_or("").to_ascii_lowercase();
        Page { url, html, host }
    }

    /// Whether the host carries NAME as one of its labels -- `cookidoo`
    /// for cookidoo.de, cookidoo.at and www.cookidoo.ch alike.
    pub fn host_is(&self, name: &str) -> bool {
        self.host.split('.').any(|label| label == name)
    }
}

/// Whether URL is a recipe site this import knows by its address --
/// Chefkoch, Cookidoo: one a refiner recognises without seeing the page.
/// What a host asks before anything is fetched, to offer the import
/// first when a link from such a site is shared. A plugin known only by
/// its markup (WP Recipe Maker) cannot be told from the address, and is
/// not claimed.
pub fn known_site(url: &str) -> bool {
    let page = Page::new(url, "");
    sites::REFINERS.iter().any(|refiner| refiner.recognises(&page))
}

/// The recipe on a page, or why there is none.
pub fn parse_page(url: &str, html: &str) -> Result<ImportedRecipe, String> {
    let page = Page::new(url, html);
    let mut recipe = schema_org::read(url, &html::json_ld_blocks(html)).ok_or_else(|| {
        t!(
            "Auf dieser Seite wurde kein Rezept gefunden. Rezeptseiten beschreiben ihr Rezept meist maschinenlesbar -- diese nicht.",
            "No recipe found on this page. Recipe sites usually describe their recipe in a machine-readable way -- this one does not."
        )
    })?;
    if recipe.image_url.is_none() {
        // The picture a page shows when it is shared: nearly always the
        // dish, and there even where the recipe data has none.
        recipe.image_url = html::meta_content(html, "og:image").and_then(|src| html::absolute_url(url, &src));
    }
    for refiner in sites::REFINERS.iter().filter(|refiner| refiner.recognises(&page)) {
        log::info!("[import] {} refines {}", refiner.name(), page.url);
        refiner.refine(&page, &mut recipe);
    }
    Ok(recipe)
}

/// The recipe as an org-chef entry at LEVEL, with IMAGE_LINK (a path
/// relative to the cookbook file) under its drawer when there is a
/// picture.
pub fn to_org(recipe: &ImportedRecipe, level: usize, image_link: Option<&str>) -> String {
    let stars = "*".repeat(level.max(1));
    let mut out = format!("{stars} {}\n", one_line(&recipe.title));

    // org-chef's keys, lowercase, in org-chef's order.
    let properties: Vec<(&str, &Option<String>)> = vec![
        ("servings", &recipe.servings),
        ("prep-time", &recipe.prep_time),
        ("cook-time", &recipe.cook_time),
        ("ready-in", &recipe.ready_in),
    ];
    let mut drawer: Vec<(&str, &str)> = vec![("source-url", recipe.source_url.as_str())];
    drawer.extend(properties.into_iter().filter_map(|(key, value)| value.as_deref().map(|value| (key, value))));
    for line in nobo_core::core::org_syntax::drawer_lines(&drawer) {
        out.push_str(&line);
        out.push('\n');
    }

    if let Some(link) = image_link {
        out.push_str(&format!("[[file:{link}]]\n"));
    }
    if let Some(description) = &recipe.description {
        out.push('\n');
        out.push_str(&prose(description));
        out.push_str("\n\n");
    }

    section(&mut out, &stars, "Ingredients", &recipe.ingredients, |_, line| format!("- {line}"));
    section(&mut out, &stars, "Directions", &recipe.steps, |index, line| format!("{}. {line}", index + 1));
    if !recipe.notes.is_empty() {
        let heading = t!("Hinweise", "Notes");
        let notes = [Group { title: None, lines: recipe.notes.clone() }];
        section(&mut out, &stars, &heading, &notes, |_, line| format!("- {line}"));
    }
    out
}

fn section(out: &mut String, stars: &str, heading: &str, groups: &[Group], item: impl Fn(usize, &str) -> String) {
    if groups.is_empty() {
        return;
    }
    out.push_str(&format!("{stars}* {heading}\n"));
    for group in groups {
        if let Some(title) = &group.title {
            out.push_str(&format!("{stars}** {}\n", one_line(title)));
        }
        for (index, line) in group.lines.iter().enumerate() {
            out.push_str(&item(index, &one_line(line)));
            out.push('\n');
        }
    }
}

/// A paragraph that cannot be mistaken for structure: a line starting
/// with `* ` would be a heading, `#+` a keyword, `:END:` a drawer's end.
fn prose(text: &str) -> String {
    let line = one_line(text);
    if line.starts_with('*') || line.starts_with("#+") || line.starts_with(':') {
        format!(" {line}")
    } else {
        line
    }
}

/// TEXT with ENTRY added under the heading CATEGORY names -- the core's
/// (see `core::org_insert`), shared with the bookmarks.
pub use nobo_core::core::org_insert::insert_under as insert;

/// A file name for the picture: the recipe's title, made safe, plus the
/// extension the image type calls for.
pub fn image_filename(title: &str, extension: &str) -> String {
    let mut slug = String::new();
    for c in title.to_lowercase().chars() {
        let mapped = match c {
            'ä' => "ae".to_string(),
            'ö' => "oe".to_string(),
            'ü' => "ue".to_string(),
            'ß' => "ss".to_string(),
            c if c.is_alphanumeric() => c.to_string(),
            _ => "-".to_string(),
        };
        if mapped == "-" && (slug.is_empty() || slug.ends_with('-')) {
            continue;
        }
        slug.push_str(&mapped);
    }
    let slug: String = slug.trim_end_matches('-').chars().take(60).collect();
    let slug = if slug.is_empty() { "recipe".to_string() } else { slug };
    format!("{}.{extension}", slug.trim_end_matches('-'))
}

#[cfg(test)]
mod tests;
