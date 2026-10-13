//! The tests of the file system helpers, as they were beside the code.

use std::path::{Path, PathBuf};
use std::{fs, io};

use super::write::create_temp_beside;
use super::*;

mod file_kind_write_guard_tests {
    use super::*;
    use crate::core::file_kind::{FileKind, FileKindSpec, StateSupport};
    use tempfile::TempDir;

    /// Kinds of this module's own, registered the way a feature registers
    /// one (see core::file_kind). The guard is a rule about capabilities,
    /// not about anybody's calendar -- and this layer must not know that
    /// features exist (tools/check-layers-rust.mjs).
    const GENERATED: FileKind = FileKind::new("test-generated");
    const DECLARED_BUT_EDITABLE: FileKind = FileKind::new("test-editable");

    inventory::submit! {
        FileKindSpec {
            kind: GENERATED,
            served_by: None,
            editable: false,
            states: StateSupport::None,
            reorderable: false,
            can_add_headings: false,
        }
    }

    inventory::submit! {
        FileKindSpec {
            kind: DECLARED_BUT_EDITABLE,
            served_by: None,
            editable: true,
            states: StateSupport::DoneOnly,
            reorderable: true,
            can_add_headings: true,
        }
    }

    /// The half `write_string` never had. Whole-content writers -- saving
    /// an editor buffer, rewriting links across the vault -- used to reach
    /// past the guard entirely.
    #[test]
    fn write_atomic_checked_refuses_a_generated_file() {
        let tmp = TempDir::new().unwrap();
        let file = write_mirror(&tmp, "kalender.org");

        let err = write_atomic_checked(&file, "* Von Hand\n").unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert!(fs::read_to_string(&file).unwrap().contains("* Termin"));
    }

    /// Restore and the mirror generators write past the guard on purpose,
    /// so the raw entry point has to stay raw.
    #[test]
    fn write_atomic_itself_stays_unguarded() {
        let tmp = TempDir::new().unwrap();
        let file = write_mirror(&tmp, "kalender.org");

        write_atomic(&file, "regenerated\n").unwrap();

        assert_eq!(fs::read_to_string(&file).unwrap(), "regenerated\n");
    }

    /// The declaration travels with the file, so this holds wherever the
    /// file sits -- a path convention would not survive a move.
    fn write_mirror(dir: &TempDir, name: &str) -> std::path::PathBuf {
        let file = dir.path().join(name);
        std::fs::write(&file, format!("#+TITLE: X\n{}\n\n* Termin\n", GENERATED.declaration())).unwrap();
        file
    }

    /// The rule, at the only place it is actually enforced: every editing
    /// path in the app ends up in write_lines.
    #[test]
    fn writing_to_a_file_declared_uneditable_is_refused() {
        let dir = TempDir::new().unwrap();
        let file = write_mirror(&dir, "anywhere.org");
        let before = std::fs::read_to_string(&file).unwrap();

        let err = write_lines(&file, &["* Geändert".to_string()]).unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), before, "content must be untouched");
    }

    #[test]
    fn splice_insert_and_append_are_refused_too_not_just_write_lines() {
        let dir = TempDir::new().unwrap();
        let file = write_mirror(&dir, "work.org");
        let before = std::fs::read_to_string(&file).unwrap();

        assert!(splice_lines(&file, 0, 1, vec!["x".to_string()]).is_err());
        assert!(insert_lines_after(&file, 0, vec!["x".to_string()]).is_err());
        assert!(append_lines(&file, vec!["x".to_string()]).is_err());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), before);
    }

    /// `remove_lines` too, which is the one that could have gone
    /// unnoticed: it writes through the same door but takes a different
    /// route to it.
    #[test]
    fn removing_lines_from_an_uneditable_file_is_refused() {
        let dir = TempDir::new().unwrap();
        let file = write_mirror(&dir, "termine.org");
        let before = std::fs::read_to_string(&file).unwrap();

        assert!(remove_lines(&file, 1, 1).is_err());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), before);
    }

    /// The loophole the guard has to keep closed: asking the LINES BEING
    /// WRITTEN instead of the file would let a write that drops the
    /// declaration through -- and that write is exactly the one that
    /// destroys a mirror. The helpers ask before they change anything, so
    /// a splice that removes the first line is still refused.
    #[test]
    fn a_write_that_strips_the_declaration_is_still_refused() {
        let dir = TempDir::new().unwrap();
        let file = write_mirror(&dir, "spiegel.org");
        let before = std::fs::read_to_string(&file).unwrap();

        assert!(splice_lines(&file, 0, 1, vec!["* Jetzt gehört sie mir".to_string()]).is_err());
        assert!(write_lines(&file, &["* Jetzt gehört sie mir".to_string()]).is_err());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), before);
    }

    /// A file may declare a kind and still be editable -- the guard must
    /// not lump every declared kind together.
    #[test]
    fn a_declared_but_editable_kind_still_writes() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("einkauf.org");
        std::fs::write(&file, format!("{}\n", DECLARED_BUT_EDITABLE.declaration())).unwrap();

        write_lines(&file, &["* TODO Milch".to_string()]).unwrap();

        assert!(std::fs::read_to_string(&file).unwrap().contains("Milch"));
    }

    #[test]
    fn an_ordinary_vault_file_still_writes() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join(".nobo").join("actions.org");

        write_lines(&file, &["* TODO A".to_string()]).unwrap();

        assert!(std::fs::read_to_string(&file).unwrap().contains("* TODO A"));
    }
}

mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_file(dir: &Path, name: &str, content: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, content).unwrap();
        path
    }

    /* --- get_dir_entries --- */

    #[test]
    fn lists_files_and_dirs_in_top_level_only() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "a.txt", "x");
        fs::create_dir(tmp.path().join("subdir")).unwrap();
        write_file(&tmp.path().join("subdir"), "nested.txt", "y");

        let entries = get_dir_entries(tmp.path());
        let names: Vec<&str> = entries.iter().map(|(_, n, _)| n.as_str()).collect();

        assert!(names.contains(&"a.txt"));
        assert!(names.contains(&"subdir"));
        assert!(!names.contains(&"nested.txt")); // max_depth(1) - keine Rekursion
    }

    #[test]
    fn dirs_are_sorted_before_files_alphabetically() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "b.txt", "x");
        write_file(tmp.path(), "a.txt", "x");
        fs::create_dir(tmp.path().join("z_dir")).unwrap();

        let entries = get_dir_entries(tmp.path());
        // Dirs (is_dir=true) zuerst, dann Files, jeweils alphabetisch.
        assert_eq!(entries[0].1, "z_dir");
        assert!(entries[0].2);
        assert_eq!(entries[1].1, "a.txt");
        assert_eq!(entries[2].1, "b.txt");
    }

    #[test]
    fn empty_directory_returns_empty_list() {
        let tmp = TempDir::new().unwrap();
        assert!(get_dir_entries(tmp.path()).is_empty());
    }

    #[test]
    fn temp_files_are_excluded() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "#note.org#", "draft");
        write_file(tmp.path(), "note.org", "final");

        let entries = get_dir_entries(tmp.path());
        let names: Vec<&str> = entries.iter().map(|(_, n, _)| n.as_str()).collect();

        assert!(!names.contains(&"#note.org#"));
        assert!(names.contains(&"note.org"));
    }

    #[test]
    fn hidden_files_are_excluded() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), ".hidden", "x");
        write_file(tmp.path(), "visible.txt", "x");

        let entries = get_dir_entries(tmp.path());
        let names: Vec<&str> = entries.iter().map(|(_, n, _)| n.as_str()).collect();

        assert!(!names.contains(&".hidden"));
        assert!(names.contains(&"visible.txt"));
    }

    /* --- get_file_entries_recursive --- */

    #[test]
    fn recursive_listing_includes_nested_files_and_dirs() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "top.txt", "x");
        fs::create_dir(tmp.path().join("sub")).unwrap();
        write_file(&tmp.path().join("sub"), "nested.txt", "y");
        fs::create_dir(tmp.path().join("sub/deeper")).unwrap();
        write_file(&tmp.path().join("sub/deeper"), "deep.txt", "z");

        let entries = get_file_entries_recursive(tmp.path());
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();

        assert!(names.contains(&"top.txt"));
        assert!(names.contains(&"sub"));
        assert!(names.contains(&"nested.txt"));
        assert!(names.contains(&"deeper"));
        assert!(names.contains(&"deep.txt"));
    }

    #[test]
    fn recursive_listing_reads_modified_at_metadata() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "a.txt", "x");

        let entries = get_file_entries_recursive(tmp.path());
        let entry = entries.iter().find(|e| e.name == "a.txt").unwrap();

        assert!(entry.modified_at.is_some());
    }

    #[test]
    fn recursive_listing_excludes_temp_files() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "#draft.org#", "x");
        write_file(tmp.path(), "note.org", "y");

        let entries = get_file_entries_recursive(tmp.path());
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();

        assert!(!names.contains(&"#draft.org#"));
        assert!(names.contains(&"note.org"));
    }

    /* --- build_file_entry --- */

    #[test]
    fn build_file_entry_reads_path_name_and_kind() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), "a.org", "x");

        let entry = build_file_entry(&path, Some(tmp.path())).unwrap();

        assert_eq!(entry.name, "a.org");
        assert!(!entry.is_dir);
        assert!(!entry.is_hidden);
        assert!(!entry.is_in_hidden_dir);
        assert!(entry.modified_at.is_some());
    }

    #[test]
    fn build_file_entry_none_for_missing_path() {
        let tmp = TempDir::new().unwrap();
        assert!(build_file_entry(&tmp.path().join("missing.org"), Some(tmp.path())).is_none());
    }

    #[test]
    fn build_file_entry_flags_own_hidden_name() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), ".hidden", "x");

        let entry = build_file_entry(&path, Some(tmp.path())).unwrap();

        assert!(entry.is_hidden);
        assert!(!entry.is_in_hidden_dir);
    }

    #[test]
    fn build_file_entry_flags_hidden_ancestor_relative_to_root() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join(".attachments")).unwrap();
        let path = write_file(&tmp.path().join(".attachments"), "photo.png", "pixels");

        // TempDir's own path is itself named `/tmp/.tmpXXXXXX` (dot-prefixed) --
        // that must NOT by itself count; only `.attachments`, which is hidden
        // *relative to the given root*, should.
        let entry = build_file_entry(&path, Some(tmp.path())).unwrap();

        assert!(!entry.is_hidden);
        assert!(entry.is_in_hidden_dir);
    }

    #[test]
    fn build_file_entry_treats_unknown_root_as_not_in_hidden_dir() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join(".attachments")).unwrap();
        let path = write_file(&tmp.path().join(".attachments"), "photo.png", "pixels");

        let entry = build_file_entry(&path, None).unwrap();

        assert!(!entry.is_in_hidden_dir);
    }

    /* --- is_hidden --- */

    #[test]
    fn is_hidden_matches_dotfiles_only() {
        assert!(is_hidden(".hidden"));
        assert!(!is_hidden("visible.txt"));
    }

    /* --- has_hidden_ancestor --- */

    #[test]
    fn has_hidden_ancestor_catches_a_hidden_ancestor_dir() {
        assert!(has_hidden_ancestor(Path::new("notes/.attachments/photo.png")));
        assert!(!has_hidden_ancestor(Path::new("notes/sub/photo.png")));
    }

    #[test]
    fn has_hidden_ancestor_ignores_the_entrys_own_hidden_name() {
        // ".hidden" is the entry itself, not an ancestor -- see `is_hidden`
        // for that case.
        assert!(!has_hidden_ancestor(Path::new("notes/.hidden")));
    }

    #[test]
    fn has_hidden_ancestor_ignores_dot_and_dotdot() {
        assert!(!has_hidden_ancestor(Path::new("./notes/../notes/photo.png")));
    }

    /* --- place_attachment / unique_path_in --- */

    #[test]
    fn place_attachment_creates_missing_dir_and_writes_file() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".attachments");

        let result = place_attachment(&dir, "photo.png", |dest| fs::write(dest, b"pixels")).unwrap();

        assert_eq!(Path::new(&result), dir.join("photo.png"));
        assert_eq!(fs::read(&result).unwrap(), b"pixels");
    }

    #[test]
    fn place_attachment_avoids_overwriting_on_name_collision() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "photo.png", "original");

        let result = place_attachment(tmp.path(), "photo.png", |dest| fs::write(dest, b"new")).unwrap();

        assert_eq!(Path::new(&result).file_name().unwrap(), "photo-2.png");
        assert_eq!(fs::read_to_string(tmp.path().join("photo.png")).unwrap(), "original");
        assert_eq!(fs::read(&result).unwrap(), b"new");
    }

    /* --- get_files_by_extension --- */

    #[test]
    fn finds_files_with_matching_extension_recursively() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "a.org", "x");
        fs::create_dir(tmp.path().join("sub")).unwrap();
        write_file(&tmp.path().join("sub"), "b.org", "y");
        write_file(tmp.path(), "c.txt", "z");

        let results = get_files_by_extension(tmp.path(), "org");

        assert_eq!(results.len(), 2);
        assert!(results.iter().any(|p| p.file_name().unwrap() == "a.org"));
        assert!(results.iter().any(|p| p.file_name().unwrap() == "b.org"));
    }

    #[test]
    fn no_matching_extension_returns_empty() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "a.txt", "x");
        assert!(get_files_by_extension(tmp.path(), "org").is_empty());
    }

    /* --- get_path_suggestions --- */

    #[test]
    fn empty_input_returns_no_suggestions() {
        assert!(get_path_suggestions("").is_empty());
    }

    #[test]
    fn suggests_matching_subdirectories_by_prefix() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("desktop")).unwrap();
        fs::create_dir(tmp.path().join("documents")).unwrap();
        fs::create_dir(tmp.path().join("music")).unwrap();

        let input = tmp.path().join("de").to_string_lossy().into_owned();
        let suggestions = get_path_suggestions(&input);

        assert_eq!(suggestions.len(), 1);
        assert!(suggestions[0].contains("desktop"));
    }

    #[test]
    fn trailing_separator_lists_all_subdirs_without_prefix_filter() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("alpha")).unwrap();
        fs::create_dir(tmp.path().join("beta")).unwrap();

        let mut input = tmp.path().to_string_lossy().into_owned();
        input.push(std::path::MAIN_SEPARATOR);

        let suggestions = get_path_suggestions(&input);
        assert_eq!(suggestions.len(), 2);
    }

    #[test]
    fn files_are_not_suggested_only_directories() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("devdir")).unwrap();
        write_file(tmp.path(), "devfile.txt", "x");

        let input = tmp.path().join("dev").to_string_lossy().into_owned();
        let suggestions = get_path_suggestions(&input);

        assert_eq!(suggestions.len(), 1);
        assert!(suggestions[0].contains("devdir"));
    }

    #[test]
    fn suggestions_are_capped_at_five() {
        let tmp = TempDir::new().unwrap();
        for i in 0..8 {
            fs::create_dir(tmp.path().join(format!("dir{}", i))).unwrap();
        }

        let input = tmp.path().join("dir").to_string_lossy().into_owned();
        let suggestions = get_path_suggestions(&input);

        assert_eq!(suggestions.len(), 5);
    }

    /* --- read_lines / write_lines --- */

    #[test]
    fn read_lines_splits_content_by_newline() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), "f.txt", "a\nb\nc");

        let lines = read_lines(&path).unwrap();
        assert_eq!(lines, vec!["a", "b", "c"]);
    }

    #[test]
    fn read_lines_missing_file_returns_err() {
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("does_not_exist.txt");
        assert!(read_lines(&missing).is_err());
    }

    #[test]
    fn write_lines_joins_with_newline_and_trailing_newline() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("out.txt");

        write_lines(&path, &["a".to_string(), "b".to_string()]).unwrap();
        let content = fs::read_to_string(&path).unwrap();

        assert_eq!(content, "a\nb\n");
    }

    #[test]
    fn write_lines_empty_vec_writes_empty_file() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("empty.txt");

        write_lines(&path, &[]).unwrap();
        let content = fs::read_to_string(&path).unwrap();

        assert_eq!(content, "");
    }

    #[test]
    fn read_write_roundtrip_preserves_lines() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("roundtrip.txt");
        let original = vec!["one".to_string(), "two".to_string(), "three".to_string()];

        write_lines(&path, &original.clone()).unwrap();
        let read_back = read_lines(&path).unwrap();

        assert_eq!(read_back, original);
    }

    /* --- splice_lines --- */

    #[test]
    fn splice_replaces_line_range() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), "f.txt", "a\nb\nc\nd");

        splice_lines(&path, 1, 3, vec!["X".to_string()]).unwrap();
        let lines = read_lines(&path).unwrap();

        assert_eq!(lines, vec!["a", "X", "d"]);
    }

    /// The empty range AT the end is a legitimate way to append, and stays
    /// one -- it names lines that exist (none of them).
    #[test]
    fn splice_of_the_empty_range_at_the_end_appends() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), "f.txt", "a\nb");

        splice_lines(&path, 2, 2, vec!["c".to_string()]).unwrap();
        let lines = read_lines(&path).unwrap();

        assert_eq!(lines, vec!["a", "b", "c"]);
    }

    /// A range past the end is refused rather than trimmed to fit.
    ///
    /// Every caller here is replacing a heading's block against numbers
    /// from a parse of this same file. A range that no longer fits means
    /// the file changed underneath, and the block belongs somewhere else
    /// now -- clamping would write it over whatever moved into those lines
    /// instead, or cut the file off at the start of the range, and say
    /// nothing. Two real corruption bugs hid behind that.
    #[test]
    fn splice_past_the_end_is_refused_and_leaves_the_file_untouched() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), "f.txt", "a\nb");

        let err = splice_lines(&path, 2, 10, vec!["c".to_string()]).unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(read_lines(&path).unwrap(), vec!["a", "b"], "nothing was written");
    }

    #[test]
    fn splice_of_an_inverted_range_is_refused() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), "f.txt", "a\nb\nc");

        let err = splice_lines(&path, 2, 1, vec!["X".to_string()]).unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(read_lines(&path).unwrap(), vec!["a", "b", "c"], "nothing was written");
    }

    /* --- write_atomic --- */

    /// The point of the whole exercise: a reader never sees half a note.
    /// What is observable from a test is the other half of that bargain --
    /// the temp file it goes through does not survive the write.
    #[test]
    fn write_leaves_no_temp_file_behind() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("note.org");

        write_lines(&path, &["* Eins".to_string()]).unwrap();
        write_lines(&path, &["* Zwei".to_string()]).unwrap();

        let leftovers: Vec<String> = fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name != "note.org")
            .collect();

        assert!(leftovers.is_empty(), "temp files left in the vault: {leftovers:?}");
        assert_eq!(read_lines(&path).unwrap(), vec!["* Zwei"]);
    }

    /// And whatever it does leave behind for the instant it exists is
    /// named so the index and the file browser ignore it -- otherwise a
    /// watcher event mid-write would put a phantom note in the sidebar.
    #[test]
    fn the_temp_file_a_write_goes_through_is_recognized_as_one() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("note.org");

        let (temp_path, _file) = create_temp_beside(&target).unwrap();
        let name = temp_path.file_name().unwrap().to_string_lossy().into_owned();

        assert!(crate::utils::temp_file_utils::is_temp_file(&name), "not filtered anywhere: {name}");
        fs::remove_file(&temp_path).unwrap();
    }

    /// A rename replaces the name it is given. Handed a symlink, that
    /// would swap the link itself for a regular file and orphan whatever
    /// it pointed at -- so the link is resolved before anything is moved.
    #[cfg(unix)]
    #[test]
    fn writing_through_a_symlink_writes_the_file_behind_it() {
        let tmp = TempDir::new().unwrap();
        let real = write_file(tmp.path(), "real.org", "* Alt\n");
        let link = tmp.path().join("link.org");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        write_lines(&link, &["* Neu".to_string()]).unwrap();

        assert!(fs::symlink_metadata(&link).unwrap().file_type().is_symlink(), "the link must survive as a link");
        assert_eq!(fs::read_to_string(&real).unwrap(), "* Neu\n", "and point at the new content");
    }

    /// A fresh file gets the umask; the file being REPLACED already has a
    /// mode the user may have chosen. It is carried over.
    #[cfg(unix)]
    #[test]
    fn writing_preserves_the_mode_of_the_file_it_replaces() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), "note.org", "* Alt\n");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();

        write_lines(&path, &["* Neu".to_string()]).unwrap();

        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o640, "the replaced file's own mode has to survive");
    }

    #[test]
    fn splice_with_empty_replacement_deletes_range() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), "f.txt", "a\nb\nc");

        splice_lines(&path, 1, 2, vec![]).unwrap();
        let lines = read_lines(&path).unwrap();

        assert_eq!(lines, vec!["a", "c"]);
    }

    /* --- insert_line_after --- */

    #[test]
    fn insert_line_after_middle() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), "f.txt", "a\nb\nc");

        insert_line_after(&path, 1, "X".to_string()).unwrap();
        let lines = read_lines(&path).unwrap();

        assert_eq!(lines, vec!["a", "X", "b", "c"]);
    }

    #[test]
    fn insert_line_after_zero_prepends() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), "f.txt", "a\nb");

        insert_line_after(&path, 0, "X".to_string()).unwrap();
        let lines = read_lines(&path).unwrap();

        assert_eq!(lines, vec!["X", "a", "b"]);
    }

    #[test]
    fn insert_line_after_beyond_end_appends() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), "f.txt", "a\nb");

        insert_line_after(&path, 99, "X".to_string()).unwrap();
        let lines = read_lines(&path).unwrap();

        assert_eq!(lines, vec!["a", "b", "X"]);
    }

    /* --- append_line --- */

    #[test]
    fn append_line_adds_to_end() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), "f.txt", "a\nb");

        append_line(&path, "c".to_string()).unwrap();
        let lines = read_lines(&path).unwrap();

        assert_eq!(lines, vec!["a", "b", "c"]);
    }

    #[test]
    fn append_line_to_empty_file() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), "f.txt", "");

        append_line(&path, "first".to_string()).unwrap();
        let lines = read_lines(&path).unwrap();

        assert_eq!(lines, vec!["first"]);
    }

    /* --- insert_lines_after --- */

    #[test]
    fn insert_lines_after_middle_keeps_order() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), "f.txt", "a\nb\nc");

        insert_lines_after(&path, 1, vec!["X".to_string(), "Y".to_string()]).unwrap();
        let lines = read_lines(&path).unwrap();

        assert_eq!(lines, vec!["a", "X", "Y", "b", "c"]);
    }

    #[test]
    fn insert_lines_after_zero_prepends_block() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), "f.txt", "a\nb");

        insert_lines_after(&path, 0, vec!["X".to_string(), "Y".to_string()]).unwrap();
        let lines = read_lines(&path).unwrap();

        assert_eq!(lines, vec!["X", "Y", "a", "b"]);
    }

    #[test]
    fn insert_lines_after_beyond_end_appends_block() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), "f.txt", "a\nb");

        insert_lines_after(&path, 99, vec!["X".to_string(), "Y".to_string()]).unwrap();
        let lines = read_lines(&path).unwrap();

        assert_eq!(lines, vec!["a", "b", "X", "Y"]);
    }

    #[test]
    fn insert_lines_after_empty_block_changes_nothing() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), "f.txt", "a\nb");

        insert_lines_after(&path, 1, vec![]).unwrap();
        let lines = read_lines(&path).unwrap();

        assert_eq!(lines, vec!["a", "b"]);
    }

    #[test]
    fn insert_lines_after_creates_file_if_missing() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("new.org");

        insert_lines_after(&path, 0, vec!["* TODO A".to_string(), "SCHEDULED: <2026-08-05 Wed>".to_string()]).unwrap();
        let lines = read_lines(&path).unwrap();

        assert_eq!(lines, vec!["* TODO A", "SCHEDULED: <2026-08-05 Wed>"]);
    }

    /* --- append_lines --- */

    #[test]
    fn append_lines_adds_block_to_end() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), "f.txt", "a\nb");

        append_lines(&path, vec!["c".to_string(), "d".to_string()]).unwrap();
        let lines = read_lines(&path).unwrap();

        assert_eq!(lines, vec!["a", "b", "c", "d"]);
    }

    #[test]
    fn append_lines_to_empty_file() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), "f.txt", "");

        append_lines(&path, vec!["first".to_string(), "second".to_string()]).unwrap();
        let lines = read_lines(&path).unwrap();

        assert_eq!(lines, vec!["first", "second"]);
    }

    #[test]
    fn append_lines_creates_missing_parent_dirs() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("gtd").join("inbox.org");

        append_lines(&path, vec!["* Capture".to_string()]).unwrap();

        assert!(path.exists());
        assert_eq!(read_lines(&path).unwrap(), vec!["* Capture"]);
    }

    #[test]
    fn append_lines_empty_block_leaves_file_untouched() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), "f.txt", "a\nb");

        append_lines(&path, vec![]).unwrap();
        let lines = read_lines(&path).unwrap();

        assert_eq!(lines, vec!["a", "b"]);
    }

    /* --- remove_lines --- */

    #[test]
    fn remove_lines_deletes_inclusive_range() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), "f.txt", "a\nb\nc\nd");

        remove_lines(&path, 2, 3).unwrap();
        let lines = read_lines(&path).unwrap();

        assert_eq!(lines, vec!["a", "d"]);
    }

    #[test]
    fn remove_lines_single_line() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), "f.txt", "a\nb\nc");

        remove_lines(&path, 2, 2).unwrap();
        let lines = read_lines(&path).unwrap();

        assert_eq!(lines, vec!["a", "c"]);
    }

    #[test]
    fn remove_lines_entire_file_leaves_empty() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), "f.txt", "a\nb");

        remove_lines(&path, 1, 2).unwrap();
        let lines = read_lines(&path).unwrap();

        assert!(lines.is_empty());
    }

    /* --- extract_line_ranges --- */

    #[test]
    fn extract_line_ranges_splits_keep_and_archived() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), "f.txt", "a\nb\nc\nd\ne");

        let (keep, archived) = extract_line_ranges(&read_lines(&path).unwrap(), &[(2, 3)]);

        assert_eq!(keep, "a\nd\ne\n");
        assert_eq!(archived, "b\nc\n");
    }

    #[test]
    fn extract_line_ranges_multiple_ranges() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), "f.txt", "a\nb\nc\nd\ne");

        let (keep, archived) = extract_line_ranges(&read_lines(&path).unwrap(), &[(1, 1), (4, 5)]);

        assert_eq!(keep, "b\nc\n");
        assert_eq!(archived, "a\nd\ne\n");
    }

    #[test]
    fn extract_line_ranges_no_matches_keeps_everything() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), "f.txt", "a\nb");

        let (keep, archived) = extract_line_ranges(&read_lines(&path).unwrap(), &[(99, 100)]);

        assert_eq!(keep, "a\nb\n");
        assert_eq!(archived, "");
    }

    /* --- rename_no_replace --- */

    #[test]
    fn rename_no_replace_moves_the_file() {
        let tmp = TempDir::new().unwrap();
        let from = tmp.path().join("alt.org");
        let to = tmp.path().join("neu.org");
        fs::write(&from, "Inhalt").unwrap();

        rename_no_replace(&from, &to).unwrap();

        assert!(!from.exists());
        assert_eq!(fs::read_to_string(&to).unwrap(), "Inhalt");
    }

    /// The whole point: an existing target is never replaced. `fs::rename`
    /// would have overwritten it without a word.
    #[test]
    fn rename_no_replace_refuses_an_existing_target() {
        let tmp = TempDir::new().unwrap();
        let from = tmp.path().join("alt.org");
        let to = tmp.path().join("neu.org");
        fs::write(&from, "verschoben").unwrap();
        fs::write(&to, "gerade vom anderen Gerät angekommen").unwrap();

        let err = rename_no_replace(&from, &to).unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read_to_string(&to).unwrap(), "gerade vom anderen Gerät angekommen");
        assert!(from.exists(), "and the source is still there");
    }

    /* --- write_atomic_checked --- */

    #[test]
    fn write_atomic_checked_writes_raw_content() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("raw.txt");

        write_atomic_checked(&path, "hello\nworld").unwrap();
        let content = fs::read_to_string(&path).unwrap();

        assert_eq!(content, "hello\nworld");
    }

    /// A vault holds attachments, and sync writes them through the same
    /// path as notes.
    #[test]
    fn write_atomic_bytes_round_trips_non_utf8_content() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("bild.png");
        let bytes: Vec<u8> = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0xff, 0xfe];

        write_atomic_bytes(&path, &bytes).unwrap();

        assert_eq!(fs::read(&path).unwrap(), bytes);
    }

    /* --- append_to_file --- */

    #[test]
    fn append_to_file_creates_new_file_if_missing() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("new_archive.txt");

        append_to_file(&path, "first entry").unwrap();
        let content = fs::read_to_string(&path).unwrap();

        assert_eq!(content, "first entry");
    }

    #[test]
    fn append_to_file_adds_newline_before_appending_to_existing() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("archive.txt");
        fs::write(&path, "existing content").unwrap();

        append_to_file(&path, "new entry").unwrap();
        let content = fs::read_to_string(&path).unwrap();

        assert_eq!(content, "existing content\nnew entry");
    }

    #[test]
    fn append_to_file_does_not_double_newline_if_already_present() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("archive.txt");
        fs::write(&path, "existing content\n").unwrap();

        append_to_file(&path, "new entry").unwrap();
        let content = fs::read_to_string(&path).unwrap();

        assert_eq!(content, "existing content\nnew entry");
    }

    /// The archive-eating bug. A read that fails for any reason other than
    /// "not there" used to fall back to an empty string, so appending one
    /// task REPLACED the archive with that task.
    #[test]
    fn append_to_file_refuses_when_the_existing_content_cannot_be_read() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("archive.txt");
        fs::write(&path, "zwei Jahre erledigter Aufgaben\n").unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();

            let result = append_to_file(&path, "neue Aufgabe");

            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
            assert!(result.is_err(), "an unreadable archive must not be replaced");
            assert_eq!(fs::read_to_string(&path).unwrap(), "zwei Jahre erledigter Aufgaben\n");
        }
    }
}
