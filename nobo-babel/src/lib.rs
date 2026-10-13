//! Running SRC blocks: org-babel, as a Tauri plugin.
//!
//! A crate of its own, and that is the whole design. A build that does
//! not want it leaves the dependency out (`--no-default-features`), and
//! then none of this is compiled: no interpreter discovery, no
//! `Command::spawn`, no session holding a child process. That is the
//! difference between "the button is hidden" and "the binary cannot do
//! it", and it is the one Apple's review asks about (App Store rule
//! 2.5.2).
//!
//! It reaches exactly one thing: `nobo-core`. Not the app, not another
//! feature -- the compiler makes sure, where a lint used to.
//!
//! # Its own command surface
//!
//! Its commands are the plugin's, not the app's: `init()` builds its own
//! `tauri_specta::Builder` with `plugin_name("nobo-babel")`, which means
//! its own `bindings.ts` next to the frontend half that uses it (see
//! src/features/babel/bindings.ts) and invoke names of the shape
//! `plugin:nobo-babel|babel_settings`. The app's bindings do not mention
//! org-babel at all.

pub mod exec;
pub mod interpreters;
pub mod session;
pub mod tangle_roots;
pub mod trust;

pub mod commands;
/// The Tauri half: the same commands with Tauri's wrapping around them.
/// Behind the feature, so a host without a webview never compiles it.
#[cfg(feature = "tauri")]
pub mod tauri_commands;
pub mod model;

/// Where this feature's settings live in `settings.json` (see
/// `config::section`). Compiled in every build: a build without babel
/// still must not drop the section when it saves.
pub const SETTINGS: nobo_core::config::section::Section<model::Settings> =
    nobo_core::config::section::Section::new("babel");

// The interpreter path used to be `Config::python_interpreter_path`, a
// core field with a feature's name on it. Moving it is one line of data
// and this migration; the field is gone from Config, so a device that
// never runs this would silently start looking for `python3` again.
//
// Registered in every build, babel or not: a build without the feature
// would otherwise leave the old field behind for good, and the section it
// writes is carried along untouched (see config::section).
inventory::submit! {
    nobo_core::core::migrations::Migration {
        owner: "babel",
        id: 1,
        description: "moved the Python interpreter path out of the general settings into babel's own section",
        run: |_context| nobo_core::config::settings::Config::update(adopt_legacy_interpreter_path),
    }
}

pub use session::SessionRegistry;


// This feature, as the app knows it (see nobo_core::core::features).
//
// `compiled_in: true` without a `cfg!` -- if this crate is linked at all,
// it is in the build, and if it is not, nothing registers and the core
// answers "not here" for a feature nobody claimed. That is the split
// paying for itself: the question the cargo feature used to answer is now
// answered by whether the code exists.
inventory::submit! {
    nobo_core::core::features::FeatureSpec {
        id: "babel",
        compiled_in: true,
        needs: &[],
        built_on: &[],
    }
}

/// What every command answers when this build has no babel. Not an error
/// to hide: the frontend asks `app_features()` and does not offer the
/// button in the first place (see store/capabilities.ts).
pub fn unavailable() -> String {
    nobo_core::t!("SRC-Blöcke können in dieser Version nicht ausgeführt werden.", "This build cannot run source blocks.")
}

#[cfg(test)]
mod sample_data_tests {
    use crate::model::BabelLanguage;
    use std::path::Path;

    /// Every SRC block in the sample vault that looks runnable actually is
    /// -- the dev vault ships a Babel playground (see
    /// scripts/seed-testdata.sh), and a block whose language this side
    /// does not know shows no Run button at all, which reads as a broken
    /// app rather than as an unsupported language.
    ///
    /// Scans rather than naming the file: another sample gaining a SRC
    /// block is covered without anyone remembering this test.
    #[test]
    fn every_runnable_sample_block_names_a_language_this_side_knows() {
        let mut runnable = 0;
        for entry in std::fs::read_dir(Path::new("../testdata")).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|e| e.to_str()) != Some("org") {
                continue;
            }
            let content = std::fs::read_to_string(&path).unwrap();
            for line in content.lines() {
                let Some(rest) = line.trim_start().strip_prefix("#+BEGIN_SRC") else { continue };
                let Some(language) = rest.split_whitespace().next() else { continue };
                // A language nobody can run (json, ...) is fine in a note;
                // one that IS runnable has to survive the mapping the Run
                // button asks (see model::BabelLanguage).
                if let Some(mapped) = BabelLanguage::from_block_language(language) {
                    runnable += 1;
                    assert!(
                        matches!(mapped, BabelLanguage::Bash | BabelLanguage::Python),
                        "{}: {language}",
                        path.display()
                    );
                }
            }
        }

        assert!(runnable >= 5, "the sample vault should ship a playground worth opening: {runnable} blocks");
    }

    /// Both engines, so a dev session exercises the two code paths that
    /// actually differ (interpreter invocation and `:var` formatting, see
    /// exec.rs) rather than Python twice.
    #[test]
    fn the_playground_covers_both_engines_and_every_header_arg_with_its_own_ui() {
        let file = "../testdata/20260827T090000--babel-spielwiese-python-bash__demo_python.org";
        let content = std::fs::read_to_string(file).expect("the Babel playground is part of the sample vault");

        assert!(content.contains("#+BEGIN_SRC python"), "no Python block");
        assert!(content.contains("#+BEGIN_SRC bash"), "no Bash block");
        // :session and :cache change what the backend does with a block,
        // :tangle is the second button on the header -- each needs one
        // block to press.
        for arg in [":session", ":cache", ":tangle", ":var", ":results"] {
            assert!(content.contains(arg), "nothing to try {arg} on");
        }
        // Balanced, or the editor sees one block running to the end of the
        // file and the buttons land in the wrong places.
        assert_eq!(
            content.matches("#+BEGIN_SRC").count(),
            content.matches("#+END_SRC").count(),
            "unbalanced SRC blocks"
        );
    }
}

/// Moves `python_interpreter_path` from the general settings into this
/// feature's section. `false` when there was nothing to move.
///
/// Idempotent by shape: taking the old key removes it, so a second run
/// finds nothing. An existing section wins -- somebody who has already
/// set an interpreter here is not overwritten by whatever the old field
/// still said.
fn adopt_legacy_interpreter_path(config: &mut nobo_core::config::settings::Config) -> Result<bool, String> {
    // Present-but-null counts as present: the field was `Option<String>`
    // and null is what "auto-discover" looked like. Matching only on a
    // string would take the key out of the copy in memory and then
    // return "nothing to do", leaving it in the file for good.
    let Some(legacy) = config.take_legacy_value("python_interpreter_path") else {
        return Ok(false);
    };
    if !config.has_section("babel") {
        config.set_section("babel", &model::Settings {
            interpreter_path: legacy.as_str().map(str::to_string),
            bash_path: None,
        })?;
    }
    Ok(true)
}

#[cfg(test)]
mod settings_migration_tests {
    use super::*;
    use nobo_core::config::settings::Config;

    /// A config as it comes off disk, plus the field an older build wrote.
    fn with_legacy_path(path: &str) -> Config {
        let mut json = serde_json::to_value(Config::default()).unwrap();
        json.as_object_mut()
            .unwrap()
            .insert("python_interpreter_path".to_string(), serde_json::json!(path));
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn it_moves_the_interpreter_path_into_this_features_section() {
        let mut config = with_legacy_path("/opt/venv/bin/python");

        assert!(adopt_legacy_interpreter_path(&mut config).unwrap());

        assert_eq!(
            config.section::<model::Settings>("babel").unwrap().interpreter_path.as_deref(),
            Some("/opt/venv/bin/python")
        );
        // And the old key is gone, so the file stops carrying two truths.
        assert!(!serde_json::to_string(&config).unwrap().contains("python_interpreter_path"));
    }

    #[test]
    fn a_second_run_has_nothing_left_to_do() {
        let mut config = with_legacy_path("/opt/venv/bin/python");
        adopt_legacy_interpreter_path(&mut config).unwrap();

        assert!(!adopt_legacy_interpreter_path(&mut config).unwrap());
    }

    /// What every device that never picked an interpreter has in its
    /// file: the key, holding null. It still has to go, or it sits there
    /// forever -- the migration only runs once.
    #[test]
    fn a_field_that_was_null_is_still_taken_out_of_the_file() {
        let mut json = serde_json::to_value(Config::default()).unwrap();
        json.as_object_mut().unwrap().insert("python_interpreter_path".to_string(), serde_json::Value::Null);
        let mut config: Config = serde_json::from_value(json).unwrap();

        assert!(adopt_legacy_interpreter_path(&mut config).unwrap(), "a null field is still a field");

        assert_eq!(config.section::<model::Settings>("babel").unwrap().interpreter_path, None);
        assert!(!serde_json::to_string(&config).unwrap().contains("python_interpreter_path"));
    }

    #[test]
    fn a_config_that_never_had_the_field_is_left_alone() {
        let mut config = Config::default();

        assert!(!adopt_legacy_interpreter_path(&mut config).unwrap());
        assert!(!config.has_section("babel"));
    }

    /// The device already chose an interpreter under the new name. The
    /// old field is stale by definition and must not win.
    #[test]
    fn an_existing_section_is_not_overwritten() {
        let mut config = with_legacy_path("/old/python");
        config
            .set_section("babel", &model::Settings { interpreter_path: Some("/new/python".into()), bash_path: None })
            .unwrap();

        adopt_legacy_interpreter_path(&mut config).unwrap();

        assert_eq!(
            config.section::<model::Settings>("babel").unwrap().interpreter_path.as_deref(),
            Some("/new/python")
        );
    }
}

/// Everything this plugin brings, in one call.
///
/// The app adds it with `.plugin(nobo_babel::init())` and knows nothing
/// else about org-babel -- the file kinds, the migration, the settings
/// section and the feature registration all announce themselves from in
/// here (see nobo_core::core's inventory collections).
///
/// The bindings are written during a debug build, next to the frontend
/// half that imports them. Generated where they are used rather than
/// into the app's one big file: a plugin's commands are its own, and so
/// are its types.
#[cfg(feature = "tauri")]
pub fn init<R: tauri::Runtime>() -> tauri::plugin::TauriPlugin<R> {
    let specta = specta_builder::<R>();
    tauri::plugin::Builder::new("nobo-babel")
        .invoke_handler(specta.invoke_handler())
        .setup(|app, _api| {
            use tauri::Manager;
            // The session registry is this plugin's state, not the app's.
            app.manage(std::sync::Arc::new(SessionRegistry::default()));
            Ok(())
        })
        .build()
}

/// This plugin's command surface, in the one place both the runtime and
/// the bindings read it from.
#[cfg(feature = "tauri")]
fn specta_builder<R: tauri::Runtime>() -> tauri_specta::Builder<R> {
    tauri_specta::Builder::<R>::new()
        .plugin_name("nobo-babel")
        .commands(tauri_specta::collect_commands![
            tauri_commands::babel_settings,
            tauri_commands::babel_save_settings,
            tauri_commands::babel_is_trusted,
            tauri_commands::babel_trust_file,
            tauri_commands::babel_execute_block,
            tauri_commands::babel_kill_session,
            tauri_commands::babel_discover_bash,
            tauri_commands::babel_discover_python_interpreters,
            tauri_commands::babel_create_venv,
            tauri_commands::babel_tangle_write,
            tauri_commands::babel_grant_tangle_root,
            tauri_commands::babel_tangle_roots,
            tauri_commands::babel_revoke_tangle_root,
        ])
}

/// Writes this plugin's `bindings.ts` next to the frontend half that
/// imports it.
///
/// A test, like the app's own export (see nobo's lib.rs): running the
/// tests is what regenerates them, so a command whose signature changed
/// without the frontend being regenerated fails the build rather than the
/// device.
#[cfg(all(test, feature = "tauri"))]
mod bindings_export {
    #[test]
    fn export_bindings() {
        super::specta_builder::<tauri::Wry>()
            .export(
                specta_typescript::Typescript::default(),
                "../../src/features/babel/bindings.ts",
            )
            .expect("failed to write the org-babel bindings");
    }
}
