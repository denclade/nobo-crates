//! Marking a file as a favourite, declared in the file itself:
//!
//! ```org
//! #+NOBO_FAVORITE: t
//! ```
//!
//! Same reasoning as the file kind next door (see file_kind's module
//! comment): it survives the vault being synced to another device,
//! survives the file being moved or renamed, and is visible to a human
//! opening the vault in Emacs. Org ignores unknown `#+` keywords, so the
//! file stays valid either way.
//!
//! Deliberately NOT settings.json -- that is per device and not synced, so
//! a note favourited on the desktop would be unmarked on the phone. And
//! deliberately not a tag: tags are the user's own vocabulary and show up
//! in every tag list and Denote filename, which a UI marker has no
//! business doing.

use std::path::Path;

use crate::core::file_kind;

pub const FAVORITE_KEYWORD: &str = "#+NOBO_FAVORITE:";

/// Whether these lines declare the file a favourite. Any non-empty value
/// counts except an explicit negative: `t`, `true`, `yes` and a bare
/// `#+NOBO_FAVORITE:` all mean the same thing, because someone hand-editing
/// this in Emacs should not have to guess the spelling.
pub fn is_favorite<S: AsRef<str>>(lines: &[S]) -> bool {
    match file_kind::leading_keyword(lines, FAVORITE_KEYWORD) {
        None => false,
        Some(value) => !matches!(value.trim().to_ascii_lowercase().as_str(), "nil" | "false" | "no" | "0"),
    }
}

/// The same question asked of a whole file body.
///
/// Truncates to the header itself. The caller used to materialise
/// `text.lines().collect::<Vec<_>>()` over the ENTIRE body just to hand
/// the first sixteen lines to `is_favorite`, once per org file in the
/// vault, under the index read lock -- for `favorite_files` and every
/// recursive directory listing. Doing the cut here means no caller can
/// get the limit wrong.
pub fn is_favorite_text(text: &str) -> bool {
    is_favorite(&text.lines().take(file_kind::SCAN_LINES).collect::<Vec<_>>())
}

/// Sets or clears the declaration, leaving everything else alone -- see
/// file_kind::set_leading_keyword, which every header declaration in the
/// app goes through.
pub fn set(path: &Path, favorite: bool) -> Result<(), String> {
    file_kind::set_leading_keyword(path, FAVORITE_KEYWORD, favorite.then_some("t"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    fn file(content: &str) -> NamedTempFile {
        let file = NamedTempFile::with_suffix(".org").unwrap();
        std::fs::write(file.path(), content).unwrap();
        file
    }

    fn content(file: &NamedTempFile) -> String {
        std::fs::read_to_string(file.path()).unwrap()
    }

    #[test]
    fn reads_the_declaration_whatever_case_or_wording_it_has() {
        assert!(is_favorite(&["#+NOBO_FAVORITE: t"]));
        assert!(is_favorite(&["#+nobo_favorite: true"]));
        assert!(is_favorite(&["#+NOBO_FAVORITE:"]), "a bare keyword still means yes");
        assert!(!is_favorite(&["#+TITLE: Notiz"]));
        assert!(!is_favorite::<String>(&[]));
    }

    /// So that unfavouriting by hand in Emacs works the way an Emacs user
    /// would write it.
    #[test]
    fn an_explicit_negative_is_not_a_favorite() {
        for value in ["nil", "false", "no", "0"] {
            assert!(!is_favorite(&[format!("#+NOBO_FAVORITE: {value}")]), "{value}");
        }
    }

    #[test]
    fn marks_a_note_below_its_existing_keywords() {
        let file = file("#+TITLE: Notiz\n#+IDENTIFIER: 20240101T101010\n\n* Erste Überschrift\nText.\n");

        set(file.path(), true).unwrap();

        assert_eq!(
            content(&file),
            "#+TITLE: Notiz\n#+IDENTIFIER: 20240101T101010\n#+NOBO_FAVORITE: t\n\n* Erste Überschrift\nText.\n",
        );
    }

    #[test]
    fn marks_a_file_that_has_no_header_at_all() {
        let file = file("* Nur eine Überschrift\n");

        set(file.path(), true).unwrap();

        assert_eq!(content(&file), "#+NOBO_FAVORITE: t\n* Nur eine Überschrift\n");
    }

    #[test]
    fn unmarking_removes_the_line_and_nothing_else() {
        let file = file("#+TITLE: Notiz\n#+NOBO_FAVORITE: t\n\n* Überschrift\n");

        set(file.path(), false).unwrap();

        assert_eq!(content(&file), "#+TITLE: Notiz\n\n* Überschrift\n");
    }

    /// Toggling twice has to leave the file byte-identical -- otherwise
    /// every favourite click would show up as a change to sync and a diff
    /// in Emacs.
    #[test]
    fn a_round_trip_leaves_the_file_exactly_as_it_was() {
        let original = "#+TITLE: Notiz\n#+FILETAGS: :arbeit:\n\n* Überschrift\nText.\n";
        let file = file(original);

        set(file.path(), true).unwrap();
        set(file.path(), false).unwrap();

        assert_eq!(content(&file), original);
    }

    /// What the sidebar reads: the mark comes off the file's own text, so
    /// a file marked by hand in Emacs shows up as a favourite too.
    #[test]
    fn what_was_written_is_what_gets_read_back() {
        let file = file("#+TITLE: Notiz\n\n* Überschrift\n");
        let lines_of = || std::fs::read_to_string(file.path()).unwrap().lines().map(String::from).collect::<Vec<_>>();

        assert!(!is_favorite(&lines_of()));
        set(file.path(), true).unwrap();
        assert!(is_favorite(&lines_of()));
        set(file.path(), false).unwrap();
        assert!(!is_favorite(&lines_of()));
    }

    #[test]
    fn setting_what_is_already_set_writes_nothing() {
        let file = file("#+NOBO_FAVORITE: t\n* Überschrift\n");
        let before = std::fs::metadata(file.path()).unwrap().modified().unwrap();

        set(file.path(), true).unwrap();

        assert_eq!(std::fs::metadata(file.path()).unwrap().modified().unwrap(), before);
    }
}
