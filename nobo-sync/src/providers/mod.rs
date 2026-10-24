// SPDX-FileCopyrightText: 2026 Dennis Burgermeister
// SPDX-License-Identifier: GPL-3.0-or-later

//! Where a synced vault actually lives.
//!
//! One trait, one feature flag each: `sync` knows what to do, a provider
//! knows where. A build can have either, both or -- with `sync` itself
//! off -- neither.

#[cfg(feature = "pocketbase")]
pub mod pocketbase;
#[cfg(feature = "webdav")]
pub mod webdav;

use async_trait::async_trait;
use std::path::Path;

/// A revision marker for the remote index -- WebDAV's ETag, PocketBase's
/// `updated` timestamp.
///
/// Captured by `download_index` at the start of a run and threaded back
/// into that same run's `upload_index`, so a lost update is caught against
/// what the run started from. Re-fetching right before the write would
/// only guard that instant: the realistic failure is two runs merely
/// overlapping, where the second device's stale-based write would compare
/// against the first's already-current index and go through.
///
/// `None` means no index existed remotely when this was captured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexVersion {
    /// No index existed remotely when this was captured.
    Absent,
    /// It existed, and this is the validator to write against -- WebDAV's
    /// strong ETag, PocketBase's `updated`.
    At(String),
    /// It existed, the server offered no usable ETag, but it did date the
    /// resource -- so this is its `Last-Modified`, to be written against
    /// with `If-Unmodified-Since`.
    ///
    /// A second-best validator, and worth having: it is still the SERVER
    /// refusing the write, not the client hoping. What it cannot see is
    /// two writes inside the same second, since that is all the resolution
    /// an HTTP date has. Compared to what this case did before -- an
    /// unconditional PUT over whatever the other device had just uploaded
    /// -- that is a narrow gap instead of an open door.
    ///
    /// WebDAV only. PocketBase has no conditional write at all (see its
    /// own module comment) and never produces this.
    LastModified(String),
    /// It existed, but the server handed back nothing to write against at
    /// all -- no usable ETag (none, or a weak `W/"..."`, which `If-Match`
    /// compares strongly and therefore never matches) AND no
    /// `Last-Modified` either. Only then is there genuinely nothing to
    /// condition the write on.
    ///
    /// Its own case rather than being folded into `Absent`, which is what
    /// an `Option` forced and what wedged WebDAV syncs permanently: the
    /// upload then sent `If-None-Match: *` -- "only create this if it does
    /// not exist" -- against an index that plainly did, so every run after
    /// the first real change failed with 412 and an error blaming a second
    /// device that was not there. Weak ETags are the likelier half of
    /// this; gzip-transforming proxies and CDNs produce them routinely.
    Unversioned,
}

/// One backend to sync against -- WebDAV or PocketBase. Both implement the
/// same operations, so the orchestrator never knows which it is talking
/// to.
///
/// Transfers work on file paths rather than buffers or streams: large
/// files stream straight from and to disk inside each provider's HTTP
/// implementation, with no generic stream types in the trait. The
/// orchestrator encrypts to a temp file first and hands that path here.
///
/// `content_hash` is the identifier throughout, never a path or title --
/// those are only ever visible inside the encrypted index.
#[async_trait]
pub trait SyncProvider: Send + Sync {
    /// The encrypted index blob plus the revision marker THIS READ saw
    /// (see `IndexVersion`'s own doc comment), or `None` if this account
    /// has never synced before (first run on any device).
    async fn download_index(&self) -> Result<Option<(Vec<u8>, IndexVersion)>, String>;

    /// Writes the index, but ONLY if the remote's CURRENT revision still
    /// matches EXPECTED_VERSION -- refuses with a distinct
    /// "sync-index-conflict:"-prefixed error if another device's sync
    /// already updated the index since EXPECTED_VERSION was captured,
    /// rather than silently overwriting their change with a working_index
    /// built from what's now a stale base.
    ///
    /// The caller does not match on that prefix; the handling is the
    /// ORDER. `orchestrator::run_sync` awaits this with a bare `?`, so a
    /// refusal returns before the pending state updates are committed and
    /// a retry recomputes the same diff against the now-current index --
    /// see the comment above that call site for why the reverse order
    /// would be a data-loss bug. The prefix is for the UI and for tests.
    async fn upload_index(&self, encrypted_blob: &[u8], expected_version: &IndexVersion) -> Result<(), String>;

    /// No-ops (not an error) if `content_hash` is already present remotely
    /// -- callers are expected to check via `list_remote_files` first where
    /// that matters for progress reporting, but providers must be safe to
    /// call unconditionally too (content-addressed storage makes a
    /// redundant upload idempotent by construction).
    async fn upload_file(&self, content_hash: &str, local_path: &Path) -> Result<(), String>;
    async fn download_file(&self, content_hash: &str, local_path: &Path) -> Result<(), String>;
    async fn delete_file(&self, content_hash: &str) -> Result<(), String>;

    /// Every content hash currently stored remotely for this account --
    /// used for (a) dedup (skip a re-upload nobo already has, e.g. an
    /// identical duplicate note), and (b) reconciliation if the encrypted
    /// index itself is ever lost or corrupted, since it's otherwise the
    /// only record of what's actually on the server.
    async fn list_remote_files(&self) -> Result<Vec<String>, String>;

    /// Short, human-readable description of what this account is connected
    /// to (provider + server), for the "connected to ..." line on the sync
    /// settings page -- never anything secret (no token/password), just
    /// what was already visible on the login form.
    fn label(&self) -> String;

    /// Who's actually logged in -- the email for PocketBase, the username
    /// for WebDAV -- shown alongside `label()` (which only says WHERE, not
    /// WHO) on the sync settings page. Never a secret either, same as
    /// `label()`.
    fn account_label(&self) -> String;

    /// Total server-side size for this account, for the settings page.
    /// Not byte-exact between providers: PocketBase reports the recorded
    /// plaintext size, so nothing has to be downloaded and decrypted to
    /// sum it, while WebDAV reports the encrypted on-disk size. Both
    /// answer the question honestly, just not comparably.
    async fn remote_storage_bytes(&self) -> Result<u64, String>;
}
