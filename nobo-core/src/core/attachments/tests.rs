// SPDX-FileCopyrightText: 2026 Dennis Burgermeister
// SPDX-License-Identifier: GPL-3.0-or-later

use super::*;
use crate::core::{history, trash};
use tempfile::TempDir;

fn write(root: &Path, rel: &str, bytes: impl AsRef<[u8]>) -> PathBuf {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, bytes).unwrap();
    path
}

fn setup() -> (TempDir, Policy, IndexManager, Mutex<Connection>) {
    let dir = TempDir::new().unwrap();
    let policy = Policy::new(dir.path(), ".attachments", 31);
    let index = IndexManager::new();
    index.scan_directory(dir.path());
    let conn = Connection::open_in_memory().unwrap();
    history::store::init_schema(&conn).unwrap();
    (dir, policy, index, Mutex::new(conn))
}

#[test]
fn discovers_unlinked_files_even_when_the_original_note_no_longer_exists() {
    let (dir, policy, _, _) = setup();
    write(dir.path(), ".attachments/old.png", [255, 0]);
    write(dir.path(), "abandoned/.attachments/old.pdf", b"pdf");
    write(dir.path(), "manual.pdf", b"not managed");
    write(dir.path(), "notes.org", "* A note\n");
    let found = find_orphaned(&policy).unwrap();
    assert_eq!(found.len(), 2);
    assert_eq!(found.iter().map(|f| f.size_bytes).sum::<u64>(), 5);
    assert!(found.iter().all(|f| f.revision.len() == 64));
}

#[test]
fn scans_configured_and_default_folders_and_keeps_hidden_archived_draft_references() {
    let (dir, _, _, _) = setup();
    let policy = Policy::new(dir.path(), "assets", 31);
    for rel in ["assets/used.png", "assets/draft.png", "assets/orphan.png", "sub/assets/used.png", ".attachments/old.png"] {
        write(dir.path(), rel, b"image");
    }
    write(dir.path(), ".gitignore", "ignored.org\n");
    write(dir.path(), ".nobo/archive.org", "[[file:../assets/used.png]]\n");
    write(dir.path(), "#draft.org#", "[[file:assets/draft.png]]\n");
    write(dir.path(), "ignored.org", "[[file:sub/assets/used.png]]\n");
    let found = find_orphaned(&policy).unwrap();
    assert_eq!(found.len(), 2);
    assert!(found.iter().any(|f| f.path.ends_with("assets/orphan.png")));
    assert!(found.iter().any(|f| f.path.ends_with(".attachments/old.png")));
}

#[test]
fn org_archive_references_protect_attachments_during_deletion_and_maintenance() {
    let (dir, policy, index, db) = setup();
    let note = write(dir.path(), "tasks.org", "* TODO Task\n[[file:.attachments/shared.png]]\n");
    write(dir.path(), ".attachments/shared.png", b"image");
    write(dir.path(), "tasks.org_archive", "* DONE Archived\n[[file:.attachments/shared.png]]\n");
    let preview = find_orphaned(&policy).unwrap();
    assert!(preview.is_empty());
    vault_write::recorded_with_policy(
        &note,
        Some(policy.clone()),
        |_| HistoryEntryKind::FileDeleted { filename: "tasks.org".into() },
        &index,
        &db,
        || fs::remove_file(&note).to_cmd(),
    )
    .unwrap();
    assert!(dir.path().join(".attachments/shared.png").exists());
    assert!(find_orphaned(&policy).unwrap().is_empty());

    // An archive created after a maintenance preview must protect its file too.
    write(dir.path(), ".attachments/new.png", b"new");
    let preview = find_orphaned(&policy).unwrap();
    assert_eq!(preview.len(), 1);
    write(dir.path(), "#draft.org_archive#", "[[file:.attachments/new.png]]\n");
    let result = recycle_orphaned(&policy, &preview, &index, &db).unwrap();
    assert_eq!((result.removed, result.skipped), (0, 1));
    assert!(dir.path().join(".attachments/new.png").exists());
}

#[test]
fn absolute_configured_folder_must_remain_inside_the_vault() {
    let (dir, _, _, _) = setup();
    let external = TempDir::new().unwrap();
    write(external.path(), "outside.png", b"private");
    write(dir.path(), "assets/old.png", b"image");
    assert_eq!(find_orphaned(&Policy::new(dir.path(), &dir.path().join("assets").to_string_lossy(), 31)).unwrap().len(), 1);
    assert!(find_orphaned(&Policy::new(dir.path(), &external.path().to_string_lossy(), 31)).unwrap().is_empty());
}

#[test]
fn selection_only_recycles_reviewed_files_and_each_is_independently_restorable() {
    let (dir, policy, index, db) = setup();
    write(dir.path(), ".attachments/a.png", [255, 1]);
    write(dir.path(), ".attachments/b.png", [255, 2]);
    write(dir.path(), ".attachments/keep.png", [255, 3]);
    let selected: Vec<_> = find_orphaned(&policy).unwrap().into_iter().filter(|f| !f.path.ends_with("keep.png")).collect();
    let result = recycle_orphaned(&policy, &selected, &index, &db).unwrap();
    assert_eq!((result.removed, result.skipped), (2, 0));
    assert!(dir.path().join(".attachments/keep.png").exists());
    assert!(!dir.path().join(".attachments/a.png").exists());
    let conn = db.lock().unwrap();
    let entries = trash::list(&conn, dir.path()).unwrap();
    assert_eq!(entries.len(), 2);
    let a = entries.iter().find(|e| e.original_path.ends_with("a.png")).unwrap();
    trash::restore(&conn, dir.path(), &a.id, &index).unwrap();
    assert_eq!(fs::read(dir.path().join(".attachments/a.png")).unwrap(), [255, 1]);
    assert!(!dir.path().join(".attachments/b.png").exists());
}

#[test]
fn newly_referenced_or_replaced_files_are_skipped_when_confirming_an_old_preview() {
    let (dir, policy, index, db) = setup();
    write(dir.path(), ".attachments/linked.png", b"old");
    write(dir.path(), ".attachments/replaced.png", b"old");
    let preview = find_orphaned(&policy).unwrap();
    write(dir.path(), "new.org", "[[file:.attachments/linked.png]]");
    write(dir.path(), ".attachments/replaced.png", b"new"); // Same size, different bytes.
    let result = recycle_orphaned(&policy, &preview, &index, &db).unwrap();
    assert_eq!((result.removed, result.skipped), (0, 2));
    assert!(dir.path().join(".attachments/linked.png").exists());
    assert_eq!(fs::read(dir.path().join(".attachments/replaced.png")).unwrap(), b"new");
}

#[test]
fn forged_paths_outside_managed_folders_are_skipped() {
    let (dir, policy, index, db) = setup();
    write(dir.path(), "important.pdf", b"private");
    let selected = vec![OrphanedAttachment {
        path: dir.path().join("important.pdf").to_string_lossy().into_owned(),
        size_bytes: 7,
        revision: blake3::hash(b"private").to_hex().to_string(),
    }];
    let result = recycle_orphaned(&policy, &selected, &index, &db).unwrap();
    assert_eq!((result.removed, result.skipped), (0, 1));
    assert!(dir.path().join("important.pdf").exists());
}

#[test]
fn incomplete_reference_scans_abort_before_any_files_are_removed() {
    let (dir, policy, index, db) = setup();
    write(dir.path(), ".attachments/old.png", b"image");
    let preview = find_orphaned(&policy).unwrap();
    write(dir.path(), "unreadable.org", [255, 254]);
    assert!(find_orphaned(&policy).is_err());
    assert!(recycle_orphaned(&policy, &preview, &index, &db).is_err());
    assert!(dir.path().join(".attachments/old.png").exists());
}

#[test]
fn maintenance_preserves_attachments_needed_to_restore_an_earlier_deleted_note() {
    let (dir, policy, index, db) = setup();
    let note = write(dir.path(), "a.org", "[[file:.attachments/shared.png]]");
    let other = write(dir.path(), "b.org", "[[file:.attachments/shared.png]]");
    write(dir.path(), ".attachments/shared.png", [255, 0, 254]);
    vault_write::recorded_with_policy(
        &note,
        Some(policy.clone()),
        |_| HistoryEntryKind::FileDeleted { filename: "a.org".into() },
        &index,
        &db,
        || fs::remove_file(&note).to_cmd(),
    )
    .unwrap();
    let entry = trash::list(&db.lock().unwrap(), dir.path()).unwrap().remove(0);
    fs::remove_file(other).unwrap(); // A deletion made outside the app.
    let preview = find_orphaned(&policy).unwrap();
    let short_policy = Policy::new(dir.path(), ".attachments", 1);
    recycle_orphaned(&short_policy, &preview, &index, &db).unwrap();
    let conn = db.lock().unwrap();
    // The attachment's maintenance entry expires first; the older note
    // must still be independently restorable for its original 31 days.
    trash::maintain(&conn, chrono::Utc::now().timestamp() + 2 * 86400).unwrap();
    trash::restore(&conn, dir.path(), &entry.id, &index).unwrap();
    assert_eq!(fs::read(dir.path().join(".attachments/shared.png")).unwrap(), [255, 0, 254]);
}

#[cfg(unix)]
#[test]
fn symlinks_are_not_candidates_and_alias_references_protect_real_attachments() {
    let (dir, policy, _, _) = setup();
    let external = TempDir::new().unwrap();
    write(dir.path(), ".attachments/real.png", b"image");
    write(external.path(), "private.png", b"private");
    std::os::unix::fs::symlink(external.path(), dir.path().join("nested")).unwrap();
    std::os::unix::fs::symlink(dir.path().join(".attachments/real.png"), dir.path().join("alias.png")).unwrap();
    std::os::unix::fs::symlink(external.path().join("private.png"), dir.path().join(".attachments/link.png")).unwrap();
    write(dir.path(), "note.org", "[[file:alias.png]]");
    assert!(find_orphaned(&policy).unwrap().is_empty());
}
