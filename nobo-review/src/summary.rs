//! What the review has to look at, counted.
//!
//! The checklist's steps are not all the same kind of thing: some are a
//! habit ("empty your pockets"), some are a list the vault can count
//! ("nothing left in the inbox"). The ones that can be counted are
//! counted here, in the same place that decides whether the review is
//! overdue -- so the hub badge, the page header and the progress bar are
//! three views of one answer rather than three calculations.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use nobo_core::core::models::Heading;
use nobo_core::core::task_utils;

use crate::state::ReviewState;
use crate::Settings;

/// The steps, in the order the checklist walks them.
///
/// Ids, not labels: what a step is called is the host's business and its
/// reader's language. The order is GTD's own -- collect before you sort,
/// sort before you decide.
pub const STEPS: &[&str] = &["collect", "braindump", "inbox", "actions", "calendar", "waiting", "stalled", "someday"];

/// A task is "waiting for" somebody when it says so: tagged `waiting` (or
/// `warten`), plain or as a context. There is no WAITING state in this
/// vault's org states, so the tag is the convention.
const WAITING_TAGS: [&str; 4] = ["waiting", "warten", "@waiting", "@warten"];

pub fn is_waiting_for(heading: &Heading) -> bool {
    heading.tags.iter().any(|tag| {
        let tag = tag.to_lowercase();
        WAITING_TAGS.contains(&tag.as_str())
    })
}

/// What each countable step still has to look at.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Counts {
    pub inbox: usize,
    pub actions: usize,
    pub waiting: usize,
    pub stalled: usize,
    pub someday: usize,
}

/// The review as a whole: what is left, how far the checklist got, and
/// whether it is due.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Summary {
    #[serde(flatten)]
    pub counts: Counts,
    /// Steps done: ticked by hand, or ticked by the vault because their
    /// list is empty.
    pub steps_done: usize,
    pub total_steps: usize,
    /// Which step ids are done, so a host does not redo the derivation.
    pub done: Vec<String>,
    pub overdue: bool,
    /// Whole days since the last completed review, or `None` for never.
    pub days_since: Option<i64>,
    pub last_completed_at: Option<String>,
    pub interval_days: u32,
}

/// Whether a step is done without anybody ticking it: its list is empty,
/// so there is nothing left to look at.
fn auto_done(step: &str, counts: &Counts) -> bool {
    match step {
        "inbox" => counts.inbox == 0,
        "actions" => counts.actions == 0,
        "waiting" => counts.waiting == 0,
        "stalled" => counts.stalled == 0,
        "someday" => counts.someday == 0,
        // "collect", "braindump" and "calendar" happen away from the
        // vault -- nothing here can know they are done.
        _ => false,
    }
}

/// The whole picture, from what was counted and what was ticked.
///
/// A pure function of its arguments, `now` included: "overdue" is a
/// statement about a moment, and a test that cannot choose the moment can
/// only assert that nothing happens.
pub fn summarize(counts: Counts, state: &ReviewState, settings: &Settings, now: DateTime<Utc>) -> Summary {
    let done: Vec<String> = STEPS
        .iter()
        .filter(|step| auto_done(step, &counts) || state.checked_steps.iter().any(|ticked| ticked == *step))
        .map(|step| step.to_string())
        .collect();

    let days_since = state
        .last_review_completed_at
        .as_deref()
        .and_then(|stamp| DateTime::parse_from_rfc3339(stamp).ok())
        .map(|last| (now - last.with_timezone(&Utc)).num_days());

    Summary {
        steps_done: done.len(),
        total_steps: STEPS.len(),
        done,
        // Never reviewed is overdue: the ritual has not started yet, and
        // saying so is the point of the badge.
        overdue: days_since.map(|days| days >= settings.interval_days as i64).unwrap_or(true),
        days_since,
        last_completed_at: state.last_review_completed_at.clone(),
        interval_days: settings.interval_days,
        counts,
    }
}

/// A project that is active but has no next step anywhere under it --
/// GTD's iron rule, and the only thing the review is strict about.
pub fn is_stalled(project: &Heading, all: &[Heading]) -> bool {
    if task_utils::is_finished(project.state.as_deref()) {
        return false;
    }
    !has_next(project, all)
}

fn has_next(heading: &Heading, all: &[Heading]) -> bool {
    if heading.state.as_deref() == Some("NEXT") {
        return true;
    }
    // The headings come back flat and in document order, so a subtree is
    // the run of deeper ones after it -- the same shape the task lists
    // use (see the port's ProjectTreeModel).
    all.iter()
        .skip_while(|candidate| candidate.line_no != heading.line_no)
        .skip(1)
        .take_while(|candidate| candidate.level > heading.level)
        .any(|child| child.state.as_deref() == Some("NEXT"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn heading(level: usize, line: usize, state: Option<&str>, tags: &[&str]) -> Heading {
        Heading {
            level,
            line_no: line,
            state: state.map(|s| s.to_string()),
            tags: tags.iter().map(|t| t.to_string()).collect(),
            ..Default::default()
        }
    }

    fn at(stamp: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(stamp).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn an_empty_list_ticks_its_own_step() {
        let counts = Counts { inbox: 0, actions: 3, waiting: 0, stalled: 0, someday: 2 };
        let summary = summarize(counts, &ReviewState::default(), &Settings::default(), at("2026-09-22T10:00:00Z"));
        assert!(summary.done.contains(&"inbox".to_string()));
        assert!(summary.done.contains(&"waiting".to_string()));
        assert!(!summary.done.contains(&"actions".to_string()));
        // Collecting happens away from the vault; nothing here can know.
        assert!(!summary.done.contains(&"collect".to_string()));
    }

    #[test]
    fn a_ticked_step_counts_even_with_items_left() {
        let state = ReviewState { checked_steps: vec!["actions".to_string()], ..Default::default() };
        let summary = summarize(Counts { actions: 5, ..Default::default() }, &state, &Settings::default(), at("2026-09-22T10:00:00Z"));
        assert!(summary.done.contains(&"actions".to_string()));
    }

    #[test]
    fn never_reviewed_is_overdue() {
        let summary = summarize(Counts::default(), &ReviewState::default(), &Settings::default(), at("2026-09-22T10:00:00Z"));
        assert!(summary.overdue);
        assert_eq!(summary.days_since, None);
    }

    #[test]
    fn overdue_is_the_interval_measured_from_the_last_one() {
        let state = ReviewState { last_review_completed_at: Some("2026-09-15T10:00:00Z".to_string()), ..Default::default() };
        let weekly = Settings::default();
        assert!(!summarize(Counts::default(), &state, &weekly, at("2026-09-21T09:00:00Z")).overdue);
        assert!(summarize(Counts::default(), &state, &weekly, at("2026-09-22T10:00:00Z")).overdue);
        assert_eq!(summarize(Counts::default(), &state, &weekly, at("2026-09-22T10:00:00Z")).days_since, Some(7));

        let fortnightly = Settings { interval_days: 14, ..Settings::default() };
        assert!(!summarize(Counts::default(), &state, &fortnightly, at("2026-09-22T10:00:00Z")).overdue);
    }

    #[test]
    fn a_project_with_a_next_step_anywhere_under_it_is_not_stalled() {
        let all = vec![
            heading(1, 1, Some("TODO"), &[]),
            heading(2, 2, Some("TODO"), &[]),
            heading(3, 3, Some("NEXT"), &[]),
            heading(1, 4, Some("TODO"), &[]),
            heading(2, 5, Some("TODO"), &[]),
        ];
        assert!(!is_stalled(&all[0], &all));
        assert!(is_stalled(&all[3], &all));
    }

    #[test]
    fn a_finished_project_is_not_stalled() {
        let all = vec![heading(1, 1, Some("DONE"), &[])];
        assert!(!is_stalled(&all[0], &all));
    }

    #[test]
    fn waiting_is_a_tag_in_either_language() {
        assert!(is_waiting_for(&heading(1, 1, None, &["waiting"])));
        assert!(is_waiting_for(&heading(1, 1, None, &["@Warten"])));
        assert!(!is_waiting_for(&heading(1, 1, None, &["work"])));
    }
}
