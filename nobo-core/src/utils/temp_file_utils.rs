use crate::core::models::NoteLoadResult;
use crate::utils::ToCmdResult;
use std::path::Path;

pub fn temp_path_for(path: &str) -> String {
    let p = Path::new(path);
    let parent = p.parent().unwrap_or(Path::new(""));
    let file_name = p.file_name().unwrap_or_default().to_string_lossy();
    parent.join(format!("#{}#", file_name)).to_string_lossy().into_owned()
}

/// Whether a bare filename is nobo's own Emacs-style autosave temp file
/// (see temp_path_for, e.g. "#note.org#" for "note.org") -- these are
/// working files nobo creates and cleans up itself, never meant to show up
/// in the file browser (see fs_utils::get_dir_entries).
pub fn is_temp_file(name: &str) -> bool {
    (name.len() > 2 && name.starts_with('#') && name.ends_with('#'))
        // Emacs lock file (created while a buffer is open, e.g. ".#note.org")
        // -- typically a symlink to "user@host.pid", not real file content.
        // Distinct from nobo's own "#name#" convention above.
        || (name.len() > 2 && name.starts_with(".#"))
}

pub fn load_with_temp_check(path: &str) -> Result<NoteLoadResult, String> {
    if !Path::new(path).exists() {
        return Err(crate::t!("Datei existiert nicht", "The file does not exist"));
    }

    let original_content = std::fs::read_to_string(path).to_cmd()?;
    let temp_path = temp_path_for(path);

    let mut temp_content = None;
    let mut has_temp = false;

    if Path::new(&temp_path).exists() {
        if let Ok(tc) = std::fs::read_to_string(&temp_path) {
            if tc != original_content {
                temp_content = Some(tc);
                has_temp = true;
            } else {
                let _ = std::fs::remove_file(&temp_path);
            }
        }
    }

    Ok(NoteLoadResult {
        content: temp_content.clone().unwrap_or_else(|| original_content.clone()),
        original_content,
        has_temp,
        temp_content,
    })
}

/// Autosaves the draft beside the note.
///
/// Atomic like `save_final`, and for a sharper reason: this is the copy
/// that exists precisely because the real one has not been written yet.
/// The moment it protects against -- an app killed while backgrounded --
/// is the same moment it is most likely to be running.
pub fn write_temp(path: &str, content: &str) -> Result<(), String> {
    crate::utils::fs_utils::write_atomic(Path::new(&temp_path_for(path)), content).to_cmd()
}

pub fn remove_temp_if_exists(path: &str) -> Result<(), String> {
    let temp_path = temp_path_for(path);
    if Path::new(&temp_path).exists() {
        std::fs::remove_file(temp_path).to_cmd()?;
    }
    Ok(())
}

/// The editor's own save -- the write this app does most often and the one
/// with the most to lose, since it replaces a note the user already has.
///
/// Goes through `fs_utils::write_atomic` for that reason: a truncate-then
/// -fill here means a note that is empty or cut in half if the app does
/// not survive the next few milliseconds, which on Android is a thing the
/// OS does on purpose to backgrounded apps.
///
/// The autosave temp file next to it (`write_temp`) deliberately does NOT
/// pay that price: it is written every couple of seconds while typing, and
/// a torn copy of a recovery file whose original is still intact costs
/// nothing worth an fsync per keystroke-pause.
pub fn save_final(path: &str, content: &str) -> Result<(), String> {
    crate::utils::fs_utils::write_atomic(Path::new(path), content).to_cmd()?;
    remove_temp_if_exists(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn temp_path_uses_emacs_convention() {
        assert_eq!(temp_path_for("/foo/bar.org"), "/foo/#bar.org#");
    }

    #[test]
    fn is_temp_file_matches_own_convention() {
        assert!(is_temp_file("#bar.org#"));
        assert!(!is_temp_file("bar.org"));
        assert!(!is_temp_file("#bar.org"));
        assert!(!is_temp_file("bar.org#"));
        assert!(!is_temp_file("#"));
        assert!(!is_temp_file("##"));
    }

    #[test]
    fn is_temp_file_matches_emacs_lock_file_convention() {
        assert!(is_temp_file(".#bar.org"));
        assert!(is_temp_file(
            ".#20250912T110310--abteilungssitzung-12092025__konferenz_schule.org"
        ));
        assert!(!is_temp_file(".bar.org"));
        assert!(!is_temp_file(".#"));
    }

    #[test]
    fn load_without_temp_file_returns_original_only() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("note.org");
        std::fs::write(&file_path, "hello").unwrap();

        let result = load_with_temp_check(file_path.to_str().unwrap()).unwrap();
        assert_eq!(result.content, "hello");
        assert!(!result.has_temp);
    }

    #[test]
    fn identical_temp_file_gets_cleaned_up() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("note.org");
        std::fs::write(&file_path, "hello").unwrap();
        write_temp(file_path.to_str().unwrap(), "hello").unwrap();

        let result = load_with_temp_check(file_path.to_str().unwrap()).unwrap();
        assert!(!result.has_temp);
        assert!(!Path::new(&temp_path_for(file_path.to_str().unwrap())).exists());
    }

    #[test]
    fn differing_temp_file_is_reported() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("note.org");
        std::fs::write(&file_path, "hello").unwrap();
        write_temp(file_path.to_str().unwrap(), "draft version").unwrap();

        let result = load_with_temp_check(file_path.to_str().unwrap()).unwrap();
        assert!(result.has_temp);
        assert_eq!(result.temp_content, Some("draft version".to_string()));
    }
}