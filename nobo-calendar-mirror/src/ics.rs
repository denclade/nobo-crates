//! ICS (RFC 5545) VEVENT -> CalendarEvent.
//!
//! The format's own chores are `nobo_ical`'s -- which property is which,
//! what a `TZID` means, how a value is escaped -- and shared with the
//! CalDAV side, which reads VTODOs out of the same format. What is here
//! is what an EVENT means to this app: which properties matter, how long
//! it lasts, and which recurrence rules have an org equivalent.

use chrono::NaiveDateTime;
use ical::parser::ical::component::IcalEvent;
use nobo_ical::When;

use crate::CalendarEvent;
use nobo_core::core::models::{Repeater, RepeaterKind};

/// Every VEVENT in `ics` that carries the minimum to be shown at all: a
/// UID and a start. Anything else is skipped rather than failing the whole
/// calendar -- one malformed entry must not cost the user the other
/// several hundred.
pub fn parse_calendar(ics: &str) -> Result<Vec<CalendarEvent>, String> {
    let reader = ical::IcalParser::new(ics.as_bytes());
    let mut events = Vec::new();

    for calendar in reader {
        let calendar = calendar.map_err(|e| format!("not a readable iCalendar feed: {e}"))?;
        for event in &calendar.events {
            if let Some(parsed) = event_from_vevent(event) {
                events.push(parsed);
            }
        }
    }
    Ok(events)
}

fn event_from_vevent(event: &IcalEvent) -> Option<CalendarEvent> {
    let uid = text(event, "UID")?;
    let when = nobo_ical::when(find(event, "DTSTART")?)?;
    let (start, all_day) = (when.naive(), when.is_all_day());

    let end = find(event, "DTEND")
        .and_then(nobo_ical::when)
        .map(When::naive)
        .or_else(|| duration_end(event, start));

    Some(CalendarEvent {
        uid,
        // A VEVENT without a SUMMARY is legal and does happen; a titleless
        // heading would be unreadable, so it gets a stand-in.
        summary: text(event, "SUMMARY").unwrap_or_else(|| "(ohne Titel)".to_string()),
        start,
        end,
        all_day,
        description: text(event, "DESCRIPTION").filter(|d| !d.trim().is_empty()),
        location: text(event, "LOCATION").filter(|l| !l.trim().is_empty()),
        // An `EXDATE` cancels individual occurrences and a
        // `RECURRENCE-ID` overrides one; org's `+1w` can express neither.
        // Writing the repeater anyway claims a weekly meeting on the two
        // weeks it was called off -- the same reason `repeater_from_rrule`
        // refuses BYDAY and UNTIL, just carried by the event rather than
        // by the rule string. Such an event is written as its single next
        // occurrence, which is at least true.
        repeater: (find(event, "EXDATE").is_none() && find(event, "RECURRENCE-ID").is_none())
            .then(|| find(event, "RRULE").and_then(|p| repeater_from_rrule(p.value.as_deref()?)))
            .flatten(),
    })
}

/// The two lookups this module does, on the properties of a VEVENT.
fn find<'a>(event: &'a IcalEvent, name: &str) -> Option<&'a ical::property::Property> {
    nobo_ical::property(&event.properties, name)
}

fn text(event: &IcalEvent, name: &str) -> Option<String> {
    nobo_ical::text(&event.properties, name)
}

/// DTEND is optional when a DURATION is given instead. Only the plain
/// `PT#H#M` / `P#D` shapes are honored; anything richer (weeks plus days
/// plus seconds) is left to DTEND-less rendering rather than guessed at.
fn duration_end(event: &IcalEvent, start: NaiveDateTime) -> Option<NaiveDateTime> {
    let raw = find(event, "DURATION")?.value.as_ref()?;
    let body = raw.strip_prefix('P')?;
    let (days_part, time_part) = match body.split_once('T') {
        Some((d, t)) => (d, Some(t)),
        None => (body, None),
    };

    let mut minutes: i64 = 0;
    if let Some(days) = days_part.strip_suffix('D') {
        minutes += days.parse::<i64>().ok()? * 24 * 60;
    } else if !days_part.is_empty() {
        return None;
    }
    if let Some(time) = time_part {
        let mut number = String::new();
        for c in time.chars() {
            match c {
                '0'..='9' => number.push(c),
                'H' => minutes += number.parse::<i64>().ok()? * 60,
                'M' => minutes += number.parse::<i64>().ok()?,
                'S' => {}
                _ => return None,
            }
            if !c.is_ascii_digit() {
                number.clear();
            }
        }
    }
    start.checked_add_signed(chrono::Duration::minutes(minutes))
}

/// The org repeater an `RRULE` corresponds to, or `None` when it has no
/// equivalent.
///
/// Only plain `FREQ` + `INTERVAL` translates: org's `+1w` says "every week
/// from this date", with no vocabulary for "every second Tuesday" (BYDAY),
/// for an end date (UNTIL/COUNT), or for anything else the rule may carry.
/// Writing `+1w` for those would state something the calendar does not
/// say, so such an event is written as its single next occurrence instead.
pub fn repeater_from_rrule(rrule: &str) -> Option<Repeater> {
    let mut freq = None;
    let mut interval = 1u32;

    for part in rrule.split(';') {
        let (key, value) = part.split_once('=')?;
        match key.to_ascii_uppercase().as_str() {
            "FREQ" => freq = Some(value.to_ascii_uppercase()),
            "INTERVAL" => interval = value.parse().ok()?,
            // Anything that narrows or bounds the recurrence has no org
            // counterpart -- see this function's own doc comment.
            "BYDAY" | "BYMONTHDAY" | "BYMONTH" | "BYSETPOS" | "BYHOUR" | "BYMINUTE"
            | "UNTIL" | "COUNT" | "BYWEEKNO" | "BYYEARDAY" => return None,
            _ => {}
        }
    }

    let unit = match freq?.as_str() {
        "HOURLY" => 'h',
        "DAILY" => 'd',
        "WEEKLY" => 'w',
        "MONTHLY" => 'm',
        "YEARLY" => 'y',
        _ => return None,
    };
    if interval == 0 {
        return None;
    }
    // An RRULE has no habit half; that shape only exists in org.
    Some(Repeater { kind: RepeaterKind::Cumulative, value: interval, unit, habit_max: None })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Local, TimeZone, Utc};
    use chrono_tz::Tz;

    fn wrap(vevent: &str) -> String {
        format!("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//EN\r\n{vevent}\r\nEND:VCALENDAR\r\n")
    }

    fn one(vevent: &str) -> CalendarEvent {
        let events = parse_calendar(&wrap(vevent)).unwrap();
        assert_eq!(events.len(), 1, "{events:?}");
        events.into_iter().next().unwrap()
    }

    const FLOATING: &str = "BEGIN:VEVENT\r\nUID:a@example.com\r\nSUMMARY:Team-Meeting Backend\r\nDTSTART:20260821T140000\r\nDTEND:20260821T150000\r\nEND:VEVENT";

    #[test]
    fn a_floating_time_is_taken_as_local_wall_clock() {
        let event = one(FLOATING);

        assert_eq!(event.uid, "a@example.com");
        assert_eq!(event.summary, "Team-Meeting Backend");
        assert_eq!(event.start.to_string(), "2026-08-21 14:00:00");
        assert_eq!(event.end.unwrap().to_string(), "2026-08-21 15:00:00");
        assert!(!event.all_day);
    }

    /// The one that silently ruins a calendar: a UTC stamp shown at the
    /// UTC hour instead of the local one.
    #[test]
    fn a_utc_stamp_is_converted_to_local_time() {
        let event = one("BEGIN:VEVENT\r\nUID:b\r\nSUMMARY:Zahnarzt\r\nDTSTART:20260821T120000Z\r\nEND:VEVENT");

        let expected = Utc
            .with_ymd_and_hms(2026, 8, 21, 12, 0, 0)
            .unwrap()
            .with_timezone(&Local)
            .naive_local();
        assert_eq!(event.start, expected);
    }

    #[test]
    fn a_tzid_stamp_is_converted_from_that_zone() {
        let event = one("BEGIN:VEVENT\r\nUID:c\r\nSUMMARY:Call\r\nDTSTART;TZID=America/New_York:20260821T090000\r\nEND:VEVENT");

        let expected = "America/New_York"
            .parse::<Tz>()
            .unwrap()
            .with_ymd_and_hms(2026, 8, 21, 9, 0, 0)
            .unwrap()
            .with_timezone(&Local)
            .naive_local();
        assert_eq!(event.start, expected);
    }

    #[test]
    fn an_unknown_zone_is_treated_as_local_rather_than_shifted_by_a_guess() {
        let event = one("BEGIN:VEVENT\r\nUID:d\r\nSUMMARY:X\r\nDTSTART;TZID=Mars/Olympus:20260821T090000\r\nEND:VEVENT");

        assert_eq!(event.start.to_string(), "2026-08-21 09:00:00");
    }

    #[test]
    fn a_whole_day_event_carries_no_time() {
        let event = one("BEGIN:VEVENT\r\nUID:e\r\nSUMMARY:Urlaub\r\nDTSTART;VALUE=DATE:20260821\r\nDTEND;VALUE=DATE:20260822\r\nEND:VEVENT");

        assert!(event.all_day);
        assert_eq!(event.start.to_string(), "2026-08-21 00:00:00");
    }

    #[test]
    fn a_duration_stands_in_for_a_missing_end() {
        let event = one("BEGIN:VEVENT\r\nUID:f\r\nSUMMARY:X\r\nDTSTART:20260821T140000\r\nDURATION:PT1H30M\r\nEND:VEVENT");

        assert_eq!(event.end.unwrap().to_string(), "2026-08-21 15:30:00");
    }

    #[test]
    fn escaped_newlines_and_commas_come_back_as_real_characters() {
        let event = one("BEGIN:VEVENT\r\nUID:g\r\nSUMMARY:X\r\nDTSTART:20260821T140000\r\nDESCRIPTION:Zoom: https://x\\ny\\, dann z\r\nEND:VEVENT");

        assert_eq!(event.description.unwrap(), "Zoom: https://x\ny, dann z");
    }

    /// Folded lines are the format's most common surprise -- an unfolded
    /// read would cut a long URL in half. Exactly ONE leading whitespace
    /// is the fold marker; a second one is real content.
    #[test]
    fn a_folded_long_line_is_rejoined() {
        let event = one("BEGIN:VEVENT\r\nUID:h\r\nSUMMARY:X\r\nDTSTART:20260821T140000\r\nDESCRIPTION:https://example.com/a-very-long\r\n -meeting-url\r\nEND:VEVENT");

        assert_eq!(event.description.unwrap(), "https://example.com/a-very-long-meeting-url");
    }

    #[test]
    fn an_event_without_a_summary_still_gets_a_readable_title() {
        let event = one("BEGIN:VEVENT\r\nUID:i\r\nDTSTART:20260821T140000\r\nEND:VEVENT");

        assert_eq!(event.summary, "(ohne Titel)");
    }

    /// One broken entry must not cost the user the rest of the feed.
    #[test]
    fn an_event_without_uid_or_start_is_skipped_not_fatal() {
        let ics = wrap(&format!(
            "BEGIN:VEVENT\r\nSUMMARY:Kein UID\r\nDTSTART:20260821T140000\r\nEND:VEVENT\r\n\
             BEGIN:VEVENT\r\nUID:no-start\r\nSUMMARY:Kein Start\r\nEND:VEVENT\r\n{FLOATING}"
        ));

        let events = parse_calendar(&ics).unwrap();

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].uid, "a@example.com");
    }

    /* --- RRULE --- */

    #[test]
    fn plain_frequencies_become_org_repeaters() {
        assert_eq!(repeater_from_rrule("FREQ=WEEKLY").unwrap(), Repeater { kind: RepeaterKind::Cumulative, value: 1, unit: 'w', habit_max: None });
        assert_eq!(repeater_from_rrule("FREQ=DAILY;INTERVAL=2").unwrap(), Repeater { kind: RepeaterKind::Cumulative, value: 2, unit: 'd', habit_max: None });
        assert_eq!(repeater_from_rrule("FREQ=MONTHLY").unwrap().unit, 'm');
        assert_eq!(repeater_from_rrule("FREQ=YEARLY").unwrap().unit, 'y');
    }

    /// Org has no vocabulary for these, and writing "+1w" anyway would
    /// state something the calendar does not say.
    #[test]
    fn rules_org_cannot_express_are_refused_rather_than_approximated() {
        assert!(repeater_from_rrule("FREQ=MONTHLY;BYDAY=2TU").is_none(), "every second Tuesday");
        assert!(repeater_from_rrule("FREQ=WEEKLY;COUNT=5").is_none(), "bounded by a count");
        assert!(repeater_from_rrule("FREQ=WEEKLY;UNTIL=20261231T000000Z").is_none(), "bounded by a date");
        assert!(repeater_from_rrule("FREQ=SECONDLY").is_none(), "no org unit for it");
        assert!(repeater_from_rrule("FREQ=WEEKLY;INTERVAL=0").is_none(), "never comes round");
    }
}
