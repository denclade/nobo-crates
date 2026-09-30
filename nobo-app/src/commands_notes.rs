use nobo_core::core::history::{self, record_best_effort};
use nobo_core::core::indexer::IndexManager;
use nobo_core::core::models::{DiffChunk, FileSnapshot, HistoryEntryKind, Note, NoteLoadResult};
use nobo_core::utils::note_utils::{FilenameSyncStatus, IdentifierChange};
use nobo_core::utils::{diff_utils, fs_utils, note_utils, tag_utils, temp_file_utils};
use nobo_core::utils::ToCmdResult;
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use nobo_core::utils::fs_utils::filename_of;

pub fn search_tags(
    query: String,
    selected: Vec<String>,
    limit: u32,
    index_state: &IndexManager,
) -> Vec<String> {
    match index_state.index.read() {
        Ok(read_lock) => tag_utils::search(&read_lock.tag_counts, &query, &selected, limit as usize),
        Err(_) => vec![],
    }
}

/// Writes the note and tells the index about it.
///
/// Updating the index here rather than leaving it to the filesystem
/// watcher is not an optimisation: the notes list, the search and every
/// other reader are built from the index, not from the disk (see
/// `commands_fs::get_directory_contents`). Without this, a note somebody
/// had just created was not in the listing when the frontend went looking
/// for the file it had been handed the name of -- so it could not open it,
/// and the note only turned up once the watcher had settled. On mobile
/// there is no watcher at all (see docs/mobile.md), so it waited for the
/// next full rescan.
///
/// Same reason `save_note_final` updates the index itself, and every other
/// command in this file that creates a file already did.
///
/// Returns the filename, its path, and the content that was written, so
/// the caller can record a history entry without reading the file again.
/// Split out from the command for the same reason the ones in
/// commands_headings are: a test can call this, a `tauri::State` cannot be
/// built in one.
fn create_note_body(
    dir: &str, title: &str, tags: &[String], author: &str,
    index: &IndexManager,
) -> Result<(String, PathBuf, Option<String>), String> {
    let filename = note_utils::generate_note_file(dir, title, tags, author).to_cmd()?;
    let path = Path::new(dir).join(&filename);

    match std::fs::read_to_string(&path) {
        Ok(content) => {
            index.update_file_with(&path, &content);
            Ok((filename, path, Some(content)))
        }
        // The write itself succeeded, so the file is there and the index
        // still has to learn about it -- only reading it back for the
        // history snapshot did not work out. Let the index do its own read.
        Err(_) => {
            index.update_file(&path);
            Ok((filename, path, None))
        }
    }
}

pub fn create_note(
    dir: String, title: String, tags: Vec<String>, author: String,
    db: &Mutex<Connection>,
    index_state: &IndexManager,
) -> Result<String, String> {
    let (filename, path, content) = create_note_body(&dir, &title, &tags, &author, &index_state)?;

    if let Some(content) = content {
        record_best_effort(&db, HistoryEntryKind::FileCreated { filename: filename.clone() }, false,
            vec![FileSnapshot { path: path.to_string_lossy().into_owned(), before: None, after: Some(content) }]);
    }

    Ok(filename)
}

pub fn open_note(path: String) -> Result<NoteLoadResult, String> {
    temp_file_utils::load_with_temp_check(&path)
}

pub fn autosave_note_temp(path: String, content: String) -> Result<(), String> {
    temp_file_utils::write_temp(&path, &content)
}

/// The editor's own save.
///
/// Updates the index with the content it just wrote, rather than leaving
/// it to the filesystem watcher. Two reasons, and the second is the real
/// one:
///
/// - The watcher does not exist on mobile (see docs/mobile.md), so a note
///   edited on a phone stayed out of the agenda, the search and the task
///   lists until something rescanned the whole vault.
/// - Even where it does exist it now waits for the write burst to settle
///   (see core::indexer's SETTLE), and it would re-read the file to learn
///   what this command already has in hand.
pub fn save_note_final(
    path: String,
    content: String,
    index_state: &IndexManager,
) -> Result<(), String> {
    temp_file_utils::save_final(&path, &content)?;
    index_state.update_file_with(Path::new(&path), &content);
    Ok(())
}

pub fn discard_note_temp(path: String) -> Result<(), String> {
    temp_file_utils::remove_temp_if_exists(&path)
}

pub fn compute_diff(original: &str, temp: &str) -> Vec<DiffChunk> {
    diff_utils::compute_word_diff(original, temp)
}

pub fn delete_note(
    path: String,
    index_state: &IndexManager,
    db: &Mutex<Connection>,
) -> Result<(), String> {
    let target = Path::new(&path);
    // Before anything else: deleting projects.org from the notes view is
    // every project gone at once (see core::app_files).
    nobo_core::core::app_files::refuse_if_app_owned(target, "löschen", "deleted")?;
    // The index is updated explicitly, like every other mutating command:
    // history's undo path needs it in sync without waiting for the
    // filesystem watcher round-trip.
    let (before, result, after) = history::snapshot_around(target, || {
        fs_utils::remove_file(target).to_cmd()?;
        let _ = temp_file_utils::remove_temp_if_exists(&path);
        index_state.remove_file(target);
        Ok(())
    });

    record_best_effort(&db, HistoryEntryKind::FileDeleted { filename: filename_of(&path) }, false,
        vec![FileSnapshot { path, before, after }]);

    result
}

/// Every favourited file in the vault, wherever it lives -- what the
/// sidebar's own Favourites section lists.
///
/// Vault-wide on purpose: the point of favouriting a note is getting back
/// to it without remembering which folder it is in. Read off the bodies
/// the index already holds, the same way the file kind is.
pub fn favorite_files(index_state: &IndexManager) -> Vec<nobo_core::core::models::DirEntry> {
    index_state.favorite_files()
}

/// Marks or unmarks a file as a favourite.
///
/// The mark goes into the file itself (see core::favorites), so it follows
/// the vault to every device -- which also means this is a normal write:
/// the index is refreshed so the sidebar's star updates, and the sync
/// picks it up like any other edit.
pub fn set_favorite(
    path: String,
    favorite: bool,
    index_state: &IndexManager,
) -> Result<(), String> {
    let path = Path::new(&path);
    nobo_core::core::favorites::set(path, favorite)?;
    index_state.update_file(path);
    Ok(())
}

pub fn check_filename_sync(path: String) -> Option<FilenameSyncStatus> {
    note_utils::check_filename_sync(Path::new(&path))
}

pub fn ignore_filename_sync(path: String) -> Result<(), String> {
    note_utils::set_filename_sync_ignored(Path::new(&path)).to_cmd()
}

pub fn preview_denote_filename(identifier: String, title: String, tags: Vec<String>, extension: String) -> String {
    note_utils::build_denote_filename(&identifier, &title, &tags, &extension)
}

/// Resolves an `[[id:IDENTIFIER]]` link target -- see LinkWidget.ts/resolveLink.ts.
pub fn get_note_by_identifier(identifier: String, index_state: &IndexManager) -> Option<Note> {
    index_state.get_note_by_identifier(&identifier)
}

/// Whether saving would change #+IDENTIFIER: (which would break every
/// [[id:...]] link pointing at this note) -- checked before save, see
/// useNoteEditor.ts's guarded saveActiveNote.
pub fn check_identifier_change(old_content: String, new_content: String) -> Option<IdentifierChange> {
    note_utils::identifier_changed(&old_content, &new_content)
}

/// Rewrites every `id:OLD_IDENTIFIER` occurrence to `id:NEW_IDENTIFIER`
/// across every .org file under base_dir, after the user opts in via the
/// identifier-change warning. Returns how many files were actually changed.
pub fn replace_link_references(base_dir: String, old_identifier: String, new_identifier: String) -> Result<u32, String> {
    note_utils::replace_link_references(Path::new(&base_dir), &old_identifier, &new_identifier).to_cmd()
}

pub fn rename_note(
    old_path: String,
    new_name: String,
    index_state: &IndexManager,
    db: &Mutex<Connection>,
) -> Result<String, String> {
    let old = Path::new(&old_path);
    nobo_core::core::app_files::refuse_if_app_owned(old, "umbenennen", "renamed")?;
    let parent = old.parent().ok_or("Invalid path")?;
    let new_path = parent.join(&new_name);

    let content = std::fs::read_to_string(old).ok();
    // The existence check IS the rename -- see fs_utils::rename_no_replace.
    // Checking first and renaming after left a window spanning a full read
    // of the source, and sync writes into this same vault from the async
    // runtime.
    fs_utils::rename_no_replace(old, &new_path).map_err(|e| match e.kind() {
        std::io::ErrorKind::AlreadyExists => "A file with that name already exists".to_string(),
        _ => e.to_string(),
    })?;

    index_state.remove_file(old);
    index_state.update_file(&new_path);

    // Two snapshots (old path deleted, new path created) rather than one
    // "renamed" pseudo-op -- undo needs to recreate the file under its OLD
    // name, not just delete the new one, so both paths need their own
    // before/after (see FileSnapshot's doc comment).
    record_best_effort(&db, HistoryEntryKind::FileRenamed { old_filename: filename_of(&old_path), new_filename: new_name }, false, vec![
        FileSnapshot { path: old_path, before: content.clone(), after: None },
        FileSnapshot { path: new_path.to_string_lossy().into_owned(), before: None, after: content },
    ]);

    Ok(new_path.to_string_lossy().into_owned())
}

/// The "rename" dialog: a Denote note's new title and tags, written into
/// its header and its filename together (see note_utils::retitle_note).
/// Returns the path the note lives at afterwards.
pub fn retitle_note(
    path: String,
    title: String,
    tags: Vec<String>,
    index_state: &IndexManager,
    db: &Mutex<Connection>,
) -> Result<String, String> {
    let old = Path::new(&path);
    // Retitling renames the file to match its new title -- the same move.
    nobo_core::core::app_files::refuse_if_app_owned(old, "umbenennen", "renamed")?;
    let before = std::fs::read_to_string(old).ok();

    let new_path = note_utils::retitle_note(old, &title, &tags).map_err(|e| match e.kind() {
        std::io::ErrorKind::AlreadyExists => "A file with that name already exists".to_string(),
        _ => e.to_string(),
    })?;

    index_state.remove_file(old);
    index_state.update_file(Path::new(&new_path));

    let after = std::fs::read_to_string(&new_path).ok();
    if new_path == path {
        // Only the header changed (the new title slugs to the old name).
        record_best_effort(&db, HistoryEntryKind::FileEdited { filename: filename_of(&path) }, false,
            vec![FileSnapshot { path: path.clone(), before, after }]);
    } else {
        // Content and name changed together -- the rename's two snapshots
        // carry both (see rename_note on why it is two and not one).
        record_best_effort(&db, HistoryEntryKind::FileRenamed { old_filename: filename_of(&path), new_filename: filename_of(&new_path) }, false, vec![
            FileSnapshot { path, before, after: None },
            FileSnapshot { path: new_path.clone(), before: None, after },
        ]);
    }

    Ok(new_path)
}

/// Duplicates a file (or denote note) into the same directory -- see
/// note_utils::duplicate_file for the naming rules. title/tags/identifier
/// are the user's (optionally edited) choices from DuplicateNoteModal;
/// `None` keeps the source's own value (identifier defaults to a fresh
/// timestamp instead -- see DuplicateOverrides).
pub fn duplicate_note(
    path: String,
    title: Option<String>,
    tags: Option<Vec<String>>,
    identifier: Option<String>,
    index_state: &IndexManager,
    db: &Mutex<Connection>,
) -> Result<String, String> {
    let overrides = note_utils::DuplicateOverrides { title, tags, identifier };
    let new_path = note_utils::duplicate_file(Path::new(&path), overrides).to_cmd()?;
    index_state.update_file(Path::new(&new_path));

    if let Ok(content) = std::fs::read_to_string(&new_path) {
        record_best_effort(&db, HistoryEntryKind::FileDuplicated { source_filename: filename_of(&path), new_filename: filename_of(&new_path) }, false,
            vec![FileSnapshot { path: new_path.clone(), before: None, after: Some(content) }]);
    }

    Ok(new_path)
}

/// Copies SOURCE_PATH (a real OS file, e.g. from an OS-level drag-and-drop
/// onto the editor) into TARGET_DIR under FILENAME, creating the directory
/// if needed -- the editor's image/PDF attach flow. A name collision gets a
/// numeric suffix rather than overwriting (see fs_utils::place_attachment).
/// TARGET_DIR is typically a hidden `.attachments`-style folder by
/// convention, but the index itself now includes hidden entries too (see
/// core::indexer), so this updates it immediately like its sibling commands
/// rather than waiting for the filesystem watcher to notice.
pub fn copy_attachment_file(
    source_path: String,
    target_dir: String,
    filename: String,
    index_state: &IndexManager,
    db: &Mutex<Connection>,
) -> Result<String, String> {
    let saved_path = fs_utils::place_attachment(Path::new(&target_dir), &filename, |dest| {
        std::fs::copy(&source_path, dest).map(|_| ())
    }).to_cmd()?;
    index_state.update_file(Path::new(&saved_path));

    // Informational only -- binary content, no before/after text snapshot
    // (see HistoryEntryKind::AttachmentAdded's doc comment). An empty
    // snapshot list is what makes undo/redo treat this as "shown in the
    // list, not restorable" rather than misreading a missing snapshot as
    // "this file didn't exist".
    record_best_effort(&db, HistoryEntryKind::AttachmentAdded { filename: filename_of(&saved_path) }, false, vec![]);

    Ok(saved_path)
}

/// Writes BYTES (e.g. a pasted clipboard image) into TARGET_DIR under
/// FILENAME, creating the directory if needed -- the editor's attach flow's
/// other half (see copy_attachment_file for the OS-file-path variant, which
/// this otherwise mirrors exactly, index update included).
pub fn save_attachment_bytes(
    target_dir: String,
    filename: String,
    bytes: Vec<u8>,
    index_state: &IndexManager,
    db: &Mutex<Connection>,
) -> Result<String, String> {
    let saved_path = fs_utils::place_attachment(Path::new(&target_dir), &filename, |dest| {
        std::fs::write(dest, &bytes)
    }).to_cmd()?;
    index_state.update_file(Path::new(&saved_path));

    record_best_effort(&db, HistoryEntryKind::AttachmentAdded { filename: filename_of(&saved_path) }, false, vec![]);

    Ok(saved_path)
}

/// Copies SOURCE_PATH to DEST_PATH verbatim -- the editor's image context
/// menu's "Save as" action. Unlike copy_attachment_file/save_attachment_bytes
/// above, DEST_PATH is a user-chosen destination from a native save dialog
/// (which already handles overwrite confirmation itself), not a managed
/// attachments folder -- so no collision-avoidance and no search-index
/// update, since the destination is by definition outside the vault.
pub fn export_file(source_path: String, dest_path: String) -> Result<(), String> {
    std::fs::copy(&source_path, &dest_path).map(|_| ()).to_cmd()
}

/// The decode half of `copy_image_to_clipboard`.
///
/// The host owns the clipboard (Tauri has a plugin, Qt has QClipboard),
/// so only the decoding is here -- which is also the half worth a test:
/// a real file decodes, a missing or corrupt one fails cleanly. Writing
/// the pixels out is a one-line passthrough on either host.
pub fn decode_image_rgba(path: &Path) -> image::ImageResult<(Vec<u8>, u32, u32)> {
    let decoded = image::open(path)?.to_rgba8();
    let (width, height) = decoded.dimensions();
    Ok((decoded.into_raw(), width, height))
}

/// Moves a file into a different directory, keeping its filename -- used by
/// the Notes view's multi-select "move to folder" bulk action. Same
/// rename+reindex shape as `rename_note`, just joined against `target_dir`
/// instead of the file's current parent.
pub fn move_note(
    path: String,
    target_dir: String,
    index_state: &IndexManager,
    db: &Mutex<Connection>,
) -> Result<String, String> {
    let old = Path::new(&path);
    nobo_core::core::app_files::refuse_if_app_owned(old, "verschieben", "moved")?;
    let file_name = old.file_name().ok_or("Invalid path")?;
    let new_path = Path::new(&target_dir).join(file_name);

    if new_path == old {
        return Ok(new_path.to_string_lossy().into_owned());
    }
    let content = std::fs::read_to_string(old).ok();
    // See rename_note above.
    fs_utils::rename_no_replace(old, &new_path).map_err(|e| match e.kind() {
        std::io::ErrorKind::AlreadyExists => "A file with that name already exists in the target folder".to_string(),
        _ => e.to_string(),
    })?;

    index_state.remove_file(old);
    index_state.update_file(&new_path);

    record_best_effort(&db, HistoryEntryKind::FileMoved { filename: filename_of(&path), target_dir }, false, vec![
        FileSnapshot { path, before: content.clone(), after: None },
        FileSnapshot { path: new_path.to_string_lossy().into_owned(), before: None, after: content },
    ]);

    Ok(new_path.to_string_lossy().into_owned())
}
#[cfg(test)]
mod editor_context_menu_tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn export_file_copies_bytes_to_the_chosen_destination() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source.png");
        let dest = tmp.path().join("exported.png");
        std::fs::write(&source, b"pixels").unwrap();

        export_file(source.to_string_lossy().into_owned(), dest.to_string_lossy().into_owned()).unwrap();

        assert_eq!(std::fs::read(&dest).unwrap(), b"pixels");
    }

    #[test]
    fn export_file_reports_a_missing_source_instead_of_panicking() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("does-not-exist.png");
        let dest = tmp.path().join("exported.png");

        assert!(export_file(source.to_string_lossy().into_owned(), dest.to_string_lossy().into_owned()).is_err());
    }

    #[test]
    fn decode_image_rgba_reads_a_real_png() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("pixel.png");
        // A minimal real 1x1 white PNG, encoded via the `image` crate itself
        // rather than a hand-typed byte literal, so this test can't rot if
        // the crate's own encoder output ever shifts.
        image::RgbaImage::from_pixel(1, 1, image::Rgba([255, 255, 255, 255]))
            .save(&path)
            .unwrap();

        let (rgba, width, height) = decode_image_rgba(&path).unwrap();

        assert_eq!((width, height), (1, 1));
        assert_eq!(rgba, vec![255, 255, 255, 255]);
    }

    #[test]
    fn decode_image_rgba_rejects_a_corrupt_file_instead_of_panicking() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("not-an-image.png");
        std::fs::write(&path, b"this is not image data").unwrap();

        assert!(decode_image_rgba(&path).is_err());
    }
}

#[cfg(test)]
mod note_creation_tests {
    use super::*;
    use tempfile::TempDir;

    /// The bug this guards: creating a note handed the frontend a filename
    /// that was not in the listing yet, because the listing comes out of
    /// the index and only the filesystem watcher ever put it there. The
    /// note appeared in the sidebar a moment later and could not be opened
    /// at all -- and on mobile, where there is no watcher, not even that.
    #[test]
    fn a_created_note_is_in_the_index_before_the_command_returns() {
        let dir = TempDir::new().unwrap();
        let index = IndexManager::new();
        index.scan_directory(dir.path());

        let (filename, _, _) = create_note_body(
            dir.path().to_str().unwrap(), "Neue Notiz", &[], "", &index,
        ).unwrap();

        let listed = index.directory_contents(dir.path(), false);
        assert!(
            listed.iter().any(|entry| entry.name == filename),
            "the note has to be listable by the name the caller was just handed, got {:?}",
            listed.iter().map(|e| &e.name).collect::<Vec<_>>(),
        );
    }

    /// What the caller does with it: opens the file at that path. An entry
    /// in the index that points nowhere would be worse than none.
    #[test]
    fn the_listed_entry_points_at_the_file_that_was_written() {
        let dir = TempDir::new().unwrap();
        let index = IndexManager::new();
        index.scan_directory(dir.path());

        let (filename, path, content) = create_note_body(
            dir.path().to_str().unwrap(), "Neue Notiz", &[], "", &index,
        ).unwrap();

        assert!(path.exists());
        assert!(content.expect("the content was read back").contains("#+TITLE:"));
        let entry = index.directory_contents(dir.path(), false)
            .into_iter().find(|entry| entry.name == filename).unwrap();
        assert_eq!(Path::new(&entry.path), path);
    }
}
