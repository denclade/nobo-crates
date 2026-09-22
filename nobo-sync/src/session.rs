//! App-level state for being logged in to a sync account, in two layers:
//! `SyncSession`, in-memory only, holding a live provider that cannot be
//! serialized, and `StoredSession`, the remembered form persisted in the
//! OS keychain.
//!
//! Restoring re-derives nothing and re-sends no password: PocketBase gets
//! a fresh token from the already-derived `auth_secret`, WebDAV reconnects
//! with the stored server credential.

use serde::{Deserialize, Serialize};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use tokio::sync::Mutex;

use super::crypto::MasterKey;
use super::providers::SyncProvider;

/// A live login. Only exists where there is something to log in to --
/// without the `sync` feature there are no providers and no keys, and the
/// state below simply never holds one.
pub struct SyncSession {
    pub provider: Box<dyn SyncProvider>,
    /// `None` only for WebDAV's explicit unencrypted opt-out (see
    /// index.rs's module doc comment and nobo_sync::commands's
    /// `sync_login_webdav`) -- PocketBase logins always derive a real key.
    pub master_key: Option<MasterKey>,
}

/// The OS-keychain entry key this is stored under (see
/// `tauri-plugin-securestore`) -- one entry, one account, at a time. A
/// second "remember me" login overwrites whatever was there before.
/// The one entry this feature keeps in the secret store, under its own
/// prefix (see `nobo_sync::SESSION`). Prefix plus name is
/// "sync_session", which is what it has always been stored as -- see
/// core::secrets on why that string is not free to change.
pub const STORED_SESSION_NAME: &str = "session";

/// What is written to the OS keychain -- never the vault password itself,
/// keeping the one-way derivation property crypto.rs maintains.
///
/// The master key is stored rather than re-derived per launch: the KDF's
/// cost exists to slow down password guessing, not an unlock already gated
/// by the keychain's own device-level protection.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum StoredSession {
    PocketBase {
        base_url: String,
        email: String,
        auth_secret: String,
        master_key_hex: String,
    },
    WebDav {
        root: String,
        username: String,
        webdav_password: String,
        /// `None` mirrors `SyncSession.master_key` -- the unencrypted opt-out.
        master_key_hex: Option<String>,
    },
}

/// `tokio::sync::Mutex`, not `std::sync::Mutex` -- held across the `.await`
/// points of an actual sync run (see nobo_sync::commands's `sync_run`), which
/// a std Mutex guard can't safely do.
/// The live session, behind an `Arc` so a caller can take a handle and
/// let the mutex go.
///
/// `sync_run` used to hold the guard for its whole network pass -- every
/// upload, every download, and the synchronous full reindex at the end.
/// Mutual exclusion between two runs is `SyncLockState`'s job, so all the
/// mutex added was that every READ of the session had to wait for the run
/// to finish: `sync_is_logged_in` and `sync_connection_info` among them,
/// which is why opening the sync settings page during a sync sat on
/// "Sitzung wird geprüft" until it was over.
pub type SyncSessionState = Mutex<Option<Arc<SyncSession>>>;

/// The same slot in a build without sync: it exists so nothing above has
/// to know, and it is always empty.

pub fn new_session_state() -> SyncSessionState {
    Mutex::new(None)
}

/// Guards against two overlapping `sync_run` calls -- see
/// `orchestrator::acquire_lock`.
pub type SyncLockState = Arc<AtomicBool>;

pub fn new_lock_state() -> SyncLockState {
    Arc::new(AtomicBool::new(false))
}

/// The currently-running realtime push listener (see
/// providers::pocketbase::realtime), if any -- only ever `Some` while
/// logged into a PocketBase account (WebDAV has no realtime equivalent).
/// `nobo_sync::commands` aborts and replaces this on every login/restore/
/// logout, so at most one listener is ever running at a time, even across
/// a login -> logout -> different-account-login sequence.
pub type RealtimeState = Mutex<Option<tokio::task::JoinHandle<()>>>;

pub fn new_realtime_state() -> RealtimeState {
    Mutex::new(None)
}
