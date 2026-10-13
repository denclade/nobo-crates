//! What both halves of iCalendar in this app have to do, whichever
//! component they are reading.
//!
//! An ICS calendar feed is VEVENTs (see nobo_calendar::ics), a CalDAV task
//! list is VTODOs (see nobo_caldav::vtodo), and what they do with them
//! could hardly be more different -- one renders a read-only mirror, the
//! other syncs both ways and must not touch a line it does not own. But
//! underneath, the format is the format: a property is looked up the same
//! way, a `TZID` parameter means the same thing, `\n` in a value is a real
//! line break, and 75 octets is where a line breaks.
//!
//! That layer lived twice, once per feature, and the copies had already
//! drifted: only one of them understood `TZID`, so a task due at 09:00
//! Berlin time was shown at 09:00 wherever the reader was.
//!
//! The `ical` crate is underneath this one and does the reading (line
//! unfolding for the parser, parameter splitting); everything here is
//! about what the values mean.

use chrono::{Local, NaiveDate, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Tz;
use ical::property::Property;

/// The property named NAME, whatever case the server writes it in.
pub fn property<'a>(properties: &'a [Property], name: &str) -> Option<&'a Property> {
    properties.iter().find(|property| property.name.eq_ignore_ascii_case(name))
}

/// Its value, with RFC 5545's escapes resolved.
pub fn text(properties: &[Property], name: &str) -> Option<String> {
    property(properties, name)?.value.as_ref().map(|value| unescape(value))
}

/// A parameter of a property (`TZID`, `VALUE`), case-insensitively.
pub fn param(property: &Property, name: &str) -> Option<String> {
    property.params.as_ref()?.iter().find(|(key, _)| key.eq_ignore_ascii_case(name)).and_then(|(_, values)| values.first().cloned())
}

/// A whole day rather than a moment: `VALUE=DATE`, or a bare `YYYYMMDD`.
pub fn is_date_only(property: &Property) -> bool {
    let Some(value) = property.value.as_deref() else { return false };
    param(property, "VALUE").is_some_and(|value| value.eq_ignore_ascii_case("DATE")) || value.len() == 8
}

/// When a property's value happens, in the reader's own wall clock.
///
/// Four shapes have to land on the same local time, or the entry shows at
/// the wrong hour:
///
/// - `VALUE=DATE` / `YYYYMMDD` -- a whole day, no time at all.
/// - a trailing `Z` -- UTC, converted to this machine's zone.
/// - `TZID=<zone>` -- that zone's wall clock, converted likewise. An
///   unknown zone is treated as already local, which is right far more
///   often than shifting it by a guess.
/// - neither -- RFC 5545 "floating" time, local by definition.
///
/// Org timestamps carry no zone at all, which is why this has to happen on
/// the way in rather than on the way to the screen.
pub fn when(property: &Property) -> Option<When> {
    let value = property.value.as_ref()?;

    if is_date_only(property) {
        return NaiveDate::parse_from_str(value, "%Y%m%d").ok().map(When::Day);
    }

    if let Some(stripped) = value.strip_suffix('Z') {
        let naive = NaiveDateTime::parse_from_str(stripped, "%Y%m%dT%H%M%S").ok()?;
        return Some(When::At(Utc.from_utc_datetime(&naive).with_timezone(&Local).naive_local()));
    }

    let naive = NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%S").ok()?;
    match param(property, "TZID").and_then(|zone| zone.parse::<Tz>().ok()) {
        Some(zone) => {
            let zoned = zone.from_local_datetime(&naive).earliest()?;
            Some(When::At(zoned.with_timezone(&Local).naive_local()))
        }
        None => Some(When::At(naive)),
    }
}

/// A day, or a moment within one -- already in local time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum When {
    Day(NaiveDate),
    At(NaiveDateTime),
}

impl When {
    /// The local wall clock either way; a whole day starts at midnight.
    pub fn naive(self) -> NaiveDateTime {
        match self {
            When::Day(day) => day.and_time(chrono::NaiveTime::MIN),
            When::At(moment) => moment,
        }
    }

    pub fn is_all_day(self) -> bool {
        matches!(self, When::Day(_))
    }
}

/// RFC 5545 text escaping, in reverse. `\n` is a real line break, which is
/// most of what a description is made of.
pub fn unescape(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') | Some('N') => out.push('\n'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

/// The inverse of `unescape`. Order matters: backslashes first, or the
/// escapes introduced below would be escaped again.
pub fn escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace(';', "\\;").replace(',', "\\,").replace('\n', "\\n")
}

/// A comma-separated list value (CATEGORIES), with escaped commas kept
/// inside their item -- so this cannot be a plain `split(',')`.
pub fn split_list(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some('n') | Some('N') => current.push('\n'),
                Some(other) => current.push(other),
                None => current.push('\\'),
            },
            ',' => out.push(std::mem::take(&mut current)),
            _ => current.push(c),
        }
    }
    out.push(current);
    out.into_iter().map(|item| item.trim().to_string()).filter(|item| !item.is_empty()).collect()
}

/// The component's lines, continuations joined back onto the line they
/// belong to.
pub fn unfold(raw: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in raw.split("\r\n").flat_map(|line| line.split('\n')) {
        let line = line.trim_end_matches('\r');
        match line.strip_prefix([' ', '\t']) {
            Some(rest) if !out.is_empty() => out.last_mut().unwrap().push_str(rest),
            _ => out.push(line.to_string()),
        }
    }
    out
}

/// RFC 5545 caps a line at 75 octets, continued with a leading space.
/// Servers do enforce it, and a long DESCRIPTION is exactly the case that
/// hits it.
///
/// The limit here is 73, not 75, and deliberately: a continuation line
/// spends one octet on its leading space, and folding at exactly the
/// maximum leaves nothing for the CRLF some readers count. Two octets of
/// headroom on every line costs nothing and takes the arithmetic out of
/// the question.
pub fn fold(line: String) -> Vec<String> {
    const LIMIT: usize = 73;
    if line.len() <= LIMIT {
        return vec![line];
    }
    let mut out = Vec::new();
    let mut current = String::new();
    for c in line.chars() {
        // Split on character boundaries, never mid-UTF-8: a byte-wise cut
        // produces a component no parser will accept.
        if current.len() + c.len_utf8() > LIMIT {
            out.push(std::mem::take(&mut current));
            current.push(' ');
        }
        current.push(c);
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn property_with(value: &str, params: Vec<(String, Vec<String>)>) -> Property {
        Property { name: "DUE".to_string(), params: Some(params), value: Some(value.to_string()) }
    }

    #[test]
    fn a_bare_date_is_a_whole_day() {
        assert_eq!(when(&property_with("20260827", vec![])), Some(When::Day(NaiveDate::from_ymd_opt(2026, 8, 27).unwrap())));
    }

    #[test]
    fn a_utc_stamp_arrives_in_local_time() {
        let utc = property_with("20260827T090000Z", vec![]);
        let expected = Utc.with_ymd_and_hms(2026, 8, 27, 9, 0, 0).unwrap().with_timezone(&Local).naive_local();

        assert_eq!(when(&utc), Some(When::At(expected)));
    }

    /// The half that only one of the two copies had. A server that sends
    /// `TZID=Europe/Berlin` means 09:00 in Berlin, not 09:00 wherever the
    /// reader happens to be.
    #[test]
    fn a_zoned_stamp_is_converted_rather_than_taken_at_face_value() {
        let zoned = property_with("20260827T090000", vec![("TZID".to_string(), vec!["Europe/Berlin".to_string()])]);
        let expected = chrono_tz::Europe::Berlin.with_ymd_and_hms(2026, 8, 27, 9, 0, 0).unwrap().with_timezone(&Local).naive_local();

        assert_eq!(when(&zoned), Some(When::At(expected)));
    }

    #[test]
    fn an_unknown_zone_is_left_as_local_rather_than_guessed_at() {
        let odd = property_with("20260827T090000", vec![("TZID".to_string(), vec!["Mars/Olympus".to_string()])]);

        assert_eq!(when(&odd), Some(When::At(NaiveDate::from_ymd_opt(2026, 8, 27).unwrap().and_hms_opt(9, 0, 0).unwrap())));
    }

    #[test]
    fn escaping_round_trips() {
        let value = "Milch, Brot; und\nZeilenumbruch \\ dazu";
        assert_eq!(unescape(&escape(value)), value);
    }

    #[test]
    fn a_list_keeps_an_escaped_comma_inside_its_item() {
        assert_eq!(split_list("Einkauf,Haus\\, Garten"), vec!["Einkauf".to_string(), "Haus, Garten".to_string()]);
    }

    /// The limit counts OCTETS, and the split has to land on a character
    /// boundary -- a byte-wise cut in the middle of a multi-byte character
    /// produces a component no parser will accept.
    #[test]
    fn folding_never_splits_a_multibyte_character() {
        let line = format!("DESCRIPTION:{}", "Größenübersicht — 日本語 ".repeat(12));

        let folded = fold(line.clone());

        assert!(folded.len() > 1, "the fixture has to actually fold");
        for part in &folded {
            assert!(part.len() <= 74, "each line stays within the limit: {part:?}");
            // Round-tripping through str is only possible if every part is
            // valid UTF-8, which a mid-character cut would not be.
            assert!(std::str::from_utf8(part.as_bytes()).is_ok());
        }
        assert_eq!(unfold(&folded.join("\r\n")), vec![line], "and it all comes back");
    }

    #[test]
    fn folding_and_unfolding_are_each_others_inverse() {
        let line = format!("DESCRIPTION:{}", "sehr lang ".repeat(30));

        assert_eq!(unfold(&fold(line.clone()).join("\r\n")), vec![line]);
    }
}
