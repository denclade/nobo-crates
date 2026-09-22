//! The same commands, as Tauri sees them, plus the two things that are
//! Tauri's alone: the progress event and the arrival notification.
//!
//! Everything that decides anything lives in `commands`, so the Tauri
//! build and a non-Tauri host cannot answer the same question
//! differently. The names are what the frontend's generated bindings
//! call, so they have to stay as they are.

use std::sync::{Arc, Mutex};

use rusqlite::Connection;
use tauri::{Emitter, State};

use nobo_core::core::indexer::IndexManager;
use nobo_core::core::notifications::{configured_language, messages, Arrival, Presence};

use crate::commands::{self, CalDavAccountView, CalDavSubscriptionView, CalDavSyncResult, ProgressReporter};
use crate::model::{CalDavKind, CalDavSubscription, RemoteList, Settings};
use crate::sync;

/// The app as the index listener the commands want.
///
/// Its own type rather than one borrowed from the app's host crate: this
/// crate is a self-contained Tauri plugin, and depending on the host for
/// five lines would tie every build of it to a crate the Sailfish port
/// has no use for. The event name is the core's `const`, so the copies
/// cannot drift.
struct AppListener(tauri::AppHandle);

impl nobo_core::core::indexer::IndexListener for AppListener {
    fn index_updated(&self) {
        let _ = self.0.emit(nobo_core::core::indexer::INDEX_UPDATED_EVENT, ());
    }
}

/// A temporary per call: an `AppHandle` is a handle, and what the
/// commands take is a `&dyn IndexListener` that only has to outlive the
/// call.
fn listener(app: &tauri::AppHandle) -> AppListener {
    AppListener(app.clone())
}

/// Posts what a finished sync brought in.
///
/// WHAT to say is the core's (`core::notifications` turns arrivals into
/// titles and bodies, in the app's language, and decides which are worth
/// showing). Only the delivery is here, and only because this crate's
/// Tauri half is the one place that knows a sync just finished.
///
/// Fire-and-forget: the desktop backend answers Ok before anything has
/// been delivered, so a machine without a notification service silently
/// shows nothing -- the right outcome for a sync that otherwise worked.
fn report(app: &tauri::AppHandle, arrivals: &[Arrival]) {
    use tauri_plugin_notification::NotificationExt;

    let language = configured_language();

    for message in messages(arrivals, presence_of(app), &language) {
        let sent = app
            .notification()
            .builder()
            .id(message.id)
            .title(&message.title)
            .body(&message.body)
            .show();
        if let Err(e) = sent {
            log::warn!("notification for '{}' not delivered: {e}", message.title);
        }
    }
}

/// Whether the app has the user's attention.
///
/// **Desktop** asks the window whether it has focus. Not the web view:
/// `document.visibilityState` still says "visible" for a window buried
/// behind three others -- exactly the case a notification is for.
///
/// **Mobile** answers foreground without asking, because anything that
/// reaches this IS the foreground: the web view that triggers a sync only
/// runs while the app is on screen, and the closed-app case goes through
/// the background worker instead. Asking Tauri would be worse than not
/// asking: on Android `is_focused` answers false whatever the app is
/// doing, which turned every in-app sync into a notification the user was
/// looking at.
///
/// A window that cannot be asked counts as foreground: staying quiet is
/// the recoverable mistake.
// `cfg(desktop)`/`cfg(mobile)` are set by tauri-build, which runs for the
// app and not for a library crate -- so the same split is written out in
// the target names it stands for.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn presence_of(app: &tauri::AppHandle) -> Presence {
    use tauri::Manager;

    match app.get_webview_window("main").map(|window| window.is_focused()) {
        Some(Ok(false)) => Presence::Background,
        _ => Presence::Foreground,
    }
}

#[cfg(any(target_os = "android", target_os = "ios"))]
fn presence_of(_app: &tauri::AppHandle) -> Presence {
    Presence::Foreground
}

/// What the list view listens to while a sync runs. Emitted per list, so
/// a sync of several does not look like one long unexplained wait.
///
/// A plain event, not a typed tauri-specta one, matching how the rest of
/// this backend talks to the frontend (see `index-updated`) -- which means
/// the frontend mirrors the name and the payload by hand (see Lists.tsx).
/// Deliberately NOT `specta::Type`: only `collect_commands!` runs here, so
/// a derive would promise a generated type that never reaches
/// bindings.ts and would leave the next reader looking for it.
pub const SYNC_PROGRESS_EVENT: &str = "caldav-sync-progress";

#[derive(Clone, serde::Serialize)]
pub struct CalDavSyncProgress {
    pub list_id: String,
    pub name: String,
    pub done: usize,
    pub total: usize,
}

/// The desktop's progress reporter: one event per item, which is what
/// the list view's ring is drawn from.
struct EventProgress {
    app: tauri::AppHandle,
}

impl ProgressReporter for EventProgress {
    fn report(&self, list_id: &str, name: &str, progress: sync::SyncProgress) {
        // Best effort by design: a dropped progress event costs a smooth
        // ring, and nothing else. The sync itself must not care.
        let _ = self.app.emit(
            SYNC_PROGRESS_EVENT,
            CalDavSyncProgress {
                list_id: list_id.to_string(),
                name: name.to_string(),
                done: progress.done,
                total: progress.total,
            },
        );
    }
}

/// The known servers, each with whether this device can log in to it.
#[tauri::command]
#[specta::specta]
pub async fn caldav_accounts() -> Result<Vec<CalDavAccountView>, String> {
    commands::caldav_accounts().await
}

/// Connects a server: checks the login, remembers the password on this
/// device and the address in the settings.
///
/// The password is stored only once the check has succeeded -- a rejected
/// login must not leave a wrong one behind in the keychain.
#[tauri::command]
#[specta::specta]
pub async fn caldav_add_account(
    name: String,
    server_url: String,
    username: String,
    password: String,
) -> Result<CalDavAccountView, String> {
    commands::caldav_add_account(name, server_url, username, password).await
}

/// Forgets a server on this device: its password and its address.
///
/// Subscriptions are deliberately untouched. They belong to the vault, not
/// to this device -- they stay, and show as needing a login again.
#[tauri::command]
#[specta::specta]
pub async fn caldav_forget_account(account_id: String) -> Result<(), String> {
    commands::caldav_forget_account(account_id).await
}

/// Stores this device's password for a server, after checking it.
///
/// What a device that only received subscriptions through the vault needs,
/// and all it needs.
#[tauri::command]
#[specta::specta]
pub async fn caldav_save_credentials(account_id: String, password: String) -> Result<(), String> {
    commands::caldav_save_credentials(account_id, password).await
}

/// What a server offers, with the password already stored.
#[tauri::command]
#[specta::specta]
pub async fn caldav_collections(account_id: String) -> Result<Vec<RemoteList>, String> {
    commands::caldav_collections(account_id).await
}

/// Every CalDAV subscription in the vault, with the login state of this
/// device.
#[tauri::command]
#[specta::specta]
pub async fn caldav_subscriptions(
    index_manager: State<'_, Arc<IndexManager>>,
) -> Result<Vec<CalDavSubscriptionView>, String> {
    commands::caldav_subscriptions(index_manager.inner()).await
}

/// Subscribes to one collection, for one component.
///
/// Writing the file IS subscribing -- see caldav::registry. Subscribing to
/// something already subscribed changes nothing and returns what is there.
#[tauri::command]
#[specta::specta]
pub async fn caldav_subscribe(
    account_id: String,
    kind: CalDavKind,
    name: String,
    url: String,
    app: tauri::AppHandle,
    index_manager: State<'_, Arc<IndexManager>>,
) -> Result<CalDavSubscription, String> {
    commands::caldav_subscribe(account_id, kind, name, url, &listener(&app), index_manager.inner()).await
}

/// Unsubscribes: removes the mirror file and the local sync state.
///
/// The file is the subscription, so this reaches the other devices through
/// the vault sync by itself.
#[tauri::command]
#[specta::specta]
pub async fn caldav_unsubscribe(
    subscription_id: String,
    app: tauri::AppHandle,
    db: State<'_, Arc<Mutex<Connection>>>,
    index_manager: State<'_, Arc<IndexManager>>,
) -> Result<(), String> {
    commands::caldav_unsubscribe(subscription_id, &listener(&app), db.inner(), index_manager.inner()).await
}

/// Syncs one subscription, for the things that concern exactly one.
///
/// Ticking a box in a list is about that list; syncing every subscription
/// on every server for it is a multi-server round trip the user is waiting
/// through for no reason (and, on a phone, paying for).
#[tauri::command]
#[specta::specta]
pub async fn caldav_sync_one(
    subscription_id: String,
    app: tauri::AppHandle,
    db: State<'_, Arc<Mutex<Connection>>>,
    index_manager: State<'_, Arc<IndexManager>>,
) -> Result<Vec<CalDavSyncResult>, String> {
    let reporter = EventProgress { app: app.clone() };
    commands::caldav_sync_one(subscription_id, &listener(&app), &reporter, db.inner(), index_manager.inner()).await
}

/// Syncs every subscription this device has the login for.
///
/// One failing list does not abort the others -- a server that went away
/// must not cost the user the lists that still work. A subscription
/// without credentials is not a failure either: it is reported as such and
/// skipped, which is the normal state on a device where nobody has logged
/// in yet.
#[tauri::command]
#[specta::specta]
pub async fn caldav_sync_all(
    app: tauri::AppHandle,
    db: State<'_, Arc<Mutex<Connection>>>,
    index_manager: State<'_, Arc<IndexManager>>,
) -> Result<Vec<CalDavSyncResult>, String> {
    let reporter = EventProgress { app: app.clone() };
    let results = commands::caldav_sync_all(&listener(&app), &reporter, db.inner(), index_manager.inner()).await?;
    // What somebody ELSE added, told to the user -- and only if this is
    // the moment for it, which core::notifications decides (nothing while
    // the window has focus, nothing for our own pushes coming back).
    report(&app, &commands::arrivals_in(&results));
    Ok(results)
}

/// Where a subscription is mirrored, so the UI can open it.
#[tauri::command]
#[specta::specta]
pub fn caldav_subscription_file(subscription_id: String) -> Result<String, String> {
    commands::caldav_subscription_file(subscription_id)
}

/// This feature's own settings, read and written on their own (see
/// config::section). The accounts in there are written by the commands
/// above; the view toggle by the Lists screen.
///
/// Compiled in every build -- a build without caldav answers with the
/// defaults and refuses the write, like every other command here.
#[tauri::command]
#[specta::specta]
pub fn caldav_settings() -> Settings {
    commands::caldav_settings()
}

/// Only what the user can change from the Lists screen. The accounts are
/// deliberately not writable from here: they are the backend's, and a
/// screen sending back a whole settings object is how the general
/// settings used to overwrite them.
#[tauri::command]
#[specta::specta]
pub fn caldav_set_lists_hide_done(hide_done: bool) -> Result<(), String> {
    commands::caldav_set_lists_hide_done(hide_done)
}
