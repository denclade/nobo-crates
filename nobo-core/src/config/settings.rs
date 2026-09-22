use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::sync::OnceLock;
use crate::utils::ToCmdResult;

fn default_24h() -> bool { true }

fn default_clock_show_seconds() -> bool { true }

/// Default attachment folder: a dot-prefixed sibling of each note, hidden
/// from the OS file browser (the "Zettelkasten" convention). Relative
/// values resolve against the note's own directory; absolute values are
/// used as-is but must stay inside `notes_path` (enforced on the frontend
/// when resolving, see `attachmentPath.ts`).
fn default_attachments_dir() -> String { String::from(".attachments") }

/// Editor Tab/Shift-Tab block-indent width (see orgIndentActions.ts) --
/// 4 spaces matches this app's own org files' existing convention (list
/// nesting is already 2 spaces per level, headings/tables aren't affected
/// by this at all), and is the common default across other editors.
fn default_tab_indent_size() -> u32 { 4 }

/// A later addition, so it needs a serde default -- a settings.json
/// written before this field existed must still deserialize.
fn default_someday_path() -> String { String::from("./.nobo/someday.org") }


/// Mirrors `crate::utils::effort_utils::DEFAULT_EFFORT_PRESETS_ORG` -- kept
/// as a separate literal (not a re-export used directly as the serde
/// default) since `#[serde(default = "...")]` needs a fn item, and the
/// effort_utils constant already documents the "1d = 8h workday" reasoning
/// once; duplicating the string itself is the simplest way to satisfy both.
fn default_effort_presets_org() -> String {
    crate::utils::effort_utils::DEFAULT_EFFORT_PRESETS_ORG.to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    #[default]
    System,
    Light,
    Dark,
}

/// Which section opens when the app starts / when navigating to `/`.
/// Variant names double as the route path segment (`Dashboard` -> `/dashboard`),
/// see `useDefaultViewPath` on the frontend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "lowercase")]
pub enum DefaultView {
    Inbox,
    Actions,
    #[default]
    Dashboard,
    Projects,
    Notes,
}

/// Default sort for the Notes file browser -- persisted (unlike its other,
/// more ephemeral view toggles, e.g. "include subfolders") since it's a
/// standing preference about how the vault reads, not a one-off search
/// state. kebab-case matches the frontend's own DirEntrySortMode string
/// union (see utils/fileUtils.ts) so no mapping is needed on that side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "kebab-case")]
pub enum NotesSortMode {
    #[default]
    DateDesc,
    DateAsc,
    TitleAsc,
    TitleDesc,
}

/// A sort mode this build does not know -- written by a newer version, or
/// one that has since been dropped -- degrades to the default.
///
/// Without this the whole file goes: `Config::load` discards a config it
/// cannot parse ENTIRELY (see its own log line), so one unknown string
/// here would cost the user their vault path, their sync and every other
/// setting. Same spirit as file_kind's unknown-kind fallback.
fn lenient_sort_mode<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<NotesSortMode, D::Error> {
    let raw = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value(raw).unwrap_or_default())
}

/// A user-named filter query for the Search "all tasks" scope (see
/// utils/taskFilter.ts on the frontend) -- persisted as the raw query
/// string rather than a structured chip list, since the same text parser
/// that drives live typing can reconstruct an equivalent filter state from
/// it on load (see useTaskFilter.ts's loadPreset).
#[derive(Debug, Clone, Serialize, Deserialize, specta::Type)]
pub struct SavedTaskFilter {
    pub name: String,
    pub query: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, specta::Type)]
pub struct Config {
    pub notes_path: String,
    pub language: String,
    pub default_author: Option<String>,
    pub actions_file_path: String,
    pub projects_file_path: String,
    pub inbox_file_path: String,
    pub archive_done_after_days: Option<u32>,

    #[serde(default = "default_24h")]
    pub use_24h_format: bool,

    /// Whether the running-timer display includes seconds or rounds to
    /// whole minutes. The tick runs every second either way, so turning
    /// it back on mid-session shows an accurate count immediately -- this
    /// only controls what is rendered, for a calmer display.
    #[serde(default = "default_clock_show_seconds")]
    pub clock_show_seconds: bool,

    #[serde(default)]
    pub theme: Theme,

    #[serde(default)]
    pub default_view: DefaultView,

    // The lenient reader only widens what is ACCEPTED; the value is still
    // a NotesSortMode, which specta has to be told explicitly.
    #[specta(type = NotesSortMode)]
    #[serde(default, deserialize_with = "lenient_sort_mode")]
    pub notes_sort_mode: NotesSortMode,

    /// Same sort-mode concept as notes_sort_mode above, one each for
    /// Inbox/Actions/Projects (see the frontend's own TaskSortMode alias,
    /// utils/taskTree.ts) -- reuses NotesSortMode's type rather than a
    /// parallel enum since the four options (date/title x asc/desc) mean
    /// exactly the same thing in both places, just measured against a
    /// task's own `LAST_ACTIVITY` property instead of a note file's mtime.
    // The lenient reader only widens what is ACCEPTED; the value is still
    // a NotesSortMode, which specta has to be told explicitly.
    #[specta(type = NotesSortMode)]
    #[serde(default, deserialize_with = "lenient_sort_mode")]
    pub inbox_sort_mode: NotesSortMode,
    // The lenient reader only widens what is ACCEPTED; the value is still
    // a NotesSortMode, which specta has to be told explicitly.
    #[specta(type = NotesSortMode)]
    #[serde(default, deserialize_with = "lenient_sort_mode")]
    pub actions_sort_mode: NotesSortMode,
    // The lenient reader only widens what is ACCEPTED; the value is still
    // a NotesSortMode, which specta has to be told explicitly.
    #[specta(type = NotesSortMode)]
    #[serde(default, deserialize_with = "lenient_sort_mode")]
    pub projects_sort_mode: NotesSortMode,

    /// Folder attachments (pasted/dropped images, PDFs, ...) are placed
    /// into. Relative values (default: dot-prefixed sibling) resolve
    /// against each note's own directory; absolute values are used as-is.
    /// Either way the resolved path must stay inside `notes_path`.
    #[serde(default = "default_attachments_dir")]
    pub attachments_dir: String,

    /// "Show all" toggles for the Actions list and the Projects sidebar
    /// (both hide DONE items by default) -- persisted rather than a local
    /// signal so the choice survives navigating away and app restarts, see
    /// useTaskWorkspace.ts.
    #[serde(default)]
    pub show_done_actions: bool,
    #[serde(default)]
    pub show_done_projects: bool,
    /// A single project's own subtask list has its own, separate "hide
    /// done" toggle (state-based, not progress-based like show_done_projects
    /// above -- see InlineSubtaskList.tsx).
    #[serde(default)]
    pub hide_done_subtasks: bool,

    /// How many spaces Tab/Shift-Tab shifts a selected block of editor
    /// lines by (see orgIndentActions.ts) -- never applies inside a table,
    /// where Tab already means "next cell".
    #[serde(default = "default_tab_indent_size")]
    pub tab_indent_size: u32,

    /// Whether the editor shows a line-number gutter (see codeMirrorSetup.ts's
    /// editorBaseSetup / OrgEditor.tsx's lineNumbersCompartment) -- off by
    /// default, since org-mode headings/outline structure are the thing
    /// that actually orients you in a note, not line numbers; toggled from
    /// EditorToolbar's "More" menu for anyone who still wants them.
    #[serde(default)]
    pub line_numbers_enabled: bool,

    /// Someday/Maybe list (GTD) -- same "one flat org file, refile in and
    /// out of it" shape as actions/inbox/projects above, just a later
    /// addition (see default_someday_path's own doc comment on why this
    /// one specifically needs a serde default).
    #[serde(default = "default_someday_path")]
    pub someday_file_path: String,

    /// Whether the app has already asked (accepted OR declined either way)
    /// about backfilling CUSTOM_ID onto headings that predate that feature
    /// (see core::heading_identity/commands_headings::check_missing_custom_ids)
    /// -- a genuinely one-time prompt, not re-asked on a later startup even
    /// if the user declined, matching how e.g. the sync feature's
    /// `sync_remembered` (in its own section since settings became
    /// per-feature -- see nobo-sync's commands.rs) is the single durable
    /// record of a yes/no the user already made.
    #[serde(default)]
    pub custom_id_backfill_prompted: bool,

    /// Off by default -- shows a "raw" button in the Projects/Actions/Inbox
    /// detail header that opens the heading's own subtree as plain org
    /// syntax (currently read-only; see RawSourceModal.tsx's own doc
    /// comment on why it's built to make an editable mode a later,
    /// additive change). Gated behind this flag rather than always shown:
    /// raw org syntax (stars, PROPERTIES drawers, TODO keywords) is
    /// confusing to a reader who only ever uses the structured
    /// title/status/tags UI, so this is opt-in for people who already know
    /// what they're looking at.
    #[serde(default)]
    pub show_raw_heading_button: bool,

    /// Off by default (Experimental, see SettingsGeneral.tsx) -- whether
    /// pulling down from the very top of a phone screen (above any
    /// registered list, see useNativeGesture.ts) triggers a sync run.
    /// Independent of pull_to_reveal_search_enabled below: this app's
    /// native touch layer only has ONE overall pullEnabled switch (see
    /// GesturesPlugin.kt's own PullGestureLayout), so the frontend arms it
    /// whenever EITHER flag is on, but only actually STARTS a sync when
    /// this one specifically is -- see mountNativeGestures' own
    /// pullRefresh handler.
    #[serde(default)]
    pub pull_to_sync_enabled: bool,

    /// Off by default (Experimental, see SettingsGeneral.tsx) -- whether
    /// pulling down further, once a sidebar list is already scrolled to
    /// its own top, reveals that list's search field (see
    /// useNativeGesture.ts's onPullReveal). Independent of
    /// pull_to_sync_enabled above.
    #[serde(default)]
    pub pull_to_reveal_search_enabled: bool,

    /// Global fallback quick-select steps for the Effort chips (see
    /// EffortPicker.tsx), in the exact same space-separated Org H:MM
    /// syntax as the real in-buffer setting it mirrors: `#+PROPERTY:
    /// Effort_ALL <values>`. A FILE's own `#+PROPERTY: Effort_ALL` line
    /// always wins over this when present (see
    /// commands_headings::get_effort_presets) -- this is only the default
    /// for files that don't define their own.
    #[serde(default = "default_effort_presets_org")]
    pub effort_presets_org: String,

    /// Named filter queries saved from the Search "all tasks" scope's
    /// filter bar (see TaskFilterBar.tsx's "Gespeicherte Filter" menu).
    #[serde(default)]
    pub saved_task_filters: Vec<SavedTaskFilter>,

    /// Everything in the file that is not a field above: one key per
    /// feature (see `config::section`), plus anything a newer build wrote
    /// that this one has never heard of.
    ///
    /// This is what makes sections safe. Saving the general settings
    /// serializes this struct, so without a place to keep them, every
    /// unknown key in `settings.json` would be dropped -- a build without
    /// caldav would silently delete the caldav section, and downgrading
    /// once would cost the settings of everything newer.
    ///
    /// Never read directly: `section()` / `set_section()` below are the
    /// door, and `specta(skip)` keeps it out of the generated types, so
    /// the frontend cannot round-trip it (it would send back the copy it
    /// loaded at startup, overwriting whatever a feature saved since).
    #[serde(flatten, default)]
    #[specta(skip)]
    sections: serde_json::Map<String, serde_json::Value>,

    /// Keys taken out on purpose, so `save` does not put them back.
    ///
    /// Saving keeps whatever the file already holds (see `save`), which
    /// is what makes sections survive a frontend that has never heard of
    /// them -- and would otherwise resurrect exactly the key a migration
    /// just moved. Never serialized: it describes this one edit, not the
    /// file.
    #[serde(skip)]
    #[specta(skip)]
    removed: Vec<String>,
}

impl Config {
    /// What FEATURE stored, or `None` if it stored nothing this build can
    /// read (see `config::section`, which turns that into a default).
    pub fn section<T: serde::de::DeserializeOwned>(&self, feature: &str) -> Option<T> {
        serde_json::from_value(self.sections.get(feature)?.clone()).ok()
    }

    /// Replaces one feature's section, leaving every other key alone.
    pub fn set_section<T: Serialize>(&mut self, feature: &str, value: &T) -> Result<(), String> {
        let value = serde_json::to_value(value).map_err(|e| format!("could not write the {feature} settings: {e}"))?;
        self.sections.insert(feature.to_string(), value);
        Ok(())
    }

    /// Whether the key exists at all -- for a migration asking whether it
    /// has already run.
    pub fn has_section(&self, feature: &str) -> bool {
        self.sections.contains_key(feature)
    }

    /// Keeps the feature sections that are already on disk.
    ///
    /// A `Config` that came from the frontend has none: the generated
    /// type does not carry the bag (`specta(skip)`), so the payload
    /// deserializes with it empty -- and saving that as-is would delete
    /// every feature's settings the moment somebody changes the theme.
    /// Whoever saves a config from outside this process calls this first
    /// (see commands_config::save_config).
    ///
    /// Merges rather than replaces: anything already set here wins, which
    /// makes it safe for a caller that has just written a section itself.
    pub fn carry_sections_from_disk(&mut self) {
        if let Some(stored) = Self::load() {
            self.carry_sections_from(stored);
        }
    }

    /// The rule behind it, for a given other config.
    pub(crate) fn carry_sections_from(&mut self, stored: Self) {
        for (key, value) in stored.sections {
            if self.removed.contains(&key) {
                continue;
            }
            self.sections.entry(key).or_insert(value);
        }
    }

    /// Takes a top-level key an older build wrote out of the file.
    ///
    /// The only way a feature can reach a setting that used to be a field
    /// in this struct -- `python_interpreter_path`, `caldav_accounts` and
    /// the rest were core fields with a feature's name on them, and are
    /// now sections (see `config::section`). Each feature reads its own
    /// once, from its own migration, and the key is gone afterwards.
    ///
    /// Deliberately untyped and deliberately awkward: this is a door for
    /// migrations, not a way for the core to keep knowing what a feature
    /// stores. It can go once no device is likely to be more than a
    /// release behind.
    /// Reads the settings, lets CHANGE have them, and saves only if it
    /// says something actually changed.
    ///
    /// The shape every migration that moves keys into a feature's own
    /// section has (see core::migrations, and the three that do it):
    /// load_for_update rather than load, because a file that cannot be
    /// parsed must stop a migration rather than be replaced by defaults;
    /// and no write at all when there was nothing to move, so a device
    /// that has already migrated does not rewrite the file on every
    /// start.
    pub fn update(change: impl FnOnce(&mut Self) -> Result<bool, String>) -> Result<(), String> {
        let mut config = Self::load_for_update()?;
        if !change(&mut config)? {
            return Ok(());
        }
        config.save()
    }

    pub fn take_legacy_value(&mut self, key: &str) -> Option<serde_json::Value> {
        // Remembered as removed, or saving would carry it back in from
        // the copy on disk (see `carry_sections_from`).
        self.removed.push(key.to_string());
        self.sections.remove(key)
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            notes_path: String::new(),
            language: String::from("de_DE"),
            default_author: None,
            // .nobo/ (dot-prefixed) so these system files stay out of the
            // Notes browser by default, same reasoning as .attachments --
            // see core::indexer::directory_contents.
            actions_file_path: String::from("./.nobo/actions.org"),
            projects_file_path: String::from("./.nobo/projects.org"),
            inbox_file_path: String::from("./.nobo/inbox.org"),
            use_24h_format: true,
            clock_show_seconds: true,
            archive_done_after_days: None,
            theme: Theme::System,
            default_view: DefaultView::Dashboard,
            notes_sort_mode: NotesSortMode::DateDesc,
            inbox_sort_mode: NotesSortMode::DateDesc,
            actions_sort_mode: NotesSortMode::DateDesc,
            projects_sort_mode: NotesSortMode::DateDesc,
            attachments_dir: default_attachments_dir(),
            show_done_actions: false,
            show_done_projects: false,
            hide_done_subtasks: false,
            tab_indent_size: default_tab_indent_size(),
            line_numbers_enabled: false,
            someday_file_path: default_someday_path(),
            custom_id_backfill_prompted: false,
            show_raw_heading_button: false,
            pull_to_sync_enabled: false,
            pull_to_reveal_search_enabled: false,
            effort_presets_org: default_effort_presets_org(),
            saved_task_filters: Vec::new(),
            sections: serde_json::Map::new(),
            removed: Vec::new(),
        }
    }
}

/// Populated once from `lib.rs`'s `.setup()` hook on Android/iOS only (see
/// `init_mobile_config_dir`) -- `None` forever on desktop, where
/// `config_dir()` keeps using the `dirs` crate exactly as before.
static MOBILE_CONFIG_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Must run first inside Tauri's `.setup()` hook, before anything else
/// calls `config_dir()` -- including opening the history database.
///
/// `dirs::config_dir()` needs `$HOME`/XDG variables that do not exist on
/// mobile and returns `None`, which would fall back to a temp directory
/// the app's settings and history must never live in. Only the host can
/// resolve the real location -- under Tauri through its path resolver,
/// via a JNI call on Android (see `nobo_tauri::settings`) -- so what the
/// core offers is somewhere to put the answer.
///
/// First writer wins. On Android the background worker runs inside the
/// app's own process, so this can genuinely be called twice -- with the
/// same path both times, since both ask Android for it. Keeping the first
/// value means a run already under way cannot have the directory change
/// underneath it.
pub fn set_config_dir(dir: PathBuf) {
    let _ = MOBILE_CONFIG_DIR.set(dir);
}

/// Shared app config directory (`~/.config/nobo/` or platform equivalent) --
/// reused by anything that persists its own local state next to
/// `settings.json` (see e.g. `nobo_babel::trust`).
pub fn config_dir() -> PathBuf {
    if let Some(dir) = MOBILE_CONFIG_DIR.get() {
        return dir.clone();
    }
    let mut path = dirs::config_dir().unwrap_or_else(std::env::temp_dir);
    path.push("nobo");
    path
}

impl Config {
    fn get_path() -> PathBuf {
        let mut path = config_dir();
        path.push("settings.json");
        path
    }

    pub fn load() -> Option<Self> {
        let path = Self::get_path();
        match fs::read_to_string(&path) {
            Ok(content) => match serde_json::from_str(&content) {
                Ok(config) => Some(config),
                Err(e) => {
                    log::warn!("Config file at {:?} is corrupted, ignoring: {}", path, e);
                    None
                }
            },
            Err(_) => None,
        }
    }

    /// The config as a base for changing one part of it.
    ///
    /// The difference to `load` matters exactly once, and then it matters
    /// a lot: a settings file that EXISTS but cannot be parsed must not
    /// become `Config::default()` here, because the caller is about to
    /// save it back -- one unreadable byte would take every other setting
    /// with it. No file at all is a first run and defaults are right.
    pub fn load_for_update() -> Result<Self, String> {
        Self::for_update(&Self::get_path())
    }

    /// The rule behind `load_for_update`, for a given path.
    fn for_update(path: &std::path::Path) -> Result<Self, String> {
        match fs::read_to_string(path) {
            Ok(content) => serde_json::from_str(&content).map_err(|e| {
                format!(
                    "the settings file at {} could not be read ({e}) -- refusing to overwrite it",
                    path.display()
                )
            }),
            // No file at all: a first run, where defaults are right.
            Err(_) => Ok(Self::default()),
        }
    }

    /// Writes the file, keeping every feature section that is already in
    /// it.
    ///
    /// The merge is here rather than at the call sites because of where
    /// the danger is: a `Config` that came from the frontend has an empty
    /// section bag (the generated type does not carry it), so saving it
    /// verbatim would delete every feature's settings. Doing it in the
    /// one place that writes the file means no caller has to remember --
    /// which is exactly what the old arrangement got wrong, where
    /// `save_config` had to re-read one named field from disk.
    ///
    /// Anything already set on `self` wins, so a feature saving its own
    /// section is not undone by the copy on disk.
    pub fn save(&self) -> Result<(), String> {
        let dir = config_dir();
        if !dir.exists() {
            fs::create_dir_all(&dir).to_cmd()?;
        }
        let mut to_write = self.clone();
        to_write.carry_sections_from_disk();
        let path = Self::get_path();
        let content = serde_json::to_string_pretty(&to_write).to_cmd()?;

        // Written beside the file and renamed over it, never into it.
        // `fs::write` truncates first, so for the length of the write
        // there IS no settings file: anyone reading it right then gets an
        // empty one, and "empty" parses as nothing, which this app reads
        // as "no vault configured". Two readers exist at any time (the
        // Android background worker, a second window) and a crash or a
        // full disk mid-write would leave that state behind for good.
        //
        // This used to be its own temp-and-rename, which is the same idea
        // without the `sync_all` that makes it true after a power loss and
        // without cleaning up its `.json.writing` when the write failed.
        // `write_atomic` is that, done once, for the whole app.
        let written = crate::utils::fs_utils::write_atomic(&path, &content).to_cmd();
        // The language may have just changed, and the text Rust produces
        // reads it from here (see core::i18n).
        crate::core::i18n::refresh();
        written
    }

    /// The vault root every synced file (notes, actions.org/projects.org/
    /// inbox.org/someday.org under their default paths, and anything else
    /// stored inside it -- see e.g. nobo_review::state) lives under. Shared
    /// by sync (nobo_sync::commands) and anything else that needs to place a
    /// file where sync will actually pick it up, rather than each caller
    /// re-deriving it from a freshly loaded Config on its own.
    pub fn require_vault_root() -> Result<PathBuf, String> {
        Self::load()
            .ok_or_else(|| crate::t!(
                "Keine Konfiguration gefunden -- zuerst einen Notiz-Ordner einrichten",
                "No configuration found -- set up a notes vault first"
            ))?
            .vault_root()
    }

    /// The same out of a `Config` already in hand -- for the caller that
    /// has read the file for something else too (see
    /// `config::section::Section::load_from`).
    pub fn vault_root(&self) -> Result<PathBuf, String> {
        if self.notes_path.is_empty() {
            return Err(crate::t!("Kein Notiz-Ordner eingerichtet", "No notes vault configured"));
        }
        Ok(PathBuf::from(&self.notes_path))
    }
}

#[cfg(test)]
mod tests {

    /// A settings file with one bad byte in it must not become
    /// Config::default() for a caller that is about to save it back --
    /// that would take every other setting with it.
    #[test]
    fn an_unreadable_settings_file_is_refused_rather_than_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "{ not json").unwrap();

        let error = Config::for_update(&path).unwrap_err();
        assert!(error.contains("refusing to overwrite"), "{error}");
    }

    /// No file at all is a first run, not a corrupt one.
    #[test]
    fn a_missing_settings_file_starts_from_the_defaults() {
        let dir = tempfile::tempdir().unwrap();

        let config = Config::for_update(&dir.path().join("settings.json")).unwrap();

        assert_eq!(config.notes_path, Config::default().notes_path);
    }

    /// The frontend sends the whole config back from a copy it loaded at
    /// startup, so it can only ever carry the fields its own type has.
    /// Everything else in the file has to survive that -- which is what
    /// the section bag is for (see config::section, and
    /// `carry_sections_from` for the other half of the rule).
    #[test]
    fn a_config_from_the_frontend_keeps_what_it_does_not_know() {
        let from_frontend: Config = serde_json::from_str(r#"{
            "notes_path": "/vault", "language": "de_DE", "default_author": null,
            "actions_file_path": "a.org", "projects_file_path": "p.org", "inbox_file_path": "i.org",
            "archive_done_after_days": null,
            "caldav": { "accounts": [], "lists_hide_done": true }
        }"#)
        .expect("a config from the frontend must still load");

        assert!(from_frontend.has_section("caldav"), "a feature section was dropped on the way in");
        assert!(serde_json::to_string(&from_frontend).unwrap().contains("lists_hide_done"));
    }
    use super::*;

    /// The whole reason lenient_sort_mode exists: Config::load throws away
    /// a config it cannot parse, so a sort mode this build does not know
    /// must cost the user that one setting -- not their vault path, their
    /// sync and everything else in the file.
    #[test]
    fn an_unknown_sort_mode_costs_only_itself() {
        let json = r#"{
            "notes_path": "/vault",
            "language": "de_DE",
            "actions_file_path": "./.nobo/actions.org",
            "projects_file_path": "./.nobo/projects.org",
            "inbox_file_path": "./.nobo/inbox.org",
            "archive_done_after_days": null,
            "python_interpreter_path": null,
            "notes_sort_mode": "favorites-first"
        }"#;

        let config: Config = serde_json::from_str(json).expect("the rest of the file still loads");

        assert_eq!(config.notes_path, "/vault");
        assert_eq!(config.notes_sort_mode, NotesSortMode::default());
    }

    #[test]
    fn a_known_sort_mode_still_arrives_intact() {
        let json = r#"{
            "notes_path": "/vault",
            "language": "de_DE",
            "actions_file_path": "a.org",
            "projects_file_path": "p.org",
            "inbox_file_path": "i.org",
            "archive_done_after_days": null,
            "python_interpreter_path": null,
            "notes_sort_mode": "title-asc"
        }"#;

        let config: Config = serde_json::from_str(json).unwrap();

        assert_eq!(config.notes_sort_mode, NotesSortMode::TitleAsc);
    }

    #[test]
    fn saved_task_filters_round_trip_through_json() {
        let mut config = Config::default();
        config.saved_task_filters.push(SavedTaskFilter {
            name: "Kurze Laptop-Aufgaben".to_string(),
            query: "@Laptop time:<15m".to_string(),
        });

        let json = serde_json::to_string(&config).unwrap();
        let restored: Config = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.saved_task_filters.len(), 1);
        assert_eq!(restored.saved_task_filters[0].name, "Kurze Laptop-Aufgaben");
        assert_eq!(restored.saved_task_filters[0].query, "@Laptop time:<15m");
    }

    #[test]
    fn config_json_missing_saved_task_filters_deserializes_to_empty_vec() {
        // A settings.json written before this field existed must still
        // load, via serde's default.
        let mut value = serde_json::to_value(Config::default()).unwrap();
        value.as_object_mut().unwrap().remove("saved_task_filters");

        let restored: Config = serde_json::from_value(value).unwrap();
        assert!(restored.saved_task_filters.is_empty());
    }
}