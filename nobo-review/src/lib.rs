//! The GTD weekly review: a checklist, and how often it is due.
//!
//! A feature rather than part of the core, and the split says which half
//! is which: the review *reads* the vault (the inbox, stalled projects,
//! the someday list) through the core, and owns nothing but its own
//! progress -- which steps are ticked, when it was last finished, and how
//! often it should happen. A build without it is a build with actions,
//! projects and notes and no weekly ritual on top.
//!
//! Two stores, on purpose:
//!
//! - **The state goes into the vault** (`.nobo/review-state.json`, see
//!   `state`): "when did I last actually do this" is a fact about the
//!   user's GTD system, not about a device, so it has to read the same on
//!   the laptop and the phone -- and the vault sync carries it without a
//!   line of sync code here.
//! - **The cadence goes into settings.json** (this feature's own section,
//!   see `SETTINGS`): how often to be nagged is a preference, and a
//!   preference belongs to the device it was set on.

pub mod commands;
pub mod state;
pub mod summary;
#[cfg(feature = "tauri")]
pub mod tauri_commands;

use serde::{Deserialize, Serialize};

/// How often the review is due, and the day it is usually done.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "tauri", derive(specta::Type))]
pub struct Settings {
    /// Days between reviews before the nav badge lights up. Weekly is the
    /// GTD-recommended cadence and the default, but not everyone's actual
    /// rhythm.
    #[serde(default = "default_interval_days")]
    #[cfg_attr(feature = "tauri", specta(type = specta_typescript::Number))]
    pub interval_days: u32,
    /// Purely informational -- shown as "your review is usually due on
    /// ..." (0 = Sunday .. 6 = Saturday, JS `Date.getDay()` convention so
    /// the frontend needs no translation table). It does not affect the
    /// overdue calculation, which is duration-based (interval_days since
    /// the last completion, see `state`) rather than calendar-anchored:
    /// simpler, and not confused by finishing a day early or late.
    #[serde(default)]
    pub reminder_weekday: Option<u8>,
}

fn default_interval_days() -> u32 {
    7
}

impl Default for Settings {
    fn default() -> Self {
        Settings { interval_days: default_interval_days(), reminder_weekday: None }
    }
}

/// This feature's own corner of settings.json (see
/// nobo_core::config::section).
pub const SETTINGS: nobo_core::config::section::Section<Settings> =
    nobo_core::config::section::Section::new("review");

inventory::submit! {
    nobo_core::core::features::FeatureSpec {
        id: "review",
        // No `cfg!`: linked means in the build (see crates/nobo-babel).
        compiled_in: true,
        needs: &[],
        built_on: &[],
    }
}

// The cadence used to sit in the general settings as
// `review_interval_days` / `review_reminder_weekday` -- two fields named
// after a feature, which is exactly what a section is for (see
// config::section, and the same move for babel, caldav and sync).
inventory::submit! {
    nobo_core::core::migrations::Migration {
        owner: "review",
        id: 1,
        description: "moved the review cadence out of the general settings into review's own section",
        run: |_context| nobo_core::config::settings::Config::update(adopt_legacy_settings),
    }
}

/// Moves the two legacy keys into this feature's section. `false` when
/// there was nothing to move, so a device that has already migrated does
/// not rewrite the file on every start.
fn adopt_legacy_settings(config: &mut nobo_core::config::settings::Config) -> Result<bool, String> {
    let interval = config.take_legacy_value("review_interval_days");
    let weekday = config.take_legacy_value("review_reminder_weekday");
    if interval.is_none() && weekday.is_none() {
        return Ok(false);
    }
    // A device that already chose in the new place keeps that: the old
    // fields are stale by definition once the section exists.
    if config.has_section("review") {
        return Ok(true);
    }

    let settings = Settings {
        interval_days: interval.and_then(|value| value.as_u64()).map(|days| days as u32).unwrap_or_else(default_interval_days),
        reminder_weekday: weekday.and_then(|value| value.as_u64()).map(|day| day as u8),
    };
    config.set_section("review", &settings)?;
    Ok(true)
}

/// Everything this plugin brings, in one call (see crates/nobo-babel for
/// what a feature crate is and why the app knows nothing else about it).
#[cfg(feature = "tauri")]
pub fn init<R: tauri::Runtime>() -> tauri::plugin::TauriPlugin<R> {
    tauri::plugin::Builder::new("nobo-review")
        .invoke_handler(specta_builder::<R>().invoke_handler())
        .build()
}

#[cfg(feature = "tauri")]
fn specta_builder<R: tauri::Runtime>() -> tauri_specta::Builder<R> {
    tauri_specta::Builder::<R>::new()
        .plugin_name("nobo-review")
        .commands(tauri_specta::collect_commands![
            tauri_commands::get_review_state,
            tauri_commands::save_review_state,
            tauri_commands::review_settings,
            tauri_commands::save_review_settings,
        ])
}

#[cfg(all(test, feature = "tauri"))]
mod bindings_export {
    /// Writing the bindings IS the test: a command whose signature changed
    /// without the frontend being regenerated fails here rather than on a
    /// device (see crates/nobo-babel's own).
    #[test]
    fn export_bindings() {
        super::specta_builder::<tauri::Wry>()
            .export(specta_typescript::Typescript::default(), "../../src/features/review/bindings.ts")
            .expect("failed to write the bindings");
    }
}

#[cfg(test)]
mod settings_migration_tests {
    use super::*;
    use nobo_core::config::settings::Config;

    fn with_legacy(days: u64, weekday: Option<u64>) -> Config {
        let mut config = Config::default();
        config
            .set_section("review_interval_days", &serde_json::json!(days))
            .expect("a plain number");
        if let Some(weekday) = weekday {
            config.set_section("review_reminder_weekday", &serde_json::json!(weekday)).expect("a plain number");
        }
        config
    }

    #[test]
    fn it_moves_the_cadence_into_this_features_section() {
        let mut config = with_legacy(14, Some(5));

        assert!(adopt_legacy_settings(&mut config).unwrap());

        let moved: Settings = config.section("review").expect("written");
        assert_eq!(moved.interval_days, 14);
        assert_eq!(moved.reminder_weekday, Some(5));
        assert!(!config.has_section("review_interval_days"), "the old key is gone");
    }

    #[test]
    fn a_device_with_nothing_to_move_writes_nothing() {
        assert!(!adopt_legacy_settings(&mut Config::default()).unwrap());
    }

    /// The device already answered in the new place; the old fields are
    /// stale by definition.
    #[test]
    fn an_existing_section_is_not_overwritten() {
        let mut config = with_legacy(14, None);
        config.set_section("review", &Settings { interval_days: 3, reminder_weekday: None }).unwrap();

        adopt_legacy_settings(&mut config).unwrap();

        assert_eq!(config.section::<Settings>("review").unwrap().interval_days, 3);
    }
}
