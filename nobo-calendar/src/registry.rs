// SPDX-FileCopyrightText: 2026 Dennis Burgermeister
// SPDX-License-Identifier: GPL-3.0-or-later

//! The subscribed calendars, read out of the mirrored files themselves.
//!
//! There is deliberately no list of calendars in settings.json. A feed
//! needs nothing but its URL to be fetched -- no account, no password (a
//! CalDAV list does, which is why that one still lives in the settings) --
//! so the mirror file under `.nobo/calendars/` can carry everything the
//! subscription consists of: `#+TITLE:` the name, `#+NOBO_CALENDAR_URL:`
//! the feed, and the absence of `#+NOBO_CALENDAR_ENABLED: nil` the fact
//! that it is switched on. The file name is the id.
//!
//! That the vault is synced then does the rest, without a single line of
//! sync code here: subscribe on one device and the file appears on the
//! others, which pick it up as a subscription; delete it and it is gone
//! everywhere. The alternative -- the list in the (unsynced) settings and
//! the file in the vault -- meant holding the same fact twice and
//! reconciling the two halves, including deciding what a file without a
//! settings entry (or the reverse) is supposed to mean.

use std::path::{Path, PathBuf};

use crate::model::CalendarSubscription;
use nobo_core::core::file_kind;

use super::identity::subscription_id;
use super::sync::{calendars_dir, is_calendar_file};

use super::model::COLOR_KEYWORD;

pub const URL_KEYWORD: &str = "#+NOBO_CALENDAR_URL:";
pub const ENABLED_KEYWORD: &str = "#+NOBO_CALENDAR_ENABLED:";
/// How often this calendar is fetched (see model::RefreshInterval). In the
/// file for the same reason as the colour: it is a property of the
/// calendar, not of the device looking at it, so it travels with the
/// vault.
pub const REFRESH_KEYWORD: &str = "#+NOBO_CALENDAR_REFRESH:";
/// Org's own false. `t` would be its true, but "on" is written as the
/// absence of the line, so that value never appears.
pub const DISABLED_VALUE: &str = "nil";

/// Every subscription in the vault, by name.
///
/// A file that carries no URL is not a subscription and is skipped rather
/// than guessed at: it is either a mirror from a version before this (see
/// `migrate_from_settings`, which is what fixes those) or something else
/// entirely that happens to sit in the directory.
pub fn list(vault_root: &Path) -> Vec<CalendarSubscription> {
    let Ok(entries) = std::fs::read_dir(calendars_dir(vault_root)) else { return Vec::new() };

    let mut subscriptions: Vec<CalendarSubscription> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| is_calendar_file(vault_root, path))
        .filter_map(|path| read(&path))
        .collect();
    subscriptions.sort_by_key(|subscription| subscription.name.to_lowercase());
    subscriptions
}

pub fn find(vault_root: &Path, id: &str) -> Option<CalendarSubscription> {
    list(vault_root).into_iter().find(|subscription| subscription.id == id)
}

/// Reads one mirror file's header back into the subscription it describes.
pub fn read(path: &Path) -> Option<CalendarSubscription> {
    let text = std::fs::read_to_string(path).ok()?;
    let lines: Vec<&str> = text.lines().collect();
    let url = file_kind::leading_keyword(&lines, URL_KEYWORD)?;
    let id = path.file_stem()?.to_string_lossy().into_owned();

    Some(CalendarSubscription {
        color: file_kind::leading_keyword(&lines, COLOR_KEYWORD).filter(|c| !c.is_empty()),
        // The URL as the fallback name matches what the Add form does with
        // an empty name field.
        name: file_kind::leading_keyword(&lines, "#+TITLE:").filter(|t| !t.is_empty()).unwrap_or_else(|| url.clone()),
        url,
        enabled: file_kind::leading_keyword(&lines, ENABLED_KEYWORD).as_deref() != Some(DISABLED_VALUE),
        // An unreadable or absent value is the default rather than an
        // error: a header written by a newer version, or by hand in
        // Emacs, must not cost the whole subscription.
        refresh: file_kind::leading_keyword(&lines, REFRESH_KEYWORD)
            .and_then(|value| serde_json::from_value(serde_json::Value::String(value.trim().to_string())).ok())
            .unwrap_or_default(),
        id,
    })
}

/// The file a subscription would live in, whether or not it exists yet.
pub fn file_for(vault_root: &Path, id: &str) -> PathBuf {
    calendars_dir(vault_root).join(format!("{id}.org"))
}

/// Writes a subscription's file with no events in it -- what adding and
/// disabling both produce.
///
/// Adding: the file has to exist before the first fetch, or a feed whose
/// server is down would leave nothing behind at all and the subscription
/// would be lost on reload. Disabling: dropping the events is what takes
/// the calendar out of the agenda, while the header keeps the
/// subscription itself (and carries the off state to the other devices).
pub fn write_without_events(vault_root: &Path, subscription: &CalendarSubscription) -> Result<(), String> {
    let target = file_for(vault_root, &subscription.id);
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("could not create {}: {e}", parent.display()))?;
    }
    nobo_core::utils::fs_utils::write_atomic(&target, &super::org::render_calendar(subscription, &[]))
        .map_err(|e| format!("could not write {}: {e}", target.display()))
}

/// Removes a subscription's file, which is what unsubscribing IS now.
/// Missing is success -- the goal is "not there".
/// Rewrites one keyword in a mirror file's header, leaving everything
/// below it alone.
///
/// Bypasses the write guard on purpose (see fs_utils::write_lines): the
/// guard keeps the app's EDITORS out of generated files, and this is the
/// generator itself changing what it generated. Used for the colour, which
/// is the one part of a subscription the user sets without anything being
/// re-fetched -- rewriting the whole file for it would blank the entries
/// until the next refresh.
pub fn set_keyword(path: &Path, keyword: &str, value: Option<&str>) -> Result<(), String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("could not read {}: {e}", path.display()))?;
    let lines: Vec<String> = text.lines().map(String::from).collect();
    // The rule is file_kind's -- only the writing is this module's
    // business, because the guard there refuses generated files.
    let Some(next) = file_kind::splice_leading_keyword(lines, keyword, value) else { return Ok(()) };

    let mut out = next.join("\n");
    out.push('\n');
    nobo_core::utils::fs_utils::write_atomic(path, &out).map_err(|e| format!("could not write {}: {e}", path.display()))
}

/// Unsubscribing is deleting the file, which is what unsubscribing IS
/// here -- and the vault sync carries it to the other devices by itself.
/// Missing is success: the goal is "not there".
pub fn remove(vault_root: &Path, id: &str) -> Result<(), String> {
    match std::fs::remove_file(file_for(vault_root, id)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("could not remove the calendar file: {e}")),
    }
}

/// The subscription a URL would produce, without writing anything.
pub fn subscription_for(name: &str, url: &str) -> CalendarSubscription {
    // Stored the way it will be fetched and hashed, not the way it was
    // pasted: `webcal://` is what a calendar app hands out and plain
    // HTTPS is what it means (see sync::normalize_url).
    let url = super::sync::normalize_url(url);
    let name = name.trim();
    CalendarSubscription {
        id: subscription_id(&url),
        color: None,
        name: if name.is_empty() { url.clone() } else { name.to_string() },
        url,
        enabled: true,
        refresh: crate::model::RefreshInterval::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vault() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(calendars_dir(dir.path())).unwrap();
        dir
    }

    /// The round trip the whole design rests on: what was written as a
    /// subscription reads back as the same subscription, on any device.
    #[test]
    fn a_written_subscription_reads_back_out_of_its_file() {
        let vault = vault();
        let subscription = subscription_for("Feiertage", "https://example.com/cal.ics");
        write_without_events(vault.path(), &subscription).unwrap();

        let found = list(vault.path());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, subscription.id);
        assert_eq!(found[0].name, "Feiertage");
        assert_eq!(found[0].url, "https://example.com/cal.ics");
        assert!(found[0].enabled);
    }

    #[test]
    fn a_disabled_subscription_survives_as_a_subscription() {
        let vault = vault();
        let mut subscription = subscription_for("Feiertage", "https://example.com/cal.ics");
        subscription.enabled = false;
        write_without_events(vault.path(), &subscription).unwrap();

        let found = list(vault.path());
        assert_eq!(found.len(), 1);
        assert!(!found[0].enabled);
    }

    /// Unsubscribing is deleting the file, so a synced deletion needs no
    /// separate mechanism to reach the other devices.
    #[test]
    fn removing_the_file_removes_the_subscription() {
        let vault = vault();
        let subscription = subscription_for("Feiertage", "https://example.com/cal.ics");
        write_without_events(vault.path(), &subscription).unwrap();

        remove(vault.path(), &subscription.id).unwrap();

        assert!(list(vault.path()).is_empty());
        // Removing what is not there is still success.
        remove(vault.path(), &subscription.id).unwrap();
    }

    /// A file from before this change, or any other org file that wandered
    /// into the directory, must not be read as a subscription with a
    /// nonsense URL.
    #[test]
    fn a_file_without_a_url_is_not_a_subscription() {
        let vault = vault();
        std::fs::write(calendars_dir(vault.path()).join("something.org"), "#+TITLE: Alt\n#+NOBO_TYPE: calendar-mirror\n\n* Termin\n")
            .unwrap();

        assert!(list(vault.path()).is_empty());
    }

    #[test]
    fn an_unnamed_calendar_falls_back_to_its_url() {
        let vault = vault();
        write_without_events(vault.path(), &subscription_for("   ", "https://example.com/cal.ics")).unwrap();

        assert_eq!(list(vault.path())[0].name, "https://example.com/cal.ics");
    }

    /// Two devices subscribing to one feed have to name one file, or the
    /// synced vault ends up mirroring it twice.
    #[test]
    fn the_same_feed_lands_in_the_same_file_on_every_device() {
        let vault = vault();
        write_without_events(vault.path(), &subscription_for("Feiertage", "https://example.com/cal.ics")).unwrap();
        write_without_events(vault.path(), &subscription_for("Holidays", " https://example.com/cal.ics ")).unwrap();

        assert_eq!(list(vault.path()).len(), 1);
    }

    /// The colour is part of the subscription, so it travels with the
    /// vault like the rest of it -- a colour only one device knew would
    /// make the agenda mean something different depending on where it is
    /// read.
    #[test]
    fn a_colour_survives_in_the_file_and_reads_back() {
        let vault = vault();
        let mut subscription = subscription_for("Feiertage", "https://example.com/cal.ics");
        subscription.color = Some("#4f9d69".into());
        write_without_events(vault.path(), &subscription).unwrap();

        assert_eq!(list(vault.path())[0].color.as_deref(), Some("#4f9d69"));
    }

    /// Setting it must not cost the entries: the calendar would be empty
    /// in the agenda until the next refresh, for a colour change.
    #[test]
    fn setting_the_colour_leaves_the_entries_alone() {
        let vault = vault();
        let subscription = subscription_for("Feiertage", "https://example.com/cal.ics");
        let path = file_for(vault.path(), &subscription.id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, format!("{}* Neujahr\n", super::super::org::render_calendar(&subscription, &[]))).unwrap();

        set_keyword(&path, COLOR_KEYWORD, Some("#4f9d69")).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("* Neujahr"), "{text}");
        assert_eq!(list(vault.path())[0].color.as_deref(), Some("#4f9d69"));

        // ... and clearing it takes back exactly that line.
        set_keyword(&path, COLOR_KEYWORD, None).unwrap();
        assert!(std::fs::read_to_string(&path).unwrap().contains("* Neujahr"));
        assert_eq!(list(vault.path())[0].color, None);
    }

    /// The interval is the calendar's own property, so it has to survive
    /// the trip through the file -- that is what makes "every 30 minutes"
    /// mean the same thing on the phone as on the laptop.
    #[test]
    fn the_refresh_interval_travels_in_the_header() {
        let vault = vault();
        let mut subscription = subscription_for("Dienstplan", "https://example.com/plan.ics");
        subscription.refresh = crate::model::RefreshInterval::Every30Minutes;

        write_without_events(vault.path(), &subscription).unwrap();

        let read_back = read(&file_for(vault.path(), &subscription.id)).unwrap();
        assert_eq!(read_back.refresh, crate::model::RefreshInterval::Every30Minutes);
    }

    /// The default is written as the absence of the line, like the
    /// enabled state: an ordinary calendar's header stays short, and a
    /// file from before this existed reads as the default.
    #[test]
    fn the_default_interval_leaves_no_line_behind() {
        let vault = vault();
        let subscription = subscription_for("Feiertage", "https://example.com/cal.ics");

        write_without_events(vault.path(), &subscription).unwrap();

        let text = std::fs::read_to_string(file_for(vault.path(), &subscription.id)).unwrap();
        assert!(!text.contains(REFRESH_KEYWORD), "{text}");
        assert_eq!(read(&file_for(vault.path(), &subscription.id)).unwrap().refresh, crate::model::RefreshInterval::default());
    }

    /// A header somebody typed by hand, or one from a newer version: the
    /// value is not understood, and the calendar is still a calendar.
    #[test]
    fn an_unreadable_interval_falls_back_to_the_default() {
        let vault = vault();
        let subscription = subscription_for("Feiertage", "https://example.com/cal.ics");
        write_without_events(vault.path(), &subscription).unwrap();
        let path = file_for(vault.path(), &subscription.id);
        set_keyword(&path, REFRESH_KEYWORD, Some("alle-zwei-wochen")).unwrap();

        assert_eq!(read(&path).unwrap().refresh, crate::model::RefreshInterval::default());
    }
}
