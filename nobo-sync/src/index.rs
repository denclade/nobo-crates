// SPDX-FileCopyrightText: 2026 Dennis Burgermeister
// SPDX-License-Identifier: GPL-3.0-or-later

//! The one per-account index blob every device downloads and uploads: the
//! sole record of path, content hash and file key. Normally encrypted, so
//! this is the only place those mappings exist at all.
//!
//! The exception is WebDAV's opt-in unencrypted mode, for a user already
//! syncing a plaintext org vault. `wrapped_key: None` marks such an entry.

use serde::{Deserialize, Serialize};
use std::io::Cursor;

use super::crypto::{self, FileKey, MasterKey, WrappedKey};
use super::diff::RemoteEntry;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexEntry {
    pub path: String,
    pub content_hash: String,
    /// `None` for a file stored unencrypted (WebDAV opt-out only, see the
    /// module doc comment) -- there's no FEK to wrap when the content was
    /// never encrypted in the first place.
    pub wrapped_key: Option<WrappedKey>,
    pub size: u64,
    pub updated_at: String,
    /// Set when a device tried to download this entry and the blob was
    /// gone from storage.
    ///
    /// The entry stays in the index rather than being dropped, because
    /// dropping it is a decision taken in the name of every OTHER device
    /// too: the pruned index gets published, and a device that still holds
    /// the file reads the missing entry as "deleted elsewhere" and unlinks
    /// its copy -- the last one. Marked instead, so a holder re-uploads it
    /// (see `diff::diff_one`) and everyone else knows why the download
    /// keeps being skipped.
    ///
    /// `#[serde(default)]` so an index written by an older build still
    /// parses; `derive_file_key` is deterministic from the content hash,
    /// so a holder can re-derive the FEK and re-upload without a new one.
    #[serde(default)]
    pub blob_missing: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SyncIndex {
    pub entries: Vec<IndexEntry>,
}

impl SyncIndex {
    pub fn find(&self, path: &str) -> Option<&IndexEntry> {
        self.entries.iter().find(|e| e.path == path)
    }

    pub fn upsert(&mut self, entry: IndexEntry) {
        if let Some(existing) = self.entries.iter_mut().find(|e| e.path == entry.path) {
            *existing = entry;
        } else {
            self.entries.push(entry);
        }
    }

    pub fn remove(&mut self, path: &str) {
        self.entries.retain(|e| e.path != path);
    }

    /// Records that this path's blob is gone from storage, keeping the
    /// entry itself -- see `IndexEntry::blob_missing`.
    pub fn mark_blob_missing(&mut self, path: &str) {
        if let Some(entry) = self.entries.iter_mut().find(|e| e.path == path) {
            entry.blob_missing = true;
        }
    }

    /// Whether any entry other than PATH points at HASH.
    ///
    /// Storage is deduplicated per content hash, so deleting a blob that a
    /// second path still references deletes that file too. Two empty
    /// notes, a duplicated note before its first edit, the same attachment
    /// in two folders -- and a rename, which is a Delete plus a Create
    /// that dedupes onto the very blob the delete is about to remove.
    pub fn hash_referenced_elsewhere(&self, hash: &str, except_path: &str) -> bool {
        self.entries.iter().any(|e| e.content_hash == hash && e.path != except_path)
    }

    pub fn to_remote_entries(&self) -> Vec<RemoteEntry> {
        self.entries
            .iter()
            .map(|e| RemoteEntry { path: e.path.clone(), content_hash: e.content_hash.clone(), blob_missing: e.blob_missing })
            .collect()
    }
}

/// The index's own encryption key is deterministically re-derived from the
/// master key (see `crypto::derive_index_key`), not a per-file FEK -- small
/// enough to encrypt/decrypt entirely in memory via `Cursor`, unlike the
/// (potentially large) file content `encrypt_stream`/`decrypt_stream` are
/// really for.
pub fn encrypt(master_key: &MasterKey, index: &SyncIndex) -> Result<Vec<u8>, String> {
    let index_key: FileKey = crypto::derive_index_key(master_key);
    let plaintext = serde_json::to_vec(index).map_err(|e| format!("failed to serialize index: {e}"))?;

    let mut ciphertext = Vec::new();
    crypto::encrypt_stream(&index_key, &mut Cursor::new(plaintext), &mut ciphertext)?;
    Ok(ciphertext)
}

pub fn decrypt(master_key: &MasterKey, ciphertext: &[u8]) -> Result<SyncIndex, String> {
    let index_key: FileKey = crypto::derive_index_key(master_key);

    let mut plaintext = Vec::new();
    crypto::decrypt_stream(&index_key, &mut Cursor::new(ciphertext), &mut plaintext)?;

    serde_json::from_slice(&plaintext).map_err(|e| format!("failed to parse decrypted index: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_entry(path: &str, hash: &str) -> IndexEntry {
        IndexEntry {
            path: path.to_string(),
            content_hash: hash.to_string(),
            wrapped_key: Some(WrappedKey { nonce: [0u8; 12], ciphertext: vec![0u8; 48] }),
            size: 123,
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            blob_missing: false,
        }
    }

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let master = crypto::derive_master_key("hunter2", &crypto::generate_salt()).unwrap();
        let mut index = SyncIndex::default();
        index.upsert(sample_entry("a.org", "hash1"));
        index.upsert(sample_entry("sub/b.org", "hash2"));

        let encrypted = encrypt(&master, &index).unwrap();
        let decrypted = decrypt(&master, &encrypted).unwrap();

        assert_eq!(decrypted.entries.len(), 2);
        assert_eq!(decrypted.find("a.org").unwrap().content_hash, "hash1");
    }

    #[test]
    fn decrypt_fails_with_wrong_master_key() {
        let master_a = crypto::derive_master_key("password a", &crypto::generate_salt()).unwrap();
        let master_b = crypto::derive_master_key("password b", &crypto::generate_salt()).unwrap();

        let mut index = SyncIndex::default();
        index.upsert(sample_entry("a.org", "hash1"));
        let encrypted = encrypt(&master_a, &index).unwrap();

        assert!(decrypt(&master_b, &encrypted).is_err());
    }

    #[test]
    fn upsert_replaces_an_existing_entry_for_the_same_path() {
        let mut index = SyncIndex::default();
        index.upsert(sample_entry("a.org", "hash1"));
        index.upsert(sample_entry("a.org", "hash2"));

        assert_eq!(index.entries.len(), 1);
        assert_eq!(index.find("a.org").unwrap().content_hash, "hash2");
    }

    #[test]
    fn remove_drops_the_entry() {
        let mut index = SyncIndex::default();
        index.upsert(sample_entry("a.org", "hash1"));
        index.remove("a.org");
        assert!(index.find("a.org").is_none());
    }
}
