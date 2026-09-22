//! Syncing the whole vault to a server of the user's own -- and, when
//! this build says no, not syncing it anywhere.
//!
//! Everything that talks to a server or touches a key sits behind the
//! `sync` cargo feature: the providers (PocketBase, WebDAV), the
//! encryption, the diff and the orchestrator. Switched off, none of it is
//! compiled -- a local-only nobo, with the vault on disk and nothing
//! leaving the device.
//!
//! What stays is `commands` and the state the app manages: the same
//! commands with the same signatures, answering that they are not in this
//! build. One frontend, one `bindings.ts`, whatever the build (see
//! `nobo_babel` for the same shape and why).

/// `sync` alone can do nothing: it knows what to do, a provider knows
/// where. A build that switches every provider off but keeps the sync is
/// not a lite variant, it is a mistake -- and one that would show up as a
/// login screen with no way to log in.
#[cfg(not(any(feature = "pocketbase", feature = "webdav")))]
compile_error!(
    "the sync needs at least one provider -- enable `pocketbase` and/or `webdav` on nobo-sync, \
     or switch `sync` off for a local-only build"
);

// Its tables, announced the way every other feature announces its own
// (see nobo_core::core::db): the database file belongs to the core, what
// is in it does not.
//
// This registration was missing. `store::init_schema` existed and was
// called by nothing but tests, so on a fresh install `sync_state` and
// `sync_meta` were never created and the first sync failed with
// `no such table: sync_meta`. The two other features that keep tables
// both register; this one was simply forgotten, and no test asked.
inventory::submit! {
    nobo_core::core::db::Schema {
        owner: "sync",
        init: |connection| store::init_schema(connection),
    }
}

pub mod crypto;
pub mod diff;
pub mod hashing;
pub mod index;
pub mod orchestrator;
pub mod providers;
pub mod rescan;
pub mod store;

pub mod commands;
#[cfg(feature = "tauri")]
pub mod tauri_commands;
pub mod model;

// The providers are features of their own: `sync` knows WHAT to do, a
// provider knows where (see the feature's module doc). Both are
// switchable, and neither is anything without the sync itself.
inventory::submit! {
    nobo_core::core::features::FeatureSpec {
        id: "sync-pocketbase",
        compiled_in: cfg!(feature = "pocketbase"),
        needs: &["sync"],
        // The libraries belong to the crate, and the crate is `sync`
        // below -- a provider is a half of it, not a build of its own.
        built_on: &[],
    }
}
inventory::submit! {
    nobo_core::core::features::FeatureSpec {
        id: "sync-webdav",
        compiled_in: cfg!(feature = "webdav"),
        needs: &["sync"],
        built_on: &[],
    }
}

/// Where this feature's settings live in `settings.json` (see
/// `config::section`). Compiled in every build: a build without sync
/// still must not drop the section when it saves.
pub const SETTINGS: nobo_core::config::section::Section<model::Settings> =
    nobo_core::config::section::Section::new("sync");

// Both used to be `Config` fields named after this feature.
inventory::submit! {
    nobo_core::core::migrations::Migration {
        owner: "sync",
        id: 1,
        description: "moved the remembered-login flag and the last-synced timestamp into sync's own section",
        run: |_context| nobo_core::config::settings::Config::update(adopt_legacy_settings),
    }
}

/// Moves `sync_remembered` and `last_synced_at` into this feature's
/// section. `false` when there was nothing to move.
fn adopt_legacy_settings(config: &mut nobo_core::config::settings::Config) -> Result<bool, String> {
    let remembered = config.take_legacy_value("sync_remembered");
    let last_synced_at = config.take_legacy_value("last_synced_at");
    if remembered.is_none() && last_synced_at.is_none() {
        return Ok(false);
    }
    if !config.has_section("sync") {
        config.set_section("sync", &model::Settings {
            remembered: remembered.and_then(|value| value.as_bool()).unwrap_or(false),
            last_synced_at: last_synced_at.and_then(|value| value.as_str().map(str::to_string)),
        })?;
    }
    Ok(true)
}
pub mod session;

// This feature, as the app knows it: in the build or not, and switchable
// on this device (see core::features).
inventory::submit! {
    nobo_core::core::features::FeatureSpec {
        id: "sync",
        // No `cfg!`: linked means in the build (see crates/nobo-babel). The
        // providers below keep theirs -- they are features of THIS crate.
        compiled_in: true,
        needs: &[],
        built_on: &["nobo-http", "nobo-webdav"],
    }
}

/// What every command answers when this build has no sync. The frontend
/// asks `app_features()` and hides the whole settings section, so nobody
/// should ever read this -- it is the honest answer for the case where
/// something does anyway.
pub fn unavailable() -> String {
    nobo_core::t!("Diese Version von nobo synchronisiert nicht.", "This build of nobo does not sync.")
}

use nobo_core::core::secrets::Secrets;

/// The stored login for the vault's own sync server. One entry,
/// "session".
///
/// Renamed from `sync_` in 2026-08; the old key is still read and moved
/// on first use (see core::secrets).
pub const SESSION: Secrets = Secrets::renamed_from("sync_", "sync:");

/// Everything this plugin brings, in one call.
///
/// The app adds it with `.plugin(nobo_sync::init())` and knows nothing
/// else about it -- the file kinds, the settings section and the feature
/// registration all announce themselves from in here (see
/// nobo_core::core's inventory collections).
/// Concrete in the runtime rather than generic: its commands take an
/// `AppHandle`, and the app runs Wry. Staying generic would mean a type
/// parameter on every command for a second runtime nobody has.
#[cfg(feature = "tauri")]
pub fn init() -> tauri::plugin::TauriPlugin<tauri::Wry> {
    tauri::plugin::Builder::new("nobo-sync")
        .invoke_handler(specta_builder().invoke_handler())
        .setup(|app, _api| {
            use tauri::Manager;
            // Its session, its lock, its realtime handle -- the app used
            // to manage all three and had to import this crate's
            // `session` module to do it.
            app.manage(session::new_session_state());
            app.manage(session::new_lock_state());
            app.manage(session::new_realtime_state());
            Ok(())
        })
        .build()
}

/// This plugin's command surface, in the one place both the runtime and
/// the bindings read it from.
#[cfg(feature = "tauri")]
fn specta_builder() -> tauri_specta::Builder<tauri::Wry> {
    tauri_specta::Builder::<tauri::Wry>::new()
        .plugin_name("nobo-sync")
        .commands(tauri_specta::collect_commands![
            tauri_commands::sync_settings,
            tauri_commands::sync_set_last_synced_at,
            tauri_commands::sync_register,
            tauri_commands::sync_request_verification_email,
            tauri_commands::sync_login_pocketbase,
            tauri_commands::sync_login_webdav,
            tauri_commands::sync_logout,
            tauri_commands::sync_is_logged_in,
            tauri_commands::sync_try_restore_session,
            tauri_commands::sync_orphaned_credentials_available,
            tauri_commands::sync_reconnect_orphaned_session,
            tauri_commands::sync_discard_orphaned_session,
            tauri_commands::sync_run,
            tauri_commands::sync_republish_vault,
            tauri_commands::sync_resolve_conflict,
            tauri_commands::sync_connection_info,
            tauri_commands::sync_fetch_conflict_remote_content,
        ])
}

/// Writes this plugin's `bindings.ts` next to the frontend half that
/// imports it.
///
/// A test, like the app's own export: running the tests is what
/// regenerates them, so a command whose signature changed without the
/// frontend being regenerated fails the build rather than the device.
#[cfg(all(test, feature = "tauri"))]
mod bindings_export {
    #[test]
    fn export_bindings() {
        super::specta_builder()
            .export(
                specta_typescript::Typescript::default(),
                "../../src/features/sync/bindings.ts",
            )
            .expect("failed to write the bindings");
    }
}

