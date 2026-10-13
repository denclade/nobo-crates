//! CalendarEvent -> org text.
//!
//! The shape, and why each part of it:
//!
//! ```org
//! * Team-Meeting Backend
//! :PROPERTIES:
//! :CUSTOM_ID: h:...
//! :ICS_UID: abc123@example.com
//! :END:
//! <2026-08-21 Fri 14:00-15:00>
//! Zoom-Link ...
//! ```
//!
//! - **No todo keyword.** A meeting is attended, not completed. Without a
//!   keyword it never reaches a single task list in the app, which is the
//!   whole point -- no view needs to learn what a calendar is.
//! - **Active timestamp** (`<>`, not `[]`), on its own line rather than
//!   behind `SCHEDULED:`. That is what org means by "show this in the
//!   agenda on that day"; `SCHEDULED:` would additionally claim the user
//!   plans to work on it.
//! - **`:ICS_UID:`** so a refresh can tell a moved event from a new one.
//! - **`:CUSTOM_ID:`, and the drawer BEFORE the timestamp.** Not
//!   decoration: org_parser absorbs a keyword-less, tag-less `*` line into
//!   the previous heading's body unless a properties drawer with a
//!   CUSTOM_ID follows it directly (see has_upcoming_custom_id). Without
//!   both, every event after the first would vanish into the first one.
//!   The id is derived from the ICS UID rather than random, so a refresh
//!   keeps the same identity for the same event -- which is also what the
//!   sync merge needs (see core::merge).

use chrono::{NaiveDateTime, Timelike};

use crate::CalendarEvent;
use nobo_core::core::heading_identity;
use nobo_core::core::org_syntax;

pub const ICS_UID_PROPERTY: &str = "ICS_UID";

/// The entries alone, under a header its own subscription kind supplies --
/// a CalDAV calendar's header names a collection and a user instead of a
/// feed URL (see caldav::registry::header), but the entries below it are
/// the same org either way, and there must not be two renderers for them.
pub fn render_events(events: &[CalendarEvent]) -> String {
    events.iter().map(render_event).collect()
}

fn render_event(event: &CalendarEvent) -> String {
    let mut lines = vec![format!("* {}", org_syntax::sanitize_title(&event.summary))];
    let custom_id = custom_id_for(&event.uid);
    lines.extend(org_syntax::drawer_lines(&[(heading_identity::CUSTOM_ID_PROPERTY, &custom_id), (ICS_UID_PROPERTY, &event.uid)]));
    lines.push(format_range(event));

    if let Some(location) = &event.location {
        lines.push(format!("Ort: {}", location.replace('\n', " ")));
    }
    if let Some(description) = &event.description {
        // A description can be anything, including lines starting with a
        // `*` -- which would silently become a new heading and swallow
        // every following event into the wrong entry.
        lines.extend(description.lines().map(org_syntax::escape_body_line));
    }
    lines.push(String::new());
    lines.join("\n") + "\n"
}

/// `<2026-08-21 Fri 14:00-15:00>`, `<2026-08-21 Fri 14:00>` without an
/// end, `<2026-08-21 Fri>` for a whole day, plus the repeater when the
/// RRULE had an org equivalent.
///
/// A range spanning midnight cannot be one timestamp with a clock range,
/// so it becomes org's `<start>--<end>` instead.
fn format_range(event: &CalendarEvent) -> String {
    let repeat = org_syntax::repeater_suffix(event.repeater.as_ref());

    if event.all_day {
        // DTEND is EXCLUSIVE for whole-day events -- a holiday running
        // through Saturday is published as ending on Sunday. Org's range
        // end is inclusive, so the last real day is DTEND minus one; when
        // that lands back on the start, it was a single day all along.
        let last_day = event.end.map(|e| e.date()).and_then(|d| d.pred_opt()).filter(|d| *d > event.start.date());

        return match last_day {
            Some(end) => {
                format!("<{}{repeat}>--<{}>", format_day(event.start), format_day(end.and_hms_opt(0, 0, 0).unwrap_or(event.start)))
            }
            None => format!("<{}{repeat}>", format_day(event.start)),
        };
    }

    match event.end {
        Some(end) if end.date() == event.start.date() && end > event.start => {
            format!("<{} {}-{}{repeat}>", format_day(event.start), format_clock(event.start), format_clock(end))
        }
        Some(end) if end > event.start => {
            format!("<{} {}{repeat}>--<{} {}>", format_day(event.start), format_clock(event.start), format_day(end), format_clock(end))
        }
        _ => format!("<{} {}{repeat}>", format_day(event.start), format_clock(event.start)),
    }
}

/// The date half of a timestamp, org's own spelling (see
/// core::org_syntax, which the CalDAV mirror writes through as well).
fn format_day(dt: NaiveDateTime) -> String {
    org_syntax::timestamp_body(dt.date(), None)
}

/// The clock half on its own -- this side needs it separately for a
/// range within one day (`09:00-11:00`), which is one timestamp, not two.
fn format_clock(dt: NaiveDateTime) -> String {
    format!("{:02}:{:02}", dt.hour(), dt.minute())
}

/// Stable across refreshes: the same ICS event keeps the same CUSTOM_ID,
/// so history, sync and any stored reference survive a re-import. A v5
/// (name-based) uuid rather than a hash of our own, so the value is a real
/// uuid like every other CUSTOM_ID in the vault.
fn custom_id_for(uid: &str) -> String {
    format!("h:{}", uuid::Uuid::new_v5(&ICS_UID_NAMESPACE, uid.as_bytes()))
}

/// Fixed namespace for `custom_id_for` -- any constant uuid works, it only
/// has to never change, or every event in every vault gets a new identity.
const ICS_UID_NAMESPACE: uuid::Uuid = uuid::uuid!("6f9b1e2c-4a37-4c8e-9d61-5b0d4f2a7c13");

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CalendarEvent;
    use chrono::NaiveDate;
    use nobo_core::core::models::{Repeater, RepeaterKind};
    use nobo_core::core::vault;
    use std::path::Path;

    fn at(date: (i32, u32, u32), time: (u32, u32)) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(date.0, date.1, date.2).unwrap().and_hms_opt(time.0, time.1, 0).unwrap()
    }

    fn event(start: NaiveDateTime, end: Option<NaiveDateTime>) -> CalendarEvent {
        CalendarEvent {
            uid: "abc123@example.com".to_string(),
            summary: "Team-Meeting Backend".to_string(),
            start,
            end,
            all_day: false,
            description: None,
            location: None,
            repeater: None,
        }
    }

    fn timestamp_line(rendered: &str) -> String {
        rendered.lines().find(|l| l.starts_with('<')).unwrap().to_string()
    }

    /// The shape the whole feature rests on: no todo keyword, an ACTIVE
    /// timestamp, and the ICS UID kept for the next refresh.
    #[test]
    fn an_event_renders_as_a_keywordless_heading_with_an_active_timestamp() {
        let rendered = render_events(&[event(at((2026, 8, 21), (14, 0)), Some(at((2026, 8, 21), (15, 0))))]);

        assert!(rendered.contains("* Team-Meeting Backend\n"), "{rendered}");
        assert!(rendered.contains("<2026-08-21 Fri 14:00-15:00>"), "{rendered}");
        assert!(rendered.contains(":ICS_UID:  abc123@example.com"), "aligned as org writes it: {rendered}");
    }

    /// A todo keyword would put every meeting into Next Actions. The
    /// parser deciding there is no state is the actual guarantee, not the
    /// absence of the string.
    #[test]
    fn the_parser_reads_a_rendered_event_as_having_no_todo_state() {
        let rendered = render_events(&[event(at((2026, 8, 21), (14, 0)), None)]);

        let headings = vault::parse_headings(Path::new("calendar.org"), &rendered);

        assert_eq!(headings.len(), 1, "{headings:?}");
        assert_eq!(headings[0].state, None);
        assert_eq!(headings[0].title, "Team-Meeting Backend");
        assert_eq!(headings[0].properties.get("ICS_UID").unwrap(), "abc123@example.com");
    }

    /// The failure this file's drawer-before-timestamp layout exists for:
    /// org_parser folds a keyword-less `*` line into the PREVIOUS
    /// heading's body unless a CUSTOM_ID drawer follows it directly. Get
    /// that wrong and a calendar collapses into a single entry.
    #[test]
    fn every_event_parses_as_its_own_heading_not_as_body_of_the_first() {
        let events: Vec<CalendarEvent> = (0..4)
            .map(|i| {
                let mut e = event(at((2026, 8, 21 + i as u32), (14, 0)), None);
                e.uid = format!("uid-{i}");
                e.summary = format!("Termin {i}");
                e
            })
            .collect();

        let rendered = render_events(&events);
        let headings = vault::parse_headings(Path::new("calendar.org"), &rendered);

        assert_eq!(headings.len(), 4, "{rendered}");
        assert_eq!(headings.iter().map(|h| h.title.as_str()).collect::<Vec<_>>(), ["Termin 0", "Termin 1", "Termin 2", "Termin 3"]);
    }

    /// Same event, later refresh: the identity has to survive, or every
    /// refresh looks like a full delete-and-recreate to sync and history.
    #[test]
    fn the_custom_id_is_derived_from_the_ics_uid_and_stays_stable() {
        let first = render_events(&[event(at((2026, 8, 21), (14, 0)), None)]);
        let moved = render_events(&[event(at((2026, 9, 30), (9, 0)), None)]);

        let id_of = |s: &str| s.lines().find(|l| l.starts_with(":CUSTOM_ID:")).unwrap().to_string();
        assert_eq!(id_of(&first), id_of(&moved));

        let mut other = event(at((2026, 8, 21), (14, 0)), None);
        other.uid = "different@example.com".to_string();
        assert_ne!(id_of(&first), id_of(&render_events(&[other])));
    }

    #[test]
    fn a_whole_day_event_gets_a_date_only_timestamp() {
        let mut e = event(at((2026, 8, 21), (0, 0)), Some(at((2026, 8, 22), (0, 0))));
        e.all_day = true;

        // DTEND is exclusive: a single all-day event ends the NEXT day and
        // must not become a two-day range.
        assert_eq!(timestamp_line(&render_events(&[e])), "<2026-08-21 Fri>");
    }

    /// Real numbers from a published school-holiday feed: Osterferien
    /// Baden-Württemberg 2026 run 30.03. through 11.04., and the feed
    /// publishes DTEND as the 12th because DTEND is exclusive. Rendering
    /// the 12th would hand the user an extra day of holiday.
    #[test]
    fn a_multi_day_all_day_event_ends_on_its_last_real_day() {
        let mut e = event(at((2026, 3, 30), (0, 0)), Some(at((2026, 4, 12), (0, 0))));
        e.all_day = true;

        assert_eq!(timestamp_line(&render_events(&[e])), "<2026-03-30 Mon>--<2026-04-11 Sat>");
    }

    #[test]
    fn a_two_day_all_day_event_is_still_a_range_not_collapsed() {
        let mut e = event(at((2026, 8, 21), (0, 0)), Some(at((2026, 8, 23), (0, 0))));
        e.all_day = true;

        assert_eq!(timestamp_line(&render_events(&[e])), "<2026-08-21 Fri>--<2026-08-22 Sat>");
    }

    /// A clock range cannot span midnight, so org's two-timestamp form is
    /// the only correct rendering.
    #[test]
    fn an_event_running_past_midnight_becomes_a_two_timestamp_range() {
        let rendered = render_events(&[event(at((2026, 8, 21), (22, 0)), Some(at((2026, 8, 22), (2, 0))))]);

        assert_eq!(timestamp_line(&rendered), "<2026-08-21 Fri 22:00>--<2026-08-22 Sat 02:00>");
    }

    #[test]
    fn an_event_without_an_end_gets_a_single_time() {
        let rendered = render_events(&[event(at((2026, 8, 21), (14, 0)), None)]);

        assert_eq!(timestamp_line(&rendered), "<2026-08-21 Fri 14:00>");
    }

    #[test]
    fn a_repeating_event_carries_the_org_repeater_inside_the_timestamp() {
        let mut e = event(at((2026, 8, 21), (14, 0)), Some(at((2026, 8, 21), (15, 0))));
        e.repeater = Some(Repeater { kind: RepeaterKind::Cumulative, value: 1, unit: 'w', habit_max: None });

        assert_eq!(timestamp_line(&render_events(&[e])), "<2026-08-21 Fri 14:00-15:00 +1w>");
    }

    /// A description line starting with `*` would otherwise become a
    /// heading and swallow every following event into itself.
    #[test]
    fn a_description_cannot_introduce_a_heading_of_its_own() {
        let mut first = event(at((2026, 8, 21), (14, 0)), None);
        first.description = Some("* Agenda\n#+BEGIN_SRC\nnormal".to_string());
        let mut second = event(at((2026, 8, 22), (9, 0)), None);
        second.uid = "second".to_string();
        second.summary = "Zweiter Termin".to_string();

        let rendered = render_events(&[first, second]);
        let headings = vault::parse_headings(Path::new("calendar.org"), &rendered);

        assert_eq!(headings.len(), 2, "the description became structure: {rendered}");
        assert_eq!(headings[1].title, "Zweiter Termin");
    }

    /// A newline in a SUMMARY would end the heading line early and push
    /// the rest into the body.
    #[test]
    fn a_multiline_summary_stays_on_the_heading_line() {
        let mut e = event(at((2026, 8, 21), (14, 0)), None);
        e.summary = "Termin\nmit Umbruch".to_string();

        let rendered = render_events(&[e]);

        assert!(rendered.contains("* Termin mit Umbruch\n"), "{rendered}");
    }

    #[test]
    fn location_and_description_land_in_the_body() {
        let mut e = event(at((2026, 8, 21), (14, 0)), None);
        e.location = Some("Raum 3".to_string());
        e.description = Some("Zoom-Link: https://example.com".to_string());

        let rendered = render_events(&[e]);

        assert!(rendered.contains("Ort: Raum 3"), "{rendered}");
        assert!(rendered.contains("Zoom-Link: https://example.com"), "{rendered}");
    }
}
