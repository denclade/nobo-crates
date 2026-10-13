//! Paths and names: canonical forms, file names, what counts as hidden.

use std::fs;
use std::path::{Path, PathBuf};

/// Canonicalizes `dir`, falling back to it unchanged when that fails --
/// the directory does not exist, or the file was just deleted and its path
/// no longer resolves.
///
/// The index is canonical-keyed throughout, so comparing a raw path
/// against it silently matches nothing on any symlink, trailing-slash or
/// case difference -- invisible when reading the two paths side by side.
/// The index and search apply this to their own inputs rather than
/// trusting every caller to remember it.
pub fn canonicalize_or_self(dir: &Path) -> PathBuf {
    fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf())
}

/// Creates DIR if it does not exist yet, returning whether it had to.
///
/// The configured vault can be gone at startup for ordinary reasons -- an
/// external drive not mounted yet, a folder moved or removed, a synced
/// directory not restored on a fresh install. Recreating it beats starting
/// up against a path nothing can be read from or written to, where every
/// later operation fails one by one with no obvious cause.
pub fn ensure_dir_exists(dir: &Path) -> Result<bool, String> {
    if dir.is_dir() {
        return Ok(false);
    }
    fs::create_dir_all(dir).map(|()| true).map_err(|e| format!("failed to create {}: {e}", dir.display()))
}

/// Basename for a path, falling back to the full path if it has none
/// (shouldn't normally happen for a real file path, but History labels are
/// display-only, so this degrades gracefully instead of erroring). Generic
/// over `&Path`/`&str`/`&String` so every history-recording command site
/// can call it directly on whatever path type it already has in hand.
pub fn filename_of<P: AsRef<Path>>(path: P) -> String {
    let path = path.as_ref();
    path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_else(|| path.to_string_lossy().into_owned())
}

/// Whether NAME (a bare filename, no path) is a Unix-style dotfile --
/// this entry's *own* name, e.g. `.attachments` itself (see
/// `has_hidden_ancestor` for "lives inside a hidden dir" instead).
pub fn is_hidden(name: &str) -> bool {
    name.starts_with('.')
}

pub(super) fn is_hidden_component(c: std::path::Component) -> bool {
    c.as_os_str().to_str().is_some_and(|s| s.starts_with('.') && s != "." && s != "..")
}

/// Whether any component of RELATIVE_PATH *before its last* (the entry's own
/// name, checked separately via `is_hidden`) is a dotfile/dotdir -- e.g.
/// true for `.attachments/photo.png` even though `photo.png` isn't hidden
/// itself. RELATIVE_PATH must already be relative to the vault root (an
/// unrelated dot-component earlier in the absolute path -- e.g. a test's own
/// `TempDir` named `/tmp/.tmpXXXXXX` -- must not count).
pub fn has_hidden_ancestor(relative_path: &Path) -> bool {
    let mut components: Vec<_> = relative_path.components().collect();
    components.pop();
    components.into_iter().any(is_hidden_component)
}
