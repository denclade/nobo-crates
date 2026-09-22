//! A feature's own corner of `settings.json`.
//!
//! The core owns the file, a feature owns a key in it -- the same split
//! `core::secrets` makes for the keychain and `core::migrations` for
//! versioning. What a feature stores there is its own typed struct; the
//! core never learns a field of it.
//!
//! ```ignore
//! // features/babel/mod.rs
//! #[derive(Default, Serialize, Deserialize, specta::Type)]
//! pub struct Settings { pub interpreter_path: Option<String> }
//! pub const SETTINGS: Section<Settings> = Section::new("babel");
//! ```
//!
//! # Why a section rather than a field in `Config`
//!
//! Because of what saving means. The frontend edits a copy of the whole
//! `Config` and sends it back, so every field in it is written by whoever
//! saves anything -- which is why the fields the backend owns had to be
//! read back from disk mid-save, and why `Config` carries `specta(skip)`
//! fields the frontend must never see. A section is read and written on
//! its own: saving the general settings cannot touch it, and a feature
//! saving its own cannot touch anything else.
//!
//! # What it promises
//!
//! - **Reading never fails.** A missing key, or one holding something
//!   this build cannot parse, reads as `T::default()`. A feature asks for
//!   its settings on every command; there is no useful way to handle "the
//!   file has a typo in it" there, and refusing to run is worse than
//!   starting from defaults.
//! - **Writing touches one key.** Everything else in the file -- other
//!   sections included, known to this build or not -- is read, kept and
//!   written back untouched (see `Config::sections`). A build without
//!   caldav must not delete the caldav section.
//! - **Nothing is cached.** Every call reads the file. Settings change
//!   rarely and are read on a command, not in a loop; a cache would need
//!   invalidating across two processes (the Android worker runs without
//!   the app).

use std::marker::PhantomData;

use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::config::settings::Config;

/// One feature's settings, stored under NAME in `settings.json`.
///
/// `const`-constructible so a feature can name it once
/// (`pub const SETTINGS: Section<Settings> = Section::new("babel")`) and
/// every command reads through the same handle.
pub struct Section<T> {
    name: &'static str,
    kind: PhantomData<fn() -> T>,
}

impl<T> Section<T>
where
    T: Serialize + DeserializeOwned + Default,
{
    pub const fn new(name: &'static str) -> Self {
        Section { name, kind: PhantomData }
    }

    /// What is stored, or the default. See the module doc on why this
    /// cannot fail.
    pub fn load(&self) -> T {
        Config::load()
            .and_then(|config| config.section(self.name))
            .unwrap_or_default()
    }

    /// The same, out of a `Config` that has already been read.
    ///
    /// For the caller that needs both halves of `settings.json` -- its own
    /// section and something general, usually the vault root. `load()`
    /// there would read and parse the same file a second time in the same
    /// command, and that is how a "reads settings once per command" turns
    /// into "twice, three times, once per item".
    pub fn load_from(&self, config: &Config) -> T {
        config.section(self.name).unwrap_or_default()
    }

    /// Replaces this section, and only this section.
    pub fn save(&self, value: &T) -> Result<(), String> {
        let mut config = Config::load_for_update()?;
        config.set_section(self.name, value)?;
        config.save()
    }

    /// Reads, changes, writes -- for a caller that wants to move one
    /// field without spelling out the rest.
    pub fn update(&self, change: impl FnOnce(&mut T)) -> Result<(), String> {
        let mut value = self.load();
        change(&mut value);
        self.save(&value)
    }

    /// Whether anything is stored under this name at all -- for a
    /// migration deciding whether it has already run.
    pub fn is_present(&self) -> bool {
        Config::load().is_some_and(|config| config.has_section(self.name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
    struct Probe {
        path: Option<String>,
        count: u32,
    }

    /// A config as it comes off disk, plus a key this build has never
    /// heard of.
    fn with_extra_key(key: &str, value: serde_json::Value) -> Config {
        let mut json = serde_json::to_value(Config::default()).unwrap();
        json.as_object_mut().unwrap().insert(key.to_string(), value);
        serde_json::from_value(json).unwrap()
    }

    /// The failure this whole type exists to prevent: a build that knows
    /// nothing about a section must not drop it when it saves.
    #[test]
    fn an_unknown_section_survives_a_save_of_everything_else() {
        let mut config = with_extra_key("from_the_future", serde_json::json!({ "whatever": 42 }));

        config.set_section("probe", &Probe { path: Some("/usr/bin/python3".into()), count: 1 }).unwrap();
        let written = serde_json::to_string(&config).unwrap();

        assert!(written.contains("from_the_future"), "{written}");
        assert!(written.contains("probe"), "{written}");
    }

    /// The exact shape of a save from the frontend: it sends back the
    /// type it knows, which has no sections in it at all. Without
    /// carrying them, changing the theme would delete every feature's
    /// settings.
    #[test]
    fn a_save_from_the_frontend_keeps_the_sections_it_never_saw() {
        let mut on_disk = Config::default();
        on_disk.set_section("probe", &Probe { path: Some("/usr/bin/python3".into()), count: 1 }).unwrap();

        // What the frontend sends: everything but the bag.
        let mut from_frontend = Config::default();
        from_frontend.notes_path = "/vault".into();
        from_frontend.carry_sections_from(on_disk);

        assert_eq!(
            from_frontend.section::<Probe>("probe"),
            Some(Probe { path: Some("/usr/bin/python3".into()), count: 1 })
        );
        assert_eq!(from_frontend.notes_path, "/vault", "the payload still wins for its own fields");
    }

    /// The trap the `removed` list exists for: a migration takes an old
    /// key out and saves -- and saving keeps whatever the file already
    /// has, which is the key it just took out.
    #[test]
    fn a_key_taken_on_purpose_is_not_carried_back_in() {
        let mut on_disk = with_extra_key("python_interpreter_path", serde_json::json!("/old/python"));
        let stored_copy = with_extra_key("python_interpreter_path", serde_json::json!("/old/python"));

        let taken = on_disk.take_legacy_value("python_interpreter_path");
        on_disk.carry_sections_from(stored_copy);

        assert_eq!(taken, Some(serde_json::json!("/old/python")));
        assert!(
            !serde_json::to_string(&on_disk).unwrap().contains("python_interpreter_path"),
            "the migrated key came back on save"
        );
    }

    /// A caller that has just written a section is not undone by what is
    /// still on disk.
    #[test]
    fn carrying_does_not_overwrite_a_section_already_set() {
        let mut on_disk = Config::default();
        on_disk.set_section("probe", &Probe { path: Some("/old".into()), count: 1 }).unwrap();

        let mut fresh = Config::default();
        fresh.set_section("probe", &Probe { path: Some("/new".into()), count: 2 }).unwrap();
        fresh.carry_sections_from(on_disk);

        assert_eq!(fresh.section::<Probe>("probe").unwrap().path.as_deref(), Some("/new"));
    }

    #[test]
    fn a_section_reads_back_what_it_wrote() {
        let mut config = Config::default();
        let probe = Probe { path: Some("/opt/py".into()), count: 3 };

        config.set_section("probe", &probe).unwrap();

        assert_eq!(config.section::<Probe>("probe"), Some(probe));
    }

    #[test]
    fn a_missing_section_is_absent_rather_than_an_error() {
        assert_eq!(Config::default().section::<Probe>("probe"), None);
        assert!(!Config::default().has_section("probe"));
    }

    /// Someone hand-edited the file, or an older build wrote a different
    /// shape. Defaults beat refusing to start.
    #[test]
    fn a_section_this_build_cannot_parse_reads_as_absent() {
        let config = with_extra_key("probe", serde_json::json!("not an object"));

        assert_eq!(config.section::<Probe>("probe"), None);
        // Still there, though -- reading it wrongly is no reason to
        // delete somebody's data.
        assert!(config.has_section("probe"));
    }
}
