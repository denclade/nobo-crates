//! What a bookmark is, as the app hands it around.

/// One bookmark: a heading with a `:URL:` in a bookmarks file.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "tauri", derive(specta::Type))]
pub struct Bookmark {
    /// Where it lives -- with `line_no` and `url`, what an edit names it by
    /// (see `Target`).
    pub file_path: String,
    #[cfg_attr(feature = "tauri", specta(type = specta_typescript::Number))]
    pub line_no: usize,
    pub title: String,
    pub url: String,
    /// The site, for showing and for `site:` -- `docs.rs`.
    pub host: String,
    /// Its own tags, `readlater` not among them.
    pub tags: Vec<String>,
    /// The tags of the collections above it, which org lets it inherit.
    pub inherited_tags: Vec<String>,
    pub read_later: bool,
    /// `:CREATED:` as written, `[2026-10-12 Mo 14:30]`.
    #[cfg_attr(feature = "tauri", specta(optional))]
    pub created: Option<String>,
    /// The user's own words under it.
    pub note: String,
    /// What the page said about itself, in the quote block under the note.
    #[cfg_attr(feature = "tauri", specta(optional))]
    pub description: Option<String>,
    /// The headings above it that are no bookmarks, outermost first.
    pub collection: Vec<String>,
    /// The file it is in, by its title: "Arbeit", "Privat".
    pub category: String,
}

/// What names a bookmark for an edit: where it was, and what it pointed
/// to. A file can change between reading and writing -- in Emacs, on
/// another device -- so the line alone is not enough to be sure it is
/// still the same bookmark (see write::locate).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "tauri", derive(specta::Type))]
pub struct Target {
    pub file_path: String,
    #[cfg_attr(feature = "tauri", specta(type = specta_typescript::Number))]
    pub line_no: usize,
    pub url: String,
}

/// A new bookmark, as the save dialog has it.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "tauri", derive(specta::Type))]
pub struct Draft {
    pub title: String,
    pub url: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub read_later: bool,
    #[serde(default)]
    pub note: String,
    #[serde(default)]
    #[cfg_attr(feature = "tauri", specta(optional))]
    pub description: Option<String>,
}

/// What an edit changes; whatever is `None` stays as it is.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "tauri", derive(specta::Type))]
pub struct Patch {
    #[serde(default)]
    #[cfg_attr(feature = "tauri", specta(optional))]
    pub title: Option<String>,
    #[serde(default)]
    #[cfg_attr(feature = "tauri", specta(optional))]
    pub url: Option<String>,
    #[serde(default)]
    #[cfg_attr(feature = "tauri", specta(optional))]
    pub tags: Option<Vec<String>>,
    #[serde(default)]
    #[cfg_attr(feature = "tauri", specta(optional))]
    pub read_later: Option<bool>,
    #[serde(default)]
    #[cfg_attr(feature = "tauri", specta(optional))]
    pub note: Option<String>,
}

/// Where a write left a bookmark.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "tauri", derive(specta::Type))]
pub struct Located {
    pub file_path: String,
    #[cfg_attr(feature = "tauri", specta(type = specta_typescript::Number))]
    pub line_no: usize,
}

/// What a page says about itself, before anything is saved.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "tauri", derive(specta::Type))]
pub struct Preview {
    pub url: String,
    pub host: String,
    #[cfg_attr(feature = "tauri", specta(optional))]
    pub title: Option<String>,
    #[cfg_attr(feature = "tauri", specta(optional))]
    pub description: Option<String>,
}

/// One org file that is -- or could be -- a bookmarks file, i.e. a
/// category.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[cfg_attr(feature = "tauri", derive(specta::Type))]
pub struct BookmarkFile {
    pub path: String,
    /// The note's own title, or the file name.
    pub title: String,
    pub is_bookmarks: bool,
}
