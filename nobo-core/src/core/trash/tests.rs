// SPDX-FileCopyrightText: 2026 Dennis Burgermeister
// SPDX-License-Identifier: GPL-3.0-or-later

use super::*;
use crate::core::vault_write;
use tempfile::TempDir;

struct Fixture {
    dir: TempDir,
    index: IndexManager,
    db: Mutex<Connection>,
}

impl Fixture {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON").unwrap();
        history::store::init_schema(&conn).unwrap();
        init_schema(&conn).unwrap();
        let index = IndexManager::new();
        index.scan_directory(dir.path());
        Self { dir, index, db: Mutex::new(conn) }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }

    fn write(&self, rel: &str, bytes: impl AsRef<[u8]>) -> PathBuf {
        let path = self.path(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, bytes).unwrap();
        self.index.update_file(&path);
        path
    }

    fn entries(&self) -> Vec<TrashEntry> {
        list(&self.db.lock().unwrap(), self.dir.path()).unwrap()
    }

    fn edit(&self, rel: &str, after: &str) {
        vault_write::edit_files_in(self.dir.path(), &[&self.path(rel)], None, &self.index, &self.db, |_| Ok((vec![after.to_string()], ())))
            .unwrap();
    }

    fn delete(&self, rel: &str) {
        let path = self.path(rel);
        vault_write::recorded(
            &path,
            |_| HistoryEntryKind::FileDeleted { filename: rel.into() },
            &self.index,
            &self.db,
            || fs::remove_file(&path).map_err(|e| e.to_string()),
        )
        .unwrap();
    }

    fn restore(&self, id: &str) -> Result<(), String> {
        restore(&self.db.lock().unwrap(), self.dir.path(), id, &self.index)
    }
}

use crate::core::history;

#[test]
fn deleting_a_note_recycles_its_binary_attachments_and_restores_both() {
    let f = Fixture::new();
    let content = "#+TITLE: Photo\n[[file:.attachments/photo.png]]\n";
    f.write("note.org", content);
    f.write(".attachments/photo.png", [0, 255, 12, 0, 254]);
    f.delete("note.org");
    assert!(!f.path("note.org").exists());
    assert!(!f.path(".attachments/photo.png").exists());
    let entry = f.entries().remove(0);
    assert_eq!(entry.attachment_count, 1);
    let deleted = chrono::DateTime::parse_from_rfc3339(&entry.deleted_at).unwrap();
    let expires = chrono::DateTime::parse_from_rfc3339(&entry.expires_at).unwrap();
    assert_eq!((expires - deleted).num_days(), 31);
    f.restore(&entry.id).unwrap();
    assert_eq!(fs::read_to_string(f.path("note.org")).unwrap(), content);
    assert_eq!(fs::read(f.path(".attachments/photo.png")).unwrap(), [0, 255, 12, 0, 254]);
    assert!(f.entries().is_empty());
}

#[test]
fn shared_attachments_survive_until_the_last_reference_including_hidden_archives() {
    let f = Fixture::new();
    f.write("a.org", "[[file:.attachments/shared.pdf]]\n");
    f.write(".nobo/archive.org", "[[file:../.attachments/shared.pdf::2][Shared]]\n");
    f.write(".attachments/shared.pdf", b"pdf");
    f.delete("a.org");
    assert!(f.path(".attachments/shared.pdf").exists());
    f.delete(".nobo/archive.org");
    assert!(!f.path(".attachments/shared.pdf").exists());
    let entries = f.entries();
    assert_eq!(entries.len(), 2);
    assert!(entries.iter().all(|e| e.attachment_count == 1), "each entry must remain independently restorable");
    let a = entries.iter().find(|e| e.original_path.ends_with("a.org")).unwrap();
    f.restore(&a.id).unwrap();
    assert!(f.path(".attachments/shared.pdf").exists());
}

#[test]
fn another_heading_in_the_same_file_protects_a_shared_attachment() {
    let f = Fixture::new();
    f.write("actions.org", "* TODO A\n[[file:.attachments/x.png]]\n* TODO B\n[[file:.attachments/x.png]]\n");
    f.write(".attachments/x.png", b"image");
    f.edit("actions.org", "* TODO B\n[[file:.attachments/x.png]]\n");
    assert!(f.path(".attachments/x.png").exists());
    assert_eq!(f.entries().len(), 1);
}

#[test]
fn saved_drafts_and_gitignored_org_files_protect_attachments() {
    let f = Fixture::new();
    f.write(".gitignore", "ignored.org\n");
    f.write("a.org", "[[file:.attachments/x.png]]");
    f.write("#draft.org#", "[[file:.attachments/x.png]]");
    f.write("ignored.org", "[[file:.attachments/x.png]]");
    f.write(".attachments/x.png", b"image");
    f.delete("a.org");
    assert!(f.path(".attachments/x.png").exists());
    f.delete("ignored.org");
    assert!(f.path(".attachments/x.png").exists());
}

#[test]
fn a_link_removed_on_save_recycles_the_attachment_without_reverting_other_text() {
    let f = Fixture::new();
    f.write("a.org", "Old text\n[[file:.attachments/x.png]]\n");
    f.write(".attachments/x.png", b"image");
    vault_write::save_note(&f.path("a.org"), "New text\n", &f.index, &f.db).unwrap();
    assert!(!f.path(".attachments/x.png").exists());
    f.restore(&f.entries()[0].id).unwrap();
    assert_eq!(fs::read_to_string(f.path("a.org")).unwrap(), "New text\n[[file:.attachments/x.png]]\n");
    assert!(f.path(".attachments/x.png").exists());
}

#[test]
fn ordinary_linked_files_outside_managed_folders_are_never_collected() {
    let f = Fixture::new();
    let outside = TempDir::new().unwrap();
    let external = outside.path().join("external.png");
    fs::write(&external, b"external").unwrap();
    f.write("a.org", format!("[[file:manual.pdf]]\n[[file:{}]]\n", external.display()));
    f.write("manual.pdf", b"manual");
    f.delete("a.org");
    assert!(f.path("manual.pdf").exists());
    assert!(external.exists());
}

#[test]
fn percent_encoding_unicode_search_suffixes_and_relative_aliases_resolve_to_one_file() {
    let f = Fixture::new();
    f.write("sub/a.org", "[[file:../.attachments/ü%20x.png::#anchor][Photo]]\n");
    f.write(".attachments/ü x.png", b"image");
    f.delete("sub/a.org");
    assert!(!f.path(".attachments/ü x.png").exists());
    assert_eq!(f.entries()[0].attachment_count, 1);
    assert!(!attachments::references(&f.path("other.org"), "[[file:.attachments/%ü.png]]").is_empty());
}

#[test]
fn a_shared_reference_with_implicit_file_scheme_is_respected() {
    let f = Fixture::new();
    f.write("a.org", "[[file:.attachments/x.png]]");
    f.write("b.org", "[[./.attachments/x.png]]");
    f.write(".attachments/x.png", b"image");
    f.delete("a.org");
    assert!(f.path(".attachments/x.png").exists());
}

#[test]
fn configured_shared_folder_is_collected_but_a_path_outside_the_vault_is_not() {
    let f = Fixture::new();
    let path = f.write("sub/a.org", "[[file:../assets/x.png]]");
    f.write("assets/x.png", b"image");
    let policy = Policy::new(f.dir.path(), &f.path("assets").to_string_lossy(), 31);
    let protection = Protection::prepare(&policy, &[&path], &f.db).unwrap();
    fs::remove_file(&path).unwrap();
    protection.finish(None, &f.index, &f.db).unwrap();
    assert!(!f.path("assets/x.png").exists());
}

#[test]
fn restoring_deleted_headings_preserves_later_edits_and_other_deletions() {
    let f = Fixture::new();
    f.write("actions.org", "* TODO A\nDescription A\n* TODO B\nDescription B\n* TODO C\n");
    f.edit("actions.org", "* TODO B\nDescription B\n* TODO C\n");
    let a = f.entries()[0].id.clone();
    f.edit("actions.org", "* TODO C\n");
    f.write("actions.org", "* DONE C\nNew description\n* TODO D\n");
    f.restore(&a).unwrap();
    let text = fs::read_to_string(f.path("actions.org")).unwrap();
    assert!(text.contains("* TODO A\nDescription A\n"));
    assert!(text.contains("* DONE C\nNew description\n* TODO D\n"));
    assert!(!text.contains("TODO B"));
}

#[test]
fn subtrees_are_restored_under_their_parent_without_duplicate_descendants() {
    let f = Fixture::new();
    f.write("projects.org", "* TODO Project\n** TODO Child\n*** TODO Nested\n** TODO Keep\n");
    f.edit("projects.org", "* TODO Project\n** TODO Keep\n");
    let id = f.entries()[0].id.clone();
    f.restore(&id).unwrap();
    let text = fs::read_to_string(f.path("projects.org")).unwrap();
    assert_eq!(text.matches("*** TODO Nested").count(), 1);
    assert!(text.contains("** TODO Child\n*** TODO Nested\n** TODO Keep"));
}

#[test]
fn restoration_refuses_to_overwrite_reused_paths_before_writing_anything() {
    let f = Fixture::new();
    f.write("a.org", "[[file:.attachments/x.png]]");
    f.write(".attachments/x.png", b"original");
    f.delete("a.org");
    f.write(".attachments/x.png", b"replacement");
    assert!(f.restore(&f.entries()[0].id).is_err());
    assert!(!f.path("a.org").exists());
    assert_eq!(fs::read(f.path(".attachments/x.png")).unwrap(), b"replacement");
}

#[test]
fn undo_and_redo_restore_and_recycle_binary_attachments_together_with_the_note() {
    let f = Fixture::new();
    f.write("a.org", "[[file:.attachments/x.png]]");
    f.write(".attachments/x.png", [255, 0, 254]);
    f.delete("a.org");
    history::undo(&f.db.lock().unwrap(), &f.index).unwrap();
    assert!(f.path("a.org").exists());
    assert_eq!(fs::read(f.path(".attachments/x.png")).unwrap(), [255, 0, 254]);
    assert!(f.entries().is_empty());
    history::redo(&f.db.lock().unwrap(), &f.index).unwrap();
    assert!(!f.path("a.org").exists());
    assert!(!f.path(".attachments/x.png").exists());
    assert_eq!(f.entries().len(), 1);
}

#[test]
fn bin_retention_is_independent_of_history_pruning_and_expires_at_exactly_31_days() {
    let f = Fixture::new();
    f.write("a.org", "* TODO Deleted\n");
    f.delete("a.org");
    let conn = f.db.lock().unwrap();
    history::store::prune(&conn, 0, 0).unwrap();
    let expiry: i64 = conn.query_row("SELECT expires_at FROM trash_entries", [], |r| r.get(0)).unwrap();
    maintain(&conn, expiry - 1).unwrap();
    assert_eq!(list(&conn, f.dir.path()).unwrap().len(), 1);
    maintain(&conn, expiry).unwrap();
    assert!(list(&conn, f.dir.path()).unwrap().is_empty());
    let blobs: i64 = conn.query_row("SELECT COUNT(*) FROM trash_files", [], |r| r.get(0)).unwrap();
    assert_eq!(blobs, 0);
}

#[test]
fn live_pending_writes_are_not_reconciled_and_interrupted_deletions_are_recovered() {
    let f = Fixture::new();
    let path = f.write("a.org", "* TODO A\n");
    let policy = Policy::new(f.dir.path(), ".attachments", 31);
    let protection = Protection::prepare(&policy, &[&path], &f.db).unwrap();
    assert!(f.entries().is_empty());
    fs::remove_file(&path).unwrap();
    drop(protection); // Simulate termination before finish.
    let entries = f.entries();
    assert_eq!(entries.len(), 1);
    f.restore(&entries[0].id).unwrap();
    assert_eq!(fs::read_to_string(path).unwrap(), "* TODO A\n");
}

#[test]
fn binary_files_can_be_deleted_and_restored_without_text_history() {
    let f = Fixture::new();
    f.write("picture.png", [255, 0, 254]);
    f.delete("picture.png");
    assert!(!f.index.index.read().unwrap().files.contains_key(&f.path("picture.png").to_string_lossy().into_owned()));
    let id = f.entries()[0].id.clone();
    f.restore(&id).unwrap();
    assert_eq!(fs::read(f.path("picture.png")).unwrap(), [255, 0, 254]);
}

#[test]
fn trash_entries_are_scoped_to_their_original_vault() {
    let f = Fixture::new();
    let other = TempDir::new().unwrap();
    f.write("a.org", "A");
    f.delete("a.org");
    let id = f.entries()[0].id.clone();
    let conn = f.db.lock().unwrap();
    assert!(list(&conn, other.path()).unwrap().is_empty());
    assert!(restore(&conn, other.path(), &id, &f.index).is_err());
    assert!(purge(&conn, other.path(), &id).is_err());
}

#[test]
fn permanent_deletion_also_removes_history_that_could_resurrect_the_content() {
    let f = Fixture::new();
    f.write("a.org", "Private original\n");
    f.edit("a.org", "Private revised\n");
    f.delete("a.org");
    let id = f.entries()[0].id.clone();
    let conn = f.db.lock().unwrap();
    purge(&conn, f.dir.path(), &id).unwrap();
    assert!(history::list(&conn, 100).unwrap().is_empty());
    assert!(history::undo(&conn, &f.index).unwrap().is_none());
    assert!(!f.path("a.org").exists());
}

#[test]
fn an_expired_restored_backup_preserves_live_history_and_redo_creates_a_new_bin_entry() {
    let f = Fixture::new();
    f.write("a.org", "[[file:.attachments/x.png]]");
    f.write(".attachments/x.png", b"image");
    f.delete("a.org");
    let conn = f.db.lock().unwrap();
    history::undo(&conn, &f.index).unwrap();
    conn.execute("UPDATE trash_entries SET expires_at = 0", []).unwrap();
    maintain(&conn, chrono::Utc::now().timestamp()).unwrap();
    assert_eq!(history::list(&conn, 100).unwrap().len(), 1);
    history::redo(&conn, &f.index).unwrap();
    assert!(!f.path("a.org").exists());
    assert!(!f.path(".attachments/x.png").exists());
    let entry = list(&conn, f.dir.path()).unwrap().remove(0);
    restore(&conn, f.dir.path(), &entry.id, &f.index).unwrap();
    assert!(f.path(".attachments/x.png").exists());
}

#[test]
fn a_queued_save_recovers_a_referenced_attachment_from_the_bin() {
    let f = Fixture::new();
    f.write("a.org", "[[file:.attachments/x.png]]");
    f.write("b.org", "Another note\n");
    f.write(".attachments/x.png", b"image");
    f.delete("a.org");
    assert!(!f.path(".attachments/x.png").exists());
    f.edit("b.org", "Another note\n[[file:.attachments/x.png]]\n");
    assert_eq!(fs::read(f.path(".attachments/x.png")).unwrap(), b"image");
}

#[test]
fn saving_a_draft_keeps_live_attachments_and_recycles_only_its_discarded_links() {
    let f = Fixture::new();
    f.write("a.org", "Note\n");
    f.write("#a.org#", "Note\n[[file:.attachments/discarded.png]]\n[[file:.attachments/kept.png]]\n");
    f.write(".attachments/discarded.png", b"discarded");
    f.write(".attachments/kept.png", b"kept");
    vault_write::save_note(&f.path("a.org"), "Note edited\n[[file:.attachments/kept.png]]\n", &f.index, &f.db).unwrap();
    assert!(f.path(".attachments/kept.png").exists());
    assert!(!f.path(".attachments/discarded.png").exists());
    let entry = f.entries().remove(0);
    f.restore(&entry.id).unwrap();
    let text = fs::read_to_string(f.path("a.org")).unwrap();
    assert!(text.starts_with("Note edited\n"));
    assert!(text.contains("[[file:.attachments/discarded.png]]"));
    assert_eq!(text.matches("[[file:.attachments/kept.png]]").count(), 1);
}

#[test]
fn replacing_a_heading_with_another_id_keeps_the_deleted_one_recoverable() {
    let f = Fixture::new();
    f.write("a.org", "* TODO Original\n:PROPERTIES:\n:CUSTOM_ID: old-id\n:END:\nOld description\n");
    f.edit("a.org", "* TODO Replacement\n:PROPERTIES:\n:CUSTOM_ID: new-id\n:END:\nNew description\n");
    let entries = f.entries();
    assert_eq!(entries.len(), 1);
    f.restore(&entries[0].id).unwrap();
    let text = fs::read_to_string(f.path("a.org")).unwrap();
    assert!(text.contains("Old description"));
    assert!(text.contains("New description"));
}

#[test]
fn restoring_a_heading_refuses_a_reused_binary_owner_file() {
    let f = Fixture::new();
    f.write("a.org", "* TODO Deleted\nOriginal\n* TODO Kept\n");
    f.edit("a.org", "* TODO Kept\n");
    let id = f.entries()[0].id.clone();
    f.write("a.org", [255, 254, 0, 13]);
    assert!(f.restore(&id).is_err());
    assert_eq!(fs::read(f.path("a.org")).unwrap(), [255, 254, 0, 13]);
    assert_eq!(f.entries().len(), 1);
}

#[test]
fn restoring_a_heading_requires_its_missing_owner_file_to_be_restored_first() {
    let f = Fixture::new();
    f.write("a.org", "* TODO Deleted\n* TODO Kept\n");
    f.edit("a.org", "* TODO Kept\n");
    let id = f.entries()[0].id.clone();
    fs::remove_file(f.path("a.org")).unwrap();
    assert!(f.restore(&id).is_err());
    assert!(!f.path("a.org").exists());
    assert_eq!(f.entries().len(), 1);
}

#[test]
fn deleting_an_archive_preserves_its_draft_even_if_it_contains_the_only_attachment_link() {
    let f = Fixture::new();
    let path = f.write("a.org_archive", "Saved archive\n");
    let draft = f.write("#a.org_archive#", "Draft\n[[file:.attachments/draft.png]]\n");
    f.write(".attachments/draft.png", [255, 3]);
    vault_write::recorded(
        &path,
        |_| HistoryEntryKind::FileDeleted { filename: "a.org_archive".into() },
        &f.index,
        &f.db,
        || {
            fs::remove_file(&path).unwrap();
            fs::remove_file(&draft).unwrap();
            Ok(())
        },
    )
    .unwrap();
    let id = f.entries()[0].id.clone();
    f.restore(&id).unwrap();
    assert_eq!(fs::read_to_string(draft).unwrap(), "Draft\n[[file:.attachments/draft.png]]\n");
    assert!(f.path(".attachments/draft.png").exists());
}

#[test]
fn a_failed_preimage_backup_prevents_the_destructive_write() {
    let f = Fixture::new();
    let path = f.write("a.org", "Unique irreplaceable content\n");
    f.db.lock()
        .unwrap()
        .execute_batch("CREATE TRIGGER fail_backup BEFORE INSERT ON trash_files BEGIN SELECT RAISE(FAIL, 'simulated full disk'); END;")
        .unwrap();
    let result = vault_write::recorded(
        &path,
        |_| HistoryEntryKind::FileDeleted { filename: "a.org".into() },
        &f.index,
        &f.db,
        || fs::remove_file(&path).map_err(|e| e.to_string()),
    );
    assert!(result.is_err());
    assert_eq!(fs::read_to_string(path).unwrap(), "Unique irreplaceable content\n");
}

#[cfg(unix)]
#[test]
fn symlinked_attachment_folders_never_delete_external_data() {
    let f = Fixture::new();
    let external = TempDir::new().unwrap();
    fs::write(external.path().join("x.png"), b"private").unwrap();
    std::os::unix::fs::symlink(external.path(), f.path(".attachments")).unwrap();
    f.write("a.org", "[[file:.attachments/x.png]]");
    f.delete("a.org");
    assert!(external.path().join("x.png").exists());
}

#[test]
fn durable_backups_use_full_sync_and_restore_the_connection_setting_on_success_and_failure() {
    let f = Fixture::new();
    let conn = f.db.lock().unwrap();
    conn.pragma_update(None, "synchronous", 1).unwrap();
    durable_transaction(&conn, |tx| {
        let sync: i64 = tx.pragma_query_value(None, "synchronous", |r| r.get(0)).unwrap();
        assert_eq!(sync, 2);
        tx.execute("INSERT INTO trash_entries(id, root, label, deleted_at, expires_at, attachments_dir) VALUES ('full', '/', 'full', 0, 1, '.attachments')", []).unwrap();
        Ok(())
    })
    .unwrap();
    assert_eq!(conn.pragma_query_value(None, "synchronous", |r| r.get::<_, i64>(0)).unwrap(), 1);
    let result: Result<(), String> = durable_transaction(&conn, |tx| {
        tx.execute("DELETE FROM trash_entries WHERE id = 'full'", []).unwrap();
        Err("simulated backup failure".into())
    });
    assert!(result.is_err());
    assert_eq!(conn.pragma_query_value(None, "synchronous", |r| r.get::<_, i64>(0)).unwrap(), 1);
    assert_eq!(conn.query_row("SELECT COUNT(*) FROM trash_entries WHERE id = 'full'", [], |r| r.get::<_, i64>(0)).unwrap(), 1);
    let tx = conn.unchecked_transaction().unwrap();
    assert!(durable_transaction(&conn, |_| Ok(())).is_err());
    drop(tx);
}

#[test]
fn undo_preflights_all_retained_files_before_restoring_any_of_them() {
    let f = Fixture::new();
    f.write("a.org", "[[file:.attachments/a.png]]\n[[file:.attachments/z.png]]\n");
    f.write(".attachments/a.png", b"original-a");
    f.write(".attachments/z.png", b"original-z");
    f.delete("a.org");
    f.write(".attachments/z.png", b"replacement");
    let conn = f.db.lock().unwrap();
    assert!(history::undo(&conn, &f.index).is_err());
    assert!(!f.path("a.org").exists());
    assert!(!f.path(".attachments/a.png").exists());
    assert_eq!(fs::read(f.path(".attachments/z.png")).unwrap(), b"replacement");
    assert_eq!(list(&conn, f.dir.path()).unwrap().len(), 1);
    assert!(!history::list(&conn, 1).unwrap()[0].undone);
}

#[test]
fn a_refile_preserves_attachment_and_does_not_create_a_deletion_entry() {
    let f = Fixture::new();
    let source = f.write("source.org", "* TODO Move\n:PROPERTIES:\n:CUSTOM_ID: move-id\n:END:\n[[file:.attachments/x.png]]\n");
    let destination = f.write("target.org", "* TODO Kept\n");
    f.write(".attachments/x.png", b"image");
    let original = fs::read_to_string(&source).unwrap();
    vault_write::edit_files_in(f.dir.path(), &[&source, &destination], None, &f.index, &f.db, |_| {
        Ok((vec![String::new(), format!("* TODO Kept\n{original}")], ()))
    })
    .unwrap();
    assert!(f.entries().is_empty());
    assert_eq!(fs::read(f.path(".attachments/x.png")).unwrap(), b"image");
}

#[test]
fn a_queued_autosave_recovers_recycled_bytes_and_its_draft_protects_them_from_gc() {
    let f = Fixture::new();
    f.write("a.org", "[[file:.attachments/x.png]]");
    f.write("b.org", "Saved note\n");
    f.write(".attachments/x.png", [255, 9]);
    f.delete("a.org");
    assert!(!f.path(".attachments/x.png").exists());
    vault_write::save_draft(&f.path("b.org"), "Draft\n[[file:.attachments/x.png]]\n", &f.index, &f.db).unwrap();
    assert_eq!(fs::read(f.path(".attachments/x.png")).unwrap(), [255, 9]);
    assert!(attachments::find_orphaned(&Policy::new(f.dir.path(), ".attachments", 31)).unwrap().is_empty());
    assert_eq!(fs::read_to_string(f.path("b.org")).unwrap(), "Saved note\n");
}

#[cfg(unix)]
#[test]
fn an_autosave_or_save_mentioning_a_missing_file_behind_an_unsafe_path_still_persists() {
    let f = Fixture::new();
    let external = TempDir::new().unwrap();
    std::os::unix::fs::symlink(external.path(), f.path("Bilder")).unwrap();
    let path = f.write("a.org", "Saved note\n");
    let text = "[[file:Bilder/missing.jpg]]\n[[file:.git/missing]]\n";
    vault_write::save_draft(&path, text, &f.index, &f.db).unwrap();
    assert_eq!(fs::read_to_string(f.path("#a.org#")).unwrap(), text);
    vault_write::save_note(&path, text, &f.index, &f.db).unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), text);
    assert!(!external.path().join("missing.jpg").exists());
}

#[test]
fn a_failing_attachment_recovery_never_drops_the_draft() {
    let f = Fixture::new();
    let path = f.write("a.org", "Saved note\n");
    let _ = std::thread::scope(|s| {
        s.spawn(|| {
            let _conn = f.db.lock().unwrap();
            panic!("poison the database lock");
        })
        .join()
    });
    assert!(f.db.is_poisoned());
    vault_write::save_draft(&path, "Typed\n[[file:.attachments/x.png]]\n", &f.index, &f.db).unwrap();
    assert_eq!(fs::read_to_string(f.path("#a.org#")).unwrap(), "Typed\n[[file:.attachments/x.png]]\n");
}

#[test]
fn reconciling_interrupted_writes_never_collects_files_without_the_vault_lock() {
    let f = Fixture::new();
    let path = f.write("a.org", "[[file:.attachments/x.png]]");
    f.write(".attachments/x.png", b"image");
    let protection = Protection::prepare(&Policy::new(f.dir.path(), ".attachments", 31), &[&path], &f.db).unwrap();
    fs::remove_file(path).unwrap();
    drop(protection);
    assert_eq!(f.entries().len(), 1);
    assert!(f.path(".attachments/x.png").exists());
}

#[test]
fn maintenance_does_not_expire_a_backup_that_cannot_be_reconciled() {
    let f = Fixture::new();
    let path = f.write("a.org", "Irreplaceable\n");
    let protection = Protection::prepare(&Policy::new(f.dir.path(), ".attachments", 31), &[&path], &f.db).unwrap();
    drop(protection);
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap(); // Reading the owner now fails with IsADirectory.
    let conn = f.db.lock().unwrap();
    maintain(&conn, chrono::Utc::now().timestamp() + 32 * 86400).unwrap();
    let count: i64 =
        conn.query_row("SELECT COUNT(*) FROM trash_files WHERE before_bytes = ?1", [b"Irreplaceable\n".as_slice()], |r| r.get(0)).unwrap();
    assert_eq!(count, 1);
}

#[test]
fn undoing_creation_keeps_the_deleted_file_recoverable_after_history_pruning() {
    let f = Fixture::new();
    let path = f.write("a.org", "Created note\n");
    let conn = f.db.lock().unwrap();
    history::record(
        &conn,
        HistoryEntryKind::FileCreated { filename: "a.org".into() },
        false,
        vec![FileSnapshot { path: path.to_string_lossy().into_owned(), before: None, after: Some("Created note\n".into()) }],
    )
    .unwrap();
    history::undo(&conn, &f.index).unwrap();
    assert!(!path.exists());
    history::store::prune(&conn, 0, 0).unwrap();
    let entry = list(&conn, f.dir.path()).unwrap().remove(0);
    restore(&conn, f.dir.path(), &entry.id, &f.index).unwrap();
    assert_eq!(fs::read_to_string(path).unwrap(), "Created note\n");
}

#[test]
fn equal_count_replacement_of_handwritten_headings_is_also_recoverable() {
    let f = Fixture::new();
    f.write("a.org", "* TODO Original\nOld description\n");
    f.edit("a.org", "* TODO Replacement\nNew description\n");
    let entry = f.entries().remove(0);
    f.restore(&entry.id).unwrap();
    let text = fs::read_to_string(f.path("a.org")).unwrap();
    assert!(text.contains("Old description"));
    assert!(text.contains("New description"));
}

#[test]
fn undo_redo_creation_recovers_attachments_and_only_shows_the_latest_deletion() {
    let f = Fixture::new();
    let content = "Created\n[[file:.attachments/x.png]]\n";
    let path = f.write("a.org", content);
    f.write(".attachments/x.png", [255, 7]);
    let conn = f.db.lock().unwrap();
    history::record(
        &conn,
        HistoryEntryKind::FileCreated { filename: "a.org".into() },
        false,
        vec![FileSnapshot { path: path.to_string_lossy().into_owned(), before: None, after: Some(content.into()) }],
    )
    .unwrap();
    history::undo(&conn, &f.index).unwrap();
    assert!(!path.exists());
    assert!(!f.path(".attachments/x.png").exists());
    assert_eq!(list(&conn, f.dir.path()).unwrap().len(), 1);
    history::redo(&conn, &f.index).unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), content);
    assert_eq!(fs::read(f.path(".attachments/x.png")).unwrap(), [255, 7]);
    assert!(list(&conn, f.dir.path()).unwrap().is_empty());
    history::undo(&conn, &f.index).unwrap();
    let entries = list(&conn, f.dir.path()).unwrap();
    assert_eq!(entries.len(), 1);
    history::store::prune(&conn, 0, 0).unwrap();
    restore(&conn, f.dir.path(), &entries[0].id, &f.index).unwrap();
    assert_eq!(fs::read_to_string(path).unwrap(), content);
    assert_eq!(fs::read(f.path(".attachments/x.png")).unwrap(), [255, 7]);
}

#[test]
fn existing_trash_databases_migrate_history_direction_without_losing_backups() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE trash_entries (
        id TEXT PRIMARY KEY, root TEXT NOT NULL, label TEXT NOT NULL, deleted_at INTEGER NOT NULL,
        expires_at INTEGER NOT NULL, pending INTEGER NOT NULL DEFAULT 1, restored INTEGER NOT NULL DEFAULT 0,
        history_id INTEGER, attachments_dir TEXT NOT NULL);
        INSERT INTO trash_entries VALUES ('old', '/', 'kept', 0, 1, 0, 0, NULL, '.attachments');",
    )
    .unwrap();
    init_schema(&conn).unwrap();
    init_schema(&conn).unwrap();
    assert_eq!(conn.query_row("SELECT history_reversed FROM trash_entries WHERE id = 'old'", [], |r| r.get::<_, i64>(0)).unwrap(), 0);
}
