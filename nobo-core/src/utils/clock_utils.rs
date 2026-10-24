// SPDX-FileCopyrightText: 2026 Dennis Burgermeister
// SPDX-License-Identifier: GPL-3.0-or-later

//! Parsing and formatting for raw `CLOCK:` lines, byte-exact with Emacs
//! `org-clock` output:
//!
//! Closed:
//!   `CLOCK: [2026-08-17 Mon 10:00]--[2026-08-17 Mon 11:30] =>  1:30`
//!   -- inactive timestamps, English weekday abbreviation regardless of
//!   locale, `--` with no spaces, and two spaces after the arrow for
//!   right-alignment. Duration is `H:MM`, hours unpadded.
//! Open:
//!   `CLOCK: [2026-08-17 Mon 18:45]` -- start only. Stopping appends the
//!   rest.
//!
//! Parsing tolerates incidental whitespace differences, since the duration
//! is never read back: it is recomputed from start and end and reformatted
//! on write, so the round trip is byte-exact by construction.

use crate::core::models::DATE_FORMAT_LOG;
use chrono::{NaiveDate, NaiveDateTime, NaiveTime};

pub const CLOCK_PREFIX: &str = "CLOCK:";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockEntry {
    pub start: NaiveDateTime,
    /// `None` for a still-open/running entry.
    pub end: Option<NaiveDateTime>,
}

impl ClockEntry {
    /// Minutes for a CLOSED entry; `None` for an open one (nothing to sum
    /// into a rollup yet -- see task_utils::annotate_clocked_time, which
    /// only ever counts closed entries, matching real org-mode's own
    /// clocktable behavior of ignoring a still-running clock).
    pub fn duration_minutes(&self) -> Option<i64> {
        self.end.map(|e| (e - self.start).num_minutes().max(0))
    }
}

fn parse_bracket_datetime(inner: &str) -> Option<NaiveDateTime> {
    // e.g. "2026-08-17 Mon 13:00" -- date, weekday (decorative, ignored
    // same as timestamp_utils::parse_single does for scheduled/deadline),
    // time.
    let mut parts = inner.split_whitespace();
    let date_str = parts.next()?;
    let _weekday = parts.next()?;
    let time_str = parts.next()?;
    let date = NaiveDate::parse_from_str(date_str, "%Y-%m-%d").ok()?;
    let time = NaiveTime::parse_from_str(time_str, "%H:%M").ok()?;
    Some(NaiveDateTime::new(date, time))
}

/// Parses a raw `CLOCK: [start]` or `CLOCK: [start]--[end] =>  H:MM` line
/// (see this module's own doc comment for the exact format) -- `None` for
/// anything that doesn't match, so a caller can just skip lines that don't
/// parse rather than crash on a hand-edited/exotic one.
pub fn parse_clock_line(line: &str) -> Option<ClockEntry> {
    let rest = line.trim().strip_prefix(CLOCK_PREFIX)?.trim();

    let start_close = rest.find(']')?;
    if !rest.starts_with('[') {
        return None;
    }
    let start = parse_bracket_datetime(&rest[1..start_close])?;

    let after_start = &rest[start_close + 1..];
    let Some(after_sep) = after_start.strip_prefix("--") else {
        return Some(ClockEntry { start, end: None });
    };
    if !after_sep.starts_with('[') {
        return None;
    }
    let end_close = after_sep.find(']')?;
    let end = parse_bracket_datetime(&after_sep[1..end_close])?;

    Some(ClockEntry { start, end: Some(end) })
}

/// Formats a closed (END given) or still-open (END `None`) CLOCK line,
/// byte-exact with what parse_clock_line accepts (see this module's own
/// doc comment for the exact format).
pub fn format_clock_line(start: NaiveDateTime, end: Option<NaiveDateTime>) -> String {
    let start_str = start.format(DATE_FORMAT_LOG);
    match end {
        None => format!("{} {}", CLOCK_PREFIX, start_str),
        Some(e) => {
            let end_str = e.format(DATE_FORMAT_LOG);
            let minutes = (e - start).num_minutes().max(0);
            let (h, m) = (minutes / 60, minutes % 60);
            format!("{} {}--{} =>  {}:{:02}", CLOCK_PREFIX, start_str, end_str, h, m)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dt(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, mo, d).unwrap().and_hms_opt(h, mi, 0).unwrap()
    }

    #[test]
    fn parses_and_reformats_the_users_own_example_byte_exact() {
        // The exact line the user gave as the compatibility spec.
        let line = "CLOCK: [2026-08-17 Mon 13:00]--[2026-08-17 Mon 14:30] =>  1:30";
        let entry = parse_clock_line(line).expect("should parse");
        assert_eq!(entry.start, dt(2026, 8, 17, 13, 0));
        assert_eq!(entry.end, Some(dt(2026, 8, 17, 14, 30)));
        assert_eq!(entry.duration_minutes(), Some(90));
        assert_eq!(format_clock_line(entry.start, entry.end), line, "byte-exact round trip");
    }

    #[test]
    fn parses_and_reformats_a_second_closed_example() {
        let line = "CLOCK: [2026-08-16 Sun 09:00]--[2026-08-16 Sun 09:45] =>  0:45";
        let entry = parse_clock_line(line).expect("should parse");
        assert_eq!(entry.duration_minutes(), Some(45));
        assert_eq!(format_clock_line(entry.start, entry.end), line);
    }

    #[test]
    fn parses_and_reformats_an_open_clock_byte_exact() {
        let line = "CLOCK: [2026-08-17 Mon 18:45]";
        let entry = parse_clock_line(line).expect("should parse");
        assert_eq!(entry.start, dt(2026, 8, 17, 18, 45));
        assert_eq!(entry.end, None);
        assert_eq!(entry.duration_minutes(), None);
        assert_eq!(format_clock_line(entry.start, entry.end), line);
    }

    #[test]
    fn handles_a_clock_spanning_midnight() {
        let line = "CLOCK: [2026-08-17 Mon 23:30]--[2026-08-18 Tue 00:15] =>  0:45";
        let entry = parse_clock_line(line).expect("should parse");
        assert_eq!(entry.duration_minutes(), Some(45));
        assert_eq!(format_clock_line(entry.start, entry.end), line);
    }

    #[test]
    fn formats_hours_over_nine_without_zero_padding_but_pads_minutes() {
        let start = dt(2026, 1, 1, 8, 0);
        let end = dt(2026, 1, 1, 20, 5);
        assert_eq!(format_clock_line(start, Some(end)), "CLOCK: [2026-01-01 Thu 08:00]--[2026-01-01 Thu 20:05] =>  12:05");
    }

    #[test]
    fn rejects_lines_that_dont_start_with_the_clock_prefix() {
        assert!(parse_clock_line("- State \"DONE\"").is_none());
        assert!(parse_clock_line("SCHEDULED: <2026-08-17 Mon>").is_none());
    }

    #[test]
    fn rejects_malformed_clock_lines() {
        assert!(parse_clock_line("CLOCK:").is_none());
        assert!(parse_clock_line("CLOCK: garbage").is_none());
        assert!(parse_clock_line("CLOCK: [not a date]").is_none());
        assert!(parse_clock_line("CLOCK: [2026-08-17 Mon 10:00]--garbage").is_none());
    }

    #[test]
    fn is_tolerant_of_a_single_space_before_the_duration_on_read() {
        // Not byte-exact on ITS OWN write (format_clock_line always emits
        // the canonical double space), but must still parse -- some other
        // tool (or a hand-edit) might not preserve the exact spacing.
        let entry = parse_clock_line("CLOCK: [2026-08-17 Mon 10:00]--[2026-08-17 Mon 11:30] => 1:30");
        assert_eq!(entry.and_then(|e| e.duration_minutes()), Some(90));
    }
}
