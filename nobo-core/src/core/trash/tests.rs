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
