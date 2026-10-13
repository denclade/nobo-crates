//! The same commands, as Tauri sees them, plus the two events that are
//! Tauri's alone: `sync_progress` while a run works, and
//! `sync-remote-changed` when the server pushes.
//!
//! One line each otherwise: everything that decides anything lives in
//! `commands`, so the Tauri build and a non-Tauri host cannot answer the
//! same question differently. The names are what the frontend's generated
//! bindings call; so are the doc comments, which tauri-specta reads off
//! the command to write `bindings.ts` -- which is why they are the same
//! text as in `commands`: they describe the contract, and the frontend is
//! who reads them.

use std::sync::{Arc, Mutex};

use rusqlite::Connection;
use tauri::{AppHandle, Emitter, State};

use nobo_core::core::indexer::IndexManager;

use crate::commands::{self, ProgressSink, SyncConnectionInfo, SyncLoginResult};
use crate::model::{ConflictInfo, Settings, SyncSummary};
use crate::orchestrator;
use crate::session::{RealtimeState, SyncLockState, SyncSessionState};

/// The desktop's progress channel: one throttled event per step, which is
/// what the sync settings page's bar is drawn from.
struct EventProgress {
    app: AppHandle,
}

impl ProgressSink for EventProgress {
    fn report(&self, progress: &orchestrator::SyncProgress) {
        let _ = self.app.emit("sync_progress", progress);
    }
}

/// The desktop's realtime channel: the frontend refetches on this.
struct EventRemote {
    app: AppHandle,
}

impl commands::RemoteChanged for EventRemote {
    fn remote_changed(&self) {
        let _ = self.app.emit("sync-remote-changed", ());
    }
}

fn remote_of(app: &AppHandle) -> Arc<dyn commands::RemoteChanged> {
    Arc::new(EventRemote { app: app.clone() })
}

/// The app as the index listener `sync_run` wants.
///
/// Its own type rather than one borrowed from the app's host crate: this
/// crate is a self-contained Tauri plugin, and depending on the host for
/// five lines would tie every build of it to a crate the Sailfish port
/// has no use for. The event name is the core's `const`, so the copies
/// cannot drift.
struct AppListener(AppHandle);

impl nobo_core::core::indexer::IndexListener for AppListener {
    fn index_updated(&self) {
        let _ = self.0.emit(nobo_core::core::indexer::INDEX_UPDATED_EVENT, ());
    }
}

/// A temporary per call, the same shape as `remote_of` above.
fn listener(app: &AppHandle) -> AppListener {
    AppListener(app.clone())
}

/// This feature's own settings, read and written on their own (see
/// config::section).
///
/// `remembered` is the backend's own (login and logout set it, see
/// `set_sync_remembered`); the frontend only reads it. What it writes is
/// the timestamp of a run it just finished.
#[tauri::command]
#[specta::specta]
pub fn sync_settings() -> Settings {
    commands::sync_settings()
}

/// Records that a run finished, as the sync status line shows it.
#[tauri::command]
#[specta::specta]
pub fn sync_set_last_synced_at(at: Option<String>) -> Result<(), String> {
    commands::sync_set_last_synced_at(at)
}

/// Registers a new PocketBase account. `password` is the user's one vault
/// password -- see crypto.rs's `derive_auth_secret` doc comment for why
/// only a one-way derivation of it (never the password, never the master
/// key it also derives) is what actually reaches the server. Triggers
/// PocketBase's own verification email immediately after; the account
/// exists but is unverified until the user clicks through it (enforced
/// server-side by the `sync_index`/`user_files` API rules, see docs/sync.md
/// -- NOT re-checked here, this command only creates the account).
#[tauri::command]
#[specta::specta]
pub async fn sync_register(email: String, password: String, base_url: String) -> Result<(), String> {
    commands::sync_register(email, password, base_url).await
}

#[tauri::command]
#[specta::specta]
pub async fn sync_request_verification_email(email: String, base_url: String) -> Result<(), String> {
    commands::sync_request_verification_email(email, base_url).await
}

#[tauri::command]
#[specta::specta]
#[allow(clippy::too_many_arguments)]
pub async fn sync_login_pocketbase(
    app: AppHandle,
    db: State<'_, Arc<Mutex<Connection>>>,
    email: String,
    password: String,
    base_url: String,
    remember: bool,
    session_state: State<'_, SyncSessionState>,
    realtime_state: State<'_, RealtimeState>,
) -> Result<SyncLoginResult, String> {
    commands::sync_login_pocketbase(
        remote_of(&app),
        db.inner(),
        email,
        password,
        base_url,
        remember,
        session_state.inner(),
        realtime_state.inner(),
    )
    .await
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
#[tauri::command]
#[specta::specta]
#[allow(clippy::too_many_arguments)]
pub async fn sync_login_webdav(
    app: AppHandle,
    db: State<'_, Arc<Mutex<Connection>>>,
    root: String,
    username: String,
    webdav_password: String,
    vault_password: Option<String>,
    encrypt: bool,
    remember: bool,
    session_state: State<'_, SyncSessionState>,
    realtime_state: State<'_, RealtimeState>,
) -> Result<(), String> {
    commands::sync_login_webdav(
        remote_of(&app),
        db.inner(),
        root,
        username,
        webdav_password,
        vault_password,
        encrypt,
        remember,
        session_state.inner(),
        realtime_state.inner(),
    )
    .await
}

#[tauri::command]
#[specta::specta]
pub async fn sync_logout(
    app: AppHandle,
    session_state: State<'_, SyncSessionState>,
    realtime_state: State<'_, RealtimeState>,
) -> Result<(), String> {
    commands::sync_logout(remote_of(&app), session_state.inner(), realtime_state.inner()).await
}

#[tauri::command]
#[specta::specta]
pub async fn sync_is_logged_in(session_state: State<'_, SyncSessionState>) -> Result<bool, String> {
    commands::sync_is_logged_in(session_state.inner()).await
}

/// Called once on app startup (see Module 6's frontend) -- silently
/// restores a "remembered" login without any password prompt. `Ok(false)`
/// (not an error) covers both "nothing was ever remembered" and "it was,
/// but restoring it failed" (expired/revoked token, server unreachable,
/// credentials changed server-side) -- either way the frontend's fallback
/// is identical: show the normal login form.
#[tauri::command]
#[specta::specta]
pub async fn sync_try_restore_session(
    app: AppHandle,
    session_state: State<'_, SyncSessionState>,
    realtime_state: State<'_, RealtimeState>,
) -> Result<bool, String> {
    commands::sync_try_restore_session(remote_of(&app), session_state.inner(), realtime_state.inner()).await
}

/// Whether a keychain entry from a previous login exists that the restore
/// path is deliberately not using -- `sync_remembered` is false, but
/// keychains outlive a settings reset and sometimes an uninstall.
///
/// Lets the frontend offer a one-click reconnect instead of either
/// silently auto-syncing or demanding the password again. False whenever a
/// session is already remembered, which is the normal restore path.
#[tauri::command]
#[specta::specta]
pub async fn sync_orphaned_credentials_available() -> Result<bool, String> {
    commands::sync_orphaned_credentials_available().await
}

/// The explicit, user-initiated counterpart to the silent startup restore
/// -- same underlying keychain data, but only ever run because the user
/// just clicked "reconnect" on the orphaned-credentials prompt (see
/// `sync_orphaned_credentials_available`), so re-marking the session as
/// remembered here is a conscious choice, not a silent reactivation.
#[tauri::command]
#[specta::specta]
pub async fn sync_reconnect_orphaned_session(
    app: AppHandle,
    session_state: State<'_, SyncSessionState>,
    realtime_state: State<'_, RealtimeState>,
) -> Result<bool, String> {
    commands::sync_reconnect_orphaned_session(remote_of(&app), session_state.inner(), realtime_state.inner()).await
}

/// The "no, forget it" counterpart -- clears the orphaned keychain entry
/// outright so the reconnect prompt doesn't keep resurfacing it forever.
#[tauri::command]
#[specta::specta]
pub async fn sync_discard_orphaned_session() -> Result<(), String> {
    commands::sync_discard_orphaned_session().await
}

#[tauri::command]
#[specta::specta]
pub async fn sync_run(
    app: AppHandle,
    db: State<'_, Arc<Mutex<Connection>>>,
    session_state: State<'_, SyncSessionState>,
    lock: State<'_, SyncLockState>,
    index_manager: State<'_, Arc<IndexManager>>,
) -> Result<SyncSummary, String> {
    let progress = EventProgress { app: app.clone() };
    commands::sync_run(&listener(&app), &progress, db.inner(), session_state.inner(), lock.inner(), index_manager.inner()).await
}

/// Marks this vault as never-synced so the next run uploads all of it.
///
/// The way out of the guard in `orchestrator::run_sync`: when the remote
/// index is genuinely gone, refusing to sync is right until the user says
/// otherwise, and this is them saying it. Everything local is then a new
/// file and gets published; nothing is deleted anywhere.
#[tauri::command]
#[specta::specta]
pub async fn sync_republish_vault(db: State<'_, Arc<Mutex<Connection>>>) -> Result<u32, String> {
    commands::sync_republish_vault(db.inner()).await
}

#[tauri::command]
#[specta::specta]
pub async fn sync_resolve_conflict(conflict: ConflictInfo, keep_local: bool, db: State<'_, Arc<Mutex<Connection>>>) -> Result<(), String> {
    commands::sync_resolve_conflict(conflict, keep_local, db.inner()).await
}

/// Read-only info for the "connected to ..." section of the sync settings
/// page -- who/where this account is, and how many files it already holds
/// remotely. `None` while not logged in (mirrors `sync_is_logged_in`
/// rather than erroring, since "not connected yet" is the normal idle
/// state here, not a failure).
#[tauri::command]
#[specta::specta]
pub async fn sync_connection_info(session_state: State<'_, SyncSessionState>) -> Result<Option<SyncConnectionInfo>, String> {
    commands::sync_connection_info(session_state.inner()).await
}

/// The remote side of a conflict's diff preview -- the local side is just
/// read straight off disk by the frontend (get_note_content/read_file,
/// whichever it already uses), so this is the only side that needs a
/// dedicated command. Read-only: see `orchestrator::fetch_remote_content`'s
/// own doc comment on why this is safe to call freely without affecting
/// how the conflict actually gets resolved later.
#[tauri::command]
#[specta::specta]
pub async fn sync_fetch_conflict_remote_content(
    path: String,
    session_state: State<'_, SyncSessionState>,
) -> Result<Option<String>, String> {
    commands::sync_fetch_conflict_remote_content(path, session_state.inner()).await
}
