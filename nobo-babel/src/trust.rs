use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use nobo_core::config::settings::config_dir;

/// blake3, not `DefaultHasher`.
///
/// This value is the whole gate on running someone else's code, and the
/// person who writes the shared file supplies BOTH documents: the harmless
/// one the user trusts, and the one that arrives later over vault sync.
/// That is a birthday collision, not a second preimage -- and
/// `DefaultHasher` is SipHash-1-3 with a fixed zero key at 64 bits, so it
/// is computable offline in about 2^32 evaluations, with as much padding
/// as wanted inside whitespace or a `#+COMMENT:` line.
///
/// std also documents its algorithm as unspecified across releases, which
/// is a poor property for a value that is written to disk.
fn content_hash(content: &str) -> String {
    blake3::hash(content.as_bytes()).to_hex().to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TrustEntry {
    /// A hex blake3 digest.
    ///
    /// An entry written by an older build holds a number here, so the
    /// whole store fails to parse and `load` falls back to an empty one:
    /// every file has to be approved once more. That is the right outcome
    /// of tightening this gate, not a migration to write -- the old
    /// decisions were made against a digest that can be collided, and
    /// carrying them over would carry the weakness with them.
    content_hash: String,
    trusted_at: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct TrustFile {
    #[serde(default)]
    files: HashMap<String, TrustEntry>,
}

fn store_path() -> PathBuf {
    config_dir().join("trusted_files.json")
}

fn load(path: &Path) -> TrustFile {
    fs::read_to_string(path)
        .ok()
        .and_then(|content| serde_json::from_str(&content).ok())
        .unwrap_or_default()
}

fn save(path: &Path, store: &TrustFile) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(store).unwrap_or_default();
    // A truncated trust store reads as "nothing is trusted", so every
    // block the user already approved asks again.
    nobo_core::utils::fs_utils::write_atomic(path, &content)
}

fn is_trusted_at(path: &Path, file_path: &str, content: &str) -> bool {
    load(path)
        .files
        .get(file_path)
        .is_some_and(|entry| entry.content_hash == content_hash(content))
}

fn trust_at(path: &Path, file_path: &str, content: &str) -> std::io::Result<()> {
    let mut store = load(path);
    store.files.insert(
        file_path.to_string(),
        TrustEntry {
            content_hash: content_hash(content),
            trusted_at: chrono::Local::now().to_rfc3339(),
        },
    );
    save(path, &store)
}

/// Whether `file_path`'s exact current `content` has already been
/// explicitly trusted for code execution. Keyed by a content hash (not just
/// the path), so any edit to the file -- even outside a SRC block --
/// invalidates a stale trust decision instead of silently carrying it over
/// to different code.
pub fn is_trusted(file_path: &str, content: &str) -> bool {
    is_trusted_at(&store_path(), file_path, content)
}

/// Records `file_path`'s current `content` as trusted. Never touches the
/// file itself -- lives entirely in the app's own config directory so a
/// trust decision can't be smuggled along when the .org file is shared.
pub fn trust(file_path: &str, content: &str) -> std::io::Result<()> {
    trust_at(&store_path(), file_path, content)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn store_file(dir: &TempDir) -> PathBuf {
        dir.path().join("trusted_files.json")
    }

    #[test]
    fn untrusted_by_default() {
        let dir = TempDir::new().unwrap();
        assert!(!is_trusted_at(&store_file(&dir), "/a.org", "echo hi"));
    }

    #[test]
    fn trusting_persists_across_loads() {
        let dir = TempDir::new().unwrap();
        let path = store_file(&dir);
        trust_at(&path, "/a.org", "echo hi").unwrap();
        assert!(is_trusted_at(&path, "/a.org", "echo hi"));
    }

    #[test]
    fn content_change_invalidates_trust() {
        let dir = TempDir::new().unwrap();
        let path = store_file(&dir);
        trust_at(&path, "/a.org", "echo hi").unwrap();
        assert!(!is_trusted_at(&path, "/a.org", "echo hi -- changed"));
    }

    #[test]
    fn trust_is_scoped_per_file_path() {
        let dir = TempDir::new().unwrap();
        let path = store_file(&dir);
        trust_at(&path, "/a.org", "echo hi").unwrap();
        assert!(!is_trusted_at(&path, "/b.org", "echo hi"));
    }
}
