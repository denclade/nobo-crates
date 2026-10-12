//! The org-syntax rules that every generator in the app has to obey.
//!
//! Both mirrors write org text -- a calendar's entries (nobo_calendar) and
//! a CalDAV list's tasks (nobo_caldav) -- and both take that text from a
//! server, i.e. from strings nobody here controls. The rules for making
//! such a string safe are org's, not the mirror's, and they were written
//! out twice, identically. That is the kind of duplication that only shows
//! up when one copy is extended (a new block keyword, a new escape) and
//! the other is not: one mirror would then be correct and the other would
//! quietly produce a file org reads as something else.

use chrono::{Datelike, NaiveDate, NaiveDateTime, NaiveTime, Timelike};

/// Org's own weekday abbreviations, as they appear inside a timestamp.
/// English regardless of the app's language: they are part of the
/// timestamp SYNTAX, and org itself reads no other spelling.
const DAY_NAMES: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

pub fn day_name(date: impl Datelike) -> &'static str {
    DAY_NAMES[date.weekday().num_days_from_monday() as usize]
}

/// The inside of a timestamp: `2026-08-27 Thu`, plus the clock when there
/// is one.
///
/// The order, the padding and the weekday spelling are org's, not the
/// caller's -- and both mirrors compose one of these for every entry they
/// write. Written out twice, they were the same string built two ways;
/// here they are the same string.
pub fn timestamp_body(day: NaiveDate, clock: Option<NaiveTime>) -> String {
    let date = format!("{:04}-{:02}-{:02} {}", day.year(), day.month(), day.day(), day_name(day));
    match clock {
        Some(time) => format!("{date} {:02}:{:02}", time.hour(), time.minute()),
        None => date,
    }
}

/// A whole active timestamp -- what appears on a heading line.
///
/// `suffix` is anything that goes INSIDE the brackets after the time: a
/// repeater (` +1w`), a clock range (`-11:00`). Empty for most entries.
pub fn active_timestamp(day: NaiveDate, clock: Option<NaiveTime>, suffix: &str) -> String {
    format!("<{}{suffix}>", timestamp_body(day, clock))
}

/// The same for a moment, which is what a mirror usually has in hand.
pub fn active_timestamp_at(moment: NaiveDateTime, suffix: &str) -> String {
    active_timestamp(moment.date(), Some(moment.time()), suffix)
}

/// A repeater as it is written inside a timestamp: ` +1w`, ` ++2d`,
/// ` .+1m` -- with the leading space, because it always follows something.
///
/// Which prefix means which kind is org's rule, not the caller's, and
/// both mirrors write one for a recurring entry (a CalDAV task's RRULE, an
/// ICS event's). Empty string for no repeater, which is what most entries
/// have.
pub fn repeater_suffix(repeater: Option<&crate::core::models::Repeater>) -> String {
    let Some(repeater) = repeater else { return String::new() };
    let prefix = match repeater.kind {
        crate::core::models::RepeaterKind::Cumulative => "+",
        crate::core::models::RepeaterKind::CatchUp => "++",
        crate::core::models::RepeaterKind::Restart => ".+",
    };
    format!(" {prefix}{}{}", repeater.value, repeater.unit)
}

/// One line of body text, made safe to sit under a heading.
///
/// Two starts have structural meaning at the beginning of a line and would
/// end the body if they arrived from a server: `#+` starts a keyword or a
/// block, `*` starts a heading. Org's own escape for the first is a
/// leading comma (which every org reader strips again); the second only
/// needs to stop being the first character.
pub fn escape_body_line(line: &str) -> String {
    let trimmed_start = line.trim_start();
    if trimmed_start.starts_with("#+") {
        format!(",{line}")
    } else if trimmed_start.starts_with('*') {
        format!(" {line}")
    } else {
        line.to_string()
    }
}

/// An active timestamp for right now, with or without the time of day --
/// what org-tempo's `<date` and `<datetime` expand to.
///
/// Here rather than in the host: the format is org's, and a second front
/// end writing `<2026-09-22 Mon>` with an English day name into a file the
/// first one reads would be a bug nobody would look for.
pub fn timestamp_now(with_time: bool) -> String {
    let now = chrono::Local::now().naive_local();
    active_timestamp(now.date(), with_time.then(|| now.time()), "")
}

/// The inverse of `escape_body_line`.
///
/// Without it the escaping was one-way, and the mirrors read their own
/// output back as if the user had written it: `escape_body_line` prefixes
/// a `*`-line with a space, `task_from_heading` read that space back into
/// the description, and the next render escaped it again. The hash the
/// mirror compares against is taken from the SERVER's copy, so the growing
/// description read as a local edit and was pushed -- every sync, one
/// space at a time, unasked.
///
/// Deliberately not "strip one leading space from anything": only a line
/// that `escape_body_line` would have produced is unescaped, so genuinely
/// indented prose keeps its indentation.
pub fn unescape_body_line(line: &str) -> String {
    if let Some(rest) = line.strip_prefix(',') {
        if rest.trim_start().starts_with("#+") {
            return rest.to_string();
        }
    }
    if let Some(rest) = line.strip_prefix(' ') {
        if rest.trim_start().starts_with('*') {
            return rest.to_string();
        }
    }
    line.to_string()
}

/// A heading's own line: everything on it has to stay on it, and nothing
/// on it may look like structure.
pub fn sanitize_title(title: &str) -> String {
    title.replace(['\n', '\r'], " ").trim().to_string()
}

/// TEXT on one line, its runs of whitespace one space: a title, a
/// property value -- anything a line break would end.
pub fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

// --- property drawers -------------------------------------------------------

pub use crate::core::models::{ORG_PROPERTIES_END, ORG_PROPERTIES_START};

/// One `:KEY: value` line as org writes it (`org-property-format`,
/// `%-10s %s`): the key padded so short values line up, the value on one
/// line.
pub fn property_line(key: &str, value: &str) -> String {
    format!("{:<10} {}", format!(":{key}:"), one_line(value)).trim_end().to_string()
}

/// A whole drawer for ENTRIES, in their order.
pub fn drawer_lines(entries: &[(&str, &str)]) -> Vec<String> {
    let mut lines = vec![ORG_PROPERTIES_START.to_string()];
    lines.extend(entries.iter().map(|(key, value)| property_line(key, value)));
    lines.push(ORG_PROPERTIES_END.to_string());
    lines
}

/// Whether LINE is a planning line (SCHEDULED, DEADLINE, CLOSED) -- what
/// org allows between a heading and its drawer.
pub fn is_planning_line(line: &str) -> bool {
    let line = line.trim_start();
    line.starts_with("SCHEDULED:") || line.starts_with("DEADLINE:") || line.starts_with("CLOSED:")
}

/// One property of a drawer, with the line (0-based) it is on.
#[derive(Debug, Clone, PartialEq)]
pub struct Property {
    pub key: String,
    pub value: String,
    pub line: usize,
}

/// A heading's `:PROPERTIES:` drawer, as line indexes (0-based).
#[derive(Debug, Clone, PartialEq)]
pub struct PropertyDrawer {
    pub start: usize,
    /// The `:END:` line.
    pub end: usize,
    pub properties: Vec<Property>,
    /// Every line between start and end was a property. A drawer with
    /// anything else in it can be read, but rewriting it whole would lose
    /// that.
    pub clean: bool,
}

impl PropertyDrawer {
    /// KEY's property -- org compares keys without regard to case.
    pub fn get(&self, key: &str) -> Option<&Property> {
        self.properties.iter().find(|property| property.key.eq_ignore_ascii_case(key))
    }
}

/// The drawer of the heading on line HEADING (0-based) of LINES, if it has
/// one: right under it, or under its planning line. Indented drawers
/// (`org-adapt-indentation`) and any case of the markers are read too. A
/// drawer runs to its `:END:`; one that reaches the next heading first
/// is none.
pub fn property_drawer<S: AsRef<str>>(lines: &[S], heading: usize) -> Option<PropertyDrawer> {
    let mut start = heading + 1;
    while start < lines.len() && is_planning_line(lines[start].as_ref()) {
        start += 1;
    }
    if start >= lines.len() || !lines[start].as_ref().trim().eq_ignore_ascii_case(ORG_PROPERTIES_START) {
        return None;
    }
    let mut properties = Vec::new();
    let mut clean = true;
    for (line, text) in lines.iter().enumerate().skip(start + 1) {
        let text = text.as_ref();
        let trimmed = text.trim();
        if trimmed.eq_ignore_ascii_case(ORG_PROPERTIES_END) {
            return Some(PropertyDrawer { start, end: line, properties, clean });
        }
        if crate::core::org_parser::parse_heading_line(text).is_some() {
            return None;
        }
        match crate::core::org_parser::parse_property(trimmed) {
            Some((key, value)) => properties.push(Property { key, value, line }),
            None => clean = false,
        }
    }
    None
}

#[cfg(test)]
mod unescape_tests {
    use super::*;

    /// Whatever `escape_body_line` does has to be undoable, or the mirror
    /// reads its own output back as a user edit and pushes it -- growing
    /// by one space on every sync.
    #[test]
    fn escaping_round_trips() {
        for line in ["* Kapitel", "  ** Unterpunkt", "#+TITLE: x", "  #+begin_src", "gewöhnlicher Text", "  eingerückt", ""] {
            assert_eq!(unescape_body_line(&escape_body_line(line)), line, "{line:?}");
        }
    }

    /// Indentation the user wrote is not escaping.
    #[test]
    fn ordinary_indentation_is_left_alone() {
        assert_eq!(unescape_body_line("  eingerückter Text"), "  eingerückter Text");
        assert_eq!(unescape_body_line(",kein Keyword"), ",kein Keyword");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    #[test]
    fn a_body_line_cannot_end_the_body() {
        assert_eq!(escape_body_line("#+BEGIN_SRC"), ",#+BEGIN_SRC");
        assert_eq!(escape_body_line("* Nicht wirklich eine Überschrift"), " * Nicht wirklich eine Überschrift");
        assert_eq!(escape_body_line("  * eingerückt"), "   * eingerückt");
        assert_eq!(escape_body_line("ganz normal"), "ganz normal");
    }

    #[test]
    fn a_title_stays_on_its_line() {
        assert_eq!(sanitize_title("Zwei\nZeilen"), "Zwei Zeilen");
        assert_eq!(sanitize_title("  Rand  "), "Rand");
    }

    #[test]
    fn a_repeater_is_org_spelled() {
        use crate::core::models::{Repeater, RepeaterKind};

        let every_week = Repeater { kind: RepeaterKind::Cumulative, value: 1, unit: 'w', habit_max: None };
        assert_eq!(repeater_suffix(Some(&every_week)), " +1w");
        assert_eq!(repeater_suffix(Some(&Repeater { kind: RepeaterKind::CatchUp, ..every_week.clone() })), " ++1w");
        assert_eq!(repeater_suffix(Some(&Repeater { kind: RepeaterKind::Restart, ..every_week.clone() })), " .+1w");
        assert_eq!(repeater_suffix(None), "");
    }

    #[test]
    fn a_timestamp_is_org_spelled() {
        let day = NaiveDate::from_ymd_opt(2026, 8, 27).unwrap();

        assert_eq!(active_timestamp(day, None, ""), "<2026-08-27 Thu>");
        assert_eq!(active_timestamp(day, day.and_hms_opt(9, 5, 0).map(|dt| dt.time()), ""), "<2026-08-27 Thu 09:05>");
        assert_eq!(active_timestamp(day, None, " +1w"), "<2026-08-27 Thu +1w>");
    }

    #[test]
    fn weekdays_are_orgs_own_spelling() {
        assert_eq!(day_name(NaiveDate::from_ymd_opt(2026, 8, 24).unwrap()), "Mon");
        assert_eq!(day_name(NaiveDate::from_ymd_opt(2026, 8, 30).unwrap()), "Sun");
    }
}

#[cfg(test)]
mod drawer_tests {
    use super::*;

    #[test]
    fn a_drawer_is_found_under_its_heading_and_planning() {
        let lines = ["* H", "SCHEDULED: <2026-10-12 Mon>", "  :properties:", "  :URL:  https://x", ":Created: [2026-10-12 Mon]", "  :END:", "Text"];
        let drawer = property_drawer(&lines, 0).expect("a drawer");
        assert_eq!((drawer.start, drawer.end), (2, 5));
        assert_eq!(drawer.get("url").map(|p| p.value.as_str()), Some("https://x"));
        assert_eq!(drawer.get("CREATED").map(|p| p.line), Some(4));
        assert!(drawer.clean);
    }

    #[test]
    fn a_drawer_never_reaches_past_the_next_heading() {
        assert!(property_drawer(&["* A", ":PROPERTIES:", ":X: 1", "* B", ":END:"], 0).is_none());
        assert!(property_drawer(&["* A", "Text", ":PROPERTIES:", ":END:"], 0).is_none());
    }

    #[test]
    fn a_drawer_with_other_lines_is_not_clean() {
        let drawer = property_drawer(&["* A", ":PROPERTIES:", "kein Property", ":X: 1", ":END:"], 0).unwrap();
        assert!(!drawer.clean);
        assert_eq!(drawer.properties.len(), 1);
    }

    #[test]
    fn properties_are_written_as_org_writes_them() {
        assert_eq!(property_line("URL", " https://x \n y"), ":URL:      https://x y");
        assert_eq!(property_line("source-url", "u"), ":source-url: u");
        assert_eq!(drawer_lines(&[("A", "1")]), [":PROPERTIES:", ":A:        1", ":END:"]);
    }
}
