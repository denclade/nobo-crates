//! What a day looks like: appointments, what is planned for it, and what
//! to do next.
//!
//! This used to be the frontend's ([`useAgenda.ts`] plus the memos in
//! `DashboardView.tsx`): which tasks fall on a day, which of them carry a
//! time, how much of the day is already spoken for, and which actions are
//! the next ones. None of that is layout -- it is what an agenda *is*,
//! and a second front end (harbour-nobo) would have had to arrive at the
//! same answers by reimplementing the same rules.
//!
//! So it lives here, once, and both front ends render what it says.
//!
//! # What an entry is
//!
//! A [`Heading`], as everywhere else. The agenda adds three things a
//! reader needs and cannot derive: which file kind it came from
//! ([`Source`]), the project it sits under (a subtask on its own says
//! nothing about where it belongs), and the timestamp that put it on this
//! day -- `SCHEDULED`, `DEADLINE` or a bare active timestamp, which is how
//! a calendar entry carries its date (see `nobo-calendar-mirror`).
//!
//! # What contributes
//!
//! The core knows two sources: the projects file and the actions file. A
//! feature adds its own by registering an [`AgendaContributor`] -- the
//! same shape [`crate::core::search::SearchContributor`] has, and for the
//! same reason: the core must not name features. A build without
//! calendars simply has no contributor, and the agenda is the vault's own
//! tasks.

use std::path::Path;

use serde::Serialize;

use crate::core::models::{Heading, OrgTimestamp};
use crate::core::vault::Vault;

/// Where an agenda entry came from. The reader shows it differently -- a
/// meeting is not a task -- and only the source can say which it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, specta::Type)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// A heading under a project in the projects file.
    Project,
    /// A standalone action from the actions file.
    Action,
    /// Contributed by a feature -- a calendar subscription, a synced
    /// list. Its `calendar` field says which one.
    Calendar,
}

/// One line of an agenda, with what the reader needs to place it.
#[derive(Debug, Clone, Serialize, specta::Type)]
pub struct AgendaEntry {
    pub heading: Heading,
    pub source: Source,
    /// The enclosing project's title, for a heading that has one.
    #[specta(optional)]
    pub project: Option<String>,
    /// The timestamp that put this entry on this day: `SCHEDULED`,
    /// `DEADLINE`, or the bare active timestamp of a calendar entry.
    /// Never `None` in a day's entries; `None` only in `next_actions`,
    /// which is what "no date at all" means there.
    #[specta(optional)]
    pub when: Option<OrgTimestamp>,
    /// Whether `when` names a time of day. A whole-day entry is shown in
    /// its own block, above the timed ones.
    pub timed: bool,
    /// The name of the contributing calendar, for `Source::Calendar`.
    #[specta(optional)]
    pub calendar: Option<String>,
    /// Its colour, as the contributor gave it (`#rrggbb`).
    #[specta(optional)]
    pub color: Option<String>,
}

/// One day, as the agenda shows it.
#[derive(Debug, Clone, Serialize, specta::Type)]
pub struct AgendaDay {
    /// The day this is about, ISO (`2026-09-22`).
    pub date: String,
    /// Entries on this day without a time: calendar entries first, since
    /// "today is a holiday" frames everything under it.
    pub all_day: Vec<AgendaEntry>,
    /// Entries with a time, earliest first.
    pub timed: Vec<AgendaEntry>,
    /// Open `NEXT` actions, whatever day they carry -- the list that
    /// answers "and what now", which is not a property of this day but is
    /// what a reader wants beside it.
    pub next_actions: Vec<AgendaEntry>,
    /// Minutes already spoken for by timed entries on this day.
    #[specta(type = specta_typescript::Number)]
    pub busy_minutes: u32,
    /// Estimated effort of this day's entries (their `EFFORT`).
    #[specta(type = specta_typescript::Number)]
    pub planned_effort_minutes: u32,
    /// Estimated effort of the next actions that carry no date -- work
    /// that is waiting without being planned.
    #[specta(type = specta_typescript::Number)]
    pub unscheduled_effort_minutes: u32,
}

/// Days a reader should be able to see at a glance have something on
/// them -- what the date picker marks with a dot.
#[derive(Debug, Clone, Serialize, specta::Type)]
pub struct AgendaMonth {
    /// ISO dates in this range that carry at least one entry.
    pub busy_days: Vec<String>,
}

/// What a feature puts in the agenda.
///
/// Registered with `inventory`, exactly like a search contributor, so the
/// core never names one. The function is given the vault and the day and
/// answers with whatever it has for it -- a calendar subscription reads
/// its mirror files, which are in the vault like everything else.
///
/// Runs while someone is looking at a day, so: no network, no re-parsing,
/// read through the [`Vault`].
pub struct AgendaContributor {
    /// The feature's id, for logs and for switching it off.
    pub source: &'static str,
    pub entries: fn(&Vault, day: &str) -> Vec<AgendaEntry>,
    /// Which days between FROM and TO (ISO, inclusive) this feature has
    /// something on -- the dots in a month view (see [`busy_days`]).
    ///
    /// Its own function rather than `entries` once per day: a month is
    /// thirty days, and building thirty lists of entries to ask each one
    /// whether it is empty is thirty passes over the mirrors.
    pub days: fn(&Vault, from: &str, to: &str) -> Vec<String>,
}

inventory::collect!(AgendaContributor);

/// Whether `stamp` covers `day` (ISO `YYYY-MM-DD`).
///
/// A range covers every day from its start to its end; a single timestamp
/// covers its own. Compared as strings on purpose: both sides are ISO, so
/// the comparison is the date comparison, and nothing has to parse.
fn covers_day(stamp: &OrgTimestamp, day: &str) -> bool {
    match stamp.end_date.as_deref() {
        Some(end) => stamp.start_date.as_str() <= day && day <= end,
        None => stamp.start_date == day,
    }
}

/// The timestamp that puts a heading on a day, and which one it was.
///
/// Order matters and is the order a reader expects: what is planned for
/// today outranks what is merely due today, and a bare active timestamp
/// is what a calendar entry has instead of either.
fn when_on(heading: &Heading, day: &str) -> Option<OrgTimestamp> {
    for candidate in [&heading.scheduled, &heading.deadline, &heading.occurrence] {
        if let Some(stamp) = candidate {
            if covers_day(stamp, day) {
                return Some(stamp.clone());
            }
        }
    }
    None
}

/// `14:30` as minutes since midnight. Org writes times one way, so this
/// is a split rather than a format parse.
fn minutes_of_day(time: &str) -> Option<u32> {
    let (hours, minutes) = time.split_once(':')?;
    Some(hours.trim().parse::<u32>().ok()? * 60 + minutes.trim().parse::<u32>().ok()?)
}

/// Minutes between start and end time of a timestamp, or 0 when it names
/// no span. What "this day is N minutes busy" counts.
fn span_minutes(stamp: &OrgTimestamp) -> u32 {
    let (Some(start), Some(end)) = (stamp.start_time.as_deref(), stamp.end_time.as_deref()) else {
        return 0;
    };
    let (Some(start), Some(end)) = (minutes_of_day(start), minutes_of_day(end)) else {
        return 0;
    };
    end.saturating_sub(start)
}

/// Flattens a project tree into entries, remembering the project each
/// heading sits under.
///
/// The project is the level-1 heading's title -- the same rule
/// `useAgenda`'s `flattenProjects` used, and the one a reader assumes:
/// "which project is this subtask part of", not "what is its parent".
fn flatten_projects(nodes: &[Heading], project: Option<&str>, out: &mut Vec<AgendaEntry>) {
    for node in nodes {
        let own_project = if node.level == 1 { Some(node.title.as_str()) } else { project };
        out.push(AgendaEntry {
            heading: node.clone(),
            source: Source::Project,
            project: own_project.map(str::to_string),
            when: None,
            timed: false,
            calendar: None,
            color: None,
        });
        flatten_projects(&node.children, own_project, out);
    }
}

/// The vault's own headings: the projects tree and the actions file.
///
/// Apart from the features, because they are the part that is the same
/// whichever day is being asked about -- a heading carries the stamps
/// that place it, and nothing has to be filtered to collect it.
fn vault_entries(vault: &Vault, projects_file: &Path, actions_file: &Path) -> Vec<AgendaEntry> {
    let mut entries = Vec::new();

    let projects = crate::core::heading_utils::projects_tree(vault.headings(projects_file));
    flatten_projects(&projects, None, &mut entries);

    entries.extend(vault.headings(actions_file).into_iter().map(|heading| AgendaEntry {
        heading,
        source: Source::Action,
        project: None,
        when: None,
        timed: false,
        calendar: None,
        color: None,
    }));
    entries
}

/// Every task this vault knows, from both files, plus what features
/// contribute for `day`.
fn all_entries(vault: &Vault, projects_file: &Path, actions_file: &Path, day: &str) -> Vec<AgendaEntry> {
    let mut entries = vault_entries(vault, projects_file, actions_file);
    for contributor in inventory::iter::<AgendaContributor> {
        entries.extend((contributor.entries)(vault, day));
    }
    entries
}

/// How many NEXT actions are worth showing beside a day. Enough to plan
/// with, few enough to read -- the number the frontend used.
const NEXT_ACTION_LIMIT: usize = 20;

/// The agenda for one day.
///
/// `day` is ISO (`2026-09-22`). Reads through the [`Vault`], so it is
/// cheap enough to ask on every navigation and never touches the disk for
/// a file the index already has.
pub fn day(vault: &Vault, projects_file: &Path, actions_file: &Path, day: &str) -> AgendaDay {
    let entries = all_entries(vault, projects_file, actions_file, day);

    let mut all_day = Vec::new();
    let mut timed = Vec::new();
    for entry in &entries {
        // A calendar entry brought its own `when` (it has no SCHEDULED);
        // everything else is placed by its timestamps.
        let when = entry.when.clone().filter(|stamp| covers_day(stamp, day)).or_else(|| when_on(&entry.heading, day));
        let Some(when) = when else { continue };
        let is_timed = when.start_time.is_some();
        let placed = AgendaEntry { timed: is_timed, when: Some(when), ..entry.clone() };
        if is_timed {
            timed.push(placed)
        } else {
            all_day.push(placed)
        }
    }

    // Calendar entries first among the whole-day ones: a holiday or a
    // school break is the frame the rest of the day sits in.
    all_day.sort_by_key(|entry| entry.source != Source::Calendar);
    timed.sort_by(|a, b| {
        let start = |entry: &AgendaEntry| entry.when.as_ref().and_then(|w| w.start_time.clone()).unwrap_or_default();
        start(a).cmp(&start(b))
    });

    let busy_minutes = timed.iter().filter_map(|entry| entry.when.as_ref().map(span_minutes)).sum();
    let planned_effort_minutes = all_day
        .iter()
        .chain(timed.iter())
        .filter(|entry| entry.source != Source::Calendar)
        .filter_map(|entry| entry.heading.effort_minutes)
        .sum();

    let mut next_actions: Vec<AgendaEntry> = entries
        .into_iter()
        .filter(|entry| entry.source != Source::Calendar && entry.heading.state.as_deref() == Some("NEXT"))
        .map(|entry| {
            let when = when_on(&entry.heading, day).or_else(|| entry.heading.scheduled.clone());
            AgendaEntry { timed: when.as_ref().is_some_and(|w| w.start_time.is_some()), when, ..entry }
        })
        .collect();
    // Dated first and by date, undated after: what is due soonest is what
    // a reader reaches for.
    next_actions.sort_by_key(|entry| entry.when.as_ref().map(|w| (0, w.start_date.clone())).unwrap_or((1, String::new())));
    let unscheduled_effort_minutes =
        next_actions.iter().filter(|entry| entry.when.is_none()).filter_map(|entry| entry.heading.effort_minutes).sum();
    next_actions.truncate(NEXT_ACTION_LIMIT);

    AgendaDay { date: day.to_string(), all_day, timed, next_actions, busy_minutes, planned_effort_minutes, unscheduled_effort_minutes }
}

/// Which days between `from` and `to` (ISO, inclusive) carry anything --
/// the dots under a date picker.
///
/// One pass over the vault rather than one `day()` per day: a month view
/// asks for thirty days at once, and thirty full agendas would parse the
/// projects tree thirty times.
pub fn busy_days(vault: &Vault, projects_file: &Path, actions_file: &Path, from: &str, to: &str) -> AgendaMonth {
    let entries = vault_entries(vault, projects_file, actions_file);
    let mut days: Vec<String> = Vec::new();
    for entry in &entries {
        for stamp in
            [entry.when.as_ref(), entry.heading.scheduled.as_ref(), entry.heading.deadline.as_ref(), entry.heading.occurrence.as_ref()]
                .into_iter()
                .flatten()
        {
            // Only the start day, not every day of a range: a dot says
            // "something begins here", and a week-long range would
            // otherwise paint the whole week.
            if stamp.start_date.as_str() >= from && stamp.start_date.as_str() <= to {
                days.push(stamp.start_date.clone());
            }
        }
    }
    // The features are asked for the range rather than read out of
    // `entries`: what a calendar has on a day is in its mirror files,
    // and a contributor is the only thing that knows which those are.
    for contributor in inventory::iter::<AgendaContributor> {
        days.extend((contributor.days)(vault, from, to).into_iter().filter(|day| day.as_str() >= from && day.as_str() <= to));
    }
    days.sort();
    days.dedup();
    AgendaMonth { busy_days: days }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn stamp(date: &str, start: Option<&str>, end: Option<&str>) -> OrgTimestamp {
        OrgTimestamp {
            start_date: date.to_string(),
            start_time: start.map(str::to_string),
            end_date: None,
            end_time: end.map(str::to_string),
            active: true,
            repeater: None,
            extras: Vec::new(),
            raw: date.to_string(),
        }
    }

    fn heading(title: &str, state: Option<&str>) -> Heading {
        Heading { title: title.to_string(), state: state.map(str::to_string), level: 1, properties: HashMap::new(), ..Default::default() }
    }

    #[test]
    fn a_range_covers_every_day_between_its_ends() {
        let mut range = stamp("2026-09-20", None, None);
        range.end_date = Some("2026-09-23".to_string());

        assert!(covers_day(&range, "2026-09-20"));
        assert!(covers_day(&range, "2026-09-22"));
        assert!(covers_day(&range, "2026-09-23"));
        assert!(!covers_day(&range, "2026-09-24"));
    }

    /// The order is what a reader assumes: what I planned for today beats
    /// what merely falls due today.
    #[test]
    fn scheduled_outranks_deadline_on_the_same_day() {
        let mut task = heading("Steuer", Some("TODO"));
        task.scheduled = Some(stamp("2026-09-22", Some("09:00"), None));
        task.deadline = Some(stamp("2026-09-22", None, None));

        let when = when_on(&task, "2026-09-22").expect("placed");

        assert_eq!(when.start_time.as_deref(), Some("09:00"));
    }

    /// A calendar entry has neither -- its date is a bare active
    /// timestamp, and without this it would never appear.
    #[test]
    fn a_bare_occurrence_places_an_entry_too() {
        let mut event = heading("Feiertag", None);
        event.occurrence = Some(stamp("2026-09-22", None, None));

        assert!(when_on(&event, "2026-09-22").is_some());
    }

    #[test]
    fn busy_minutes_count_only_what_has_a_span() {
        assert_eq!(span_minutes(&stamp("2026-09-22", Some("12:00"), Some("13:30"))), 90);
        assert_eq!(span_minutes(&stamp("2026-09-22", Some("12:00"), None)), 0, "a start alone is not a span");
        assert_eq!(span_minutes(&stamp("2026-09-22", None, None)), 0);
    }

    /// A subtask says nothing about where it belongs; the agenda has to.
    #[test]
    fn a_subtask_carries_the_project_it_sits_under() {
        let mut project = heading("Umbau Büro", Some("TODO"));
        let mut child = heading("Kabel verlegen", Some("NEXT"));
        child.level = 2;
        let mut grandchild = heading("Kabelkanal kaufen", Some("TODO"));
        grandchild.level = 3;
        child.children = vec![grandchild];
        project.children = vec![child];

        let mut out = Vec::new();
        flatten_projects(&[project], None, &mut out);

        assert_eq!(out.len(), 3);
        assert_eq!(out[1].project.as_deref(), Some("Umbau Büro"));
        assert_eq!(out[2].project.as_deref(), Some("Umbau Büro"), "a grandchild belongs to the project too");
    }
}
