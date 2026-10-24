// SPDX-FileCopyrightText: 2026 Dennis Burgermeister
// SPDX-License-Identifier: GPL-3.0-or-later

//! Two-way CalDAV sync for VTODO task lists (Nextcloud Tasks, Apple
//! Reminders, any RFC 4791 server).
//!
//! One remote list maps to one dedicated org file, never into the user's
//! own actions.org/projects.org: the whole file is rewritten whenever the
//! server wins, and mixing hand-written tasks into that would put them one
//! remote edit away from being overwritten.

pub mod client;
pub mod events;
pub mod identity;
pub mod org;
pub mod plan;
pub mod registry;
pub mod sort;
pub mod store;
pub mod sync;
pub mod vtodo;

pub mod commands;
pub mod model;
#[cfg(feature = "tauri")]
pub mod tauri_commands;

/// Where this feature's settings live in `settings.json` (see
/// `config::section`). Compiled in every build: a build without caldav
/// still must not drop the section when it saves.
pub const SETTINGS: nobo_core::config::section::Section<model::Settings> = nobo_core::config::section::Section::new("caldav");

// The accounts and the list-view toggle used to be `Config` fields with
// this feature's name on them -- `caldav_accounts` even had to be hidden
// from the frontend and re-read from disk on every save (see
// commands_config::save_config, which no longer does either).
// Its tables, announced like everything else it brings (see
// nobo_core::core::db): the database file is the core's, what is in it is
// not, and the app opens the connection without knowing who wants a table.
inventory::submit! {
    nobo_core::core::db::Schema {
        owner: "caldav",
        init: |connection| store::init_schema(connection),
    }
}

inventory::submit! {
    nobo_core::core::migrations::Migration {
        owner: "caldav",
        id: 1,
        description: "moved the known servers and the list-view toggle out of the general settings into caldav's own section",
        run: |_context| nobo_core::config::settings::Config::update(adopt_legacy_settings),
    }
}

/// Moves `caldav_accounts` and `lists_hide_done` into this feature's
/// section. `false` when there was nothing to move.
///
/// An existing section wins, and the two old keys are taken either way --
/// leaving one behind would migrate it again on the next start.
fn adopt_legacy_settings(config: &mut nobo_core::config::settings::Config) -> Result<bool, String> {
    let accounts = config.take_legacy_value("caldav_accounts");
    let hide_done = config.take_legacy_value("lists_hide_done");
    if accounts.is_none() && hide_done.is_none() {
        return Ok(false);
    }
    if !config.has_section("caldav") {
        config.set_section(
            "caldav",
            &model::Settings {
                accounts: accounts.and_then(|value| serde_json::from_value(value).ok()).unwrap_or_default(),
                lists_hide_done: hide_done.and_then(|value| serde_json::from_value(value).ok()).unwrap_or(false),
            },
        )?;
    }
    Ok(true)
}

// The tasks in this vault's synced lists, as a search of their own.
//
// They are headings in mirror files, so an ordinary search finds them
// too -- this is the one that finds *only* them, which is what someone
// asking "where was that on my shopping list" wants (see core::search).
inventory::submit! {
    nobo_core::core::search::SearchContributor {
        source: "caldav",
        search: |query| {
            registry::list(query.vault.root())
                .into_iter()
                .filter(|subscription| subscription.kind == model::CalDavKind::Tasks)
                .flat_map(|subscription| query.vault.headings(&subscription.file(query.vault.root())))
                .filter(|heading| query.matches(&heading.title))
                .collect()
        },
    }
}

// This feature, as the app knows it: in the build or not, and switchable
// on this device (see core::features).
inventory::submit! {
    nobo_core::core::features::FeatureSpec {
        id: "caldav",
        // No `cfg!`: linked means in the build (see crates/nobo-babel).
        compiled_in: true,
        // Nothing. This used to say `["calendar"]`, which switched the
        // task lists off with the calendars -- two different things that
        // only ever shared a file format (see
        // crates/nobo-calendar-mirror, which owns it now). What the
        // calendars ARE needed for is the CalDAV calendar half, and that
        // half lives inside the calendar settings page: no page, no
        // section, nothing to switch off separately.
        needs: &[],
        built_on: &["nobo-http", "nobo-webdav", "nobo-ical", "nobo-calendar-mirror"],
    }
}

/// What every command answers when this build has no CalDAV. The frontend
/// asks `app_features()` and leaves the section out entirely (see
/// store/capabilities.ts).
pub fn unavailable() -> String {
    nobo_core::t!(
        "Diese Version von nobo verbindet sich nicht mit CalDAV-Servern.",
        "This build of nobo does not connect to CalDAV servers."
    )
}

use nobo_core::core::file_kind::{FileKind, FileKindSpec, ReadOnly, StateSupport, WriteVeto};
use nobo_core::core::secrets::Secrets;

/// This device's password per CalDAV account, by account id.
///
/// Renamed from `caldav_password:` in 2026-08; the old name is still
/// read and whatever is found under it is moved on first use (see
/// core::secrets). Drop the first argument once no device is likely to
/// be more than a release behind.
pub const PASSWORDS: Secrets = Secrets::renamed_from("caldav_password:", "caldav:");

/// A two-way mirror of a CalDAV VTODO list.
///
/// Editable, but only in the ways CalDAV can represent -- see the row
/// below, which is everything the rest of the app needs to know about
/// these files (`core::file_kind`).
pub const LIST_KIND: FileKind = FileKind::new("caldav-list");

// A list this device has no login for is read-only here: the edit could
// not be uploaded and the next successful sync elsewhere would overwrite
// it. The UI turns that into "log in to edit" (see caldav_subscriptions).
inventory::submit! {
    WriteVeto {
        kind: LIST_KIND,
        reason: ReadOnly::NeedsLogin,
        vetoes: |path| Box::pin(async move { !commands::can_sync(&path).await }),
    }
}

inventory::submit! {
    FileKindSpec {
        kind: LIST_KIND,
        served_by: Some("caldav"),
        editable: true,
        // A CalDAV task has a title, notes, dates and a done flag -- and
        // no notion of the app's intermediate workflow states, so
        // offering them would promise a sync that cannot happen.
        states: StateSupport::DoneOnly,
        // Line order IS the list order here, and it is synced back as
        // X-APPLE-SORT-ORDER.
        reorderable: true,
        can_add_headings: true,
    }
}

/// Syncs one subscription, whatever kind it is.
///
/// The one place that knows a task list goes through `sync::sync_list` and
/// a calendar through `events::sync_calendar`. Both the app's own command
/// and the Android background worker call this rather than switching on
/// the kind themselves -- they used to, and a change to what a calendar
/// reports would then have applied in the foreground and not in the
/// background.
///
/// The progress sink is the only thing the two callers genuinely differ
/// in: the app reports per-task progress to a view, a background run has
/// nobody to report to and passes `&()`.
///
/// Takes the mutex, not a guard: there is network IO between the reads and
/// the writes, and holding the app's one database lock across that would
/// stall everything else for as long as the server takes.
static SYNCING_SUBSCRIPTION: nobo_core::core::once_at_a_time::OncePerKey = nobo_core::core::once_at_a_time::OncePerKey::new();

pub async fn sync_subscription(
    transport: &dyn sync::CalDavTransport,
    db: &std::sync::Mutex<rusqlite::Connection>,
    subscription: &model::CalDavSubscription,
    file: &std::path::Path,
    sink: &dyn sync::ProgressSink,
) -> Result<model::ListSyncOutcome, String> {
    // One pass per subscription at a time, taken HERE because this is the
    // one door all three entrances go through: the single-list command,
    // the all-lists loop, and the Android worker. Guarding a command
    // instead left the other two open -- and the single-list command is
    // the one the list view polls every 20 seconds, so two passes could
    // both see a heading with no uid and both create it, turning one typed
    // line into two tasks on the server.
    //
    // Per subscription rather than globally: making one open list's poll
    // block a manual sync of another would trade this bug for a worse one.
    //
    // Being turned away is not a failure. A pass IS running and will do
    // the work; an empty outcome says truthfully that THIS call did
    // nothing.
    let Some(_busy) = SYNCING_SUBSCRIPTION.try_enter(&subscription.id) else {
        log::info!("CalDAV: '{}' is already syncing, skipping this pass", subscription.name);
        return Ok(model::ListSyncOutcome::default());
    };

    match subscription.kind {
        model::CalDavKind::Tasks => {
            let target = sync::ListTarget { subscription, file };
            sync::sync_list(transport, db, &target, sink).await
        }
        // A calendar has no push half, so most of ListSyncOutcome is
        // meaningless for it -- but the two numbers that are not (what
        // came back, and what of it is new) belong in the same shape, so
        // every caller can treat one subscription like any other.
        model::CalDavKind::Events => events::sync_calendar(transport, db, subscription, file).await.map(|outcome| model::ListSyncOutcome {
            pulled: outcome.event_count,
            arrived: outcome.arrived,
            ..Default::default()
        }),
    }
}

#[cfg(test)]
mod sample_data_tests {
    use super::model::CalDavKind;
    use super::registry;
    use nobo_core::core::vault;
    use std::path::Path;

    /// The sample vault ships a few CalDAV task lists so a fresh container
    /// has something to look at -- with no credentials anywhere, which is
    /// exactly the read-only state a second device is in before anyone
    /// logs in there (see caldav::registry).
    ///
    /// Scans rather than naming files: a list added later is covered
    /// without anyone remembering to extend this.
    #[test]
    fn every_shipped_sample_list_reads_back_as_a_subscription() {
        let vault = Path::new("../testdata");
        let lists = registry::list_of_kind(vault, CalDavKind::Tasks);

        assert!(lists.len() >= 2, "expected several sample lists: {lists:?}");
        for list in &lists {
            assert_eq!(list.kind, CalDavKind::Tasks);
            assert!(!list.username.is_empty(), "{list:?}");
            // The file it was read from has to be the file it would be
            // written to, or a sync would mirror the same list a second
            // time under a different name.
            assert_eq!(
                list.id,
                super::identity::subscription_id(&list.username, &list.url),
                "{} is not named after its collection",
                list.name
            );
        }

        // More than one server, so the settings page's grouping by account
        // is visible in the sandbox at all.
        let accounts: std::collections::HashSet<&str> = lists.iter().map(|list| list.account_id.as_str()).collect();
        assert!(accounts.len() >= 2, "expected lists from two servers: {lists:?}");
    }

    /// The entries have to survive the parser as separate tasks -- the
    /// same trap the calendar samples guard against (a heading without a
    /// state or an id is absorbed into the one above it).
    #[test]
    fn the_sample_tasks_parse_as_separate_entries() {
        let vault = Path::new("../testdata");
        for list in registry::list_of_kind(vault, CalDavKind::Tasks) {
            let path = list.file(vault);
            let content = std::fs::read_to_string(&path).unwrap();
            let headings = vault::parse_headings(&path, &content);

            assert!(headings.len() >= 3, "{} collapsed into {} entries", list.name, headings.len());
            assert!(headings.iter().all(|h| h.state.is_some()), "a task needs a state: {}", list.name);
            assert!(headings.iter().all(|h| h.properties.contains_key("CALDAV_UID")), "{}", list.name);
        }
    }
}

#[cfg(test)]
mod search_tests {
    use super::model::CalDavKind;
    use super::registry::{subscription_for, write_empty};
    use nobo_core::core::indexer::IndexManager;
    use nobo_core::core::search::{self, SearchQuery};
    use nobo_core::core::vault::Vault;

    /// Only the synced lists, and only their tasks -- the point of asking
    /// "where was that on one of my lists" rather than searching the whole
    /// vault.
    #[test]
    fn it_finds_a_task_in_a_synced_list() {
        let vault = tempfile::tempdir().unwrap();
        let subscription = subscription_for(CalDavKind::Tasks, "Einkauf", "dennis", "https://cloud.example.com/dav/einkauf/");
        write_empty(vault.path(), &subscription).unwrap();
        let file = subscription.file(vault.path());
        let text = std::fs::read_to_string(&file).unwrap();
        std::fs::write(&file, format!("{text}* TODO Milch kaufen\n")).unwrap();

        let index = IndexManager::new();
        index.scan_directory(vault.path());
        let terms = vec!["milch".to_string()];
        let query = SearchQuery { raw: "milch", terms: &terms, vault: &Vault::new(&index, vault.path()) };

        let hits = search::search("caldav", &query);

        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].title, "Milch kaufen");
    }

    /// A note that happens to say the same thing is not on a list.
    #[test]
    fn it_ignores_ordinary_notes() {
        let vault = tempfile::tempdir().unwrap();
        std::fs::write(vault.path().join("notizen.org"), "* TODO Milch kaufen\n").unwrap();
        let index = IndexManager::new();
        index.scan_directory(vault.path());
        let terms = vec!["milch".to_string()];
        let query = SearchQuery { raw: "milch", terms: &terms, vault: &Vault::new(&index, vault.path()) };

        assert!(search::search("caldav", &query).is_empty());
    }
}

/// Everything this plugin brings, in one call.
///
/// The app adds it with `.plugin(nobo_caldav::init())` and knows nothing
/// else about it -- the file kinds, the settings section and the feature
/// registration all announce themselves from in here (see
/// nobo_core::core's inventory collections).
/// Concrete in the runtime rather than generic: its commands take an
/// `AppHandle`, and the app runs Wry. Staying generic would mean a type
/// parameter on every command for a second runtime nobody has.
#[cfg(feature = "tauri")]
pub fn init() -> tauri::plugin::TauriPlugin<tauri::Wry> {
    tauri::plugin::Builder::new("nobo-caldav").invoke_handler(specta_builder().invoke_handler()).build()
}

/// This plugin's command surface, in the one place both the runtime and
/// the bindings read it from.
#[cfg(feature = "tauri")]
fn specta_builder() -> tauri_specta::Builder<tauri::Wry> {
    tauri_specta::Builder::<tauri::Wry>::new().plugin_name("nobo-caldav").commands(tauri_specta::collect_commands![
        tauri_commands::caldav_settings,
        tauri_commands::caldav_set_lists_hide_done,
        tauri_commands::caldav_accounts,
        tauri_commands::caldav_add_account,
        tauri_commands::caldav_forget_account,
        tauri_commands::caldav_collections,
        tauri_commands::caldav_subscriptions,
        tauri_commands::caldav_subscribe,
        tauri_commands::caldav_unsubscribe,
        tauri_commands::caldav_save_credentials,
        tauri_commands::caldav_sync_all,
        tauri_commands::caldav_sync_one,
        tauri_commands::caldav_subscription_file,
    ])
}

#[cfg(all(test, feature = "tauri"))]
mod bindings_export {
    /// Validate export without depending on an app checkout. An explicit
    /// NOBO_BINDINGS_DIR also keeps the generated files for a host to consume.
    #[test]
    fn export_bindings() {
        let temporary = tempfile::tempdir().expect("temporary bindings directory");
        let root = std::env::var_os("NOBO_BINDINGS_DIR")
            .filter(|value| !value.is_empty())
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| temporary.path().to_path_buf());
        let directory = root.join("caldav");
        std::fs::create_dir_all(&directory).expect("bindings directory");
        let output = directory.join("bindings.ts");
        super::specta_builder().export(specta_typescript::Typescript::default(), &output).expect("failed to write the bindings");
        assert!(std::fs::metadata(output).expect("exported bindings").len() > 0);
    }
}
