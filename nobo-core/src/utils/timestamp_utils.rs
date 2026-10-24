// SPDX-FileCopyrightText: 2026 Dennis Burgermeister
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::core::models::{OrgTimestamp, Repeater, RepeaterKind};
use chrono::{Datelike, NaiveDate};

const DAY_NAMES: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

/// Parse INNER (without brackets). Brackets are chosen by ACTIVE state.
/// INNER e.g. "2026-08-05 Wed 14:00-15:30 +1w"
fn parse_single(inner: &str, active: bool) -> Option<OrgTimestamp> {
    let mut tokens = inner.split_whitespace().peekable();

    let start_date = tokens.next()?.to_string();
    if !is_iso_date(&start_date) {
        return None;
    }

    // Skip weekday if present, default in emacs
    if let Some(t) = tokens.peek() {
        if DAY_NAMES.iter().any(|d| t.starts_with(d)) || is_localized_day(t) {
            tokens.next();
        }
    }

    let mut start_time = None;
    let mut end_time = None;
    let mut repeater = None;
    let mut extras = Vec::new();

    for token in tokens {
        if let Some(rep) = parse_repeater(token) {
            repeater = Some(rep);
        } else if let Some((s, e)) = token.split_once('-').filter(|(s, e)| is_clock(s) && is_clock(e)) {
            start_time = Some(s.to_string());
            end_time = Some(e.to_string());
        } else if is_clock(token) {
            start_time = Some(token.to_string());
        } else {
            // A warning period (`-5d`), a delay, a diary sexp -- anything
            // org allows here that this build does not model. Kept, and
            // written back after the repeater, instead of falling off the
            // end of this loop as it used to.
            extras.push(token.to_string());
        }
    }

    Some(OrgTimestamp { start_date, start_time, end_date: None, end_time, active, repeater, extras, raw: inner.trim().to_string() })
}

fn is_iso_date(s: &str) -> bool {
    s.len() == 10
        && s.as_bytes()[4] == b'-'
        && s.as_bytes()[7] == b'-'
        && s.chars().enumerate().all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit())
}

fn is_clock(s: &str) -> bool {
    match s.split_once(':') {
        Some((h, m)) => {
            !h.is_empty() && h.len() <= 2 && m.len() == 2 && h.chars().all(|c| c.is_ascii_digit()) && m.chars().all(|c| c.is_ascii_digit())
        }
        None => false,
    }
}

/// Non-englisch weekdays (e.g. Mo, Di in german)
fn is_localized_day(s: &str) -> bool {
    !s.is_empty() && !s.chars().next().unwrap().is_ascii_digit() && !s.starts_with('+') && !s.starts_with('-') && !s.starts_with('.')
}

fn parse_repeater(token: &str) -> Option<Repeater> {
    let (kind, rest) = if let Some(r) = token.strip_prefix("++") {
        (RepeaterKind::CatchUp, r)
    } else if let Some(r) = token.strip_prefix(".+") {
        (RepeaterKind::Restart, r)
    } else if let Some(r) = token.strip_prefix('+') {
        (RepeaterKind::Cumulative, r)
    } else {
        return None;
    };

    // `.+1d/3d` -- a habit. The part before the slash is the repeater
    // proper; the rest is org's maximum interval, kept verbatim so the
    // whole token round-trips (see Repeater::habit_max).
    let (rest, habit_max) = match rest.split_once('/') {
        Some((head, tail)) if !tail.is_empty() => (head, Some(format!("/{tail}"))),
        _ => (rest, None),
    };

    let unit = rest.chars().last()?;
    if !"hdwmy".contains(unit) {
        return None;
    }
    let value: u32 = rest[..rest.len() - unit.len_utf8()].parse().ok()?;

    Some(Repeater { kind, value, unit, habit_max })
}

/// Parses RAW timestamps, e.g. `<2026-08-05 Wed>--<2026-08-07 Fri>`
pub fn parse_timestamp(raw: &str) -> Option<OrgTimestamp> {
    let s = raw.trim();

    if let Some(idx) = range_separator(s) {
        let (left, right) = (&s[..idx], &s[idx + 2..]);
        let mut start = parse_bracketed(left)?;
        let end = parse_bracketed(right)?;
        start.end_date = Some(end.start_date);
        if start.end_time.is_none() {
            start.end_time = end.start_time;
        }
        start.raw = s.to_string();
        return Some(start);
    }

    parse_bracketed(s)
}

/// Where `--` joins two bracketed timestamps into a range, rather than
/// being part of one.
///
/// `<a>--<b>` is a range; `<2026-08-05 Wed +1w --2d>` is a single
/// timestamp with a repeater-scoped delay in it. Splitting unconditionally
/// on `--` broke the second into two halves that neither parse, so the
/// whole DEADLINE was dropped on the next write.
fn range_separator(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    s.match_indices("--")
        .find(|(i, _)| {
            let before = bytes[..*i].iter().rev().find(|b| !b.is_ascii_whitespace());
            let after = bytes[*i + 2..].iter().find(|b| !b.is_ascii_whitespace());
            matches!(before, Some(b'>') | Some(b']')) && matches!(after, Some(b'<') | Some(b'['))
        })
        .map(|(i, _)| i)
}

fn parse_bracketed(s: &str) -> Option<OrgTimestamp> {
    let s = s.trim();
    if let Some(inner) = s.strip_prefix('<').and_then(|r| r.strip_suffix('>')) {
        parse_single(inner, true)
    } else if let Some(inner) = s.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
        parse_single(inner, false)
    } else {
        parse_single(s, true)
    }
}

fn day_name_for(date: &str) -> Option<&'static str> {
    let parsed = NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?;
    Some(DAY_NAMES[parsed.weekday().num_days_from_monday() as usize])
}

/// Formats TS back to string, inclusive DAY_NAME and brackets.
pub fn format_timestamp(ts: &OrgTimestamp) -> String {
    if ts.is_unparsed() {
        return format!("<{}>", ts.raw);
    }

    let (open, close) = if ts.active { ('<', '>') } else { ('[', ']') };

    let mut inner = ts.start_date.clone();
    if let Some(d) = day_name_for(&ts.start_date) {
        inner.push(' ');
        inner.push_str(d);
    }

    match (&ts.start_time, &ts.end_time, &ts.end_date) {
        (Some(st), Some(et), None) => {
            inner.push_str(&format!(" {}-{}", st, et));
        }
        (Some(st), _, _) => {
            inner.push(' ');
            inner.push_str(st);
        }
        _ => {}
    }

    if let Some(rep) = &ts.repeater {
        let prefix = match rep.kind {
            RepeaterKind::Cumulative => "+",
            RepeaterKind::CatchUp => "++",
            RepeaterKind::Restart => ".+",
        };
        inner.push_str(&format!(" {}{}{}", prefix, rep.value, rep.unit));
        if let Some(habit_max) = &rep.habit_max {
            inner.push_str(habit_max);
        }
    }

    // After the repeater, in the order org itself uses.
    for extra in &ts.extras {
        inner.push(' ');
        inner.push_str(extra);
    }

    if let Some(end_date) = &ts.end_date {
        let mut end_inner = end_date.clone();
        if let Some(d) = day_name_for(end_date) {
            end_inner.push(' ');
            end_inner.push_str(d);
        }
        if let Some(et) = &ts.end_time {
            end_inner.push(' ');
            end_inner.push_str(et);
        }
        return format!("{}{}{}--{}{}{}", open, inner, close, open, end_inner, close);
    }

    format!("{}{}{}", open, inner, close)
}
#[cfg(test)]
mod tests {
    use super::*;

    /// Everything org allows in a timestamp has to survive a parse and a
    /// write, whether or not this build knows what it means.
    ///
    /// Each of these used to come back shorter: the token loop recognised
    /// only repeaters and clocks, and `format_timestamp` could only emit
    /// date, weekday, time and repeater. The heading did not have to be
    /// edited for it -- a clock-in, an undo, a bulk action or an ancestor
    /// touch rewrites the block too.
    #[test]
    fn every_shape_org_allows_round_trips_unchanged() {
        for original in [
            "<2026-08-05 Wed>",
            "<2026-08-05 Wed 14:00>",
            "<2026-08-05 Wed 14:00-15:30>",
            "<2026-08-05 Wed +1w>",
            // A warning period.
            "<2026-08-05 Wed -5d>",
            // A repeater plus a delay.
            "<2026-08-05 Wed +1w -2d>",
            // A habit: repeat one day after completion, at most three
            // days after the scheduled date.
            "<2026-08-05 Wed .+1d/3d>",
            "[2026-08-05 Wed]",
            "<2026-08-05 Wed>--<2026-08-07 Fri>",
        ] {
            let parsed = parse_timestamp(original).unwrap_or_else(|| panic!("did not parse: {original}"));
            assert_eq!(format_timestamp(&parsed), original, "round trip changed it");
        }
    }

    /// The habit's repeater has to stay a repeater, or `repeat_on_finish`
    /// stops firing and ticking the task off archives it instead of moving
    /// it on -- the routine stops existing.
    #[test]
    fn a_habit_keeps_a_working_repeater() {
        let parsed = parse_timestamp("<2026-08-05 Wed .+1d/3d>").unwrap();
        let repeater = parsed.repeater.expect("a habit still repeats");

        assert_eq!(repeater.kind, crate::core::models::RepeaterKind::Restart);
        assert_eq!(repeater.value, 1);
        assert_eq!(repeater.unit, 'd');
        assert_eq!(repeater.habit_max.as_deref(), Some("/3d"));
    }

    /// `--` inside one timestamp is a delay, not a range separator. The
    /// unconditional split left two halves that neither parsed, so the
    /// whole DEADLINE was dropped on the next write.
    #[test]
    fn a_delay_is_not_mistaken_for_a_range() {
        let parsed = parse_timestamp("<2026-08-05 Wed +1w --2d>").unwrap();

        assert_eq!(parsed.start_date, "2026-08-05");
        assert!(parsed.end_date.is_none(), "not a range");
        assert!(parsed.repeater.is_some(), "the repeater still works");
        assert_eq!(format_timestamp(&parsed), "<2026-08-05 Wed +1w --2d>");
    }

    #[test]
    fn a_real_range_still_splits() {
        let parsed = parse_timestamp("<2026-08-05 Wed>--<2026-08-07 Fri>").unwrap();

        assert_eq!(parsed.start_date, "2026-08-05");
        assert_eq!(parsed.end_date.as_deref(), Some("2026-08-07"));
    }
}
