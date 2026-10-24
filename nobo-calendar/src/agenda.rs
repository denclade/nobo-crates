// SPDX-FileCopyrightText: 2026 Dennis Burgermeister
// SPDX-License-Identifier: GPL-3.0-or-later

//! Subscribed calendars in the agenda.
//!
//! The mirror files are ordinary org in the vault, so their entries could
//! be found by whatever reads the vault -- but the agenda is asked for one
//! day at a time and has to say which calendar an entry came from and in
//! which colour. That is what a contributor is for (see
//! `nobo_core::core::agenda::AgendaContributor`): the core never names a
//! feature, and a build without this crate simply has no calendars in its
//! agenda.

use nobo_core::core::agenda::{AgendaContributor, AgendaEntry, Source};
use nobo_core::core::file_kind;
use nobo_core::core::vault::Vault;

use crate::model::COLOR_KEYWORD;

inventory::submit! {
    AgendaContributor {
        source: "calendar",
        entries: entries_for,
        days: days_for,
    }
}

/// Every mirrored event that falls on DAY.
///
/// Reads through the vault, never from disk and never over the network:
/// this runs while somebody is paging through days.
fn entries_for(vault: &Vault, day: &str) -> Vec<AgendaEntry> {
    vault
        .files_of_kind(crate::MIRROR_KIND)
        .into_iter()
        .flat_map(|(path, text)| {
            let (calendar, color) = label_of(&text, &path);
            vault
                .headings(&path)
                .into_iter()
                .filter(|heading| covers(heading, day))
                .map(|heading| {
                    let when = heading.occurrence.clone();
                    AgendaEntry {
                        timed: when.as_ref().is_some_and(|stamp| stamp.start_time.is_some()),
                        heading,
                        source: Source::Calendar,
                        project: None,
                        when,
                        calendar: Some(calendar.clone()),
                        color: color.clone(),
                    }
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

/// The days between FROM and TO a mirrored event starts on.
///
/// Its start day only, the same rule the core's own dots follow: a dot
/// says "something begins here", and a week-long holiday would otherwise
/// paint the whole week.
fn days_for(vault: &Vault, from: &str, to: &str) -> Vec<String> {
    vault
        .files_of_kind(crate::MIRROR_KIND)
        .into_iter()
        .flat_map(|(path, _)| vault.headings(&path))
        .filter_map(|heading| heading.occurrence)
        .filter(|stamp| stamp.start_date.as_str() >= from && stamp.start_date.as_str() <= to)
        .map(|stamp| stamp.start_date)
        .collect()
}

/// Whether an entry falls on DAY. A range covers every day between its
/// ends; both sides are ISO, so the comparison is the date comparison.
fn covers(heading: &nobo_core::core::models::Heading, day: &str) -> bool {
    let Some(stamp) = heading.occurrence.as_ref() else { return false };
    match stamp.end_date.as_deref() {
        Some(end) => stamp.start_date.as_str() <= day && day <= end,
        None => stamp.start_date == day,
    }
}

/// How a calendar presents itself: the name and colour its mirror states.
/// Both kinds of mirror write the same two keywords (see `org`, and
/// caldav's registry).
fn label_of(text: &str, path: &std::path::Path) -> (String, Option<String>) {
    let lines: Vec<&str> = text.lines().collect();
    (
        file_kind::leading_keyword(&lines, "#+TITLE:")
            .filter(|title| !title.is_empty())
            .unwrap_or_else(|| nobo_core::utils::fs_utils::filename_of(path)),
        file_kind::leading_keyword(&lines, COLOR_KEYWORD).filter(|color| !color.is_empty()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use nobo_core::core::indexer::IndexManager;

    fn vault_with(text: &str) -> (tempfile::TempDir, IndexManager) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".nobo/calendars")).unwrap();
        std::fs::write(dir.path().join(".nobo/calendars/a.org"), text).unwrap();
        let index = IndexManager::new();
        index.scan_directory(dir.path());
        (dir, index)
    }

    // Shaped like the mirrors the sync actually writes (see the samples
    // under testdata/.nobo/calendars): the CUSTOM_ID is what makes a
    // heading without a todo state count as structure at all.
    const MIRROR: &str = "#+TITLE: Familie\n\
#+NOBO_TYPE: calendar-mirror\n\
#+NOBO_CALENDAR_COLOR: #ff8800\n\n\
* Zahnarzt\n\
:PROPERTIES:\n:CUSTOM_ID: h:1\n:ICS_UID: one\n:END:\n\
<2026-09-22 Tue 09:00>\n\n\
* Urlaub\n\
:PROPERTIES:\n:CUSTOM_ID: h:2\n:ICS_UID: two\n:END:\n\
<2026-09-24 Thu>--<2026-09-27 Sun>\n";

    #[test]
    fn a_mirrored_event_lands_on_its_day_with_its_calendar() {
        let (dir, index) = vault_with(MIRROR);
        let vault = Vault::new(&index, dir.path());

        let entries = entries_for(&vault, "2026-09-22");

        assert_eq!(entries.len(), 1, "{entries:?}");
        assert_eq!(entries[0].heading.title, "Zahnarzt");
        assert_eq!(entries[0].calendar.as_deref(), Some("Familie"));
        assert_eq!(entries[0].color.as_deref(), Some("#ff8800"));
        assert!(entries[0].timed, "a 09:00 entry is not a whole-day one");
    }

    #[test]
    fn a_range_covers_every_day_between_its_ends() {
        let (dir, index) = vault_with(MIRROR);
        let vault = Vault::new(&index, dir.path());

        assert_eq!(entries_for(&vault, "2026-09-25").len(), 1);
        assert!(entries_for(&vault, "2026-09-28").is_empty());
        assert!(!entries_for(&vault, "2026-09-25")[0].timed, "a whole-day range carries no time");
    }

    /// What the month view draws its dots from: the days a calendar has
    /// something starting on, over a whole range rather than one day.
    #[test]
    fn the_days_of_a_range_are_the_days_events_start_on() {
        let (dir, index) = vault_with(MIRROR);
        let vault = Vault::new(&index, dir.path());

        assert_eq!(days_for(&vault, "2026-09-01", "2026-09-30"), vec!["2026-09-22", "2026-09-24"]);
        // The range's own days, not every day it covers.
        assert!(!days_for(&vault, "2026-09-01", "2026-09-30").contains(&"2026-09-25".to_string()));
        assert!(days_for(&vault, "2026-10-01", "2026-10-31").is_empty());
    }

    #[test]
    fn a_file_that_is_not_a_mirror_contributes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("notes.org"), "* Zahnarzt\n<2026-09-22 Tue 09:00>\n").unwrap();
        let index = IndexManager::new();
        index.scan_directory(dir.path());

        assert!(entries_for(&Vault::new(&index, dir.path()), "2026-09-22").is_empty());
    }
}
