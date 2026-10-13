//! E2EE primitives for the cloud sync module (see docs/sync.md).
//!
//! Envelope encryption, not "one master key encrypts everything directly":
//! the master key (derived from the user's password) only ever wraps/unwraps
//! small, random, per-file File Encryption Keys (FEKs) -- it never touches
//! actual note/attachment content itself. This buys two things at once:
//!
//! 1. **Nonce safety.** AES-GCM/ChaCha20-Poly1305 both catastrophically fail
//!    (full plaintext recovery) if a (key, nonce) pair is ever reused. A
//!    vault synced daily for years, all encrypted under one static key,
//!    makes that a real risk to manage carefully. Randomly generating a
//!    fresh FEK per file instead means each file's encryption uses its own
//!    key -- nonce reuse would only ever matter *within* one file's own
//!    stream, which `encrypt_stream`'s STREAM construction already
//!    guarantees can't happen (see below).
//! 2. **Streaming.** Wrapping a 32-byte FEK is one cheap, one-shot AEAD
//!    call regardless of file size; the actual (potentially large PDF)
//!    content is encrypted separately, chunk by chunk, without ever
//!    materializing the whole plaintext or ciphertext in memory at once.
//!
//! The streaming functions use a STREAM construction, where each chunk's
//! nonce is derived internally from the stream nonce, a counter and a
//! last-chunk bit -- so unlike hand-rolling "nonce = base ^ counter" a
//! caller cannot get it subtly wrong.

use chacha20poly1305::aead::generic_array::GenericArray;
use chacha20poly1305::aead::stream::{DecryptorBE32, EncryptorBE32};
use chacha20poly1305::{aead::Aead, ChaCha20Poly1305, KeyInit};
use rand::RngCore;
use std::io::{Read, Write};
use zeroize::Zeroizing;

pub const SALT_LEN: usize = 16;
pub const KEY_LEN: usize = 32;
/// STREAM nonce -- 7 bytes, not the cipher's full 12-byte nonce: the STREAM
/// construction itself reserves the remaining 5 bytes (4-byte big-endian
/// counter + 1-byte "last chunk" flag), see `EncryptorBE32`/`DecryptorBE32`.
const STREAM_NONCE_LEN: usize = 7;
/// Plaintext read per chunk during streaming encryption -- large enough
/// that per-chunk AEAD overhead (16-byte tag) is negligible, small enough
/// that memory use stays flat regardless of file size (the whole point of
/// streaming a "large PDF" instead of loading it whole, per the original
/// requirement).
const CHUNK_LEN: usize = 64 * 1024;

pub type MasterKey = Zeroizing<[u8; KEY_LEN]>;
pub type FileKey = Zeroizing<[u8; KEY_LEN]>;

/// A File Encryption Key, wrapped (encrypted) under the master key -- this
/// is what actually gets persisted (in the encrypted index, see
/// index.rs), never the FEK in the clear.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WrappedKey {
    /// Random, one-shot -- safe as plain random per RFC 8439 guidance for a
    /// single small (32-byte) message; this is not the same nonce-reuse
    /// risk the STREAM construction above exists to avoid, since it's used
    /// exactly once per FEK, and a vault has orders of magnitude fewer
    /// files than "gigabytes of daily-synced chunks over years".
    pub nonce: [u8; 12],
    /// The wrapped 32-byte key plus its 16-byte Poly1305 tag (48 bytes).
    pub ciphertext: Vec<u8>,
}

fn map_err<E: std::fmt::Display>(context: &str) -> impl Fn(E) -> String + '_ {
    move |e| format!("{context}: {e}")
}

/// Random salt -- for tests/callers that don't have (or don't want to rely
/// on) a stable identifier; the actual account flow uses `derive_salt`
/// below instead, see its own doc comment for why.
pub fn generate_salt() -> [u8; SALT_LEN] {
    let mut salt = [0u8; SALT_LEN];
    rand::rngs::OsRng.fill_bytes(&mut salt);
    salt
}

/// Deterministic per-account salt derived from a stable public identifier
/// (email for PocketBase; server URL + username for WebDAV) -- avoids the
/// classic chicken-and-egg problem of needing a server round-trip to fetch
/// a random salt on a new device *before* the user has authenticated
/// there. A salt only needs to be unique per account, not secret or
/// unpredictable, so deriving it from something already known to the user
/// (before they've typed their password even) achieves that with no extra
/// plumbing or server-side storage at all.
pub fn derive_salt(identity: &str) -> [u8; SALT_LEN] {
    let hash = blake3::hash(identity.trim().to_lowercase().as_bytes());
    let mut salt = [0u8; SALT_LEN];
    salt.copy_from_slice(&hash.as_bytes()[..SALT_LEN]);
    salt
}

/// Argon2id, OWASP's current *minimum* recommended profile (m=19 MiB,
/// t=2, p=1) rather than a heavier desktop-oriented one -- this runs on
/// first unlock on whatever phone the user has, and Argon2's memory-hardness
/// is a real battery/latency cost there, not just a desktop nicety. Argon2id
/// specifically (not Argon2i/d alone) for its hybrid resistance to both
/// side-channel and GPU/ASIC cracking.
pub fn derive_master_key(password: &str, salt: &[u8; SALT_LEN]) -> Result<MasterKey, String> {
    use argon2::{Algorithm, Argon2, Params, Version};

    let params = Params::new(19 * 1024, 2, 1, Some(KEY_LEN)).map_err(map_err("invalid Argon2 params"))?;
    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);

    let mut key = [0u8; KEY_LEN];
    argon2.hash_password_into(password.as_bytes(), salt, &mut key).map_err(map_err("key derivation failed"))?;
    Ok(Zeroizing::new(key))
}

/// For "remember this login" (see session.rs/nobo_sync::commands) -- the
/// derived master key, OS-keychain-stored instead of re-derived from the
/// password on every launch. Hex rather than raw bytes purely because it's
/// going into a plain-string keychain entry/JSON blob; the keychain itself
/// (not this encoding) is what actually protects it at rest.
pub fn master_key_to_hex(key: &MasterKey) -> String {
    hex::encode(key.as_ref())
}

pub fn master_key_from_hex(s: &str) -> Result<MasterKey, String> {
    let bytes = hex::decode(s).map_err(map_err("invalid stored key encoding"))?;
    let array: [u8; KEY_LEN] = bytes.try_into().map_err(|_| "stored key has the wrong length".to_string())?;
    Ok(Zeroizing::new(array))
}

/// A fresh, random File Encryption Key -- one per file, never reused across
/// files (see the module doc for why).
pub fn generate_file_key() -> FileKey {
    let mut key = [0u8; KEY_LEN];
    rand::rngs::OsRng.fill_bytes(&mut key);
    Zeroizing::new(key)
}

pub fn wrap_key(master_key: &MasterKey, file_key: &FileKey) -> Result<WrappedKey, String> {
    let cipher = ChaCha20Poly1305::new(GenericArray::from_slice(master_key.as_ref()));
    let mut nonce_bytes = [0u8; 12];
    rand::rngs::OsRng.fill_bytes(&mut nonce_bytes);
    let nonce = GenericArray::from_slice(&nonce_bytes);

    let file_key_bytes: &[u8] = file_key.as_ref();
    let ciphertext = cipher.encrypt(nonce, file_key_bytes).map_err(map_err("failed to wrap file key"))?;

    Ok(WrappedKey { nonce: nonce_bytes, ciphertext })
}

/// Fails (rather than returning garbage) if `master_key` is wrong or
/// `wrapped` was tampered with -- ChaCha20-Poly1305's authentication tag
/// makes both cases indistinguishable from each other, which is the
/// correct behavior here (nothing downstream should be able to tell "wrong
/// password" from "corrupted/tampered data" from the error alone, since
/// leaking that distinction is a minor oracle).
pub fn unwrap_key(master_key: &MasterKey, wrapped: &WrappedKey) -> Result<FileKey, String> {
    let cipher = ChaCha20Poly1305::new(GenericArray::from_slice(master_key.as_ref()));
    let nonce = GenericArray::from_slice(&wrapped.nonce);

    let plaintext = cipher
        .decrypt(nonce, wrapped.ciphertext.as_slice())
        .map_err(map_err("failed to unwrap file key (wrong password or corrupted data)"))?;

    let key: [u8; KEY_LEN] = plaintext.try_into().map_err(|_| "unwrapped key has the wrong length".to_string())?;
    Ok(Zeroizing::new(key))
}

/// The only value that leaves the device for authentication, derived from
/// the master key rather than the password -- so it is one-way with
/// respect to both. Capturing it lets a server operator authenticate as
/// the user but decrypt nothing.
///
/// Uses BLAKE3's key-derivation mode with a fixed, versioned context
/// string, so it can never collide with another derivation from the same
/// master key.
pub fn derive_auth_secret(master_key: &MasterKey) -> String {
    let derived = blake3::derive_key("nobo sync auth-secret v1", master_key.as_ref());
    hex::encode(derived)
}

/// The key that encrypts the one per-account index blob (see
/// index.rs) -- deterministically re-derivable from the master
/// key alone (same domain-separation technique as `derive_auth_secret`,
/// different context string so the two can never collide), so unlike a
/// file's FEK, the index never needs its own `WrappedKey` stored anywhere:
/// any device that can derive `master_key` can re-derive this on demand.
pub fn derive_index_key(master_key: &MasterKey) -> FileKey {
    Zeroizing::new(blake3::derive_key("nobo sync index-key v1", master_key.as_ref()))
}

/// A file's FEK, derived deterministically from the master key and content
/// hash rather than randomly.
///
/// Required by content-addressed storage: two paths with identical
/// plaintext share a content hash, so only the first ciphertext reaches
/// the server. A random FEK per upload leaves the second path's index
/// entry wrapping a key that encrypted nothing stored -- unwrapping
/// succeeds, decrypting the actual blob never does.
///
/// Deriving it from the content hash means every entry sharing that hash
/// shares the key that really encrypted the stored ciphertext. The nonce
/// stays freshly random per call, so semantic security is unchanged.
pub fn derive_file_key(master_key: &MasterKey, content_hash: &str) -> FileKey {
    let mut input = Vec::with_capacity(KEY_LEN + content_hash.len());
    input.extend_from_slice(master_key.as_ref());
    input.extend_from_slice(content_hash.as_bytes());
    Zeroizing::new(blake3::derive_key("nobo sync file-key v1", &input))
}

/// Encrypts `reader` into `writer` in `CHUNK_LEN`-sized pieces under
/// `file_key`, writing a small plaintext header (just the STREAM nonce --
/// not secret) first so the resulting blob is self-describing and
/// `decrypt_stream` doesn't need the nonce passed in separately.
pub fn encrypt_stream(file_key: &FileKey, reader: &mut impl Read, writer: &mut impl Write) -> Result<(), String> {
    let mut nonce_bytes = [0u8; STREAM_NONCE_LEN];
    rand::rngs::OsRng.fill_bytes(&mut nonce_bytes);
    writer.write_all(&nonce_bytes).map_err(map_err("failed to write stream header"))?;

    let cipher = ChaCha20Poly1305::new(GenericArray::from_slice(file_key.as_ref()));
    let mut encryptor = EncryptorBE32::from_aead(cipher, GenericArray::from_slice(&nonce_bytes));

    let mut buf = vec![0u8; CHUNK_LEN];
    let mut filled = 0usize;

    loop {
        let n = reader.read(&mut buf[filled..]).map_err(map_err("failed to read plaintext"))?;
        filled += n;

        if filled < CHUNK_LEN && n != 0 {
            // Short read, not necessarily EOF (e.g. a pipe) -- keep filling
            // this chunk rather than treating it as the final one early.
            continue;
        }

        if filled == CHUNK_LEN {
            let ciphertext = encryptor.encrypt_next(buf.as_slice()).map_err(map_err("stream encryption failed"))?;
            writer.write_all(&ciphertext).map_err(map_err("failed to write ciphertext chunk"))?;
            filled = 0;
        } else {
            // n == 0 here: true EOF with a partial (possibly empty) final chunk.
            let ciphertext = encryptor.encrypt_last(&buf[..filled]).map_err(map_err("stream encryption failed (final chunk)"))?;
            writer.write_all(&ciphertext).map_err(map_err("failed to write final ciphertext chunk"))?;
            break;
        }
    }

    Ok(())
}

pub fn decrypt_stream(file_key: &FileKey, reader: &mut impl Read, writer: &mut impl Write) -> Result<(), String> {
    let mut nonce_bytes = [0u8; STREAM_NONCE_LEN];
    reader.read_exact(&mut nonce_bytes).map_err(map_err("failed to read stream header"))?;

    let cipher = ChaCha20Poly1305::new(GenericArray::from_slice(file_key.as_ref()));
    let mut decryptor = DecryptorBE32::from_aead(cipher, GenericArray::from_slice(&nonce_bytes));

    // Each ciphertext chunk is exactly CHUNK_LEN + 16 (Poly1305 tag) bytes,
    // except the last one, which is whatever's left (>= 16 bytes for the
    // tag alone, even for an originally-empty file).
    const CIPHER_CHUNK_LEN: usize = CHUNK_LEN + 16;
    let mut buf = vec![0u8; CIPHER_CHUNK_LEN];
    let mut filled = 0usize;

    loop {
        let n = reader.read(&mut buf[filled..]).map_err(map_err("failed to read ciphertext"))?;
        filled += n;

        if filled < CIPHER_CHUNK_LEN && n != 0 {
            continue;
        }

        if filled == CIPHER_CHUNK_LEN {
            let plaintext = decryptor
                .decrypt_next(buf.as_slice())
                .map_err(map_err("stream decryption failed (wrong key or corrupted/tampered data)"))?;
            writer.write_all(&plaintext).map_err(map_err("failed to write plaintext chunk"))?;
            filled = 0;
        } else {
            let plaintext = decryptor
                .decrypt_last(&buf[..filled])
                .map_err(map_err("stream decryption failed (wrong key, corrupted data, or truncated file)"))?;
            writer.write_all(&plaintext).map_err(map_err("failed to write final plaintext chunk"))?;
            break;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_master_key_is_deterministic() {
        let salt = generate_salt();
        let a = derive_master_key("correct horse battery staple", &salt).unwrap();
        let b = derive_master_key("correct horse battery staple", &salt).unwrap();
        assert_eq!(a.as_ref(), b.as_ref());
    }

    #[test]
    fn derive_salt_is_deterministic_and_case_insensitive() {
        assert_eq!(derive_salt("user@example.com"), derive_salt("User@Example.com "));
    }

    #[test]
    fn derive_salt_differs_by_identity() {
        assert_ne!(derive_salt("alice@example.com"), derive_salt("bob@example.com"));
    }

    #[test]
    fn derive_auth_secret_is_deterministic() {
        let master = derive_master_key("hunter2", &generate_salt()).unwrap();
        assert_eq!(derive_auth_secret(&master), derive_auth_secret(&master));
    }

    #[test]
    fn derive_file_key_is_deterministic_for_the_same_content_hash() {
        let master = derive_master_key("hunter2", &generate_salt()).unwrap();
        let a = derive_file_key(&master, "abc123");
        let b = derive_file_key(&master, "abc123");
        assert_eq!(a.as_ref(), b.as_ref());
    }

    /// The exact bug this function exists to fix: two files with identical
    /// content (same content_hash) MUST derive the same FEK, or the second
    /// one's index entry ends up wrapping a key that doesn't match
    /// whatever ciphertext content-addressed dedup actually left on the
    /// server (see providers::upload_file and this function's own doc
    /// comment).
    #[test]
    fn derive_file_key_differs_by_content_hash() {
        let master = derive_master_key("hunter2", &generate_salt()).unwrap();
        let a = derive_file_key(&master, "hash-a");
        let b = derive_file_key(&master, "hash-b");
        assert_ne!(a.as_ref(), b.as_ref());
    }

    #[test]
    fn derive_file_key_differs_from_index_key() {
        let master = derive_master_key("hunter2", &generate_salt()).unwrap();
        assert_ne!(derive_file_key(&master, "abc123").as_ref(), derive_index_key(&master).as_ref());
    }

    #[test]
    fn master_key_hex_roundtrip() {
        let master = derive_master_key("hunter2", &generate_salt()).unwrap();
        let hex = master_key_to_hex(&master);
        let restored = master_key_from_hex(&hex).unwrap();
        assert_eq!(master.as_ref(), restored.as_ref());
    }

    #[test]
    fn master_key_from_hex_rejects_garbage() {
        assert!(master_key_from_hex("not hex at all").is_err());
        assert!(master_key_from_hex("deadbeef").is_err(), "too short");
    }

    #[test]
    fn derive_index_key_is_deterministic_and_differs_from_auth_secret() {
        let master = derive_master_key("hunter2", &generate_salt()).unwrap();
        assert_eq!(derive_index_key(&master).as_ref(), derive_index_key(&master).as_ref());
        assert_ne!(hex::encode(derive_index_key(&master).as_ref()), derive_auth_secret(&master));
    }

    #[test]
    fn derive_auth_secret_differs_from_the_master_key_itself() {
        let master = derive_master_key("hunter2", &generate_salt()).unwrap();
        let auth_secret = derive_auth_secret(&master);
        assert_ne!(auth_secret, hex::encode(master.as_ref()), "must not just be the master key re-encoded");
    }

    #[test]
    fn derive_auth_secret_differs_between_different_master_keys() {
        let master_a = derive_master_key("password a", &generate_salt()).unwrap();
        let master_b = derive_master_key("password b", &generate_salt()).unwrap();
        assert_ne!(derive_auth_secret(&master_a), derive_auth_secret(&master_b));
    }

    #[test]
    fn derive_master_key_differs_by_password() {
        let salt = generate_salt();
        let a = derive_master_key("password one", &salt).unwrap();
        let b = derive_master_key("password two", &salt).unwrap();
        assert_ne!(a.as_ref(), b.as_ref());
    }

    #[test]
    fn derive_master_key_differs_by_salt() {
        let a = derive_master_key("same password", &generate_salt()).unwrap();
        let b = derive_master_key("same password", &generate_salt()).unwrap();
        assert_ne!(a.as_ref(), b.as_ref());
    }

    #[test]
    fn wrap_unwrap_roundtrip() {
        let salt = generate_salt();
        let master = derive_master_key("hunter2", &salt).unwrap();
        let file_key = generate_file_key();

        let wrapped = wrap_key(&master, &file_key).unwrap();
        let unwrapped = unwrap_key(&master, &wrapped).unwrap();

        assert_eq!(file_key.as_ref(), unwrapped.as_ref());
    }

    #[test]
    fn unwrap_fails_with_wrong_master_key() {
        let file_key = generate_file_key();
        let master_a = derive_master_key("password a", &generate_salt()).unwrap();
        let master_b = derive_master_key("password b", &generate_salt()).unwrap();

        let wrapped = wrap_key(&master_a, &file_key).unwrap();
        assert!(unwrap_key(&master_b, &wrapped).is_err());
    }

    #[test]
    fn unwrap_fails_with_tampered_ciphertext() {
        let master = derive_master_key("hunter2", &generate_salt()).unwrap();
        let file_key = generate_file_key();
        let mut wrapped = wrap_key(&master, &file_key).unwrap();

        wrapped.ciphertext[0] ^= 0xFF;
        assert!(unwrap_key(&master, &wrapped).is_err());
    }

    #[test]
    fn stream_roundtrip_small_plaintext() {
        let file_key = generate_file_key();
        let plaintext = b"a small note, well under one chunk".to_vec();

        let mut ciphertext = Vec::new();
        encrypt_stream(&file_key, &mut plaintext.as_slice(), &mut ciphertext).unwrap();

        let mut decrypted = Vec::new();
        decrypt_stream(&file_key, &mut ciphertext.as_slice(), &mut decrypted).unwrap();

        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn stream_roundtrip_empty_plaintext() {
        let file_key = generate_file_key();
        let plaintext: Vec<u8> = Vec::new();

        let mut ciphertext = Vec::new();
        encrypt_stream(&file_key, &mut plaintext.as_slice(), &mut ciphertext).unwrap();

        let mut decrypted = Vec::new();
        decrypt_stream(&file_key, &mut ciphertext.as_slice(), &mut decrypted).unwrap();

        assert_eq!(decrypted, plaintext);
    }

    /// Exercises the chunk-boundary logic (several full CHUNK_LEN chunks
    /// plus a partial final one) -- the case a same-size buffer or a
    /// single-shot AEAD call would never catch.
    #[test]
    fn stream_roundtrip_multi_chunk_plaintext() {
        let file_key = generate_file_key();
        let mut plaintext = vec![0u8; CHUNK_LEN * 3 + 12_345];
        rand::rngs::OsRng.fill_bytes(&mut plaintext);

        let mut ciphertext = Vec::new();
        encrypt_stream(&file_key, &mut plaintext.as_slice(), &mut ciphertext).unwrap();

        let mut decrypted = Vec::new();
        decrypt_stream(&file_key, &mut ciphertext.as_slice(), &mut decrypted).unwrap();

        assert_eq!(decrypted, plaintext);
    }

    /// Exactly one full chunk, no partial remainder -- the boundary where
    /// the "is this read the last one" check above is easiest to get
    /// off-by-one wrong.
    #[test]
    fn stream_roundtrip_exact_chunk_boundary() {
        let file_key = generate_file_key();
        let mut plaintext = vec![0u8; CHUNK_LEN * 2];
        rand::rngs::OsRng.fill_bytes(&mut plaintext);

        let mut ciphertext = Vec::new();
        encrypt_stream(&file_key, &mut plaintext.as_slice(), &mut ciphertext).unwrap();

        let mut decrypted = Vec::new();
        decrypt_stream(&file_key, &mut ciphertext.as_slice(), &mut decrypted).unwrap();

        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn stream_decrypt_fails_with_wrong_key() {
        let file_key_a = generate_file_key();
        let file_key_b = generate_file_key();
        let plaintext = b"secret note content".to_vec();

        let mut ciphertext = Vec::new();
        encrypt_stream(&file_key_a, &mut plaintext.as_slice(), &mut ciphertext).unwrap();

        let mut decrypted = Vec::new();
        assert!(decrypt_stream(&file_key_b, &mut ciphertext.as_slice(), &mut decrypted).is_err());
    }

    #[test]
    fn stream_decrypt_fails_with_tampered_ciphertext() {
        let file_key = generate_file_key();
        let plaintext = b"secret note content that fits in one chunk".to_vec();

        let mut ciphertext = Vec::new();
        encrypt_stream(&file_key, &mut plaintext.as_slice(), &mut ciphertext).unwrap();
        // Flip a byte inside the ciphertext body (past the 7-byte plaintext
        // header) -- must be caught by the Poly1305 tag, not silently
        // produce corrupted plaintext.
        let last = ciphertext.len() - 1;
        ciphertext[last] ^= 0xFF;

        let mut decrypted = Vec::new();
        assert!(decrypt_stream(&file_key, &mut ciphertext.as_slice(), &mut decrypted).is_err());
    }
}
