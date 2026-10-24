// SPDX-FileCopyrightText: 2026 Dennis Burgermeister
// SPDX-License-Identifier: GPL-3.0-or-later

//! Read-only ICS calendar subscriptions.
//!
//! Each subscribed calendar is fetched, translated to org and written to
//! its own file under `<vault>/.nobo/calendars/`. One file per calendar,
//! not one shared file: the whole file is rewritten on every refresh, so
//! two calendars sharing it would make every refresh of one a full-content
//! conflict for the other on the next sync (see nobo_sync).
//!
//! Entries are plain headings with an active timestamp and NO todo
//! keyword. That single detail is what keeps them out of every task list
//! in the app without a single filter having to know calendars exist --
//! and it is what org itself means by a calendar entry: something you
//! attend, not something you complete.

// Subscribed calendars in the agenda, for a host that does not assemble
// one in its own frontend (see agenda).
pub mod agenda;
pub mod identity;
pub mod org;
pub mod registry;
pub mod store;
pub mod sync;

pub mod commands;
pub mod model;
#[cfg(feature = "tauri")]
pub mod tauri_commands;

// This feature, as the app knows it: in the build or not, and switchable
// on this device (see core::features).
// Its tables, announced like everything else it brings (see
// nobo_core::core::db): the database file is the core's, what is in it is
// not, and the app opens the connection without knowing who wants a table.
inventory::submit! {
    nobo_core::core::db::Schema {
        owner: "calendar",
        init: |connection| store::init_schema(connection),
    }
}

inventory::submit! {
    nobo_core::core::features::FeatureSpec {
        id: "calendar",
        // No `cfg!`: if this crate is linked it is in the build, and if
        // it is not, nothing registers (see crates/nobo-babel for the
        // same note).
        compiled_in: true,
        needs: &[],
        built_on: &["nobo-http", "nobo-ical", "nobo-calendar-mirror"],
    }
}

/// The mirror file's shape is shared with the CalDAV calendars (see
/// crates/nobo-calendar-mirror, which owns the kind, the directory and
/// what an entry looks like). Re-exported under the names this feature
/// always used them by.
pub use nobo_calendar_mirror::{dir as calendars_dir, MIRROR_KIND};

/// This feature's own corner of settings.json: how often the feeds are
/// refreshed on this device, and when that last happened (see
/// model::Settings).
pub const SETTINGS: nobo_core::config::section::Section<crate::model::Settings> = nobo_core::config::section::Section::new("calendar");

#[cfg(test)]
mod sample_data_tests {
    use nobo_core::core::vault;
    use std::path::Path;

    /// The sample vault ships pre-generated mirrors so a fresh container
    /// shows calendar entries without a network round trip first (see
    /// scripts/seed-testdata.sh). They are generated output committed by
    /// hand, so nothing else would notice if a format change made them
    /// stop parsing -- which is exactly when the sample would quietly
    /// become useless.
    ///
    /// Scans the directory rather than naming files: a sample added later
    /// is covered without anyone remembering to extend this.
    #[test]
    fn every_shipped_sample_calendar_still_parses_as_separate_entries() {
        let dir = Path::new("../testdata/.nobo/calendars");
        let files: Vec<_> = std::fs::read_dir(dir)
            .expect("sample calendars directory missing")
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "org"))
            .collect();
        assert!(files.len() >= 2, "expected the holiday AND the school-holiday sample: {files:?}");

        let mut saw_a_range = false;
        for path in files {
            let content = std::fs::read_to_string(&path).unwrap();
            let headings = vault::parse_headings(&path, &content);

            assert!(headings.len() > 3, "{} collapsed into {} entries", path.display(), headings.len());
            assert!(headings.iter().all(|h| h.state.is_none()), "a calendar entry must carry no todo state");
            assert!(headings.iter().all(|h| h.occurrence.is_some()), "every entry needs a date to appear in the agenda");
            assert!(headings.iter().all(|h| h.properties.contains_key("ICS_UID")));
            // Without the declaration the mirror is an ordinary, editable
            // note -- and the write guard would let edits through that the
            // next refresh then throws away.
            assert_eq!(
                nobo_core::core::file_kind::from_lines(&content.lines().collect::<Vec<_>>()),
                crate::MIRROR_KIND,
                "{} is missing its kind declaration",
                path.display(),
            );

            saw_a_range |= headings.iter().any(|h| h.occurrence.as_ref().unwrap().end_date.is_some());
        }
        // The whole point of the second sample: single days and multi-day
        // ranges take different branches in calendar::org::format_range.
        assert!(saw_a_range, "no sample exercises a multi-day range");
    }

    /// The samples double as the fixture for "a mirror file IS the
    /// subscription" -- if the shipped ones stopped being readable as
    /// subscriptions, a fresh container would come up with two calendar
    /// files and no calendars.
    #[test]
    fn every_shipped_sample_reads_back_as_a_subscription() {
        let vault = Path::new("../testdata");
        let subscriptions = crate::registry::list(vault);

        assert_eq!(subscriptions.len(), 2, "{subscriptions:?}");
        for subscription in &subscriptions {
            assert!(subscription.url.starts_with("https://"), "{subscription:?}");
            assert!(subscription.enabled);
            // The file it was read from has to be the file it would be
            // written to, or a refresh mirrors the same feed a second
            // time under a different name.
            assert_eq!(
                subscription.id,
                crate::identity::subscription_id(&subscription.url),
                "{} is not named after its feed",
                subscription.name
            );
        }
    }
}

/// Everything this plugin brings, in one call.
///
/// The app adds it with `.plugin(nobo_calendar::init())` and knows nothing
/// else about it -- the file kinds, the settings section and the feature
/// registration all announce themselves from in here (see
/// nobo_core::core's inventory collections).
/// Concrete in the runtime rather than generic: its commands take an
/// `AppHandle`, and the app runs Wry. Staying generic would mean a type
/// parameter on every command for a second runtime nobody has.
#[cfg(feature = "tauri")]
pub fn init() -> tauri::plugin::TauriPlugin<tauri::Wry> {
    tauri::plugin::Builder::new("nobo-calendar").invoke_handler(specta_builder().invoke_handler()).build()
}

/// This plugin's command surface, in the one place both the runtime and
/// the bindings read it from.
#[cfg(feature = "tauri")]
fn specta_builder() -> tauri_specta::Builder<tauri::Wry> {
    tauri_specta::Builder::<tauri::Wry>::new().plugin_name("nobo-calendar").commands(tauri_specta::collect_commands![
        tauri_commands::calendar_list,
        tauri_commands::calendar_add,
        tauri_commands::calendar_set_enabled,
        tauri_commands::calendar_forget,
        tauri_commands::calendar_refresh_all,
        tauri_commands::calendar_settings,
        tauri_commands::calendar_set_refresh,
        tauri_commands::calendar_events,
        tauri_commands::calendar_set_color,
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
        let directory = root.join("calendar");
        std::fs::create_dir_all(&directory).expect("bindings directory");
        let output = directory.join("bindings.ts");
        super::specta_builder().export(specta_typescript::Typescript::default(), &output).expect("failed to write the bindings");
        assert!(std::fs::metadata(output).expect("exported bindings").len() > 0);
    }
}
