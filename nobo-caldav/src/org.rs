//! CalDavTask <-> org heading.
//!
//! Unlike the read-only ICS calendar mirror (nobo_calendar), these ARE
//! tasks: they carry a real todo keyword, so they show up in Next Actions
//! and search like anything else. Only the file they live in is special.
//!
//! ```org
//! #+TITLE: Einkauf
//! #+NOBO_TYPE: caldav-list
//!
//! * TODO Milch kaufen                                      :Einkauf:Privat:
//! DEADLINE: <2026-08-25 Tue>
//! :PROPERTIES:
//! :CUSTOM_ID: h:...
//! :CALDAV_UID: abc-123
//! :END:
//! Bio, wenn da
//! ```
//!
//! Two things are deliberately NOT in the file:
//!
//! - **The ETag.** It changes on every server-side edit, so keeping it
//!   here would make every pull a content change to the file -- which the
//!   vault's own cloud sync (nobo_sync) would then upload, and a second
//!   device would download and rewrite again. Two sync systems fighting
//!   over one file, with no human having changed anything. It lives in
//!   `caldav_state` instead.
//! - **Anything but TODO/DONE.** A VTODO is a checkbox; the app's
//!   intermediate states have no CalDAV counterpart, and offering them
//!   here would promise a sync that cannot happen (see
//!   file_kind::StateSupport::DoneOnly).

use chrono::{NaiveDate, NaiveDateTime};

use super::model::{CalDavTask, TaskDate, TaskStatus};
use nobo_core::core::heading_identity;
use nobo_core::core::org_syntax;
use nobo_core::core::models::Heading;

pub const CALDAV_UID_PROPERTY: &str = "CALDAV_UID";

/// Fixed namespace for deriving a stable CUSTOM_ID from a CalDAV UID --
/// any constant uuid works, it only has to never change.
const CALDAV_UID_NAMESPACE: uuid::Uuid = uuid::uuid!("2a7e4c11-93b6-4d8f-8a52-1c6f0b39d7e4");

/// The whole file for one list, tasks in the order given.
///
/// Order is content, not presentation: CalDAV has no inherent order for
/// VTODOs, so the file's line order IS this app's sort order (the
/// counterpart to X-APPLE-SORT-ORDER, see sync.rs).
pub fn render_list(subscription: &super::model::CalDavSubscription, tasks: &[CalDavTask]) -> String {
    // The header carries the subscription itself (collection URL and
    // user), and this is the only writer of the file -- so it has to come
    // from the one place that defines it, or a rewrite would quietly drop
    // the file's own identity and the list would stop being a
    // subscription on every device.
    let mut out = super::registry::header(subscription);
    for (task, level) in nested(tasks) {
        out.push_str(&render_task_at(task, level));
    }
    out
}

/// The flat, merge-ordered list laid out as a tree: every task follows its
/// parent, indented one level deeper.
///
/// Order within a level is the order given, so the user's sorting survives
/// -- nesting reorganizes, it does not re-sort. A task whose parent is not
/// in this list (deleted, or in another list) stays at the top level
/// rather than disappearing.
pub fn nested(tasks: &[CalDavTask]) -> Vec<(&CalDavTask, usize)> {
    let present: std::collections::HashSet<&str> = tasks.iter().map(|t| t.uid.as_str()).collect();
    let is_root = |task: &CalDavTask| match &task.parent_uid {
        Some(parent) => !present.contains(parent.as_str()),
        None => true,
    };

    let mut out = Vec::new();
    for task in tasks.iter().filter(|t| is_root(t)) {
        push_with_children(task, tasks, 1, &mut out);
    }
    out
}

fn push_with_children<'a>(
    task: &'a CalDavTask,
    all: &'a [CalDavTask],
    level: usize,
    out: &mut Vec<(&'a CalDavTask, usize)>,
) {
    out.push((task, level));
    // Bounded: org has no depth limit, but a cycle in the parent links
    // (which a server can hand out) would otherwise recurse forever.
    if level >= MAX_NESTING {
        return;
    }
    for child in all.iter().filter(|c| c.parent_uid.as_deref() == Some(task.uid.as_str())) {
        push_with_children(child, all, level + 1, out);
    }
}

/// Deep enough for any real task list, shallow enough that a cycle in the
/// server's parent links cannot blow the stack.
const MAX_NESTING: usize = 12;

/// One task at the top level. Used for hashing a single task's block (see
/// caldav::store::hash_block), where nesting is irrelevant.
pub fn render_task(task: &CalDavTask) -> String {
    render_task_at(task, 1)
}

fn render_task_at(task: &CalDavTask, level: usize) -> String {
    let mut lines = vec![heading_line(task, level)];

    let mut planning = Vec::new();
    let repeat = repeat_suffix(task);
    if let Some(start) = task.start {
        planning.push(format!("SCHEDULED: {}", org_timestamp(start, &repeat)));
    }
    if let Some(due) = task.due {
        planning.push(format!("DEADLINE: {}", org_timestamp(due, &repeat)));
    }
    if !planning.is_empty() {
        lines.push(planning.join(" "));
    }

    lines.push(":PROPERTIES:".to_string());
    lines.push(format!(":{}: {}", heading_identity::CUSTOM_ID_PROPERTY, custom_id_for(&task.uid)));
    lines.push(format!(":{CALDAV_UID_PROPERTY}: {}", task.uid));
    lines.push(":END:".to_string());

    if let Some(description) = task.description.as_ref().filter(|d| !d.trim().is_empty()) {
        lines.extend(description.lines().map(org_syntax::escape_body_line));
    }
    lines.push(String::new());
    lines.join("\n") + "\n"
}

fn heading_line(task: &CalDavTask, level: usize) -> String {
    let state = org_state(task.status);
    let title = task.summary.replace(['\n', '\r'], " ");
    let title = title.trim();
    let mut line = format!("{} {state} {title}", "*".repeat(level.max(1)));
    if !task.categories.is_empty() {
        // Org tags cannot contain spaces or colons; a category that does
        // is kept out rather than written as syntax the parser would then
        // read back as several tags.
        let tags: Vec<_> = task.categories.iter().filter(|c| is_valid_org_tag(c)).cloned().collect();
        if !tags.is_empty() {
            line.push_str(&format!("    :{}:", tags.join(":")));
        }
    }
    line
}

pub fn is_valid_org_tag(tag: &str) -> bool {
    !tag.is_empty() && tag.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '@' || c == '#' || c == '%')
}

/// A CalDAV task is a checkbox, so it renders as exactly two states.
///
/// The server may well hold IN-PROCESS or CANCELLED -- both read as
/// "not done" and "done" here. That information is not lost: a push only
/// rewrites STATUS when the checkbox itself was toggled (see
/// vtodo::apply_to_raw's `rewrite_status`), so a task left alone keeps
/// whatever finer state another client gave it.
pub fn org_state(status: TaskStatus) -> &'static str {
    if status.is_finished() { "DONE" } else { "TODO" }
}

/// The inverse, from whatever state the file happens to carry. Only
/// doneness is read -- everything else is org-side vocabulary CalDAV has
/// no word for.
pub fn caldav_status(state: Option<&str>) -> TaskStatus {
    match state.unwrap_or("").to_ascii_uppercase().as_str() {
        "DONE" | "OBE" | "WONT-DO" => TaskStatus::Completed,
        _ => TaskStatus::NeedsAction,
    }
}

/// Stable across pulls: the same remote task keeps its CUSTOM_ID, so
/// history, links and the sync merge survive a rewrite of the file.
pub fn custom_id_for(uid: &str) -> String {
    format!("h:{}", uuid::Uuid::new_v5(&CALDAV_UID_NAMESPACE, uid.as_bytes()))
}

/// `repeat` is the RRULE's org form, appended inside the timestamp so the
/// user can see the task recurs. It is never written back (the server owns
/// the recurrence, see model::CalDavTask::repeater).
fn org_timestamp(date: TaskDate, repeat: &str) -> String {
    match date {
        TaskDate::Day(day) => org_syntax::active_timestamp(day, None, repeat),
        TaskDate::At(moment) => org_syntax::active_timestamp_at(moment, repeat),
    }
}

fn repeat_suffix(task: &CalDavTask) -> String {
    org_syntax::repeater_suffix(task.repeater.as_ref())
}


/// Reads a heading parsed out of a list file back into the task shape, so
/// a local edit can be pushed.
///
/// `None` for a heading without a CALDAV_UID -- something the user typed
/// into the file by hand. Those are not silently adopted here; sync.rs
/// decides what to do with them.
pub fn task_from_heading(heading: &Heading) -> Option<CalDavTask> {
    let uid = property(heading, CALDAV_UID_PROPERTY)?;
    Some(CalDavTask {
        uid,
        summary: heading.title.clone(),
        // Unescaped, because `render_task` escaped it on the way out --
        // see org_syntax::unescape_body_line for what one-way escaping
        // cost.
        description: Some(
            heading
                .content
                .lines()
                .map(org_syntax::unescape_body_line)
                .collect::<Vec<_>>()
                .join("\n")
                .trim()
                .to_string(),
        )
        .filter(|c| !c.is_empty()),
        status: caldav_status(heading.state.as_deref()),
        // The server stamps its own COMPLETED when it has none; org's
        // CLOSED is a local formatting of the same moment and is parsed
        // back here only when present.
        completed_at: heading.closed.as_deref().and_then(parse_closed),
        start: heading.scheduled.as_ref().and_then(task_date_from_org),
        due: heading.deadline.as_ref().and_then(task_date_from_org),
        categories: heading.tags.clone(),
        // Both come from context the heading alone does not carry: the
        // parent from the tree it sits in (see parent_uids_by_level), the
        // repeater from the server. Filled in by the caller.
        parent_uid: None,
        repeater: None,
        // Both belong to the server; the file cannot carry them.
        created: None,
        // Restored by the caller from the last known server state -- see
        // CalDavTask::unmappable_categories. An org heading cannot carry
        // them, so they are not recoverable from the file alone.
        unmappable_categories: Vec::new(),
        sort_order: None,
        raw: None,
    })
}

/// The parent uid for each heading in a parsed list file, by position.
///
/// Derived from org nesting rather than stored per heading: the file's
/// indentation IS the relationship, so dragging a subtask under another
/// task is what re-parents it -- and a stored property would then
/// contradict what the user can see.
pub fn parent_uids(headings: &[Heading]) -> Vec<Option<String>> {
    // The uid of the innermost heading seen at each level so far.
    let mut ancestors: Vec<(usize, String)> = Vec::new();
    let mut out = Vec::with_capacity(headings.len());

    for heading in headings {
        ancestors.retain(|(level, _)| *level < heading.level);
        out.push(ancestors.last().map(|(_, uid)| uid.clone()));
        if let Some(uid) = uid_of(heading) {
            ancestors.push((heading.level, uid));
        }
    }
    out
}

pub fn uid_of(heading: &Heading) -> Option<String> {
    property(heading, CALDAV_UID_PROPERTY)
}

/// Trimmed, and blank counts as absent -- a CalDAV list file is written by
/// this app, so an empty `:CALDAV_UID:` means "none", not "the empty uid".
/// The case-insensitive lookup itself is the shared one (see
/// models::property_value).
fn property(heading: &Heading, name: &str) -> Option<String> {
    nobo_core::core::models::property_value(&heading.properties, name)
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn parse_closed(value: &str) -> Option<NaiveDateTime> {
    NaiveDateTime::parse_from_str(value.trim(), "%Y-%m-%d %a %H:%M").ok()
}

fn task_date_from_org(ts: &nobo_core::core::models::OrgTimestamp) -> Option<TaskDate> {
    let date = NaiveDate::parse_from_str(&ts.start_date, "%Y-%m-%d").ok()?;
    match ts.start_time.as_deref() {
        Some(time) => chrono::NaiveTime::parse_from_str(time, "%H:%M")
            .ok()
            .map(|t| TaskDate::At(date.and_time(t))),
        None => Some(TaskDate::Day(date)),
    }
}

#[cfg(test)]
mod tests {
    /// The renderer takes the whole subscription now (its header IS the
    /// subscription, see registry) -- these tests only care about the
    /// tasks below it, so they all render the same nondescript list.
    fn list(name: &str) -> super::super::model::CalDavSubscription {
        super::super::registry::subscription_for(
            super::super::model::CalDavKind::Tasks,
            name,
            "dennis",
            "https://cloud.example.com/dav/cal/dennis/einkauf/",
        )
    }

    use super::*;
    use crate::model::TaskStatus;
    use nobo_core::core::file_kind;
    use nobo_core::core::vault;
    use std::path::Path;

    fn task(uid: &str, summary: &str) -> CalDavTask {
        CalDavTask {
            uid: uid.to_string(),
            summary: summary.to_string(),
            description: None,
            status: TaskStatus::NeedsAction,
            completed_at: None,
            start: None,
            due: None,
            categories: Vec::new(),
            unmappable_categories: Vec::new(),
            parent_uid: None,
            repeater: None,
            created: None,
            sort_order: None,
            raw: None,
        }
    }

    fn parse(rendered: &str) -> Vec<nobo_core::core::models::Heading> {
        vault::parse_headings(Path::new("einkauf.org"), &rendered)
    }

    /// What makes the whole special-file-type machinery apply to this file
    /// -- without it the app treats it as an ordinary note and offers
    /// states CalDAV cannot represent.
    #[test]
    fn the_file_declares_itself_a_caldav_list() {
        let rendered = render_list(&list("Einkauf"), &[task("a", "Milch")]);

        assert_eq!(file_kind::from_lines(&rendered.lines().collect::<Vec<_>>()), crate::LIST_KIND);
        assert!(rendered.starts_with("#+TITLE: Einkauf\n"), "{rendered}");
    }

    /// The ETag deliberately does NOT live here: it changes on every
    /// server-side edit, and a file that changes on every pull would be
    /// uploaded and re-downloaded by the vault's own sync forever.
    #[test]
    fn the_file_carries_the_uid_but_never_the_etag() {
        let rendered = render_list(&list("Einkauf"), &[task("abc-123", "Milch")]);

        assert!(rendered.contains(":CALDAV_UID: abc-123"), "{rendered}");
        assert!(!rendered.to_uppercase().contains("ETAG"), "{rendered}");
    }

    #[test]
    fn every_task_parses_back_as_its_own_heading() {
        let tasks: Vec<_> = (0..4).map(|i| task(&format!("uid-{i}"), &format!("Aufgabe {i}"))).collect();

        let headings = parse(&render_list(&list("Einkauf"), &tasks));

        assert_eq!(headings.len(), 4);
        assert_eq!(headings[2].title, "Aufgabe 2");
        assert_eq!(uid_of(&headings[2]).as_deref(), Some("uid-2"));
    }

    /// A checkbox, not a workflow: the file only ever holds these two.
    #[test]
    fn a_task_renders_as_open_or_done_and_nothing_else() {
        for (status, expected) in [
            (TaskStatus::NeedsAction, "TODO"),
            (TaskStatus::InProcess, "TODO"),
            (TaskStatus::Completed, "DONE"),
            (TaskStatus::Cancelled, "DONE"),
        ] {
            let mut t = task("a", "X");
            t.status = status;
            let headings = parse(&render_list(&list("L"), &[t]));
            assert_eq!(headings[0].state.as_deref(), Some(expected), "{status:?}");
        }
    }

    #[test]
    fn dates_map_onto_scheduled_and_deadline() {
        let mut t = task("a", "Milch");
        t.start = Some(TaskDate::Day(NaiveDate::from_ymd_opt(2026, 8, 21).unwrap()));
        t.due = Some(TaskDate::Day(NaiveDate::from_ymd_opt(2026, 8, 25).unwrap()));

        let headings = parse(&render_list(&list("L"), &[t]));

        assert_eq!(headings[0].scheduled.as_ref().unwrap().start_date, "2026-08-21");
        assert_eq!(headings[0].deadline.as_ref().unwrap().start_date, "2026-08-25");
    }

    #[test]
    fn a_due_time_survives_the_round_trip() {
        let due = Some(TaskDate::At(NaiveDate::from_ymd_opt(2026, 8, 25).unwrap().and_hms_opt(14, 30, 0).unwrap()));
        let mut t = task("a", "Anruf");
        t.due = due;

        let headings = parse(&render_list(&list("L"), &[t]));
        let back = task_from_heading(&headings[0]).unwrap();

        assert_eq!(back.due, due);
    }

    #[test]
    fn categories_become_org_tags() {
        let mut t = task("a", "Milch");
        t.categories = vec!["Einkauf".to_string(), "Privat".to_string()];

        let headings = parse(&render_list(&list("L"), &[t]));

        assert_eq!(headings[0].tags, vec!["Einkauf", "Privat"]);
    }

    /// A category with a space has no org tag form. Writing it as one
    /// would produce syntax the parser reads back as several tags.
    #[test]
    fn a_category_with_no_tag_form_is_left_out_of_the_heading_line() {
        let mut t = task("a", "Milch");
        t.categories = vec!["Einkauf".to_string()];
        t.unmappable_categories = vec!["Haus, Garten".to_string()];

        let headings = parse(&render_list(&list("L"), &[t]));

        assert_eq!(headings[0].tags, vec!["Einkauf"]);
    }

    #[test]
    fn the_description_becomes_the_body_and_comes_back_unchanged() {
        let mut t = task("a", "Milch");
        t.description = Some("Bio, wenn da\nsonst normale".to_string());

        let headings = parse(&render_list(&list("L"), &[t]));
        let back = task_from_heading(&headings[0]).unwrap();

        assert_eq!(back.description.as_deref(), Some("Bio, wenn da\nsonst normale"));
    }

    /// Task notes are free text and routinely start a line with `*` or
    /// `#+` -- unescaped, the first would become a heading of its own and
    /// the second would swallow the rest of the file.
    #[test]
    fn a_description_cannot_introduce_structure_of_its_own() {
        let mut first = task("a", "Erste");
        first.description = Some("* Nicht wirklich\n#+BEGIN_SRC".to_string());
        let second = task("b", "Zweite");

        let headings = parse(&render_list(&list("L"), &[first, second]));

        assert_eq!(headings.len(), 2, "description became structure");
        assert_eq!(headings[1].title, "Zweite");
    }

    #[test]
    fn a_multiline_summary_stays_on_the_heading_line() {
        let headings = parse(&render_list(&list("L"), &[task("a", "Titel\nmit Umbruch")]));

        assert_eq!(headings[0].title, "Titel mit Umbruch");
    }

    /* --- subtasks --- */

    fn child(uid: &str, summary: &str, parent: &str) -> CalDavTask {
        let mut t = task(uid, summary);
        t.parent_uid = Some(parent.to_string());
        t
    }

    /// How both Nextcloud and Reminders model subtasks; org's own form for
    /// the same thing is nesting.
    #[test]
    fn a_subtask_is_rendered_one_level_below_its_parent() {
        let rendered = render_list(&list("Einkauf"), &[task("p", "Einkauf planen"), child("c", "Milch", "p")]);

        let headings = parse(&rendered);
        assert_eq!(headings.len(), 2);
        assert_eq!(headings[0].level, 1);
        assert_eq!(headings[1].level, 2);
        assert_eq!(headings[1].title, "Milch");
    }

    #[test]
    fn nesting_survives_the_round_trip_back_into_parent_links() {
        let rendered = render_list(&list("Einkauf"), &[task("p", "Planen"), child("c", "Milch", "p"), task("x", "Extra")]);

        let headings = parse(&rendered);
        let parents = parent_uids(&headings);

        assert_eq!(parents, vec![None, Some("p".to_string()), None]);
    }

    #[test]
    fn nesting_goes_deeper_than_one_level() {
        let mut grandchild = child("g", "Bio-Milch", "c");
        grandchild.summary = "Bio".to_string();
        let rendered = render_list(&list("L"), &[task("p", "Planen"), child("c", "Milch", "p"), grandchild]);

        let headings = parse(&rendered);

        assert_eq!(headings.iter().map(|h| h.level).collect::<Vec<_>>(), vec![1, 2, 3]);
        assert_eq!(parent_uids(&headings)[2], Some("c".to_string()));
    }

    /// A parent in another list, or simply not synced here, cannot be
    /// shown. Dropping the task entirely would be far worse than showing
    /// it at the top level.
    #[test]
    fn a_task_whose_parent_is_missing_still_appears() {
        let rendered = render_list(&list("L"), &[child("c", "Milch", "nicht-hier")]);

        let headings = parse(&rendered);

        assert_eq!(headings.len(), 1);
        assert_eq!(headings[0].level, 1);
    }

    /// A server can hand out parent links that form a cycle. Following
    /// them naively recurses until the stack runs out.
    #[test]
    fn a_cycle_in_the_parent_links_does_not_hang() {
        let rendered = render_list(&list("L"), &[child("a", "A", "b"), child("b", "B", "a")]);

        // Both are unreachable as roots, so nothing is emitted rather than
        // the process dying -- the important part is that it terminates.
        assert!(parse(&rendered).len() <= 2);
    }

    #[test]
    fn siblings_keep_the_order_they_were_given() {
        let rendered = render_list(&list("L"), &[task("p", "Planen"), child("b", "Brot", "p"), child("a", "Milch", "p")]);

        let headings = parse(&rendered);

        assert_eq!(headings[1].title, "Brot");
        assert_eq!(headings[2].title, "Milch");
    }

    /* --- recurrence --- */

    #[test]
    fn a_recurring_task_shows_its_interval_in_the_timestamp() {
        let mut t = task("a", "Müll rausbringen");
        t.due = Some(TaskDate::Day(NaiveDate::from_ymd_opt(2026, 8, 25).unwrap()));
        t.repeater = Some(nobo_core::core::models::Repeater {
            kind: nobo_core::core::models::RepeaterKind::Cumulative,
            value: 1,
            unit: 'w',
            habit_max: None,
        });

        let rendered = render_list(&list("L"), &[t]);

        assert!(rendered.contains("DEADLINE: <2026-08-25 Tue +1w>"), "{rendered}");
    }

    /* --- reading a file back --- */

    #[test]
    fn a_local_edit_reads_back_as_the_task_to_push() {
        let mut t = task("abc-123", "Milch kaufen");
        t.categories = vec!["Einkauf".to_string()];
        let rendered = render_list(&list("Einkauf"), &[t]);

        // What a user edit looks like: a different title, ticked off.
        let edited = rendered.replace("* TODO Milch kaufen", "* DONE Hafermilch kaufen");
        let headings = parse(&edited);
        let back = task_from_heading(&headings[0]).unwrap();

        assert_eq!(back.uid, "abc-123");
        assert_eq!(back.summary, "Hafermilch kaufen");
        assert_eq!(back.status, TaskStatus::Completed);
        assert_eq!(back.categories, vec!["Einkauf"]);
    }

    /// A heading the user typed into the file by hand has no UID. It is
    /// not silently adopted here -- sync.rs decides whether it becomes a
    /// new remote task.
    #[test]
    fn a_heading_without_a_uid_is_not_a_caldav_task() {
        let headings = parse("* TODO Von Hand getippt\n:PROPERTIES:\n:CUSTOM_ID: h:1\n:END:\n");

        assert!(task_from_heading(&headings[0]).is_none());
    }

    /// Same remote task, later pull: the identity has to survive, or every
    /// refresh looks like a delete-and-recreate to history and links.
    #[test]
    fn the_custom_id_is_derived_from_the_uid_and_stays_stable() {
        assert_eq!(custom_id_for("abc-123"), custom_id_for("abc-123"));
        assert_ne!(custom_id_for("abc-123"), custom_id_for("abc-124"));
    }

    #[test]
    fn done_states_of_every_flavour_read_as_completed() {
        for state in ["DONE", "OBE", "WONT-DO"] {
            assert_eq!(caldav_status(Some(state)), TaskStatus::Completed, "{state}");
        }
        for state in ["TODO", "NEXT", "IN-PROGRESS", "PLANNING"] {
            assert_eq!(caldav_status(Some(state)), TaskStatus::NeedsAction, "{state}");
        }
    }
}
