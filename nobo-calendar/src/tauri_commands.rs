//! The same commands, as Tauri sees them.
//!
//! One line each: everything that decides anything lives in `commands`,
//! so the Tauri build and a non-Tauri host cannot answer the same
//! question differently. The names and the doc comments are what
//! tauri-specta reads off the command to write `bindings.ts`, so both are
//! the same text as in `commands` -- they describe the contract, and the
//! frontend is who reads them.
//!
//! The `AppHandle` is only ever handed on as an `IndexListener` -- that
//! is the whole of what these commands used it for (see
//! nobo_core::core::indexer).

use std::sync::{Arc, Mutex};

use rusqlite::Connection;
use tauri::State;

use nobo_core::core::indexer::IndexManager;

use crate::commands::{self, CalendarEntry};
use crate::model::{CalendarRefreshResult, CalendarSubscription};

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
        use tauri::Emitter;
        let _ = self.0.emit(nobo_core::core::indexer::INDEX_UPDATED_EVENT, ());
    }
}

/// A temporary per call: an `AppHandle` is a handle, and what the
/// commands take is a `&dyn IndexListener` that only has to outlive the
/// call.
fn listener(app: &tauri::AppHandle) -> AppListener {
    AppListener(app.clone())
}

/// The subscribed calendars, read out of the vault (see
/// nobo_calendar::registry) -- which is also why one added on another
/// device is simply there after a sync, with nothing to import.
#[tauri::command]
#[specta::specta]
pub async fn calendar_list() -> Result<Vec<CalendarSubscription>, String> {
    commands::calendar_list().await
}

/// Subscribes to a feed and returns the subscription.
///
/// Subscribing to a feed that is already subscribed changes nothing and
/// hands back the existing entry: the id is derived from the URL, so both
/// would be the same one file anyway.
#[tauri::command]
#[specta::specta]
pub async fn calendar_add(
    name: String,
    url: String,
    app: tauri::AppHandle,
    index_manager: State<'_, Arc<IndexManager>>,
) -> Result<CalendarSubscription, String> {
    commands::calendar_add(name, url, &listener(&app), index_manager.inner()).await
}

/// Switches a calendar off (its events leave the agenda, the subscription
/// stays) or back on. Refreshing is what fetches the events back; see
/// calendar_refresh_all.
#[tauri::command]
#[specta::specta]
pub async fn calendar_set_enabled(
    id: String,
    enabled: bool,
    app: tauri::AppHandle,
    index_manager: State<'_, Arc<IndexManager>>,
) -> Result<(), String> {
    commands::calendar_set_enabled(id, enabled, &listener(&app), index_manager.inner()).await
}

/// Unsubscribes: the mirror file is the subscription, so removing it is
/// the whole operation -- and the vault sync carries the removal to the
/// other devices by itself.
#[tauri::command]
#[specta::specta]
pub async fn calendar_forget(
    id: String,
    app: tauri::AppHandle,
    db: State<'_, Arc<Mutex<Connection>>>,
    index_manager: State<'_, Arc<IndexManager>>,
) -> Result<(), String> {
    commands::calendar_forget(id, &listener(&app), db.inner(), index_manager.inner()).await
}

/// Refreshes every enabled subscription.
///
/// One failing feed does not abort the others -- its error is reported in
/// its own result row instead, so a calendar whose URL rotted is visible
/// rather than silently costing the user the calendars that still work.
#[tauri::command]
#[specta::specta]
pub async fn calendar_refresh_all(
    reason: crate::model::RefreshReason,
    app: tauri::AppHandle,
    db: State<'_, Arc<Mutex<Connection>>>,
    index_manager: State<'_, Arc<IndexManager>>,
) -> Result<Vec<CalendarRefreshResult>, String> {
    commands::calendar_refresh_all(reason, &listener(&app), db.inner(), index_manager.inner()).await
}

/// How often this device refreshes its ICS subscriptions, and when it
/// last did.
#[tauri::command]
#[specta::specta]
pub fn calendar_settings() -> crate::model::Settings {
    commands::calendar_settings()
}

/// How often ONE calendar is fetched by itself.
///
/// Written into the mirror file's header like the colour, so it is the
/// calendar's own property and reaches the other devices through the
/// vault -- see registry::REFRESH_KEYWORD.
#[tauri::command]
#[specta::specta]
pub async fn calendar_set_refresh(
    id: String,
    interval: crate::model::RefreshInterval,
    app: tauri::AppHandle,
    index_manager: State<'_, Arc<IndexManager>>,
) -> Result<(), String> {
    commands::calendar_set_refresh(id, interval, &listener(&app), index_manager.inner()).await
}

/// Sets (or clears) the colour a calendar's entries are marked with.
///
/// One command for both kinds: an ICS feed and a CalDAV calendar are the
/// same thing in the agenda, and both mirror into the same directory under
/// the same file name, so the id is enough to find either.
///
/// Written into the mirror's header rather than into the settings, like
/// everything else about a subscription -- so the calendar looks the same
/// on every device.
#[tauri::command]
#[specta::specta]
pub async fn calendar_set_color(
    id: String,
    color: Option<String>,
    app: tauri::AppHandle,
    index_manager: State<'_, Arc<IndexManager>>,
) -> Result<(), String> {
    commands::calendar_set_color(id, color, &listener(&app), index_manager.inner()).await
}

/// Every entry from every mirrored calendar, flat.
///
/// One command rather than "list the directory, then read each file":
/// the agenda needs all of them together anyway, and the frontend has no
/// business knowing where the mirrors live.
#[tauri::command]
#[specta::specta]
pub async fn calendar_events(index_manager: State<'_, Arc<IndexManager>>) -> Result<Vec<CalendarEntry>, String> {
    commands::calendar_events(index_manager.inner()).await
}
