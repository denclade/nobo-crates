//! ICS calendar subscriptions: refreshing them, and the read-only rule
//! that keeps the generated files generated.

// The `return` in every "not in this build" branch is redundant in
// exactly the configuration where that branch is the whole function --
// and required in the one where it is not. clippy only ever sees the
// first of the two, so the lint is off here rather than in 18 places.
#![allow(clippy::needless_return)]

use nobo_core::config::settings::Config;
use crate::model::CalendarSubscription;
use crate::registry;
use crate::model::CalendarRefreshResult;
use crate::sync;
use nobo_core::core::file_kind;
use nobo_core::core::indexer::IndexManager;
use rusqlite::Connection;
use std::sync::Mutex;
use nobo_core::core::indexer::{notify_file_changed, notify_files_changed, IndexListener};

/// The mirror files a pass actually wrote.
///
/// A feed that answered 304, or that could not be reached, wrote nothing:
/// reindexing it would be a read and a parse for a file that is exactly as
/// it was, and telling the frontend would wake every listener for nothing
/// (see nobo_core::core::indexer::notify_files_changed).
fn written_files(results: &[CalendarRefreshResult], vault_root: &std::path::Path) -> Vec<std::path::PathBuf> {
    results
        .iter()
        .filter(|result| result.error.is_none() && !result.unchanged)
        .map(|result| registry::file_for(vault_root, &result.id))
        .collect()
}

// See core::cost: what a pass costs, and how often it is supposed to
// happen. The count next to that sentence is what caught the refresh
// flood -- fifty passes in a second, all of them the same work.
inventory::submit! {
    nobo_core::core::cost::Heavy {
        id: "calendar-refresh",
        what: "fetches every enabled ICS feed and rewrites the mirrors that changed",
        scale: nobo_core::core::cost::Scale::Remote,
        expected: "on the periodic sync, on a manual refresh, and when a calendar is added -- never twice at once (see OnceAtATime)",
    }
}

/// The one refresh pass, whoever asked for it (see the command below).
static REFRESHING: nobo_core::core::once_at_a_time::OnceAtATime =
    nobo_core::core::once_at_a_time::OnceAtATime::new();

/// A calendar feed can be large and slow; a whole-request timeout keeps a
/// hanging server from leaving Settings' refresh button spinning forever.
fn client() -> Result<reqwest::Client, String> {
    nobo_http::builder()
        .timeout(nobo_http::ONE_FETCH)
        .build()
        .map_err(|e| nobo_http::describe_error(&e))
}

/// The subscribed calendars, read out of the vault (see
/// nobo_calendar::registry) -- which is also why one added on another
/// device is simply there after a sync, with nothing to import.
pub async fn calendar_list() -> Result<Vec<CalendarSubscription>, String> {

Ok(registry::list(&Config::require_vault_root()?))
}

/// Subscribes to a feed and returns the subscription.
///
/// Subscribing to a feed that is already subscribed changes nothing and
/// hands back the existing entry: the id is derived from the URL, so both
/// would be the same one file anyway.
pub async fn calendar_add(
    name: String,
    url: String,
    listener: &dyn IndexListener,
    index_manager: &IndexManager,
) -> Result<CalendarSubscription, String> {

let vault_root = Config::require_vault_root()?;
let subscription = registry::subscription_for(&name, &url);
if let Some(existing) = registry::find(&vault_root, &subscription.id) {
    return Ok(existing);
}

// Written before the first fetch: a feed whose server is down must
// still leave a subscription behind, not disappear on the next reload.
registry::write_without_events(&vault_root, &subscription)?;
notify_file_changed(listener, &index_manager, &registry::file_for(&vault_root, &subscription.id));
Ok(subscription)
}

/// Switches a calendar off (its events leave the agenda, the subscription
/// stays) or back on. Refreshing is what fetches the events back; see
/// calendar_refresh_all.
pub async fn calendar_set_enabled(
    id: String,
    enabled: bool,
    listener: &dyn IndexListener,
    index_manager: &IndexManager,
) -> Result<(), String> {

let vault_root = Config::require_vault_root()?;
let mut subscription = registry::find(&vault_root, &id).ok_or("this calendar is no longer subscribed")?;
subscription.enabled = enabled;
registry::write_without_events(&vault_root, &subscription)?;
notify_file_changed(listener, &index_manager, &registry::file_for(&vault_root, &subscription.id));
Ok(())
}

/// Unsubscribes: the mirror file is the subscription, so removing it is
/// the whole operation -- and the vault sync carries the removal to the
/// other devices by itself.
pub async fn calendar_forget(
    id: String,
    listener: &dyn IndexListener,
    db: &Mutex<Connection>,
    index_manager: &IndexManager,
) -> Result<(), String> {

let vault_root = Config::require_vault_root()?;
// Only an ICS feed. A CalDAV calendar mirrors into the same directory
// under the same naming scheme, but unsubscribing from one also has to
// clear its rows in caldav_state -- deleting the file from here would
// leave those behind (see nobo_caldav::commands::caldav_unsubscribe, which
// is the way out for those).
if registry::read(&registry::file_for(&vault_root, &id)).is_none() {
    return Err("this calendar is not an ICS subscription".to_string());
}
let gone = registry::file_for(&vault_root, &id);
registry::remove(&vault_root, &id)?;
// Without this the next subscription to the same feed would inherit
// the old validators and be told "unchanged" against a file that no
// longer exists.
{
    let conn = db.lock().map_err(|_| "database lock poisoned".to_string())?;
    let _ = crate::store::forget(&conn, &id);
}
notify_file_changed(listener, &index_manager, &gone);
Ok(())
}

/// Refreshes every enabled subscription.
///
/// One failing feed does not abort the others -- its error is reported in
/// its own result row instead, so a calendar whose URL rotted is visible
/// rather than silently costing the user the calendars that still work.
pub async fn calendar_refresh_all(
    reason: crate::model::RefreshReason,
    listener: &dyn IndexListener,
    db: &Mutex<Connection>,
    index_manager: &IndexManager,
) -> Result<Vec<CalendarRefreshResult>, String> {

let _cost = nobo_core::core::cost::timed_by("calendar-refresh", "calendar");

// One pass at a time. Every caller here wants the same thing -- the
// feeds fetched -- so a second one arriving mid-pass is not a second
// job, it is the same job (see core::once_at_a_time). Without this,
// a resume event next to a visibility change, or a frontend that
// reloads while a pass is in flight, turns into as many concurrent
// passes as there were triggers: the same requests to the same
// servers, each ending in a full vault rescan.
// Already running is an answer, not a failure -- see the same spot in
// nobo_caldav::commands::caldav_sync_all. The pass in flight does the
// work; a second one would only fetch the same feeds again.
let Some(_busy) = REFRESHING.try_enter() else {
    log::info!("calendar refresh skipped: a pass is already running");
    return Ok(Vec::new());
};

let vault_root = Config::require_vault_root()?;
let client = client().inspect_err(|e| log::warn!("calendar refresh: no HTTP client ({e})"))?;

// How often a feed is worth asking is the user's answer, per calendar
// (see model::CalendarSubscription::refresh) -- and it is checked here
// rather than by the caller: the clock, the app start and the button all
// end up in this one function, and a rule spread over three callers is a
// rule that holds in two of them.
let now = chrono::Utc::now().timestamp_millis();
let mut last_refresh = crate::SETTINGS.load().last_refresh_at;

let mut results = Vec::new();
for subscription in registry::list(&vault_root) {
    if !subscription.enabled {
        continue;
    }
    if !subscription.refresh.due(reason, last_refresh.get(&subscription.id).copied(), now) {
        continue;
    }
    // Noted before the fetch, not after: a feed that takes ten seconds or
    // fails outright still had its turn, and retrying it on the next tick
    // is what an interval is there to prevent.
    last_refresh.insert(subscription.id.clone(), now);

    let refreshed = sync::refresh_one(&client, db, &vault_root, &subscription).await;
    results.push(match refreshed {
        Ok(refreshed) => CalendarRefreshResult {
            id: subscription.id,
            name: subscription.name,
            event_count: refreshed.event_count,
            unchanged: refreshed.unchanged,
            error: None,
        },
        Err(e) => {
            // Logged as well as returned: the caller shows it in the
            // calendar settings, but a refresh that fails during the
            // startup pass has nobody looking at that page -- and on
            // a phone the log is the only way to find out why a feed
            // stopped arriving (the background worker next door logs
            // its own failures for the same reason).
            log::warn!("calendar refresh of '{}' failed: {e}", subscription.name);
            CalendarRefreshResult {
                id: subscription.id,
                name: subscription.name,
                event_count: 0,
                unchanged: false,
                error: Some(e),
            }
        }
    });
}

// Exactly the mirrors that landed on disk -- a feed that answered 304
// wrote nothing and has nothing to reindex (see written_files).
notify_files_changed(listener, &index_manager, written_files(&results, &vault_root));

// One write for the whole pass, and only for the calendars that had
// their turn -- whether they had anything new or answered 304. Asking
// again in five minutes because nothing changed would be the opposite of
// what an interval is for.
//
// Ids that no longer exist are dropped here rather than kept forever:
// unsubscribing removes the file, and its timestamp has nothing left to
// describe.
let known: std::collections::HashSet<String> =
    registry::list(&vault_root).into_iter().map(|subscription| subscription.id).collect();
last_refresh.retain(|id, _| known.contains(id));
if let Err(e) = crate::SETTINGS.update(|settings| settings.last_refresh_at = last_refresh) {
    log::warn!("could not remember when the calendars were last refreshed: {e}");
}

Ok(results)
}


/// How often this device refreshes its ICS subscriptions, and when it
/// last did.
pub fn calendar_settings() -> crate::model::Settings {
    crate::SETTINGS.load()
}

/// How often ONE calendar is fetched by itself.
///
/// Written into the mirror file's header like the colour, so it is the
/// calendar's own property and reaches the other devices through the
/// vault -- see registry::REFRESH_KEYWORD.
pub async fn calendar_set_refresh(
    id: String,
    interval: crate::model::RefreshInterval,
    listener: &dyn IndexListener,
    index_manager: &IndexManager,
) -> Result<(), String> {
    let vault_root = Config::require_vault_root()?;
    let file = registry::file_for(&vault_root, &id);
    if !file.exists() {
        return Err("this calendar is no longer subscribed".to_string());
    }

    let value = match serde_json::to_value(interval) {
        Ok(serde_json::Value::String(value)) => value,
        _ => return Err("unknown refresh interval".to_string()),
    };
    // The default is written as the absence of the line, the same way an
    // enabled calendar carries no ENABLED line.
    let write = (interval != crate::model::RefreshInterval::default()).then_some(value);
    registry::set_keyword(&file, registry::REFRESH_KEYWORD, write.as_deref())?;

    notify_file_changed(listener, &index_manager, &file);
    Ok(())
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
pub async fn calendar_set_color(
    id: String,
    color: Option<String>,
    listener: &dyn IndexListener,
    index_manager: &IndexManager,
) -> Result<(), String> {

let vault_root = Config::require_vault_root()?;
let file = registry::file_for(&vault_root, &id);
if !file.exists() {
    return Err("this calendar is no longer subscribed".to_string());
}
registry::set_keyword(&file, crate::model::COLOR_KEYWORD, color.as_deref().filter(|c| !c.is_empty()))?;
notify_file_changed(listener, &index_manager, &file);
Ok(())
}

/// One entry, and which calendar it came from.
///
/// Flattened onto the heading: every reader of this treats it as a heading
/// with one extra field, and a nested shape would make the calendar name
/// something to unwrap before any of the existing time helpers apply.
#[derive(Debug, Clone, serde::Serialize)]
#[cfg_attr(feature = "tauri", derive(specta::Type))]
pub struct CalendarEntry {
    #[serde(flatten)]
    pub heading: nobo_core::core::models::Heading,
    /// What to call the calendar in the agenda.
    pub calendar: String,
    /// And what to mark it with, if a colour was picked.
    pub color: Option<String>,
}

/// Every entry from every mirrored calendar, flat.
///
/// One command rather than "list the directory, then read each file":
/// the agenda needs all of them together anyway, and the frontend has no
/// business knowing where the mirrors live.
pub async fn calendar_events(
    index_manager: &IndexManager,
) -> Result<Vec<CalendarEntry>, String> {

let vault_root = Config::require_vault_root()?;
let dir = sync::calendars_dir(&vault_root);
let Ok(entries) = std::fs::read_dir(&dir) else { return Ok(Vec::new()) };

let vault = nobo_core::core::vault::Vault::new(index_manager, &vault_root);
let mut events = Vec::new();
for entry in entries.flatten() {
    let path = entry.path();
    if !sync::is_calendar_file(&vault_root, &path) {
        continue;
    }
    let (calendar, color) = calendar_label(&path);
    events.extend(
        vault
            .headings(&path)
            .into_iter()
            .map(|heading| CalendarEntry { heading, calendar: calendar.clone(), color: color.clone() }),
    );
}
Ok(events)
}

/// How a calendar presents itself: its name and its colour, as its mirror
/// file states them.
///
/// One read for both, and it works for either kind -- an ICS feed and a
/// CalDAV calendar write the same two keywords (see calendar::org,
/// caldav::registry).
fn calendar_label(path: &std::path::Path) -> (String, Option<String>) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return (nobo_core::utils::fs_utils::filename_of(path), None);
    };
    let lines: Vec<&str> = text.lines().collect();
    (
        file_kind::leading_keyword(&lines, "#+TITLE:")
            .filter(|title| !title.is_empty())
            .unwrap_or_else(|| nobo_core::utils::fs_utils::filename_of(path)),
        file_kind::leading_keyword(&lines, crate::model::COLOR_KEYWORD).filter(|color| !color.is_empty()),
    )
}

#[cfg(test)]
mod tests {
    use super::calendar_label;

    /// The agenda labels an entry with its calendar, and the name comes
    /// from the file both kinds of mirror write it into -- an ICS feed
    /// and a CalDAV calendar alike (see calendar::org, caldav::registry).
    #[test]
    fn the_calendar_is_named_by_its_mirrors_own_title() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("6f1d9a3c.org");
        std::fs::write(&file, "#+TITLE: Familie\n#+NOBO_TYPE: calendar-mirror\n\n* Zahnarzt\n").unwrap();

        assert_eq!(calendar_label(&file).0, "Familie");
    }

    /// A mirror without a title is still an entry's origin -- the file
    /// name says more than nothing at all.
    #[test]
    fn a_nameless_mirror_falls_back_to_its_file_name() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("6f1d9a3c.org");
        std::fs::write(&file, "#+NOBO_TYPE: calendar-mirror\n\n* Zahnarzt\n").unwrap();

        assert_eq!(calendar_label(&file).0, "6f1d9a3c.org");
    }
}

#[cfg(test)]
mod refresh_guard_tests {
    use super::*;

    /// A second pass arriving mid-pass is the same pass, and gets told
    /// so instead of doubling every request (see core::once_at_a_time,
    /// and the burst that prompted this: dozens of triggers in one
    /// second, each ending in a full vault rescan).
    #[test]
    fn a_second_refresh_is_turned_away_while_one_runs() {
        let first = REFRESHING.try_enter().expect("nothing running");

        assert!(REFRESHING.try_enter().is_none());

        drop(first);
        assert!(REFRESHING.try_enter().is_some(), "and free again afterwards");
    }

    /// Reindexing is what an empty pass must not pay for: a feed that
    /// answered 304 has a mirror on disk that is byte for byte what the
    /// index already holds. And a pass where NOTHING landed must not wake
    /// the frontend at all.
    #[test]
    fn only_the_feeds_that_wrote_are_reindexed() {
        let unchanged = CalendarRefreshResult {
            id: "a".into(), name: "Feiertage".into(), event_count: 3, unchanged: true, error: None,
        };
        let failed = CalendarRefreshResult {
            id: "b".into(), name: "Ferien".into(), event_count: 0, unchanged: false,
            error: Some("could not reach the calendar".into()),
        };
        let fetched = CalendarRefreshResult {
            id: "c".into(), name: "Team".into(), event_count: 5, unchanged: false, error: None,
        };

        let vault = std::path::Path::new("/tresor");

        assert!(written_files(&[unchanged.clone(), failed.clone()], vault).is_empty());
        assert_eq!(
            written_files(&[unchanged, failed, fetched], vault),
            vec![registry::file_for(vault, "c")],
            "only the feed that actually fetched"
        );
    }
}
