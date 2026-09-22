// The `return` in every "not in this build" branch is redundant in
// exactly the configuration where that branch is the whole function --
// and required in the one where it is not. clippy only ever sees the
// first of the two, so the lint is off here rather than in 18 places.
#![allow(clippy::needless_return)]

use rusqlite::Connection;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use nobo_core::config::settings::{config_dir, Config};
use nobo_core::core::indexer::{IndexListener, IndexManager};
use nobo_core::core::secrets::Intent;
use crate::model::{ConflictInfo, Settings, SyncSummary};
use crate::orchestrator;
#[cfg(feature = "pocketbase")]
use crate::providers::pocketbase::auth as pb_auth;
#[cfg(feature = "pocketbase")]
use crate::providers::pocketbase::PocketBaseProvider;
#[cfg(feature = "webdav")]
use crate::providers::webdav::WebDavProvider;
#[cfg(feature = "webdav")]
use crate::providers::SyncProvider;
use crate::session::{RealtimeState, SyncLockState, SyncSessionState};
use crate::session::{self, StoredSession};
use crate::session::SyncSession;
use crate::{crypto, store};

// What a sync run costs, and how often it is supposed to happen (see
// core::cost). The most expensive thing this app does on its own.
inventory::submit! {
    nobo_core::core::cost::Heavy {
        id: "vault-sync",
        what: "hashes the vault, diffs it against the server and transfers what differs",
        scale: nobo_core::core::cost::Scale::Vault,
        expected: "on the periodic run, on a vault change, and on resume -- one at a time (see the lock in orchestrator)",
    }
}

/// For the small one-shot JSON calls below, never file transfer -- so
/// unlike the providers' own clients, a whole-request timeout is safe here,
/// not just a connect timeout. With no timeout, an unreachable server
/// hangs forever with no error reaching the frontend, leaving the login
/// button permanently disabled.
#[cfg(feature = "pocketbase")]
fn short_lived_client() -> Result<reqwest::Client, String> {
    nobo_http::builder()
        .timeout(nobo_http::SOMEBODY_IS_WAITING)
        .build()
        .map_err(|e| nobo_http::describe_error(&e))
}

/// What a host does when the server says the vault changed elsewhere.
///
/// A seam rather than an `AppHandle`: the desktop emits
/// `sync-remote-changed` for its frontend to pick up (see
/// `tauri_commands`); a host without an event channel can simply reread.
///
/// Not behind the `pocketbase` feature although only PocketBase has a
/// realtime channel: the login and session commands take one either way,
/// and a parameter that appears and disappears with a feature is a
/// second signature for every host to get right.
pub trait RemoteChanged: Send + Sync {
    fn remote_changed(&self);
}

/// How far a sync run has got, as the host wants to hear it.
///
/// Same seam, same reason: the desktop emits `sync_progress` and draws a
/// bar, a host with nowhere to put it passes `NoProgress`.
pub trait ProgressSink: Sync {
    fn report(&self, progress: &orchestrator::SyncProgress);
}

/// The sink for a host that has nowhere to report to.
pub struct NoProgress;

impl ProgressSink for NoProgress {
    fn report(&self, _progress: &orchestrator::SyncProgress) {}
}

/// Aborts any running realtime listener and, for a fresh PocketBase
/// session, starts one that tells the host.
///
/// Called wherever a session can change, so at most one listener runs and
/// never a stale one from a previous account. `provider` is `None` for
/// WebDAV, which has no realtime support, and on logout.
#[cfg(feature = "pocketbase")]
async fn replace_realtime_listener(
    remote: &std::sync::Arc<dyn RemoteChanged>,
    realtime_state: &RealtimeState,
    provider: Option<&PocketBaseProvider>,
) {
    let mut guard = realtime_state.lock().await;
    if let Some(old) = guard.take() {
        old.abort();
    }
    if let Some(provider) = provider {
        let remote = remote.clone();
        *guard = Some(provider.spawn_realtime_listener(move || remote.remote_changed()));
    }
}

/// Tauri keeps its own handle on the one tokio runtime the app runs, and
/// a detached task has to land on it; without Tauri there is only the
/// ambient one.
fn spawn<F>(future: F)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    #[cfg(feature = "tauri")]
    {
        tauri::async_runtime::spawn(future);
    }
    #[cfg(not(feature = "tauri"))]
    {
        tokio::spawn(future);
    }
}

fn forget_stored_session() {
    // Best-effort: a failure to clear the OS keychain entry (locked/
    // unavailable) must not block logout from taking effect in memory --
    // same "don't let a secondary concern fail the primary action"
    // convention as history::record_best_effort elsewhere in this backend.
    super::SESSION.delete_detached(session::STORED_SESSION_NAME);
}

/// The "no, forget it" pairing -- clears the OS keychain entry and flips
/// `sync_remembered` off. Shared by logout, discard-orphaned, and a login
/// performed without "remember me" (which must not leave an EARLIER
/// remembered session around to silently resurrect on the next launch, see
/// `sync_remembered`'s own doc comment on Config).
fn forget_and_unremember() {
    forget_stored_session();
    set_sync_remembered(false);
}

/// Persists STORED to the OS keychain and marks `sync_remembered` on when
/// REMEMBER is true, otherwise runs `forget_and_unremember`. Shared by
/// `sync_login_pocketbase`/`sync_login_webdav` -- the two only ever
/// differed in which `StoredSession` variant to build, not in this
/// bookkeeping around it.
fn persist_or_forget_session(remember: bool, stored: StoredSession) {
    if !remember {
        forget_and_unremember();
        return;
    }
    // Best-effort, same convention as forget_stored_session -- the login
    // itself already succeeded by this point, so a keychain failure
    // (locked, unavailable, or on Android/iOS a securestore plugin issue)
    // must not discard an otherwise-good session over what's only ever a
    // "log in again next time" convenience, not a required part of logging
    // in at all.
    match serde_json::to_string(&stored) {
        Ok(json) => {
            spawn(async move {
                match super::SESSION.save_interactive(session::STORED_SESSION_NAME, &json).await {
                    Ok(()) => set_sync_remembered(true),
                    Err(e) => log::warn!("failed to remember sync session: {e}"),
                }
            });
        }
        Err(e) => log::warn!("failed to serialize sync session for remembering: {e}"),
    }
}

/// The settings.json-side half of "is a session remembered" (see Config's
/// own `sync_remembered` doc comment) -- flips it and persists, but only
/// when it's actually changing, so a login/logout on a fresh install with
/// no config file yet doesn't fail loudly over having nothing to update.
fn set_sync_remembered(remembered: bool) {
    let settings = super::SETTINGS.load();
    if settings.remembered == remembered {
        return;
    }
    if let Err(e) = super::SETTINGS.update(|settings| settings.remembered = remembered) {
        log::warn!("failed to persist the remembered-login flag: {e}");
    }
}

/// Not full RFC 5322 validation -- the server (via its own verification
/// email) is the real authority on whether an address is genuine. This
/// only catches obvious garbage before it costs a network round trip and
/// a wasted registration attempt against the backend.
#[cfg(feature = "pocketbase")]
fn validate_email(email: &str) -> Result<(), String> {
    let trimmed = email.trim();
    let invalid = || nobo_core::t!("Ungültige E-Mail-Adresse", "Invalid email address");
    if trimmed.len() < 3 || trimmed.len() > 254 {
        return Err(invalid());
    }
    let Some((local, domain)) = trimmed.split_once('@') else {
        return Err(invalid());
    };
    if local.is_empty() || domain.is_empty() || !domain.contains('.') || domain.starts_with('.') || domain.ends_with('.') {
        return Err(invalid());
    }
    Ok(())
}

// No canonicalization needed on Config::require_vault_root()'s result here
// -- IndexManager canonicalizes its own path inputs internally now (see
// core::indexer's module doc comment), and plain file I/O (this value's
// only other use, in orchestrator::run_sync) doesn't care whether a path is
// canonical.

fn sync_work_dir() -> PathBuf {
    config_dir().join("sync-tmp")
}

#[derive(Debug, serde::Serialize)]
#[cfg_attr(feature = "tauri", derive(specta::Type))]
pub struct SyncLoginResult {
    pub verified: bool,
}

#[derive(Debug, serde::Serialize)]
#[cfg_attr(feature = "tauri", derive(specta::Type))]
pub struct SyncConnectionInfo {
    /// e.g. "PocketBase — https://sync.example.com" -- see each
    /// SyncProvider impl's own `label()`.
    pub label: String,
    /// e.g. the PocketBase email or WebDAV username -- see each
    /// SyncProvider impl's own `account_label()`.
    pub account: String,
    pub remote_file_count: u32,
    /// See SyncProvider::remote_storage_bytes' own doc comment on why this
    /// isn't directly comparable between providers. `None` if it couldn't
    /// be computed (a provider-specific request failure) -- deliberately
    /// doesn't fail the WHOLE connection-info call over what's genuinely
    /// secondary information, the file count/label above are still shown.
    #[cfg_attr(feature = "tauri", specta(type = Option<specta_typescript::Number>))]
    pub storage_used_bytes: Option<u64>,
}

/// Registers a new PocketBase account. `password` is the user's one vault
/// password -- see crypto.rs's `derive_auth_secret` doc comment for why
/// only a one-way derivation of it (never the password, never the master
/// key it also derives) is what actually reaches the server. Triggers
/// PocketBase's own verification email immediately after; the account
/// exists but is unverified until the user clicks through it (enforced
/// server-side by the `sync_index`/`user_files` API rules, see docs/sync.md
/// -- NOT re-checked here, this command only creates the account).
pub async fn sync_register(email: String, password: String, base_url: String) -> Result<(), String> {
    #[cfg(not(feature = "pocketbase"))]
    {
        let _ = (&email, &password, &base_url);
        return Err(super::unavailable());
    }

    #[cfg(feature = "pocketbase")]
    {
        validate_email(&email)?;
        if password.len() < 8 {
            return Err(nobo_core::t!(
                "Das Passwort muss mindestens 8 Zeichen lang sein",
                "The password must be at least 8 characters long"
            ));
        }

        let salt = crypto::derive_salt(&email);
        let master_key = crypto::derive_master_key(&password, &salt)?;
        let auth_secret = crypto::derive_auth_secret(&master_key);

        let client = short_lived_client()?;
        pb_auth::register(&client, &base_url, &email, &auth_secret).await?;
        pb_auth::request_verification(&client, &base_url, &email).await?;
        Ok(())
    }
}

pub async fn sync_request_verification_email(email: String, base_url: String) -> Result<(), String> {
    #[cfg(not(feature = "pocketbase"))]
    {
        let _ = (&email, &base_url);
        return Err(super::unavailable());
    }

    #[cfg(feature = "pocketbase")]
    {
        validate_email(&email)?;
        let client = short_lived_client()?;
        pb_auth::request_verification(&client, &base_url, &email).await
    }
}

pub async fn sync_login_pocketbase(
    remote: std::sync::Arc<dyn RemoteChanged>,
    db: &Mutex<Connection>,
    email: String,
    password: String,
    base_url: String,
    remember: bool,
    session_state: &SyncSessionState,
    realtime_state: &RealtimeState,
) -> Result<SyncLoginResult, String> {
    #[cfg(not(feature = "pocketbase"))]
    {
        let _ = (&remote, &db, &email, &password, &base_url, &remember, &session_state, &realtime_state);
        return Err(super::unavailable());
    }

    #[cfg(feature = "pocketbase")]
    {
        log::info!("sync_login_pocketbase: invoked for {base_url}");
        validate_email(&email)?;

        let salt = crypto::derive_salt(&email);
        let master_key = crypto::derive_master_key(&password, &salt)?;
        let auth_secret = crypto::derive_auth_secret(&master_key);
        log::info!("sync_login_pocketbase: master key derived, calling pb_auth::login");

        let client = short_lived_client()?;
        let auth = pb_auth::login(&client, &base_url, &email, &auth_secret).await;
        log::info!("sync_login_pocketbase: pb_auth::login returned: {}", if auth.is_ok() { "Ok".to_string() } else { format!("Err({:?})", auth.as_ref().err()) });
        let auth = auth?;
        let verified = auth.verified;
        let provider = PocketBaseProvider::new(base_url.clone(), &auth)?.with_email(email.clone());

        {
            let identity = format!("pocketbase|{}|{}", base_url.trim(), email.trim().to_lowercase());
            let conn = store::lock(&db)?;
            crate::store::ensure_remote_identity(&conn, &identity).map_err(|e| e.to_string())?;
        }

        persist_or_forget_session(remember, StoredSession::PocketBase {
            base_url,
            email,
            auth_secret,
            master_key_hex: crypto::master_key_to_hex(&master_key),
        });

        replace_realtime_listener(&remote, &realtime_state, Some(&provider)).await;

        let mut guard = session_state.lock().await;
        *guard = Some(Arc::new(SyncSession { provider: Box::new(provider), master_key: Some(master_key) }));

        Ok(SyncLoginResult { verified })
    }
}

/// WebDAV has no account system, so `webdav_password` is the server's own
/// Basic Auth credential, entirely separate from `vault_password`, which
/// never leaves the device. The server URL and username double as the
/// stable identity `derive_salt` needs.
///
/// `encrypt = false` is a deliberate opt-out for a user already syncing a
/// plaintext org vault. The frontend is expected to warn before setting
/// it; this command cannot confirm the intent. `vault_password` is
/// required only when encrypting, and ignored otherwise.
pub async fn sync_login_webdav(
    remote: std::sync::Arc<dyn RemoteChanged>,
    db: &Mutex<Connection>,
    root: String,
    username: String,
    webdav_password: String,
    vault_password: Option<String>,
    encrypt: bool,
    remember: bool,
    session_state: &SyncSessionState,
    realtime_state: &RealtimeState,
) -> Result<(), String> {
    #[cfg(not(feature = "webdav"))]
    {
        let _ = (&remote, &db, &root, &username, &webdav_password, &vault_password, &encrypt, &remember, &session_state, &realtime_state);
        return Err(super::unavailable());
    }

    #[cfg(feature = "webdav")]
    {
        let master_key = if encrypt {
            let vault_password = vault_password.filter(|p| !p.is_empty()).ok_or_else(|| nobo_core::t!(
                "Bei aktivierter Verschlüsselung wird ein Vault-Passwort benötigt",
                "A vault password is required when encryption is enabled"
            ))?;
            if vault_password.len() < 8 {
                return Err(nobo_core::t!(
                    "Das Vault-Passwort muss mindestens 8 Zeichen lang sein",
                    "The vault password must be at least 8 characters long"
                ));
            }
            let identity = format!("{}|{}", root.trim(), username.trim());
            let salt = crypto::derive_salt(&identity);
            Some(crypto::derive_master_key(&vault_password, &salt)?)
        } else {
            None
        };

        let provider = WebDavProvider::new(root.clone(), username.clone(), webdav_password.clone())?;
        // Fail fast on bad credentials/URL rather than silently "succeeding"
        // into a session that can never actually sync.
        provider.download_index().await?;

        {
            let identity = format!("webdav|{}|{}", root.trim(), username.trim());
            let conn = store::lock(&db)?;
            crate::store::ensure_remote_identity(&conn, &identity).map_err(|e| e.to_string())?;
        }

        persist_or_forget_session(remember, StoredSession::WebDav {
            root,
            username,
            webdav_password,
            master_key_hex: master_key.as_ref().map(crypto::master_key_to_hex),
        });

        #[cfg(not(feature = "pocketbase"))]
        let _ = (&remote, &realtime_state);
        // No realtime support for WebDAV -- still clears any listener left
        // over from a previous PocketBase session (login without an explicit
        // logout in between). Nothing to clear in a build without that
        // provider, which is the only thing that starts one.
        #[cfg(feature = "pocketbase")]
        replace_realtime_listener(&remote, &realtime_state, None).await;

        let mut guard = session_state.lock().await;
        *guard = Some(Arc::new(SyncSession { provider: Box::new(provider), master_key }));
        Ok(())
    }
}

pub async fn sync_logout(remote: std::sync::Arc<dyn RemoteChanged>, session_state: &SyncSessionState, realtime_state: &RealtimeState) -> Result<(), String> {

*session_state.lock().await = None;
#[cfg(not(feature = "pocketbase"))]
let _ = (&remote, &realtime_state);
// Only PocketBase ever starts one.
#[cfg(feature = "pocketbase")]
replace_realtime_listener(&remote, &realtime_state, None).await;
forget_and_unremember();
Ok(())
}

/// Logs back in from whatever is in the keychain, shared by the silent
/// startup restore and the explicit reconnect. The two differ in whether
/// this runs at all, what happens to `sync_remembered` after -- and in
/// INTENT: a reconnect is a click, so it waits out a keyring prompt and
/// ignores the circuit breaker, while the startup restore must not hold
/// the app up for a dialog nobody asked for (see
/// nobo_core::core::secrets::Intent).
///
/// Every failure branch logs which one it was: keychain access, corrupt
/// stored JSON, a revoked auth secret and an unreachable server otherwise
/// all look identical from outside -- silently back at the login form.
async fn restore_session_from_keychain(
    // Only the PocketBase branch needs these two -- it is the only one
    // with a realtime listener to (re)start.
    #[cfg_attr(not(feature = "pocketbase"), allow(unused_variables))] remote: &std::sync::Arc<dyn RemoteChanged>,
    session_state: &SyncSessionState,
    #[cfg_attr(not(feature = "pocketbase"), allow(unused_variables))] realtime_state: &RealtimeState,
    intent: Intent,
) -> Result<bool, String> {
    let json = match super::SESSION.load_as(session::STORED_SESSION_NAME, intent).await {
        Ok(Some(json)) => json,
        Ok(None) => return Ok(false),
        Err(e) => {
            log::warn!("restore_session_from_keychain: OS keychain read failed: {e}");
            return Ok(false);
        }
    };
    let stored = match serde_json::from_str::<StoredSession>(&json) {
        Ok(s) => s,
        Err(e) => {
            log::warn!("restore_session_from_keychain: stored session JSON was corrupt: {e}");
            return Ok(false);
        }
    };

    let session = match stored {
        // A session this build has no provider for: not an error, just
        // nothing to restore. The user is shown the login form, which in
        // such a build offers only what it can (see app_features).
        #[cfg(not(feature = "pocketbase"))]
        StoredSession::PocketBase { .. } => {
            log::info!("a stored PocketBase session was found, but this build has no PocketBase provider");
            return Ok(false);
        }
        #[cfg(not(feature = "webdav"))]
        StoredSession::WebDav { .. } => {
            log::info!("a stored WebDAV session was found, but this build has no WebDAV provider");
            return Ok(false);
        }
        #[cfg(feature = "pocketbase")]
        StoredSession::PocketBase { base_url, email, auth_secret, master_key_hex } => {
            let master_key = match crypto::master_key_from_hex(&master_key_hex) {
                Ok(k) => k,
                Err(e) => { log::warn!("restore_session_from_keychain: stored master key was invalid: {e}"); return Ok(false); }
            };
            let client = match short_lived_client() {
                Ok(c) => c,
                Err(e) => { log::warn!("restore_session_from_keychain: failed to build HTTP client: {e}"); return Ok(false); }
            };
            // A fresh token every restore -- sidesteps needing to persist
            // or refresh a raw JWT ourselves (see docs/sync.md's stated
            // gap on this). If the auth_secret itself was revoked
            // server-side, this simply fails and falls through to "show
            // the login form" below, same as any other restore failure.
            let auth = match pb_auth::login(&client, &base_url, &email, &auth_secret).await {
                Ok(a) => a,
                Err(e) => { log::warn!("restore_session_from_keychain: silent re-login against {base_url} failed: {e}"); return Ok(false); }
            };
            let provider = match PocketBaseProvider::new(base_url, &auth) {
                Ok(p) => p.with_email(email),
                Err(e) => { log::warn!("restore_session_from_keychain: failed to construct PocketBase provider: {e}"); return Ok(false); }
            };
            replace_realtime_listener(remote, realtime_state, Some(&provider)).await;
            SyncSession { provider: Box::new(provider), master_key: Some(master_key) }
        }
        #[cfg(feature = "webdav")]
        StoredSession::WebDav { root, username, webdav_password, master_key_hex } => {
            let master_key = match master_key_hex {
                Some(hex) => match crypto::master_key_from_hex(&hex) {
                    Ok(k) => Some(k),
                    Err(e) => { log::warn!("restore_session_from_keychain: stored master key was invalid: {e}"); return Ok(false); }
                },
                None => None,
            };
            let provider = match WebDavProvider::new(root, username, webdav_password) {
                Ok(p) => p,
                Err(e) => { log::warn!("restore_session_from_keychain: failed to construct WebDAV provider: {e}"); return Ok(false); }
            };
            if let Err(e) = provider.download_index().await {
                log::warn!("restore_session_from_keychain: WebDAV reachability check failed: {e}");
                return Ok(false);
            }
            #[cfg(feature = "pocketbase")]
            replace_realtime_listener(remote, realtime_state, None).await;
            SyncSession { provider: Box::new(provider), master_key }
        }
    };

    *session_state.lock().await = Some(Arc::new(session));
    Ok(true)
}

/// Called once on app startup (see Module 6's frontend) -- silently
/// restores a "remembered" login without any password prompt. `Ok(false)`
/// (not an error) covers both "nothing was ever remembered" and "it was,
/// but restoring it failed" (expired/revoked token, server unreachable,
/// credentials changed server-side) -- either way the frontend's fallback
/// is identical: show the normal login form.
pub async fn sync_try_restore_session(remote: std::sync::Arc<dyn RemoteChanged>, session_state: &SyncSessionState, realtime_state: &RealtimeState) -> Result<bool, String> {

// Gated on this flag rather than the keychain entry alone: a settings
// reset wipes this file, while the OS keychain entry survives it, so a
// stale entry would otherwise keep sync on.
if !super::SETTINGS.load().remembered {
    return Ok(false);
}
if restore_session_from_keychain(&remote, session_state, realtime_state, Intent::Background).await? {
    return Ok(true);
}
// One retry after a short delay before giving up -- this runs at the
// very start of the app's life, and on macOS in particular the OS
// keychain has occasionally not been ready to hand back an entry this
// early yet (observed as a restore that fails silently right at cold
// launch but then succeeds a moment later from the exact same code
// path, e.g. the user manually clicking "reconnect"). Cheap even when
// it doesn't help: a real "nothing remembered"/"credentials revoked"
// case just retries once and gets the same `false` back.
log::info!("sync_try_restore_session: first attempt failed, retrying once after a short delay");
tokio::time::sleep(std::time::Duration::from_millis(500)).await;
restore_session_from_keychain(&remote, session_state, realtime_state, Intent::Background).await
}

/// Whether a keychain entry from a previous login exists that the restore
/// path is deliberately not using -- `sync_remembered` is false, but
/// keychains outlive a settings reset and sometimes an uninstall.
///
/// Lets the frontend offer a one-click reconnect instead of either
/// silently auto-syncing or demanding the password again. False whenever a
/// session is already remembered, which is the normal restore path.
pub async fn sync_orphaned_credentials_available() -> Result<bool, String> {

if super::SETTINGS.load().remembered {
    return Ok(false);
}
match super::SESSION.load(session::STORED_SESSION_NAME).await {
    Ok(entry) => Ok(entry.is_some()),
    Err(e) => {
        log::warn!("sync_orphaned_credentials_available: OS keychain read failed: {e}");
        Ok(false)
    }
}
}

/// The explicit, user-initiated counterpart to the silent startup restore
/// -- same underlying keychain data, but only ever run because the user
/// just clicked "reconnect" on the orphaned-credentials prompt (see
/// `sync_orphaned_credentials_available`), so re-marking the session as
/// remembered here is a conscious choice, not a silent reactivation.
pub async fn sync_reconnect_orphaned_session(remote: std::sync::Arc<dyn RemoteChanged>, session_state: &SyncSessionState, realtime_state: &RealtimeState) -> Result<bool, String> {

let restored = restore_session_from_keychain(&remote, session_state, realtime_state, Intent::Interactive).await?;
if restored {
    set_sync_remembered(true);
}
Ok(restored)
}

/// The "no, forget it" counterpart -- clears the orphaned keychain entry
/// outright so the reconnect prompt doesn't keep resurfacing it forever.
pub async fn sync_discard_orphaned_session() -> Result<(), String> {

forget_and_unremember();
Ok(())
}

pub async fn sync_is_logged_in(session_state: &SyncSessionState) -> Result<bool, String> {

Ok(session_state.lock().await.is_some())
}

/// Marks this vault as never-synced so the next run uploads all of it.
///
/// The way out of the guard in `orchestrator::run_sync`: when the remote
/// index is genuinely gone, refusing to sync is right until the user says
/// otherwise, and this is them saying it. Everything local is then a new
/// file and gets published; nothing is deleted anywhere.
pub async fn sync_republish_vault(db: &Mutex<Connection>) -> Result<u32, String> {
    let conn = super::store::lock(db)?;
    super::store::clear_all_base_hashes(&conn).map(|n| n as u32).map_err(|e| e.to_string())
}

pub async fn sync_run(
    listener: &dyn IndexListener,
    progress_sink: &dyn ProgressSink,
    db: &Mutex<Connection>,
    session_state: &SyncSessionState,
    lock: &SyncLockState,
    index_manager: &IndexManager,
) -> Result<SyncSummary, String> {

let _cost = nobo_core::core::cost::timed_by("vault-sync", "sync");
let _lock_guard = orchestrator::acquire_lock(&lock)?;

let vault_root = Config::require_vault_root()?;
let work_dir = sync_work_dir();

// The handle is cloned out and the mutex released immediately: a sync
// run is minutes of network plus a full reindex, and holding it that long
// made every read of the session -- `sync_is_logged_in`,
// `sync_connection_info` -- block behind it. Two runs are kept apart by
// `SyncLockState` above, which is what that is for.
let session = {
    let guard = session_state.lock().await;
    guard.clone().ok_or("not logged in to a sync account")?
};

// At most one emit per interval, except the last file, which always
// sends so the bar lands on 100% rather than wherever the last tick
// fell. One IPC event per changed file is a burst of round trips for a
// sync touching thousands, each triggering a signal update.
const THROTTLE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(150);
let mut last_emit: Option<std::time::Instant> = None;
let summary = orchestrator::run_sync(session.provider.as_ref(), db, &vault_root, session.master_key.as_ref(), &work_dir, |progress| {
    let is_final = progress.current >= progress.total;
    let due = last_emit.is_none_or(|t| t.elapsed() >= THROTTLE_INTERVAL);
    if is_final || due {
        last_emit = Some(std::time::Instant::now());
        progress_sink.report(&progress);
    }
})
.await
// Logged as well as returned: the caller shows it on the sync
// settings page, which nobody has open during a background pass --
// and "the vault stopped syncing" has to be findable afterwards.
.inspect_err(|e| log::warn!("vault sync failed: {e}"))?;

// run_sync writes files directly with no knowledge of IndexManager,
// and the watcher does not fire for this process's own writes -- so
// the Notes browser, which only reads the index cache, would never
// show newly-synced files. A full rescan is simpler than tracking
// which paths changed and no more expensive than the startup one.
if summary.uploaded + summary.downloaded + summary.deleted_remote + summary.deleted_local > 0 {
    index_manager.scan_directory(&vault_root);
    listener.index_updated();
}

Ok(summary)
}

/// Read-only info for the "connected to ..." section of the sync settings
/// page -- who/where this account is, and how many files it already holds
/// remotely. `None` while not logged in (mirrors `sync_is_logged_in`
/// rather than erroring, since "not connected yet" is the normal idle
/// state here, not a failure).
pub async fn sync_connection_info(session_state: &SyncSessionState) -> Result<Option<SyncConnectionInfo>, String> {

// Cloned out for the same reason as `sync_run`: this makes two network
// round trips, and doing them under the session mutex blocked every other
// question about the session for their duration.
let session = {
    let guard = session_state.lock().await;
    let Some(session) = guard.clone() else { return Ok(None) };
    session
};
let remote_file_count = session.provider.list_remote_files().await?.len() as u32;
let storage_used_bytes = session.provider.remote_storage_bytes().await.ok();
Ok(Some(SyncConnectionInfo {
    label: session.provider.label(),
    account: session.provider.account_label(),
    remote_file_count,
    storage_used_bytes,
}))
}

pub async fn sync_resolve_conflict(
    conflict: ConflictInfo,
    keep_local: bool,
    db: &Mutex<Connection>,
) -> Result<(), String> {

let conn = store::lock(&db)?;
orchestrator::resolve_conflict(&conn, &conflict, keep_local)
}

/// The remote side of a conflict's diff preview -- the local side is just
/// read straight off disk by the frontend (get_note_content/read_file,
/// whichever it already uses), so this is the only side that needs a
/// dedicated command. Read-only: see `orchestrator::fetch_remote_content`'s
/// own doc comment on why this is safe to call freely without affecting
/// how the conflict actually gets resolved later.
pub async fn sync_fetch_conflict_remote_content(
    path: String,
    session_state: &SyncSessionState,
) -> Result<Option<String>, String> {

let work_dir = sync_work_dir();
let session = {
    let guard = session_state.lock().await;
    guard.clone().ok_or("not logged in to a sync account")?
};
orchestrator::fetch_remote_content(session.provider.as_ref(), session.master_key.as_ref(), &work_dir, &path).await
}

/// This feature's own settings, read and written on their own (see
/// config::section).
///
/// `remembered` is the backend's own (login and logout set it, see
/// `set_sync_remembered`); the frontend only reads it. What it writes is
/// the timestamp of a run it just finished.
pub fn sync_settings() -> Settings {
    super::SETTINGS.load()
}

/// Records that a run finished, as the sync status line shows it.
pub fn sync_set_last_synced_at(at: Option<String>) -> Result<(), String> {
    super::SETTINGS.update(|settings| settings.last_synced_at = at)
}
