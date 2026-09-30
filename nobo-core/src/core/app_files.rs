//! The files the app keeps for itself, and the one rule about them: they
//! are never renamed, moved or deleted as a side effect.
//!
//! The app finds its projects, actions, inbox and someday lists -- and
//! everything under `.nobo/` -- by NAME. A file that moves is, as far as
//! the app can tell, a file that is gone: the Projects view comes up
//! empty, and the next capture starts a fresh projects.org beside the old
//! one, which then sits unseen in a hidden folder.
//!
//! That is exactly what happened. The notes editor promoted every plain
//! `.org` file it saved into a Denote note, and promoting RENAMES the file
//! to `<identifier>--<title>.org`. Opening `.nobo/projects.org` in the
//! editor and saving once was enough to make every project disappear.
//!
//! So this is asked by every command that renames, moves, promotes or
//! deletes a whole file -- in the command layer both frontends go through,
//! not only in a UI that happens to hide the button.

use std::path::{Path, PathBuf};

use crate::config::settings::Config;
use crate::utils::fs_utils;

/// Whether PATH is a file the app keeps for itself -- see the module doc.
///
/// Two ways to be one, and either is enough:
///
/// - it lives in a hidden folder of the vault (`.nobo/…`), where every
///   file is the app's own: the GTD lists at their default paths, the
///   CalDAV and calendar mirrors, the archive, the review state;
/// - it is one of the four configured GTD files, wherever the user has
///   put them -- the settings allow a projects.org outside `.nobo/`.
///
/// Compared as canonical paths. The configured ones are stored as the
/// folder picker produced them, often relative to the vault, while PATH
/// comes from a scan that canonicalizes its root; comparing the strings
/// as they stand is a mismatch this codebase has hit more than once.
pub fn is_app_owned(path: &Path, config: &Config) -> bool {
    let Ok(root) = config.vault_root() else { return false };
    let root = fs_utils::canonicalize_or_self(&root);
    let path = fs_utils::canonicalize_or_self(path);

    if let Ok(relative) = path.strip_prefix(&root) {
        // The file's own name counts too: a dotfile at the vault's top
        // level is just as much the app's as one inside `.nobo/`.
        let own_name_hidden = relative.file_name().and_then(|n| n.to_str()).is_some_and(fs_utils::is_hidden);
        if fs_utils::has_hidden_ancestor(relative) || own_name_hidden {
            return true;
        }
    }

    configured_gtd_files(config, &root).iter().any(|gtd| *gtd == path)
}

fn configured_gtd_files(config: &Config, root: &Path) -> Vec<PathBuf> {
    [&config.inbox_file_path, &config.actions_file_path, &config.projects_file_path, &config.someday_file_path]
        .iter()
        .filter(|p| !p.is_empty())
        .map(|p| {
            let p = Path::new(p.as_str());
            let absolute = if p.is_absolute() { p.to_path_buf() } else { root.join(p) };
            fs_utils::canonicalize_or_self(&absolute)
        })
        .collect()
}

/// Refuses ACTION on PATH when it is one of the app's own files.
///
/// ACTION is the verb for the message ("umbenennen" / "rename"). Without
/// a configuration there is no vault to protect and nothing is refused --
/// the command then fails on its own terms, as it always did.
pub fn refuse_if_app_owned(path: &Path, action_de: &str, action_en: &str) -> Result<(), String> {
    let Some(config) = Config::load() else { return Ok(()) };
    refuse_for(path, &config, action_de, action_en)
}

fn refuse_for(path: &Path, config: &Config, action_de: &str, action_en: &str) -> Result<(), String> {
    if is_app_owned(path, config) {
        let name = fs_utils::filename_of(path);
        return Err(crate::t!(
            "\"{name}\" gehört der App -- Projekte, Aufgaben, Eingang und alles unter .nobo/ werden über ihren Namen gefunden. Die Datei lässt sich deshalb nicht {action_de}; ihr Inhalt lässt sich ganz normal bearbeiten.",
            "\"{name}\" belongs to the app -- projects, actions, inbox and everything under .nobo/ are found by their name, so this file cannot be {action_en}. Its content can be edited as usual."
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn vault() -> (TempDir, Config) {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join(".nobo")).unwrap();
        std::fs::create_dir_all(dir.path().join("arbeit")).unwrap();
        for f in [".nobo/projects.org", ".nobo/actions.org", "arbeit/notiz.org", "eigene-projekte.org"] {
            std::fs::write(dir.path().join(f), "* x\n").unwrap();
        }
        let mut config = Config::default();
        config.notes_path = dir.path().to_string_lossy().into_owned();
        (dir, config)
    }

    /// The case that lost a user's projects: the default projects.org.
    #[test]
    fn everything_under_the_hidden_folder_is_the_apps() {
        let (dir, config) = vault();
        assert!(is_app_owned(&dir.path().join(".nobo/projects.org"), &config));
        assert!(is_app_owned(&dir.path().join(".nobo/actions.org"), &config));
    }

    #[test]
    fn an_ordinary_note_is_not() {
        let (dir, config) = vault();
        assert!(!is_app_owned(&dir.path().join("arbeit/notiz.org"), &config));
    }

    /// The settings let projects.org live anywhere in the vault; moving it
    /// out of `.nobo/` must not move it out of the protection.
    #[test]
    fn a_configured_gtd_file_is_the_apps_wherever_it_lives() {
        let (dir, mut config) = vault();
        config.projects_file_path = "eigene-projekte.org".into();
        assert!(is_app_owned(&dir.path().join("eigene-projekte.org"), &config));
    }

    /// The refusal names the file and says what still works -- a bare
    /// "not allowed" would read as the file being broken.
    #[test]
    fn the_refusal_says_which_file_and_what_is_still_possible() {
        let (dir, config) = vault();
        let err = refuse_for(&dir.path().join(".nobo/projects.org"), &config, "umbenennen", "renamed").unwrap_err();
        assert!(err.contains("projects.org"), "{err}");
        assert!(refuse_for(&dir.path().join("arbeit/notiz.org"), &config, "umbenennen", "renamed").is_ok());
    }

    /// Stored relative, as the folder picker writes it, and asked with the
    /// absolute path the scan reports -- the mismatch this compares around.
    #[test]
    fn relative_and_absolute_spellings_of_one_file_agree() {
        let (dir, mut config) = vault();
        config.projects_file_path = "./eigene-projekte.org".into();
        assert!(is_app_owned(&dir.path().join("eigene-projekte.org"), &config));
    }
}
