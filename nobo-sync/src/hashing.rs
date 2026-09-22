//! BLAKE3 content hashing -- fast enough to re-hash a whole vault on every
//! sync run without it being the bottleneck, which is the whole point of
//! using it for change detection instead of relying on mtimes (mtimes lie
//! across cloud-synced filesystems, different devices' clocks, etc.).

use std::path::Path;

pub fn hash_bytes(content: &[u8]) -> String {
    blake3::hash(content).to_hex().to_string()
}

pub fn hash_file(path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("failed to read {}: {e}", path.display()))?;
    Ok(hash_bytes(&bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_content_hashes_the_same() {
        assert_eq!(hash_bytes(b"hello"), hash_bytes(b"hello"));
    }

    #[test]
    fn different_content_hashes_differently() {
        assert_ne!(hash_bytes(b"hello"), hash_bytes(b"world"));
    }

    #[test]
    fn hash_file_matches_hash_bytes() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("a.org");
        std::fs::write(&path, "content").unwrap();

        assert_eq!(hash_file(&path).unwrap(), hash_bytes(b"content"));
    }
}
