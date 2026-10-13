//! Org-mode timestamp repeaters: what happens when a recurring task is
//! marked finished.
//!
//! A repeating task is never closed. It stays a single heading whose
//! SCHEDULED/DEADLINE moves forward instead -- so the file keeps one entry
//! per real-world routine rather than accumulating one DONE heading per
//! occurrence, and the LOGBOOK carries the completion history.
//!
//! Parsing and serializing the `+1w`/`++1w`/`.+1w` suffix itself lives in
//! timestamp_utils; this module only decides where the next occurrence
//! lands and rewrites the edit accordingly.

use chrono::{Days, Duration, Local, Months, NaiveDate, NaiveDateTime, NaiveTime};

use crate::core::models::{
    property_value, set_property, state_log_line, Heading, HeadingUpdatePayload, OrgTimestamp, Repeater, RepeaterKind, DATE_FORMAT_CLOSED,
    DATE_FORMAT_LOG, TODO_STATES,
};
use crate::core::task_utils;
use crate::utils::timestamp_utils;

const LAST_REPEAT_PROPERTY: &str = "LAST_REPEAT";
/// Per-heading override for the state a repeat lands back on, same
/// property name Emacs org uses.
const REPEAT_TO_STATE_PROPERTY: &str = "REPEAT_TO_STATE";

/// The rewritten edit for a repeating task that was just marked finished.
///
/// Carries a `heading` too, not only the payload: its state is already the
/// one the repeat lands on, so the block builder sees no state transition
/// and doesn't log a second, misleading `"TODO" from "NEXT"` note next to
/// the completion note this puts in `payload.history`.
pub struct RepeatedEdit {
    pub heading: Heading,
    pub payload: HeadingUpdatePayload,
}

/// Rewrites a "mark it finished" edit into the next occurrence of a
/// repeating task, or `None` when nothing about this edit repeats -- no
/// finishing state in `payload`, an already-finished heading (re-closing
/// it must not skip an interval), or no repeater on either timestamp.
///
/// `payload`'s own SCHEDULED/DEADLINE win over the heading's, so finishing
/// a task in the same save that reschedules it advances from the new date.
pub fn repeat_on_finish(heading: &Heading, payload: &HeadingUpdatePayload) -> Option<RepeatedEdit> {
    let finishing_state = payload.state.as_deref()?;
    if !task_utils::is_finished(Some(finishing_state)) {
        return None;
    }
    if task_utils::is_finished(heading.state.as_deref()) {
        return None;
    }

    let now = Local::now().naive_local();
    let scheduled = payload.scheduled.clone().unwrap_or_else(|| heading.scheduled.clone());
    let deadline = payload.deadline.clone().unwrap_or_else(|| heading.deadline.clone());

    let next_scheduled = scheduled.as_ref().and_then(|ts| next_occurrence(ts, now));
    let next_deadline = deadline.as_ref().and_then(|ts| next_occurrence(ts, now));
    if next_scheduled.is_none() && next_deadline.is_none() {
        return None;
    }

    let mut repeated = payload.clone();
    let repeat_to = repeat_to_state(heading);
    repeated.state = Some(repeat_to.clone());
    if next_scheduled.is_some() {
        repeated.scheduled = Some(next_scheduled);
    }
    if next_deadline.is_some() {
        repeated.deadline = Some(next_deadline);
    }

    let mut properties = payload.properties.clone().unwrap_or_else(|| heading.properties.clone());
    set_property(&mut properties, LAST_REPEAT_PROPERTY, format!("[{}]", now.format(DATE_FORMAT_CLOSED)));
    repeated.properties = Some(properties);

    // Without this the completion leaves no trace at all: the block ends on
    // the same state it started from, so the builder's own state note never
    // fires, and there is no CLOSED line on a task that never closes.
    let mut history = payload.history.clone().unwrap_or_else(|| heading.history.clone());
    history.insert(0, state_log_line(finishing_state, heading.state.as_deref().unwrap_or(""), &now.format(DATE_FORMAT_LOG).to_string()));
    repeated.history = Some(history);

    let mut advanced = heading.clone();
    advanced.state = Some(repeat_to);
    Some(RepeatedEdit { heading: advanced, payload: repeated })
}

/// Where TS lands after a completion at `completed_at`, or `None` if it
/// carries no (usable) repeater.
///
/// The three org repeater kinds differ only in where they count from:
/// - `+` one interval from the scheduled date, even if that is still in
///   the past -- a missed dose stays due.
/// - `++` keeps adding intervals until the result is in the future -- three
///   forgotten weeks collapse into one upcoming occurrence, not three.
/// - `.+` one interval from today, keeping the time of day -- the clock
///   restarts when you actually did it.
pub fn next_occurrence(ts: &OrgTimestamp, completed_at: NaiveDateTime) -> Option<OrgTimestamp> {
    let repeater = ts.repeater.as_ref()?;
    // A zero interval has no next occurrence, and would spin forever in the
    // catch-up loop below.
    if repeater.value == 0 {
        return None;
    }

    let start = NaiveDate::parse_from_str(&ts.start_date, "%Y-%m-%d").ok()?;
    let time_of_day = ts.start_time.as_deref().and_then(|t| NaiveTime::parse_from_str(t, "%H:%M").ok()).unwrap_or(NaiveTime::MIN);

    let base = match repeater.kind {
        RepeaterKind::Restart => completed_at.date().and_time(time_of_day),
        _ => start.and_time(time_of_day),
    };

    let mut next = shift(base, repeater)?;
    if repeater.kind == RepeaterKind::CatchUp {
        while next <= completed_at {
            next = shift(next, repeater)?;
        }
    }

    let mut advanced = ts.clone();
    advanced.start_date = next.format("%Y-%m-%d").to_string();
    // A multi-day range keeps its length rather than collapsing onto the
    // new start date.
    if let Some(end_date) = &ts.end_date {
        let shifted_end = NaiveDate::parse_from_str(end_date, "%Y-%m-%d").ok().and_then(|d| d.checked_add_signed(next.date() - start));
        advanced.end_date = shifted_end.map(|d| d.format("%Y-%m-%d").to_string());
    }
    if ts.start_time.is_some() {
        advanced.start_time = Some(next.format("%H:%M").to_string());
    }
    // `raw` is what the parser read; regenerating from the fields is the
    // whole point here, so it must not be handed back stale.
    advanced.raw = String::new();
    advanced.raw = timestamp_utils::format_timestamp(&advanced);
    Some(advanced)
}

fn shift(from: NaiveDateTime, repeater: &Repeater) -> Option<NaiveDateTime> {
    let value = repeater.value;
    match repeater.unit {
        'h' => from.checked_add_signed(Duration::hours(i64::from(value))),
        'd' => from.checked_add_days(Days::new(u64::from(value))),
        'w' => from.checked_add_days(Days::new(u64::from(value) * 7)),
        'm' => from.checked_add_months(Months::new(value)),
        'y' => from.checked_add_months(Months::new(value.checked_mul(12)?)),
        _ => None,
    }
}

/// `:REPEAT_TO_STATE:` if the heading names one, otherwise the first
/// keyword of the workflow -- org's own rule, and the one that keeps a
/// repeating task out of whatever mid-flight state it was finished from.
fn repeat_to_state(heading: &Heading) -> String {
    property_value(&heading.properties, REPEAT_TO_STATE_PROPERTY)
        .map(|v| v.trim().to_string())
        .filter(|v| TODO_STATES.contains(&v.as_str()))
        .unwrap_or_else(|| TODO_STATES[0].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn at(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap()
    }

    fn ts(raw: &str) -> OrgTimestamp {
        timestamp_utils::parse_timestamp(raw).unwrap()
    }

    fn heading(state: Option<&str>, scheduled: Option<OrgTimestamp>) -> Heading {
        Heading {
            id: "h1".to_string(),
            file_path: "projects.org".to_string(),
            line_no: 1,
            last_line: 1,
            level: 1,
            state: state.map(String::from),
            title: "Pflanzen gießen".to_string(),
            scheduled,
            ..Default::default()
        }
    }

    fn finish(state: &str) -> HeadingUpdatePayload {
        HeadingUpdatePayload { state: Some(state.to_string()), ..Default::default() }
    }

    /* --- next_occurrence: where each kind counts from --- */

    #[test]
    fn a_plus_repeater_moves_exactly_one_interval_from_the_scheduled_date() {
        // Three weeks late, and it lands two weeks in the PAST: taking a
        // pill late does not mean the next one is due a week from now.
        let next = next_occurrence(&ts("<2026-08-01 Sat +1w>"), at("2026-08-22 10:00")).unwrap();

        assert_eq!(next.start_date, "2026-08-08");
    }

    #[test]
    fn a_catch_up_repeater_skips_the_missed_intervals_and_lands_in_the_future() {
        // Same three weeks late -- but you don't clean the windows three
        // times to make up for it.
        let next = next_occurrence(&ts("<2026-08-01 Sat ++1w>"), at("2026-08-22 10:00")).unwrap();

        assert_eq!(next.start_date, "2026-08-29");
    }

    #[test]
    fn a_catch_up_repeater_still_advances_when_the_date_is_already_the_completion_day() {
        // Not "already in the future, leave it" -- doing it today has to
        // move it off today, or it stays due forever.
        let next = next_occurrence(&ts("<2026-08-22 Sat ++1w>"), at("2026-08-22 10:00")).unwrap();

        assert_eq!(next.start_date, "2026-08-29");
    }

    #[test]
    fn a_restart_repeater_counts_from_the_day_it_was_actually_done() {
        // Haircut two weeks late: the next one is four weeks from today,
        // not four weeks from the date you missed.
        let next = next_occurrence(&ts("<2026-08-01 Sat .+4w>"), at("2026-08-15 10:00")).unwrap();

        assert_eq!(next.start_date, "2026-09-12");
    }

    #[test]
    fn a_restart_repeater_keeps_the_time_of_day_rather_than_the_completion_time() {
        let next = next_occurrence(&ts("<2026-08-01 Sat 07:30 .+1d>"), at("2026-08-15 22:41")).unwrap();

        assert_eq!(next.start_date, "2026-08-16");
        assert_eq!(next.start_time.as_deref(), Some("07:30"));
    }

    #[test]
    fn the_repeater_and_the_weekday_name_survive_into_the_new_timestamp() {
        let next = next_occurrence(&ts("<2026-08-01 Sat +1w>"), at("2026-08-01 10:00")).unwrap();

        assert_eq!(timestamp_utils::format_timestamp(&next), "<2026-08-08 Sat +1w>");
    }

    #[test]
    fn month_and_year_units_land_on_the_same_day_of_month() {
        assert_eq!(next_occurrence(&ts("<2026-01-31 Sat +1m>"), at("2026-01-31 10:00")).unwrap().start_date, "2026-02-28");
        assert_eq!(next_occurrence(&ts("<2026-08-01 Sat +1y>"), at("2026-08-01 10:00")).unwrap().start_date, "2027-08-01");
    }

    #[test]
    fn a_range_keeps_its_length_instead_of_collapsing_onto_the_new_start() {
        let next = next_occurrence(&ts("<2026-08-01 Sat +1w>--<2026-08-03 Mon>"), at("2026-08-01 10:00")).unwrap();

        assert_eq!(next.start_date, "2026-08-08");
        assert_eq!(next.end_date.as_deref(), Some("2026-08-10"));
    }

    #[test]
    fn a_timestamp_without_a_repeater_has_no_next_occurrence() {
        assert!(next_occurrence(&ts("<2026-08-01 Sat>"), at("2026-08-01 10:00")).is_none());
    }

    #[test]
    fn a_zero_interval_is_refused_rather_than_looping_forever() {
        // "++0d" would otherwise never reach the future in the catch-up
        // loop -- a hung app, not a wrong date.
        assert!(next_occurrence(&ts("<2026-08-01 Sat ++0d>"), at("2026-08-22 10:00")).is_none());
    }

    /* --- repeat_on_finish: what the edit turns into --- */

    #[test]
    fn finishing_a_repeating_task_reopens_it_instead_of_closing_it() {
        let h = heading(Some("NEXT"), Some(ts("<2026-08-01 Sat +1w>")));

        let edit = repeat_on_finish(&h, &finish("DONE")).unwrap();

        assert_eq!(edit.payload.state.as_deref(), Some("TODO"));
        assert_eq!(edit.payload.scheduled.as_ref().unwrap().as_ref().unwrap().start_date, "2026-08-08");
        // The block builder must see no transition, or it logs a second,
        // misleading "TODO from NEXT" note next to the completion note.
        assert_eq!(edit.heading.state, edit.payload.state);
    }

    #[test]
    fn the_completion_is_recorded_even_though_the_task_never_closes() {
        let h = heading(Some("TODO"), Some(ts("<2026-08-01 Sat +1w>")));

        let edit = repeat_on_finish(&h, &finish("DONE")).unwrap();

        let logged = edit.payload.history.unwrap();
        assert!(logged[0].starts_with("- State \"DONE"), "{logged:?}");
        assert!(logged[0].contains("from \"TODO"), "{logged:?}");
        assert!(edit.payload.properties.unwrap().contains_key("LAST_REPEAT"));
    }

    #[test]
    fn a_repeat_to_state_property_overrides_the_default() {
        let mut h = heading(Some("IN-PROGRESS"), Some(ts("<2026-08-01 Sat +1w>")));
        h.properties.insert("REPEAT_TO_STATE".to_string(), "NEXT".to_string());

        let edit = repeat_on_finish(&h, &finish("DONE")).unwrap();

        assert_eq!(edit.payload.state.as_deref(), Some("NEXT"));
    }

    #[test]
    fn a_repeat_to_state_naming_no_real_keyword_falls_back_instead_of_writing_it() {
        let mut h = heading(Some("TODO"), Some(ts("<2026-08-01 Sat +1w>")));
        h.properties.insert("REPEAT_TO_STATE".to_string(), "ERLEDIGT-BALD".to_string());

        let edit = repeat_on_finish(&h, &finish("DONE")).unwrap();

        assert_eq!(edit.payload.state.as_deref(), Some("TODO"));
    }

    #[test]
    fn a_deadline_repeater_advances_too() {
        let mut h = heading(Some("TODO"), None);
        h.deadline = Some(ts("<2026-08-01 Sat +1m>"));

        let edit = repeat_on_finish(&h, &finish("DONE")).unwrap();

        assert_eq!(edit.payload.deadline.as_ref().unwrap().as_ref().unwrap().start_date, "2026-09-01");
    }

    #[test]
    fn a_plain_task_is_left_entirely_alone() {
        let h = heading(Some("TODO"), Some(ts("<2026-08-01 Sat>")));

        assert!(repeat_on_finish(&h, &finish("DONE")).is_none());
    }

    #[test]
    fn re_finishing_an_already_finished_task_does_not_skip_another_interval() {
        // Two DONE clicks in a row, or a bulk action over a selection that
        // already contains it -- either would otherwise advance twice.
        let h = heading(Some("DONE"), Some(ts("<2026-08-01 Sat +1w>")));

        assert!(repeat_on_finish(&h, &finish("DONE")).is_none());
    }

    #[test]
    fn reopening_a_repeating_task_is_not_a_completion() {
        let h = heading(Some("DONE"), Some(ts("<2026-08-01 Sat +1w>")));

        assert!(repeat_on_finish(&h, &finish("TODO")).is_none());
    }

    #[test]
    fn every_finishing_state_repeats_it_not_just_done() {
        for state in ["DONE", "OBE", "WONT-DO"] {
            let h = heading(Some("TODO"), Some(ts("<2026-08-01 Sat +1w>")));
            assert!(repeat_on_finish(&h, &finish(state)).is_some(), "{state}");
        }
    }

    #[test]
    fn a_reschedule_saved_together_with_the_completion_advances_from_the_new_date() {
        let h = heading(Some("TODO"), Some(ts("<2026-08-01 Sat +1w>")));
        let payload = HeadingUpdatePayload {
            state: Some("DONE".to_string()),
            scheduled: Some(Some(ts("<2026-09-01 Tue +1w>"))),
            ..Default::default()
        };

        let edit = repeat_on_finish(&h, &payload).unwrap();

        assert_eq!(edit.payload.scheduled.as_ref().unwrap().as_ref().unwrap().start_date, "2026-09-08");
    }

    #[test]
    fn an_edit_that_does_not_touch_the_state_is_not_a_completion() {
        let h = heading(Some("TODO"), Some(ts("<2026-08-01 Sat +1w>")));
        let payload = HeadingUpdatePayload { title: Some("Umbenannt".to_string()), ..Default::default() };

        assert!(repeat_on_finish(&h, &payload).is_none());
    }

    #[test]
    fn existing_properties_and_log_entries_are_kept_alongside_the_new_ones() {
        let mut h = heading(Some("TODO"), Some(ts("<2026-08-01 Sat +1w>")));
        h.properties = HashMap::from([("CUSTOM_ID".to_string(), "h:abc".to_string())]);
        h.history = vec!["- State \"TODO\"        from \"\"            [2026-07-01 Wed 09:00]".to_string()];

        let edit = repeat_on_finish(&h, &finish("DONE")).unwrap();

        assert_eq!(edit.payload.properties.as_ref().unwrap().get("CUSTOM_ID").unwrap(), "h:abc");
        assert_eq!(edit.payload.history.as_ref().unwrap().len(), 2);
    }
}
