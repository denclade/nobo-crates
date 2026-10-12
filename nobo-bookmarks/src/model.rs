//! What a bookmark is, as the app hands it around.

/// One bookmark: a heading with a `:URL:` in a bookmarks file.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "tauri", derive(specta::Type))]
pub struct Bookmark {
    /// What names it in the app across edits -- its file and its address
    /// (and which one, should the file hold that address twice). Lines
    /// move; this does not.
    pub key: String,
    /// A fingerprint of its lines as they were read (see `Target`).
    pub revision: String,
    /// Where it lives -- with `line_no` and `url`, what an edit names it by
    /// (see `Target`).
    pub file_path: String,
    #[cfg_attr(feature = "tauri", specta(type = specta_typescript::Number))]
    pub line_no: usize,
    pub title: String,
    pub url: String,
    /// The site, for showing and for `site:` -- `heise.de`.
    pub host: String,
    /// Its own tags, `readlater` not among them.
    pub tags: Vec<String>,
    /// The tags of the collections above it, which org lets it inherit.
    pub inherited_tags: Vec<String>,
    pub read_later: bool,
    /// `:CREATED:` as written, `[2026-10-12 Mo 14:30]`.
    #[cfg_attr(feature = "tauri", specta(optional))]
    pub created: Option<String>,
    /// `:UPDATED:`, when it was last changed -- set by every edit.
    #[cfg_attr(feature = "tauri", specta(optional))]
    pub updated: Option<String>,
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
    /// The bookmark's `revision` as the user saw it. An edit to an entry
    /// that has changed since -- in Emacs, by a sync -- is refused rather
    /// than written over it. Empty: not checked.
    #[serde(default)]
    pub revision: String,
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
    /// The page's description; an empty one removes it.
    #[serde(default)]
    #[cfg_attr(feature = "tauri", specta(optional))]
    pub description: Option<String>,
}

/// Where a write left a bookmark.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "tauri", derive(specta::Type))]
pub struct Located {
    /// Its key after the write (see `Bookmark::key`).
    pub key: String,
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

/// One collection: a heading that is no bookmark, in a bookmarks file.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[cfg_attr(feature = "tauri", derive(specta::Type))]
pub struct Collection {
    pub file_path: String,
    /// Its titles, outermost first: `["Rust", "Async"]`.
    pub path: Vec<String>,
    /// Its heading's line (1-based) -- to open the file there.
    #[cfg_attr(feature = "tauri", specta(type = specta_typescript::Number))]
    pub line_no: usize,
}
