//! Directories outside the vault that the user has explicitly allowed
//! tangling into.
//!
//! The vault is writable because the user chose it during setup; anything
//! else has to be chosen the same way, and this is where those choices are
//! remembered.
//!
//! **A prompt we draw ourselves grants nothing.** Under Flatpak this app
//! ships with no `--filesystem=` at all (see com.nobo.app.yml), so the
//! sandbox permits its own data directory and whatever a desktop portal
//! hands over at runtime -- nothing else. A modal reading "may nobo write
//! to /home/u/.bashrc?" would be answered yes and then fail with EACCES
//! there, while on an unpackaged build the very same yes would succeed.
//! Same dialog, two different meanings, and on the build where it works it
//! is the ONLY thing standing between a shared vault and `~/.bashrc`.
//!
//! So the ask is the operating system's own directory picker (see
//! `babel_grant_tangle_root` and the modal that calls it). The picking IS
//! the grant: on a sandboxed build the portal opens the directory to the
//! app, on an unsandboxed one the user has said where. That is the same
//! mechanism `commands_notes::export_file` has always relied on, and the
//! reason it works under Flatpak today.
//!
//! A directory rather than a single file, for three reasons: `write_atomic`
//! puts its temp file NEXT TO the target and renames, which a one-file
//! grant cannot do; tangling is usually several blocks into one project
//! folder, so a folder is what keeps the question to once; and
//! `permission_utils::allow_directory_access` already widens Tauri's own
//! scopes exactly this way for the vault.
//!
//! Stored per device in the config directory, next to `trusted_files.json`
//! -- deliberately NOT in the vault, which syncs: a filesystem grant is
//! true of one machine and must not travel to another.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

use nobo_core::config::settings::config_dir;
use nobo_core::utils::fs_utils::canonicalize_or_self;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct GrantEntry {
    /// Canonical at the time of granting -- see `is_granted` on what that
    /// does and does not promise later.
    directory: String,
    granted_at: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct GrantFile {
    #[serde(default)]
    directories: Vec<GrantEntry>,
}

fn store_path() -> PathBuf {
    config_dir().join("tangle_roots.json")
}

fn load(path: &Path) -> GrantFile {
    fs::read_to_string(path).ok().and_then(|content| serde_json::from_str(&content).ok()).unwrap_or_default()
}

fn save(path: &Path, store: &GrantFile) -> std::io::Result<()> {
    let json = serde_json::to_string_pretty(store).unwrap_or_else(|_| "{}".to_string());
    nobo_core::utils::fs_utils::write_atomic(path, &json)
}

/// Whether TARGET lies inside a directory the user has granted.
///
/// Compared on canonicalised paths so `..` and symlinks cannot be used to
/// leave a granted folder while looking like they stay in it. A granted
/// directory that no longer exists canonicalises to itself and simply
/// stops matching, which is the safe direction.
pub fn is_granted(target: &Path) -> bool {
    is_granted_at(&store_path(), target)
}

/// The store-path-explicit form, like `trust::is_trusted_at` and for the
/// same reason: a test may not write to the real config directory.
pub fn is_granted_at(store: &Path, target: &Path) -> bool {
    let target = canonicalize_or_self(target);
    load(store).directories.iter().any(|entry| is_inside(&target, Path::new(&entry.directory)))
}

/// Whether TARGET is DIRECTORY itself or below it.
///
/// `Path::starts_with` and not a string prefix: `/home/u/notes-backup`
/// starts with the STRING `/home/u/notes` without being inside it.
pub fn is_inside(target: &Path, directory: &Path) -> bool {
    canonicalize_or_self(target).starts_with(canonicalize_or_self(directory))
}

/// Remembers DIRECTORY as tangle-writable. Idempotent.
pub fn grant(directory: &Path) -> Result<(), String> {
    grant_at(&store_path(), directory)
}

pub fn grant_at(path: &Path, directory: &Path) -> Result<(), String> {
    let directory = canonicalize_or_self(directory);
    let mut store = load(path);
    let key = directory.to_string_lossy().into_owned();
    if store.directories.iter().any(|entry| entry.directory == key) {
        return Ok(());
    }
    store.directories.push(GrantEntry { directory: key, granted_at: chrono::Local::now().to_rfc3339() });
    save(path, &store).map_err(|e| e.to_string())
}

/// Every directory granted so far, for a settings screen to list and for
/// tests to read back.
pub fn granted() -> Vec<String> {
    load(&store_path()).directories.into_iter().map(|entry| entry.directory).collect()
}

/// Forgets DIRECTORY. A directory that was never granted is not an error
/// -- the caller is a settings list, and the answer to "it is gone" is
/// the same either way.
pub fn revoke(directory: &Path) -> Result<(), String> {
    revoke_at(&store_path(), directory)
}

pub fn revoke_at(path: &Path, directory: &Path) -> Result<(), String> {
    // Matched on the stored string AND on the canonical form: the list
    // hands back exactly what was stored, but a path typed or picked
    // again may differ by a symlink or a trailing separator.
    let raw = directory.to_string_lossy().into_owned();
    let canonical = canonicalize_or_self(directory).to_string_lossy().into_owned();
    let mut store = load(path);
    let before = store.directories.len();
    store.directories.retain(|entry| entry.directory != raw && entry.directory != canonical);
    if store.directories.len() == before {
        return Ok(());
    }
    save(path, &store).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// The check that keeps a grant for one folder from covering another
    /// whose name merely starts the same way.
    #[test]
    fn a_sibling_with_a_longer_name_is_not_inside() {
        let granted = PathBuf::from("/home/u/projekte");

        assert!(is_inside(Path::new("/home/u/projekte/x/setup.py"), &granted));
        assert!(is_inside(Path::new("/home/u/projekte"), &granted));
        assert!(!is_inside(Path::new("/home/u/projekte-alt/setup.py"), &granted));
        assert!(!is_inside(Path::new("/home/u/.bashrc"), &granted));
    }

    /// A folder is granted once and covers what is under it -- that is
    /// what makes the question a one-time one.
    #[test]
    fn a_granted_folder_covers_what_is_under_it_and_nothing_else() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = dir.path().join("tangle_roots.json");
        let projekte = dir.path().join("projekte");
        std::fs::create_dir_all(projekte.join("tief")).unwrap();

        assert!(!is_granted_at(&store, &projekte.join("setup.py")), "nothing is granted to begin with");

        grant_at(&store, &projekte).unwrap();

        assert!(is_granted_at(&store, &projekte.join("setup.py")));
        assert!(is_granted_at(&store, &projekte.join("tief/x.sh")), "and below it");
        assert!(!is_granted_at(&store, &dir.path().join(".bashrc")), "but not beside it");
    }

    /// Granting the same folder twice is one entry, not two.
    #[test]
    fn granting_the_same_folder_again_changes_nothing() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = dir.path().join("tangle_roots.json");

        grant_at(&store, dir.path()).unwrap();
        grant_at(&store, dir.path()).unwrap();

        assert_eq!(load(&store).directories.len(), 1);
    }

    /// Taking a grant back is what makes the list worth showing.
    #[test]
    fn revoking_stops_a_folder_from_being_granted() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = dir.path().join("tangle_roots.json");
        let projekte = dir.path().join("projekte");
        std::fs::create_dir_all(&projekte).unwrap();
        grant_at(&store, &projekte).unwrap();
        assert!(is_granted_at(&store, &projekte.join("setup.py")));

        revoke_at(&store, &projekte).unwrap();

        assert!(!is_granted_at(&store, &projekte.join("setup.py")));
        assert!(load(&store).directories.is_empty());
        // Doing it twice is not an error -- two windows, one list.
        revoke_at(&store, &projekte).unwrap();
    }

    /// `..` must not walk out of a granted folder -- the whole point of
    /// comparing canonicalised paths.
    #[test]
    fn dot_dot_cannot_leave_a_granted_folder() {
        let dir = tempfile::TempDir::new().unwrap();
        let granted = dir.path().join("projekte");
        std::fs::create_dir_all(&granted).unwrap();
        std::fs::write(dir.path().join("geheim.txt"), "x").unwrap();

        assert!(is_inside(&granted.join("setup.py"), &granted));
        assert!(!is_inside(&granted.join("../geheim.txt"), &granted));
    }
}
