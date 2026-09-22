//! CalDAV subscriptions: connecting a server, subscribing to its task
//! lists and calendars, and running the sync.
//!
//! Two halves that are kept strictly apart, because only one of them can
//! be synced between devices:
//!
//! - **The subscription** -- which collection, whose account, what it is
//!   called -- lives in the mirrored file in the vault (see
//!   caldav::registry). It travels with the vault, so subscribing once is
//!   enough for all devices.
//! - **The password** never leaves the device. It goes into the OS
//!   keychain under a key derived from user and server
//!   (identity::account_id), so any device can look up whether it has the
//!   login for a given subscription -- and say so plainly when it does
//!   not, instead of the subscription simply being absent there.

// The `return` in every "not in this build" branch is redundant in
// exactly the configuration where that branch is the whole function --
// and required in the one where it is not. clippy only ever sees the
// first of the two, so the lint is off here rather than in 18 places.
#![allow(clippy::needless_return)]


use nobo_core::config::settings::Config;
use crate::model::CalDavAccount;
use crate::model::Settings;
use crate::client::CalDavClient;
use crate::model::{ListSyncOutcome, RemoteList};
use crate::identity;
use crate::model::{CalDavKind, CalDavSubscription};
use crate::registry;
use crate::store;
use crate::sync;
use nobo_core::core::indexer::IndexManager;
use nobo_core::core::notifications;
use nobo_core::core::indexer::{notify_file_changed, notify_files_changed, IndexListener};
use rusqlite::Connection;
use std::sync::Mutex;

/// How a host says how far a sync has got.
///
/// A seam rather than an `AppHandle`: the desktop emits an event per item
/// so its list view can draw a ring (see `tauri_commands`), while a host
/// with no event channel passes `NoProgress` and the sync neither knows
/// nor cares. Per list, so a sync of several does not look like one long
/// unexplained wait.
pub trait ProgressReporter: Sync {
    fn report(&self, list_id: &str, name: &str, progress: sync::SyncProgress);
}

/// The reporter for a host that has nowhere to report to.
pub struct NoProgress;

impl ProgressReporter for NoProgress {
    fn report(&self, _list_id: &str, _name: &str, _progress: sync::SyncProgress) {}
}

/// Binds a reporter to the one list a pass is on, so `sync_subscription`
/// keeps taking a plain `ProgressSink`.
struct ListProgress<'a> {
    reporter: &'a dyn ProgressReporter,
    list_id: String,
    name: String,
}

impl sync::ProgressSink for ListProgress<'_> {
    fn report(&self, progress: sync::SyncProgress) {
        self.reporter.report(&self.list_id, &self.name, progress);
    }
}

/// Builds a client for a subscription, reading its password from this
/// device's keychain.
///
/// Async because the keychain read is: a locked keyring blocks its caller
/// forever otherwise (see core::secrets).
async fn client_for(subscription: &CalDavSubscription) -> Result<CalDavClient, String> {
    let password = super::PASSWORDS.load(&subscription.account_id)
        .await?
        .ok_or(MISSING_CREDENTIALS)?;
    CalDavClient::new(&subscription.url, &subscription.username, &password)
}

/// Not a message for the user -- the frontend recognises this and offers
/// the login instead (see SettingsCalDav).
const MISSING_CREDENTIALS: &str = "no-credentials";

/// Whether this device holds the login for the subscription mirrored at
/// PATH -- false for a file that is not a subscription at all.
///
/// What "read-only on a device without the password" is decided by (see
/// commands_fs::file_capabilities).
pub async fn can_sync(path: &std::path::Path) -> bool {
let Some(subscription) = registry::read(path) else { return false };
super::PASSWORDS.load(&subscription.account_id).await.ok().flatten().is_some()
}

/// A server this device knows, and whether it can log in to it.
#[derive(Debug, Clone, serde::Serialize)]
#[cfg_attr(feature = "tauri", derive(specta::Type))]
pub struct CalDavAccountView {
    pub id: String,
    pub name: String,
    pub url: String,
    pub username: String,
    pub has_credentials: bool,
}

fn label_for(username: &str, url: &str) -> String {
    let host = url.trim_start_matches("https://").trim_start_matches("http://");
    format!("{username}@{host}")
}

/// The address to ASK a derived account's server at.
///
/// Not the bare host: a PROPFIND against `https://cloud.example.com/`
/// answers 405, because the DAV endpoint sits somewhere below it. The
/// collection's parent is the calendar home, which is exactly what the
/// listing wants -- and what the user would have typed. (The account ID
/// stays keyed on the host alone, see identity::account_id: one login per
/// server, whatever path it is reached under.)
fn home_of(collection_url: &str) -> String {
    crate::client::parent_url(collection_url).unwrap_or_else(|| collection_url.to_string())
}

/// Every CalDAV server this device knows, without asking the keychain.
///
/// Two sources, and the second is the important one: the servers someone
/// typed in here (config), plus every server the vault's subscriptions
/// point at. That second half is what makes a device that has only
/// received subscriptions through the vault show them grouped under their
/// server, with nothing to set up but the password.
fn known_accounts() -> Vec<CalDavAccount> {
    // Reading only: an unreadable settings file costs the servers typed in
    // on this device, and the ones derived from the vault below still
    // work. (The two commands that WRITE the config refuse instead, see
    // Config::load_for_update.)
    //
    // One read for both halves: this section and the vault root live in
    // the same file, and `SETTINGS.load()` next to `require_vault_root()`
    // would open and parse it twice for one command.
    let Some(config) = Config::load() else { return Vec::new() };
    let mut accounts = super::SETTINGS.load_from(&config).accounts;

    let Ok(vault_root) = config.vault_root() else { return accounts };
    for subscription in registry::list(&vault_root) {
        if accounts.iter().any(|account| account.id == subscription.account_id) {
            continue;
        }
        accounts.push(CalDavAccount {
            id: subscription.account_id.clone(),
            // Nobody typed a name for a derived account -- the login it
            // belongs to is the honest label.
            name: label_for(&subscription.username, &identity::server_of(&subscription.url)),
            url: home_of(&subscription.url),
            username: subscription.username.clone(),
        });
    }
    accounts
}

/// The known servers, each with whether this device can log in to it.
pub async fn caldav_accounts() -> Result<Vec<CalDavAccountView>, String> {

let mut views = Vec::new();
for account in known_accounts() {
    let has_credentials = super::PASSWORDS.load(&account.id).await.ok().flatten().is_some();
    views.push(CalDavAccountView {
        id: account.id,
        name: account.name,
        url: account.url,
        username: account.username,
        has_credentials,
    });
}
views.sort_by_key(|account| account.name.to_lowercase());
Ok(views)
}

/// Connects a server: checks the login, remembers the password on this
/// device and the address in the settings.
///
/// The password is stored only once the check has succeeded -- a rejected
/// login must not leave a wrong one behind in the keychain.
pub async fn caldav_add_account(
    name: String,
    server_url: String,
    username: String,
    password: String,
) -> Result<CalDavAccountView, String> {

let url = server_url.trim().to_string();
let username = username.trim().to_string();
CalDavClient::new(&url, &username, &password)?.list_collections().await?;

let id = identity::account_id(&username, &url);
// Read before the keychain write: a settings file that cannot be
// parsed must stop this here, not after a password has been stored
// for an account that then fails to be remembered.
let mut settings = super::SETTINGS.load_from(&Config::load_for_update()?);
super::PASSWORDS.save_interactive(&id, &password).await?;

let name = match name.trim() {
    "" => label_for(&username, &identity::server_of(&url)),
    typed => typed.to_string(),
};
// Re-connecting a server already known updates it in place: two
// entries with one id would be two rows over one keychain entry.
match settings.accounts.iter_mut().find(|account| account.id == id) {
    Some(existing) => {
        existing.name.clone_from(&name);
        existing.url.clone_from(&url);
    }
    None => settings.accounts.push(CalDavAccount {
        id: id.clone(),
        name: name.clone(),
        url: url.clone(),
        username: username.clone(),
    }),
}
super::SETTINGS.save(&settings)?;

Ok(CalDavAccountView { id, name, url, username, has_credentials: true })
}

/// Forgets a server on this device: its password and its address.
///
/// Subscriptions are deliberately untouched. They belong to the vault, not
/// to this device -- they stay, and show as needing a login again.
pub async fn caldav_forget_account(account_id: String) -> Result<(), String> {

super::PASSWORDS.delete(&account_id).await?;
super::SETTINGS.update(|settings| settings.accounts.retain(|account| account.id != account_id))
}

/// Stores this device's password for a server, after checking it.
///
/// What a device that only received subscriptions through the vault needs,
/// and all it needs.
pub async fn caldav_save_credentials(
    account_id: String,
    password: String,
) -> Result<(), String> {

let account = find_account(&account_id).ok_or("this server is not known here")?;
CalDavClient::new(&account.url, &account.username, &password)?.list_collections().await?;
super::PASSWORDS.save_interactive(&account_id, &password).await
}

/// What a server offers, with the password already stored.
pub async fn caldav_collections(account_id: String) -> Result<Vec<RemoteList>, String> {

let account = find_account(&account_id).ok_or("this server is not known here")?;
// The user is looking at a spinner in the subscribe dialog: worth
// waiting for a keyring prompt over, unlike the background reads.
let password = super::PASSWORDS.load_interactive(&account_id)
    .await?
    .ok_or(MISSING_CREDENTIALS)?;
CalDavClient::new(&account.url, &account.username, &password)?.list_collections().await
}

/// One known server by id.
fn find_account(account_id: &str) -> Option<CalDavAccount> {
    known_accounts().into_iter().find(|account| account.id == account_id)
}

/// One subscription plus what this particular device can do with it.
#[derive(Debug, Clone, serde::Serialize)]
#[cfg_attr(feature = "tauri", derive(specta::Type))]
pub struct CalDavSubscriptionView {
    #[serde(flatten)]
    pub subscription: CalDavSubscription,
    /// False on a device that has never been given this account's
    /// password. The subscription is still here and its mirrored content
    /// still readable -- it just cannot be synced until someone logs in.
    pub has_credentials: bool,
    /// Absolute path of the mirror file, so the UI can open it.
    pub file: String,
    /// How many entries the mirror currently holds -- tasks in a list,
    /// appointments in a calendar. Read from the index, which has the file
    /// parsed already; it is what makes "subscribed" say something rather
    /// than just being a state.
    pub entry_count: u32,
}

/// Every CalDAV subscription in the vault, with the login state of this
/// device.
pub async fn caldav_subscriptions(
    index_manager: &IndexManager,
) -> Result<Vec<CalDavSubscriptionView>, String> {

let vault_root = Config::require_vault_root()?;
let vault = nobo_core::core::vault::Vault::new(index_manager, &vault_root);
let mut views = Vec::new();
// One keychain read per account, not per subscription: the keyring can
// be slow, and a server's lists all share one entry.
let mut checked: std::collections::HashMap<String, bool> = std::collections::HashMap::new();
for subscription in registry::list(&vault_root) {
    let has_credentials = match checked.get(&subscription.account_id) {
        Some(known) => *known,
        None => {
            let found = super::PASSWORDS.load(&subscription.account_id)
                .await
                .ok()
                .flatten()
                .is_some();
            checked.insert(subscription.account_id.clone(), found);
            found
        }
    };
    let file = subscription.file(&vault_root);
    views.push(CalDavSubscriptionView {
        entry_count: vault.headings(&file).len() as u32,
        file: file.to_string_lossy().into_owned(),
        subscription,
        has_credentials,
    });
}
Ok(views)
}

/// Subscribes to one collection, for one component.
///
/// Writing the file IS subscribing -- see caldav::registry. Subscribing to
/// something already subscribed changes nothing and returns what is there.
pub async fn caldav_subscribe(
    account_id: String,
    kind: CalDavKind,
    name: String,
    url: String,
    listener: &dyn IndexListener,
    index_manager: &IndexManager,
) -> Result<CalDavSubscription, String> {

let vault_root = Config::require_vault_root()?;
let account = find_account(&account_id).ok_or("this server is not known here")?;
let subscription = registry::subscription_for(kind, &name, &account.username, &url);
if let Some(existing) = registry::find(&vault_root, &subscription.id) {
    return Ok(existing);
}

registry::write_empty(&vault_root, &subscription)?;
notify_file_changed(listener, &index_manager, &subscription.file(&vault_root));
Ok(subscription)
}

/// Unsubscribes: removes the mirror file and the local sync state.
///
/// The file is the subscription, so this reaches the other devices through
/// the vault sync by itself.
pub async fn caldav_unsubscribe(
    subscription_id: String,
    listener: &dyn IndexListener,
    db: &Mutex<Connection>,
    index_manager: &IndexManager,
) -> Result<(), String> {

let vault_root = Config::require_vault_root()?;
let Some(subscription) = registry::find(&vault_root, &subscription_id) else { return Ok(()) };
{
    let conn = db.lock().map_err(|_| "database lock poisoned".to_string())?;
    store::forget_list(&conn, &subscription.id).map_err(|e| e.to_string())?;
}
let gone = subscription.file(&vault_root);
registry::remove(&vault_root, &subscription)?;
notify_file_changed(listener, &index_manager, &gone);
Ok(())
}

/// One result row per subscribed list.
#[derive(Debug, Clone, serde::Serialize)]
#[cfg_attr(feature = "tauri", derive(specta::Type))]
pub struct CalDavSyncResult {
    pub list_id: String,
    pub name: String,
    pub outcome: ListSyncOutcome,
    /// Set when the list could not be synced at all. Per-task failures are
    /// inside `outcome.errors` instead.
    pub error: Option<String>,
    /// Not an error: this device simply has no login for the account (see
    /// caldav::registry). The UI offers one instead of reporting a
    /// failure.
    pub missing_credentials: bool,
}

// See core::cost: what a pass costs and how often it should happen.
inventory::submit! {
    nobo_core::core::cost::Heavy {
        id: "caldav-sync",
        what: "asks every subscribed list for changes and pushes local edits back",
        scale: nobo_core::core::cost::Scale::Remote,
        expected: "on the periodic sync, on a manual one, and when the open list is polled -- never twice at once (see OnceAtATime)",
    }
}

/// Reindexes the lists a run actually changed, and only those.
///
/// A sync that found nothing to do must stay silent: the Lists view polls
/// its open list every few seconds (see Lists.tsx), and an `index-updated`
/// per poll would have every store in the app re-read itself -- keychain
/// lookups included -- for a file nobody touched.
///
/// The lists that DID change are named rather than rescanned: syncing one
/// shopping list used to walk and reparse the whole vault.
fn notify_if_changed(
    listener: &dyn IndexListener,
    index_manager: &IndexManager,
    vault_root: &std::path::Path,
    results: &[CalDavSyncResult],
) {
    // Every mirror this pass looked at gets reindexed, whether or not THIS
    // pass changed anything.
    //
    // The Android worker has no AppHandle and no IndexManager, so its
    // writes never reached the running app's index. Coming back to the
    // app, the foreground pass then took the ctag fast path -- the worker
    // had already stored the new ctag -- returned an empty outcome, and
    // the filter below dropped it. So the list view, which says outright
    // that this event is "the only signal that it changed", showed nothing
    // of what the notification had just announced. Not on refetch, not on
    // pull-to-refresh.
    //
    // Reindexing a file that did not change costs one read and one parse;
    // the paths are already in hand.
    let touched: Vec<_> = results
        .iter()
        .filter_map(|result| registry::find(vault_root, &result.list_id))
        .map(|subscription| subscription.file(vault_root))
        .collect();
    for file in &touched {
        index_manager.update_file(file);
    }

    // The frontend event stays behind the "something happened" filter --
    // it drives a refetch, and firing it for every quiet poll would put
    // the list view through one every twenty seconds.
    let changed = results
        .iter()
        .filter(|result| result.outcome != ListSyncOutcome::default())
        .filter_map(|result| registry::find(vault_root, &result.list_id))
        .map(|subscription| subscription.file(vault_root));
    notify_files_changed(listener, index_manager, changed);
}

/// Syncs one subscription with a client that is already built.
async fn sync_one(
    reporter: &dyn ProgressReporter,
    db: &Mutex<Connection>,
    client: &Result<CalDavClient, String>,
    subscription: CalDavSubscription,
    file: &std::path::Path,
) -> CalDavSyncResult {
    let outcome = match client {
        Ok(client) => {
            let sink = ListProgress {
                reporter,
                list_id: subscription.id.clone(),
                name: subscription.name.clone(),
            };
            crate::sync_subscription(client, db, &subscription, file, &sink).await
        }
        Err(e) => Err(e.clone()),
    };

    // A sync that fails leaves nothing else behind: no request in the
    // server's log, no change on disk. Without this line there is nothing
    // to look at afterwards.
    match &outcome {
        Ok(o) => log::info!("CalDAV sync of '{}': {o:?}", subscription.name),
        Err(e) if e == MISSING_CREDENTIALS => log::info!("CalDAV '{}': no login on this device", subscription.name),
        Err(e) => log::warn!("CalDAV sync of '{}' failed: {e}", subscription.name),
    }

    match outcome {
        Ok(outcome) => CalDavSyncResult {
            list_id: subscription.id,
            name: subscription.name,
            outcome,
            error: None,
            missing_credentials: false,
        },
        Err(e) => CalDavSyncResult {
            list_id: subscription.id,
            name: subscription.name,
            outcome: ListSyncOutcome::default(),
            missing_credentials: e == MISSING_CREDENTIALS,
            error: (e != MISSING_CREDENTIALS).then_some(e),
        },
    }
}

/// Syncs one subscription, for the things that concern exactly one.
///
/// Ticking a box in a list is about that list; syncing every subscription
/// on every server for it is a multi-server round trip the user is waiting
/// through for no reason (and, on a phone, paying for).
pub async fn caldav_sync_one(
    subscription_id: String,
    listener: &dyn IndexListener,
    reporter: &dyn ProgressReporter,
    db: &Mutex<Connection>,
    index_manager: &IndexManager,
) -> Result<Vec<CalDavSyncResult>, String> {

let _cost = nobo_core::core::cost::timed_by("caldav-sync", "caldav");

let vault_root = Config::require_vault_root()?;
let Some(subscription) = registry::find(&vault_root, &subscription_id) else { return Ok(Vec::new()) };

let file = subscription.file(&vault_root);
let client = client_for(&subscription).await;
let result = sync_one(reporter, db, &client, subscription, &file).await;

notify_if_changed(listener, &index_manager, &vault_root, std::slice::from_ref(&result));
Ok(vec![result])
}

/// Syncs every subscription this device has the login for.
///
/// One failing list does not abort the others -- a server that went away
/// must not cost the user the lists that still work. A subscription
/// without credentials is not a failure either: it is reported as such and
/// skipped, which is the normal state on a device where nobody has logged
/// in yet.
pub async fn caldav_sync_all(
    listener: &dyn IndexListener,
    reporter: &dyn ProgressReporter,
    db: &Mutex<Connection>,
    index_manager: &IndexManager,
) -> Result<Vec<CalDavSyncResult>, String> {

let _cost = nobo_core::core::cost::timed_by("caldav-sync", "caldav");

// One pass at a time: a second caller wants what this one is already
// doing (see core::once_at_a_time). Two passes would double every
// request to the server AND every keychain read, and race each
// other's writes into the same mirror files.
//
// And "one is already running" is an ANSWER, not a failure. It used to
// be an `Err`, which the frontend showed as a red toast -- for the most
// ordinary situation there is: the Lists view polls its open list, the
// periodic sync ticks, a vault change triggers one, and any two of those
// can land in the same second. Nothing is lost either: the pass that IS
// running does the work and reports it through the same event.
let Some(_busy) = SYNCING.try_enter() else {
    log::info!("CalDAV sync skipped: a pass is already running");
    return Ok(Vec::new());
};

let vault_root = Config::require_vault_root()?;

// One keychain read and one HTTP client per ACCOUNT, not per
// subscription: the keyring can be slow (see core::secrets) and a
// fresh client per list throws away every connection to a server the
// next list is about to talk to again.
let mut clients: std::collections::HashMap<String, Result<CalDavClient, String>> = std::collections::HashMap::new();

let mut results = Vec::new();
for subscription in registry::list(&vault_root) {
    let file = subscription.file(&vault_root);
    if !clients.contains_key(&subscription.account_id) {
        clients.insert(subscription.account_id.clone(), client_for(&subscription).await);
    }
    let client = clients.get(&subscription.account_id).expect("just inserted");
    results.push(sync_one(reporter, db, client, subscription, &file).await);
}

notify_if_changed(listener, &index_manager, &vault_root, &results);
// What somebody ELSE added is worth telling the user about -- but only
// the host knows whether anybody is looking, so it announces (see
// `arrivals_in` and core::notifications).
Ok(results)
}

/// The one sync pass, whoever asked for it.
static SYNCING: nobo_core::core::once_at_a_time::OnceAtATime =
    nobo_core::core::once_at_a_time::OnceAtATime::new();

/// What of a finished run is worth telling the user about.
///
/// A failed subscription contributes nothing rather than a zero: the
/// notification layer counts arrivals, and "nothing arrived" and "we
/// could not ask" must not look the same to it.
pub fn arrivals_in(results: &[CalDavSyncResult]) -> Vec<notifications::Arrival> {
    results
        .iter()
        .filter(|result| result.error.is_none())
        .map(|result| notifications::Arrival { name: result.name.clone(), arrived: result.outcome.arrived })
        .collect()
}

/// Where a subscription is mirrored, so the UI can open it.
pub fn caldav_subscription_file(subscription_id: String) -> Result<String, String> {

let vault_root = Config::require_vault_root()?;
let subscription = registry::find(&vault_root, &subscription_id).ok_or("this subscription no longer exists")?;
Ok(subscription.file(&vault_root).to_string_lossy().into_owned())
}

/// This feature's own settings, read and written on their own (see
/// config::section). The accounts in there are written by the commands
/// above; the view toggle by the Lists screen.
///
/// Compiled in every build -- a build without caldav answers with the
/// defaults and refuses the write, like every other command here.
pub fn caldav_settings() -> Settings {
    super::SETTINGS.load()
}

/// Only what the user can change from the Lists screen. The accounts are
/// deliberately not writable from here: they are the backend's, and a
/// screen sending back a whole settings object is how the general
/// settings used to overwrite them.
pub fn caldav_set_lists_hide_done(hide_done: bool) -> Result<(), String> {
    super::SETTINGS.update(|settings| settings.lists_hide_done = hide_done)
}
