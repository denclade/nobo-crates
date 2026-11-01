// SPDX-FileCopyrightText: 2026 Dennis Burgermeister
// SPDX-License-Identifier: GPL-3.0-or-later

use nobo_core::config::settings::Config;
use nobo_core::core::attachments::{self, AttachmentCleanupResult, OrphanedAttachment};
use nobo_core::core::indexer::IndexManager;
use nobo_core::core::trash::Policy;
use nobo_core::core::trash::{self, TrashEntry};
use nobo_core::core::vault_write;
use rusqlite::Connection;
use std::sync::Mutex;

pub fn trash_list(db: &Mutex<Connection>) -> Result<Vec<TrashEntry>, String> {
    let root = Config::require_vault_root()?;
    let conn = db.lock().map_err(|e| e.to_string())?;
    trash::list(&conn, &root)
}

pub fn trash_restore(id: String, db: &Mutex<Connection>, index: &IndexManager) -> Result<(), String> {
    let root = Config::require_vault_root()?;
    vault_write::with_vault_lock(&root, || {
        let conn = db.lock().map_err(|e| e.to_string())?;
        trash::restore(&conn, &root, &id, index)
    })
}

pub fn trash_purge(id: String, db: &Mutex<Connection>) -> Result<(), String> {
    let root = Config::require_vault_root()?;
    let conn = db.lock().map_err(|e| e.to_string())?;
    trash::purge(&conn, &root, &id)
}

fn attachment_policy() -> Result<Policy, String> {
    let config = Config::load().ok_or_else(|| nobo_core::t!("Keine Konfiguration gefunden.", "No configuration found."))?;
    Ok(Policy::new(&config.vault_root()?, &config.attachments_dir, config.trash_retention_days))
}

pub fn find_orphaned_attachments() -> Result<Vec<OrphanedAttachment>, String> {
    let policy = attachment_policy()?;
    vault_write::with_vault_lock(&policy.root, || attachments::find_orphaned(&policy).map_err(|e| e.to_string()))
}

pub fn recycle_orphaned_attachments(
    selected: Vec<OrphanedAttachment>,
    db: &Mutex<Connection>,
    index: &IndexManager,
) -> Result<AttachmentCleanupResult, String> {
    attachments::recycle_orphaned(&attachment_policy()?, &selected, index, db)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{commands_headings, commands_notes};
    use nobo_core::core::{history, models::HeadingTarget};
    use std::fs;

    fn fixture() -> (tempfile::TempDir, IndexManager, Mutex<Connection>) {
        let dir = tempfile::tempdir().unwrap();
        let index = IndexManager::new();
        index.scan_directory(dir.path());
        let conn = Connection::open_in_memory().unwrap();
        history::store::init_schema(&conn).unwrap();
        (dir, index, Mutex::new(conn))
    }

    #[test]
    fn single_and_bulk_heading_commands_recycle_their_attachments() {
        for bulk in [false, true] {
            let (dir, index, db) = fixture();
            let path = dir.path().join("tasks.org");
            fs::create_dir(dir.path().join(".attachments")).unwrap();
            fs::write(dir.path().join(".attachments/x.png"), b"image").unwrap();
            fs::write(&path, "* TODO Delete\n[[file:.attachments/x.png]]\n** TODO Child\n* TODO Keep\n").unwrap();
            let target = HeadingTarget { line_no: 1, title: Some("Delete".into()), custom_id: None };
            if bulk {
                commands_headings::bulk_delete_headings(path.to_string_lossy().into_owned(), vec![target], &index, &db).unwrap();
            } else {
                commands_headings::delete_heading_in_file(path.to_string_lossy().into_owned(), target, &index, &db).unwrap();
            }
            assert_eq!(fs::read_to_string(&path).unwrap(), "* TODO Keep\n");
            assert!(!dir.path().join(".attachments/x.png").exists());
            let conn = db.lock().unwrap();
            let entry = trash::list(&conn, dir.path()).unwrap().remove(0);
            trash::restore(&conn, dir.path(), &entry.id, &index).unwrap();
            assert!(fs::read_to_string(&path).unwrap().contains("** TODO Child"));
            assert!(dir.path().join(".attachments/x.png").exists());
        }
    }

    #[test]
    fn deleting_a_note_also_recovers_its_unsaved_draft_and_draft_only_attachments() {
        let (dir, index, db) = fixture();
        fs::create_dir(dir.path().join(".attachments")).unwrap();
        fs::write(dir.path().join("a.org"), "Saved note\n").unwrap();
        fs::write(dir.path().join("#a.org#"), "Unsaved\n[[file:.attachments/x.png]]\n").unwrap();
        fs::write(dir.path().join(".attachments/x.png"), b"image").unwrap();
        commands_notes::delete_note(dir.path().join("a.org").to_string_lossy().into_owned(), &index, &db).unwrap();
        assert!(!dir.path().join("#a.org#").exists());
        assert!(!dir.path().join(".attachments/x.png").exists());
        let conn = db.lock().unwrap();
        let entry = trash::list(&conn, dir.path()).unwrap().remove(0);
        trash::restore(&conn, dir.path(), &entry.id, &index).unwrap();
        assert!(fs::read_to_string(dir.path().join("#a.org#")).unwrap().contains("Unsaved"));
        assert!(dir.path().join(".attachments/x.png").exists());
    }

    #[test]
    fn discarding_a_draft_recycles_only_attachments_not_used_by_the_saved_note() {
        let (dir, index, db) = fixture();
        fs::create_dir(dir.path().join(".attachments")).unwrap();
        fs::write(dir.path().join("a.org"), "[[file:.attachments/kept.png]]\n").unwrap();
        fs::write(dir.path().join("#a.org#"), "[[file:.attachments/kept.png]]\n[[file:.attachments/x.png]]\n").unwrap();
        fs::write(dir.path().join(".attachments/kept.png"), b"kept").unwrap();
        fs::write(dir.path().join(".attachments/x.png"), b"image").unwrap();
        commands_notes::discard_note_temp(dir.path().join("a.org").to_string_lossy().into_owned(), &index, &db).unwrap();
        assert!(dir.path().join(".attachments/kept.png").exists());
        assert!(!dir.path().join(".attachments/x.png").exists());
        assert_eq!(trash::list(&db.lock().unwrap(), dir.path()).unwrap().len(), 1);
    }
}
