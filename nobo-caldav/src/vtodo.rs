//! VTODO <-> CalDavTask.
//!
//! The format's chores are `nobo_ical`'s -- properties, parameters,
//! escaping, folding, and what a `TZID` means -- and shared with the ICS
//! calendar side, which reads VEVENTs out of the same format. What is
//! here is what a TODO means to this app.
//!
//! Reading parses; writing does NOT regenerate the component. A push
//! rewrites only the properties this app owns and leaves every other line
//! of the fetched VTODO exactly where it was -- see `apply_to_raw`, and
//! CalDavTask::raw's own comment for what would otherwise be destroyed.

use chrono::{DateTime, Local, NaiveDateTime, TimeZone, Utc};
use ical::parser::ical::component::IcalTodo;
use ical::property::Property;
// Folding a line at 75 octets and joining the continuations back is the
// format's rule, the same one the calendar side follows.
use nobo_ical::{fold, unfold, When};

use super::model::{CalDavTask, TaskDate, TaskStatus};

/// Properties this app owns. A push replaces exactly these lines and
/// leaves everything else untouched; anything added here starts being
/// overwritten on the server, so the list is deliberately short.
const OWNED_PROPERTIES: &[&str] = &["SUMMARY", "DESCRIPTION", "CATEGORIES", "LAST-MODIFIED", "DTSTAMP"];

/// The planning dates, owned only for a task that does NOT recur. A
/// recurring VTODO's dates belong to its RRULE: the server works out when
/// the next occurrence is due, and writing a date over that would fight
/// the rule -- and lose, differently on every server.
const DATE_PROPERTIES: &[&str] = &["DTSTART", "DUE"];

/// The completion properties, owned only when the checkbox was actually
/// toggled here. Left alone otherwise, so a task another client set to
/// IN-PROCESS keeps that instead of being flattened to NEEDS-ACTION on
/// every unrelated push (see org::org_state).
const STATUS_PROPERTIES: &[&str] = &["STATUS", "COMPLETED", "PERCENT-COMPLETE"];

/// Every VTODO in `ics`. A calendar object usually holds exactly one, but
/// recurrence overrides can add more -- only the master (no RECURRENCE-ID)
/// is taken, since this app has no vocabulary for the others and would
/// otherwise show the same task several times.
pub fn parse_tasks(ics: &str) -> Result<Vec<CalDavTask>, String> {
    let mut tasks = Vec::new();
    for calendar in ical::IcalParser::new(ics.as_bytes()) {
        let calendar = calendar.map_err(|e| format!("not a readable VTODO: {e}"))?;
        for todo in &calendar.todos {
            if find(todo, "RECURRENCE-ID").is_some() {
                continue;
            }
            if let Some(task) = task_from_todo(todo, ics) {
                tasks.push(task);
            }
        }
    }
    Ok(tasks)
}

fn task_from_todo(todo: &IcalTodo, raw: &str) -> Option<CalDavTask> {
    let uid = text(todo, "UID")?;
    let mut task = CalDavTask {
        uid,
        summary: text(todo, "SUMMARY").unwrap_or_default(),
        description: text(todo, "DESCRIPTION").filter(|d| !d.trim().is_empty()),
        status: status_of(todo),
        completed_at: find(todo, "COMPLETED").and_then(date_time_of),
        start: find(todo, "DTSTART").and_then(task_date_of),
        due: find(todo, "DUE").and_then(task_date_of),
        categories: Vec::new(),
        parent_uid: parent_of(todo),
        repeater: find(todo, "RRULE").and_then(|p| p.value.as_deref()).and_then(nobo_calendar_mirror::ics::repeater_from_rrule),
        unmappable_categories: find(todo, "CATEGORIES")
            .and_then(|p| p.value.clone())
            .map(|v| nobo_ical::split_list(&v))
            .unwrap_or_default(),
        // RFC 5545 section 3.8.7.1 requires CREATED in UTC. Converting it
        // to a local wall clock would shift its absolute sort position.
        created: find(todo, "CREATED")
            .and_then(|p| p.value.as_deref())
            .and_then(|value| NaiveDateTime::parse_from_str(value.trim(), "%Y%m%dT%H%M%SZ").ok()),
        sort_order: find(todo, "X-APPLE-SORT-ORDER").and_then(|p| p.value.as_ref()).and_then(|v| v.trim().parse().ok()),
        raw: Some(raw.to_string()),
    };
    // Split once, here, so nothing downstream has to remember that only
    // some categories have an org tag form.
    let (mappable, rest): (Vec<_>, Vec<_>) =
        std::mem::take(&mut task.unmappable_categories).into_iter().partition(|c| super::org::is_valid_org_tag(c));
    task.categories = mappable;
    task.unmappable_categories = rest;
    Some(task)
}

/// STATUS, or -- when the server omits it, which Apple Reminders does --
/// inferred from COMPLETED/PERCENT-COMPLETE. Without that fallback a task
/// ticked off in Reminders comes back as still open and gets pushed back
/// as such.
fn status_of(todo: &IcalTodo) -> TaskStatus {
    if let Some(explicit) = text(todo, "STATUS") {
        return TaskStatus::from_ics(&explicit);
    }
    if find(todo, "COMPLETED").is_some() {
        return TaskStatus::Completed;
    }
    match text(todo, "PERCENT-COMPLETE").and_then(|p| p.trim().parse::<u8>().ok()) {
        Some(100) => TaskStatus::Completed,
        Some(p) if p > 0 => TaskStatus::InProcess,
        _ => TaskStatus::NeedsAction,
    }
}

/// The uid of this task's parent, if it has one.
///
/// RFC 5545 makes `RELTYPE` default to PARENT, so a bare `RELATED-TO:x`
/// is a parent link -- reading only explicitly tagged ones would miss the
/// subtasks of every client that relies on the default. CHILD and SIBLING
/// links are deliberately ignored: org nesting has one parent per node,
/// and inventing a second interpretation would produce a different tree
/// than the server shows.
fn parent_of(todo: &IcalTodo) -> Option<String> {
    todo.properties
        .iter()
        .filter(|p| p.name.eq_ignore_ascii_case("RELATED-TO"))
        .find(|p| match nobo_ical::param(p, "RELTYPE") {
            Some(kind) => kind.eq_ignore_ascii_case("PARENT"),
            None => true,
        })
        .and_then(|p| p.value.clone())
        .filter(|v| !v.trim().is_empty())
}

fn find<'a>(todo: &'a IcalTodo, name: &str) -> Option<&'a Property> {
    nobo_ical::property(&todo.properties, name)
}

fn text(todo: &IcalTodo, name: &str) -> Option<String> {
    nobo_ical::text(&todo.properties, name)
}

/// Re-exported where the rest of this crate already asks for it: escaping
/// a value is the format's rule, not this module's (see nobo_ical).
pub use nobo_ical::escape;

/// A day or a moment, whichever the property carries.
///
/// A zoned stamp (`TZID=Europe/Berlin`) is converted here rather than on
/// the way to the screen -- org timestamps carry no zone at all. That
/// conversion is `nobo_ical`'s, and this side did not use to do it: until
/// the two copies of this code became one crate, a task due at 09:00
/// Berlin time was written as 09:00 wherever the reader was.
fn task_date_of(property: &Property) -> Option<TaskDate> {
    match nobo_ical::when(property)? {
        When::Day(day) => Some(TaskDate::Day(day)),
        When::At(moment) => Some(TaskDate::At(moment)),
    }
}

fn date_time_of(property: &Property) -> Option<NaiveDateTime> {
    nobo_ical::when(property).map(When::naive)
}

/// Rewrites `raw` so it carries `task`'s owned properties, leaving every
/// other line of the component exactly as the server sent it.
///
/// This is the whole reason CalDavTask keeps `raw` around. A VTODO from
/// Nextcloud or Reminders routinely carries alarms, RELATED-TO links to a
/// parent task, RRULEs and per-client X-properties -- none of which this
/// app understands. Regenerating the component from the parsed fields
/// would push a version with all of that deleted, and the server would
/// accept it: a two-way sync that quietly strips other clients' data.
///
/// `None` when there is no original, i.e. a task created here; the caller
/// then builds a fresh minimal component (`render_new`).
pub fn apply_to_raw(raw: &str, task: &CalDavTask, now: DateTime<Utc>, rewrite_status: bool) -> String {
    let owns_dates = task.repeater.is_none();
    let mut out: Vec<String> = Vec::new();
    let mut inside_todo = false;
    // The component being read, twice: once as it arrived, once without
    // the properties this app owns. Which of the two is written back is
    // only known at END:VTODO -- see the RECURRENCE-ID check there.
    let mut original: Vec<String> = Vec::new();
    let mut without_owned: Vec<String> = Vec::new();
    let mut is_override = false;
    // How deep inside the VTODO we are. A VALARM is a component of its
    // own, and `is_owned` matches on the property NAME alone -- so
    // SUMMARY and DESCRIPTION were stripped out of every alarm too.
    // DESCRIPTION is REQUIRED for a DISPLAY alarm and SUMMARY+DESCRIPTION
    // for an EMAIL one, so what went back was not merely lossy but invalid
    // per RFC 5545 -- and this function's own doc promises the opposite,
    // as does docs/calendars-and-tasks.md. A nested component is passed
    // through untouched, like a recurrence override.
    let mut nested = 0usize;
    // Where in `without_owned` the first nested component starts, so the
    // properties this app owns go ABOVE it. Appending them at END:VTODO
    // put the task's own SUMMARY after a VALARM -- valid, but it reads as
    // if it belonged to the alarm, and every diff against another client's
    // copy shows the whole component as reordered.
    let mut nested_starts_at: Option<usize> = None;

    // Whole logical lines: `unfold` has already joined every continuation
    // (a line RFC 5545 folded onto the next, marked by a leading space or
    // tab) back into the property it belongs to. This loop therefore never
    // has to carry "am I inside a folded value" state -- it used to, and
    // that branch had been unreachable since `unfold` was put in front of
    // it. Folding back is the job of the join at the end.
    for line in unfold(raw) {
        let upper = line.trim().to_ascii_uppercase();
        if upper == "BEGIN:VTODO" {
            inside_todo = true;
            is_override = false;
            nested = 0;
            nested_starts_at = None;
            original.clear();
            without_owned.clear();
            out.push(line);
            continue;
        }
        if upper == "END:VTODO" {
            // A recurrence override (RFC 5545's RECURRENCE-ID) is a single
            // occurrence of a repeating task, written by whoever moved or
            // renamed that one instance. This app never reads it --
            // parse_tasks takes the master component only -- so it has
            // nothing to say about it, and writing the master's summary,
            // status and dates into it would silently overwrite what
            // another client meant by it. Passed through as it arrived.
            if is_override {
                out.extend(std::mem::take(&mut original));
            } else {
                // Everything we own goes in at the end of the VTODO's own
                // properties -- before the first nested component if there
                // is one -- so ordering stays stable across pushes even
                // for properties the original did not have at all.
                let mut body = std::mem::take(&mut without_owned);
                let at = nested_starts_at.unwrap_or(body.len()).min(body.len());
                let tail = body.split_off(at);
                out.extend(body);
                out.extend(owned_lines(task, now, rewrite_status, owns_dates));
                out.extend(tail);
            }
            original.clear();
            without_owned.clear();
            out.push(line);
            inside_todo = false;
            continue;
        }
        if !inside_todo {
            out.push(line);
            continue;
        }

        if upper.starts_with("RECURRENCE-ID") && nested == 0 {
            is_override = true;
        }

        // Inside a VALARM (or anything else nested), every line belongs to
        // that component and none of it is ours to replace.
        if upper.starts_with("BEGIN:") {
            if nested == 0 {
                nested_starts_at.get_or_insert(without_owned.len());
            }
            nested += 1;
        }
        let in_nested = nested > 0;
        if upper.starts_with("END:") && nested > 0 {
            nested -= 1;
        }

        original.push(line.clone());
        if !in_nested && is_owned(&line, rewrite_status, owns_dates) {
            continue;
        }
        without_owned.push(line);
    }

    // Folded on the way out.
    //
    // The read side calls `unfold`, so everything above works on whole
    // logical lines -- and the join put them back unfolded. A long
    // DESCRIPTION or SUMMARY therefore went to the server over the 75
    // octets RFC 5545 allows, whether it came from another client or from
    // this app (only `render_new` folded). A strict server refuses the
    // PUT, and a refused PUT used to be recorded as done.
    //
    // A second pass over already-short lines is a no-op, so the folding in
    // `owned_lines`/`render_new` can stay where it is.
    out.into_iter().flat_map(nobo_ical::fold).collect::<Vec<_>>().join("\r\n") + "\r\n"
}

/// A brand-new component, for a task this app created. Deliberately
/// minimal: anything not written here is the server's to add.
pub fn render_new(task: &CalDavTask, now: DateTime<Utc>) -> String {
    let mut lines = vec![
        "BEGIN:VCALENDAR".to_string(),
        "VERSION:2.0".to_string(),
        "PRODID:-//nobo//org-mode task sync//EN".to_string(),
        "BEGIN:VTODO".to_string(),
        format!("UID:{}", task.uid),
        format!("CREATED:{}", stamp(now)),
    ];
    // Written here and only here: an explicit order is what keeps the new
    // task where the user put it. Without one every client falls back to
    // CREATED, which answers a different question.
    if let Some(value) = task.sort_order {
        lines.push(format!("X-APPLE-SORT-ORDER:{value}"));
    }
    lines.extend(owned_lines(task, now, true, true));
    lines.push("END:VTODO".to_string());
    lines.push("END:VCALENDAR".to_string());
    lines.into_iter().flat_map(fold).collect::<Vec<_>>().join("\r\n") + "\r\n"
}

fn owned_lines(task: &CalDavTask, now: DateTime<Utc>, rewrite_status: bool, owns_dates: bool) -> Vec<String> {
    let mut lines =
        vec![format!("SUMMARY:{}", escape(&task.summary)), format!("DTSTAMP:{}", stamp(now)), format!("LAST-MODIFIED:{}", stamp(now))];
    if rewrite_status {
        lines.push(format!("STATUS:{}", task.status.as_ics()));
    }

    if let Some(description) = task.description.as_ref().filter(|d| !d.trim().is_empty()) {
        lines.push(format!("DESCRIPTION:{}", escape(description)));
    }
    if owns_dates {
        if let Some(start) = task.start {
            lines.push(format!("DTSTART{}", date_value(start)));
        }
        if let Some(due) = task.due {
            lines.push(format!("DUE{}", date_value(due)));
        }
    }
    // Re-parenting has to reach the server, or dragging a subtask under a
    // different task would be a local-only illusion.
    if let Some(parent) = &task.parent_uid {
        lines.push(format!("RELATED-TO;RELTYPE=PARENT:{}", escape(parent)));
    }
    // Both lists, always: writing CATEGORIES from the org tags alone would
    // delete every category that has no org tag form (see
    // CalDavTask::unmappable_categories).
    let all_categories: Vec<_> = task.categories.iter().chain(task.unmappable_categories.iter()).collect();
    if !all_categories.is_empty() {
        let joined = all_categories.iter().map(|c| escape(c)).collect::<Vec<_>>().join(",");
        lines.push(format!("CATEGORIES:{joined}"));
    }
    // Both, not just STATUS: Nextcloud's list shows the checkbox from
    // PERCENT-COMPLETE, and a COMPLETED status without it reads as done in
    // one client and open in the other.
    if rewrite_status && task.status.is_finished() {
        lines.push(format!("COMPLETED:{}", stamp(task.completed_at.map(to_utc).unwrap_or(now))));
        lines.push("PERCENT-COMPLETE:100".to_string());
    }

    lines.into_iter().flat_map(fold).collect()
}

fn is_owned(line: &str, rewrite_status: bool, owns_dates: bool) -> bool {
    let name = line.split([':', ';']).next().unwrap_or("").trim();
    if OWNED_PROPERTIES.iter().any(|p| p.eq_ignore_ascii_case(name)) {
        return true;
    }
    if rewrite_status && STATUS_PROPERTIES.iter().any(|p| p.eq_ignore_ascii_case(name)) {
        return true;
    }
    if owns_dates && DATE_PROPERTIES.iter().any(|p| p.eq_ignore_ascii_case(name)) {
        return true;
    }
    // Only the PARENT link, never a CHILD or SIBLING one: taking those
    // over as well would delete relationships this app has no way to
    // reproduce.
    name.eq_ignore_ascii_case("RELATED-TO") && is_parent_link(line)
}

fn is_parent_link(line: &str) -> bool {
    let params = line.split(':').next().unwrap_or("");
    match params.split(';').find(|p| p.to_ascii_uppercase().starts_with("RELTYPE=")) {
        Some(reltype) => reltype.to_ascii_uppercase().ends_with("PARENT"),
        None => true,
    }
}

fn date_value(date: TaskDate) -> String {
    match date {
        TaskDate::Day(d) => format!(";VALUE=DATE:{}", d.format("%Y%m%d")),
        TaskDate::At(dt) => format!(":{}", stamp(to_utc(dt))),
    }
}

fn to_utc(naive: NaiveDateTime) -> DateTime<Utc> {
    Local.from_local_datetime(&naive).earliest().map(|dt| dt.with_timezone(&Utc)).unwrap_or_else(|| Utc.from_utc_datetime(&naive))
}

fn stamp(dt: DateTime<Utc>) -> String {
    dt.format("%Y%m%dT%H%M%SZ").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 23, 10, 34, 23).unwrap()
    }

    /// A realistic Nextcloud VTODO: several properties this app does not
    /// understand, which a push must not touch.
    const NEXTCLOUD: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Nextcloud Tasks//EN\r\n\
BEGIN:VTODO\r\nUID:abc-123\r\nCREATED:20260801T090000Z\r\nSUMMARY:Milch kaufen\r\n\
DESCRIPTION:Bio\\, wenn da\r\nSTATUS:NEEDS-ACTION\r\nDUE;VALUE=DATE:20260825\r\n\
CATEGORIES:Einkauf,Privat\r\nRELATED-TO;RELTYPE=PARENT:parent-uid\r\n\
X-APPLE-SORT-ORDER:12\r\nBEGIN:VALARM\r\nTRIGGER:-PT15M\r\nACTION:DISPLAY\r\n\
DESCRIPTION:Nicht vergessen\r\nSUMMARY:Erinnerung\r\nEND:VALARM\r\n\
END:VTODO\r\nEND:VCALENDAR\r\n";

    fn parse_one(ics: &str) -> CalDavTask {
        let tasks = parse_tasks(ics).unwrap();
        assert_eq!(tasks.len(), 1, "{tasks:?}");
        tasks.into_iter().next().unwrap()
    }

    /* --- reading --- */

    #[test]
    fn reads_the_fields_the_app_maps_to_org() {
        let task = parse_one(NEXTCLOUD);

        assert_eq!(task.uid, "abc-123");
        assert_eq!(task.summary, "Milch kaufen");
        assert_eq!(task.description.as_deref(), Some("Bio, wenn da"));
        assert_eq!(task.status, TaskStatus::NeedsAction);
        assert_eq!(task.due, Some(TaskDate::Day(NaiveDate::from_ymd_opt(2026, 8, 25).unwrap())));
        assert_eq!(task.categories, vec!["Einkauf", "Privat"]);
        assert!(task.unmappable_categories.is_empty());
        assert_eq!(task.sort_order, Some(12));
    }

    /// A server that says which zone it means is believed -- a task due
    /// at 09:00 Berlin time is 09:00 in Berlin, not 09:00 wherever the
    /// reader sits. This side used to ignore `TZID` altogether (the
    /// calendar side never did); both now go through nobo_ical::when.
    #[test]
    fn a_due_date_with_a_zone_is_converted_to_local_time() {
        let task = parse_one(
            "BEGIN:VCALENDAR\r\nBEGIN:VTODO\r\nUID:zoned\r\nSUMMARY:Anruf\r\n\
DUE;TZID=Europe/Berlin:20260825T090000\r\nEND:VTODO\r\nEND:VCALENDAR\r\n",
        );

        let expected = chrono_tz::Europe::Berlin.with_ymd_and_hms(2026, 8, 25, 9, 0, 0).unwrap().with_timezone(&Local).naive_local();
        assert_eq!(task.due, Some(TaskDate::At(expected)));
    }

    /// Apple Reminders omits STATUS entirely -- without the fallback a
    /// task ticked off there comes back open and gets pushed back as open.
    #[test]
    fn a_missing_status_is_inferred_rather_than_assumed_open() {
        let completed =
            parse_one("BEGIN:VCALENDAR\r\nBEGIN:VTODO\r\nUID:a\r\nCOMPLETED:20260820T100000Z\r\nEND:VTODO\r\nEND:VCALENDAR\r\n");
        assert_eq!(completed.status, TaskStatus::Completed);

        let started = parse_one("BEGIN:VCALENDAR\r\nBEGIN:VTODO\r\nUID:b\r\nPERCENT-COMPLETE:40\r\nEND:VTODO\r\nEND:VCALENDAR\r\n");
        assert_eq!(started.status, TaskStatus::InProcess);

        let full = parse_one("BEGIN:VCALENDAR\r\nBEGIN:VTODO\r\nUID:c\r\nPERCENT-COMPLETE:100\r\nEND:VTODO\r\nEND:VCALENDAR\r\n");
        assert_eq!(full.status, TaskStatus::Completed);
    }

    #[test]
    fn a_utc_timestamp_is_converted_to_local_wall_clock() {
        let task = parse_one("BEGIN:VCALENDAR\r\nBEGIN:VTODO\r\nUID:a\r\nDUE:20260825T120000Z\r\nEND:VTODO\r\nEND:VCALENDAR\r\n");

        let expected = Utc.with_ymd_and_hms(2026, 8, 25, 12, 0, 0).unwrap().with_timezone(&Local).naive_local();
        assert_eq!(task.due, Some(TaskDate::At(expected)));
    }

    /// A category with a space has no org tag form. Writing CATEGORIES
    /// back from the org tags alone would delete it from the server -- a
    /// two-way sync quietly destroying data it merely could not display.
    #[test]
    fn a_category_with_no_org_tag_form_survives_a_push() {
        let ics =
            "BEGIN:VCALENDAR\r\nBEGIN:VTODO\r\nUID:a\r\nSUMMARY:X\r\nCATEGORIES:Haus\\, Garten,Arbeit\r\nEND:VTODO\r\nEND:VCALENDAR\r\n";
        let task = parse_one(ics);
        assert_eq!(task.categories, vec!["Arbeit"], "only this one has a tag form");
        assert_eq!(task.unmappable_categories, vec!["Haus, Garten"]);

        let pushed = apply_to_raw(task.raw.as_ref().unwrap(), &task, now(), true);

        let reparsed = parse_one(&pushed);
        assert_eq!(reparsed.categories, vec!["Arbeit"]);
        assert_eq!(reparsed.unmappable_categories, vec!["Haus, Garten"], "{pushed}");
    }

    #[test]
    fn an_escaped_comma_stays_inside_one_category() {
        let task =
            parse_one("BEGIN:VCALENDAR\r\nBEGIN:VTODO\r\nUID:a\r\nCATEGORIES:Haus\\, Garten,Arbeit\r\nEND:VTODO\r\nEND:VCALENDAR\r\n");

        assert_eq!(task.unmappable_categories, vec!["Haus, Garten"]);
        assert_eq!(task.categories, vec!["Arbeit"]);
    }

    /// A recurrence override is a second VTODO with the same UID. Showing
    /// it would duplicate the task in the list.
    #[test]
    fn a_recurrence_override_is_not_a_second_task() {
        let ics = "BEGIN:VCALENDAR\r\nBEGIN:VTODO\r\nUID:a\r\nSUMMARY:Master\r\nEND:VTODO\r\n\
BEGIN:VTODO\r\nUID:a\r\nRECURRENCE-ID:20260901T090000Z\r\nSUMMARY:Override\r\nEND:VTODO\r\nEND:VCALENDAR\r\n";

        let tasks = parse_tasks(ics).unwrap();

        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].summary, "Master");
    }

    /* --- subtasks and recurrence --- */

    /// RFC 5545 makes RELTYPE default to PARENT, so a bare RELATED-TO is a
    /// parent link -- reading only the tagged ones would miss the subtasks
    /// of every client that relies on the default.
    #[test]
    fn a_parent_link_is_read_with_or_without_an_explicit_reltype() {
        let tagged = parse_one("BEGIN:VCALENDAR\r\nBEGIN:VTODO\r\nUID:a\r\nRELATED-TO;RELTYPE=PARENT:p\r\nEND:VTODO\r\nEND:VCALENDAR\r\n");
        assert_eq!(tagged.parent_uid.as_deref(), Some("p"));

        let bare = parse_one("BEGIN:VCALENDAR\r\nBEGIN:VTODO\r\nUID:a\r\nRELATED-TO:p\r\nEND:VTODO\r\nEND:VCALENDAR\r\n");
        assert_eq!(bare.parent_uid.as_deref(), Some("p"));
    }

    /// A CHILD or SIBLING link is a different relationship. Reading one as
    /// a parent would build a different tree than the server shows.
    /// An anonymized Nextcloud regression: only the task that was dragged
    /// carries X-APPLE-SORT-ORDER, every other one has nothing but
    /// CREATED. Both clients that show this list order it by the explicit
    /// value where there is one and by CREATED otherwise -- on the same
    /// scale, seconds since 2001-01-01.
    #[test]
    fn the_sort_key_falls_back_to_created_because_only_dragging_writes_the_property() {
        let dragged = parse_one(
            "BEGIN:VCALENDAR\r\nBEGIN:VTODO\r\nUID:a\r\nSUMMARY:Dragged task\r\n\
CREATED:20260823T160042Z\r\nX-APPLE-SORT-ORDER:809193116\r\nEND:VTODO\r\nEND:VCALENDAR\r\n",
        );
        let untouched = parse_one(
            "BEGIN:VCALENDAR\r\nBEGIN:VTODO\r\nUID:b\r\nSUMMARY:Untouched task\r\n\
CREATED:20260823T155155Z\r\nEND:VTODO\r\nEND:VCALENDAR\r\n",
        );

        // The dragged one keeps the value it was given...
        assert_eq!(dragged.sort_key(), Some(809193116));
        // ...which is EARLIER than its own creation time (809193642) --
        // that is what moving it up the list did.
        assert_eq!(untouched.sort_key(), Some(809193115));
        assert!(untouched.sort_key() < dragged.sort_key(), "the dragged task sorts just after it");
    }

    #[test]
    fn creation_time_is_kept_in_utc_even_across_a_local_dst_fold() {
        for value in ["20260823T155155Z", "20261025T003000Z", "20261025T013000Z"] {
            let raw = format!("BEGIN:VCALENDAR\r\nBEGIN:VTODO\r\nUID:a\r\nCREATED:{value}\r\nEND:VTODO\r\nEND:VCALENDAR\r\n");
            let task = parse_one(&raw);
            let utc = NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%SZ").unwrap();
            assert_eq!(task.created, Some(utc));
            assert_eq!(task.sort_key(), Some(utc.and_utc().timestamp() - 978_307_200));
        }
    }

    #[test]
    fn a_task_with_neither_has_no_position_of_its_own() {
        let task = parse_one("BEGIN:VCALENDAR\r\nBEGIN:VTODO\r\nUID:a\r\nSUMMARY:X\r\nEND:VTODO\r\nEND:VCALENDAR\r\n");

        assert_eq!(task.sort_key(), None);
    }

    #[test]
    fn a_child_or_sibling_link_is_not_a_parent() {
        let task = parse_one("BEGIN:VCALENDAR\r\nBEGIN:VTODO\r\nUID:a\r\nRELATED-TO;RELTYPE=CHILD:c\r\nEND:VTODO\r\nEND:VCALENDAR\r\n");

        assert_eq!(task.parent_uid, None);
    }

    /// ...and must survive a push untouched, since this app has no way to
    /// reproduce it.
    #[test]
    fn a_push_rewrites_only_the_parent_link_not_other_relations() {
        let ics = "BEGIN:VCALENDAR\r\nBEGIN:VTODO\r\nUID:a\r\nSUMMARY:X\r\n\
RELATED-TO;RELTYPE=PARENT:alt\r\nRELATED-TO;RELTYPE=SIBLING:geschwister\r\nEND:VTODO\r\nEND:VCALENDAR\r\n";
        let mut task = parse_one(ics);
        task.parent_uid = Some("neu".to_string());

        let pushed = apply_to_raw(task.raw.as_ref().unwrap(), &task, now(), false);

        assert!(pushed.contains("RELATED-TO;RELTYPE=PARENT:neu"), "{pushed}");
        assert!(!pushed.contains("PARENT:alt"), "{pushed}");
        assert!(pushed.contains("RELATED-TO;RELTYPE=SIBLING:geschwister"), "{pushed}");
    }

    #[test]
    fn removing_the_parent_removes_the_link() {
        let ics = "BEGIN:VCALENDAR\r\nBEGIN:VTODO\r\nUID:a\r\nSUMMARY:X\r\nRELATED-TO;RELTYPE=PARENT:p\r\nEND:VTODO\r\nEND:VCALENDAR\r\n";
        let mut task = parse_one(ics);
        task.parent_uid = None;

        let pushed = apply_to_raw(task.raw.as_ref().unwrap(), &task, now(), false);

        assert!(!pushed.contains("RELATED-TO"), "{pushed}");
    }

    #[test]
    fn an_rrule_is_read_as_the_org_repeater_it_corresponds_to() {
        let task = parse_one("BEGIN:VCALENDAR\r\nBEGIN:VTODO\r\nUID:a\r\nRRULE:FREQ=WEEKLY;INTERVAL=2\r\nEND:VTODO\r\nEND:VCALENDAR\r\n");

        let repeater = task.repeater.unwrap();
        assert_eq!(repeater.value, 2);
        assert_eq!(repeater.unit, 'w');
    }

    /// The recurrence belongs to the server: it decides when the next
    /// occurrence is due. Writing a date over that would fight the rule --
    /// and lose differently on every server.
    #[test]
    fn a_recurring_task_keeps_the_servers_dates_and_rule_on_a_push() {
        let ics = "BEGIN:VCALENDAR\r\nBEGIN:VTODO\r\nUID:a\r\nSUMMARY:Müll\r\n\
DUE;VALUE=DATE:20260825\r\nRRULE:FREQ=WEEKLY\r\nEND:VTODO\r\nEND:VCALENDAR\r\n";
        let mut task = parse_one(ics);
        task.summary = "Müll rausbringen".to_string();
        task.due = Some(TaskDate::Day(NaiveDate::from_ymd_opt(2030, 1, 1).unwrap()));

        let pushed = apply_to_raw(task.raw.as_ref().unwrap(), &task, now(), false);

        assert!(pushed.contains("SUMMARY:Müll rausbringen"), "{pushed}");
        assert!(pushed.contains("DUE;VALUE=DATE:20260825"), "the server's date must stand: {pushed}");
        assert!(!pushed.contains("20300101"), "{pushed}");
        assert!(pushed.contains("RRULE:FREQ=WEEKLY"), "{pushed}");
    }

    /// A task that does NOT recur is the ordinary case, and its dates are
    /// the app's to change.
    #[test]
    fn a_one_off_tasks_dates_are_still_written() {
        let mut task = parse_one(NEXTCLOUD);
        task.due = Some(TaskDate::Day(NaiveDate::from_ymd_opt(2030, 1, 1).unwrap()));

        let pushed = apply_to_raw(task.raw.as_ref().unwrap(), &task, now(), false);

        assert!(pushed.contains("DUE;VALUE=DATE:20300101"), "{pushed}");
    }

    /* --- writing: patch, never regenerate --- */

    /// The single most destructive thing this module could do: push a
    /// component rebuilt from the fields it understands, silently deleting
    /// the alarm, the parent link and the other client's sort order.
    #[test]
    fn a_push_keeps_every_property_the_app_does_not_own() {
        let mut task = parse_one(NEXTCLOUD);
        task.summary = "Hafermilch kaufen".to_string();

        let pushed = apply_to_raw(task.raw.as_ref().unwrap(), &task, now(), true);

        assert!(pushed.contains("RELATED-TO;RELTYPE=PARENT:parent-uid"), "{pushed}");
        assert!(pushed.contains("BEGIN:VALARM"), "{pushed}");
        assert!(pushed.contains("TRIGGER:-PT15M"), "{pushed}");
        assert!(pushed.contains("X-APPLE-SORT-ORDER:12"), "{pushed}");
        assert!(pushed.contains("CREATED:20260801T090000Z"), "{pushed}");
        assert!(pushed.contains("UID:abc-123"), "{pushed}");
    }

    /// An alarm's own SUMMARY and DESCRIPTION are not this app's to
    /// replace.
    ///
    /// `is_owned` matches on the property name alone, and both names are
    /// in `OWNED_PROPERTIES` -- so every push stripped them out of every
    /// VALARM in the component. DESCRIPTION is REQUIRED for a DISPLAY
    /// alarm and SUMMARY+DESCRIPTION for an EMAIL one, so what went back
    /// was invalid per RFC 5545, not merely reduced.
    #[test]
    fn a_push_leaves_an_alarms_own_summary_and_description_alone() {
        let mut task = parse_one(NEXTCLOUD);
        task.summary = "Hafermilch kaufen".to_string();

        let pushed = apply_to_raw(task.raw.as_ref().unwrap(), &task, now(), true);

        assert!(pushed.contains("DESCRIPTION:Nicht vergessen"), "the alarm keeps its own: {pushed}");
        assert!(pushed.contains("SUMMARY:Erinnerung"), "{pushed}");
        // And the task's own are still replaced exactly once.
        assert_eq!(pushed.matches("SUMMARY:Hafermilch kaufen").count(), 1, "{pushed}");
        assert!(!pushed.contains("SUMMARY:Milch kaufen"), "{pushed}");
    }

    #[test]
    fn a_push_replaces_the_properties_it_does_own_exactly_once() {
        let mut task = parse_one(NEXTCLOUD);
        task.summary = "Hafermilch kaufen".to_string();
        task.status = TaskStatus::Completed;

        let pushed = apply_to_raw(task.raw.as_ref().unwrap(), &task, now(), true);

        // Counted on the VTODO's own level: the fixture's VALARM carries a
        // SUMMARY of its own now, and that one is not this app's.
        let vtodo_level = pushed.split("BEGIN:VALARM").next().unwrap();
        assert_eq!(vtodo_level.matches("SUMMARY:").count(), 1, "{pushed}");
        assert!(pushed.contains("SUMMARY:Hafermilch kaufen"), "{pushed}");
        assert!(!pushed.contains("SUMMARY:Milch kaufen"), "stale value left behind: {pushed}");
        assert_eq!(pushed.matches("STATUS:").count(), 1, "{pushed}");
        assert!(pushed.contains("STATUS:COMPLETED"), "{pushed}");
    }

    /// Nextcloud draws its checkbox from PERCENT-COMPLETE, so a COMPLETED
    /// status without it shows as done in one client and open in the other.
    #[test]
    fn completing_a_task_writes_both_markers_the_clients_read() {
        let mut task = parse_one(NEXTCLOUD);
        task.status = TaskStatus::Completed;
        task.completed_at = Some(Utc.with_ymd_and_hms(2026, 8, 21, 10, 34, 23).unwrap().with_timezone(&Local).naive_local());

        let pushed = apply_to_raw(task.raw.as_ref().unwrap(), &task, now(), true);

        assert!(pushed.contains("COMPLETED:20260821T103423Z"), "{pushed}");
        assert!(pushed.contains("PERCENT-COMPLETE:100"), "{pushed}");
    }

    /// Reopening one has to remove them again, or the task stays ticked in
    /// Nextcloud while showing TODO here.
    #[test]
    fn reopening_a_task_removes_the_completion_markers() {
        let done = "BEGIN:VCALENDAR\r\nBEGIN:VTODO\r\nUID:a\r\nSUMMARY:X\r\nSTATUS:COMPLETED\r\n\
COMPLETED:20260820T100000Z\r\nPERCENT-COMPLETE:100\r\nEND:VTODO\r\nEND:VCALENDAR\r\n";
        let mut task = parse_one(done);
        task.status = TaskStatus::NeedsAction;
        task.completed_at = None;

        let pushed = apply_to_raw(task.raw.as_ref().unwrap(), &task, now(), true);

        assert!(!pushed.contains("COMPLETED:"), "{pushed}");
        assert!(!pushed.contains("PERCENT-COMPLETE"), "{pushed}");
        assert!(pushed.contains("STATUS:NEEDS-ACTION"), "{pushed}");
    }

    /// The reason STATUS is only conditionally owned: a task another
    /// client set to IN-PROCESS reads as "not done" here, and a push that
    /// rewrote STATUS anyway would flatten it to NEEDS-ACTION -- silently
    /// undoing the other client's state on every unrelated edit.
    #[test]
    fn editing_a_task_without_toggling_it_leaves_a_finer_remote_state_alone() {
        let started = "BEGIN:VCALENDAR\r\nBEGIN:VTODO\r\nUID:a\r\nSUMMARY:X\r\nSTATUS:IN-PROCESS\r\n\
PERCENT-COMPLETE:40\r\nEND:VTODO\r\nEND:VCALENDAR\r\n";
        let mut task = parse_one(started);
        assert_eq!(task.status, TaskStatus::InProcess);
        task.summary = "Umbenannt".to_string();

        let pushed = apply_to_raw(task.raw.as_ref().unwrap(), &task, now(), false);

        assert!(pushed.contains("SUMMARY:Umbenannt"), "{pushed}");
        assert!(pushed.contains("STATUS:IN-PROCESS"), "{pushed}");
        assert!(pushed.contains("PERCENT-COMPLETE:40"), "{pushed}");
    }

    #[test]
    fn ticking_a_task_off_does_rewrite_the_status() {
        let started = "BEGIN:VCALENDAR\r\nBEGIN:VTODO\r\nUID:a\r\nSUMMARY:X\r\nSTATUS:IN-PROCESS\r\n\
PERCENT-COMPLETE:40\r\nEND:VTODO\r\nEND:VCALENDAR\r\n";
        let mut task = parse_one(started);
        task.status = TaskStatus::Completed;

        let pushed = apply_to_raw(task.raw.as_ref().unwrap(), &task, now(), true);

        assert!(pushed.contains("STATUS:COMPLETED"), "{pushed}");
        assert!(pushed.contains("PERCENT-COMPLETE:100"), "{pushed}");
        assert!(!pushed.contains("PERCENT-COMPLETE:40"), "{pushed}");
    }

    #[test]
    fn clearing_a_due_date_drops_the_property_instead_of_blanking_it() {
        let mut task = parse_one(NEXTCLOUD);
        task.due = None;

        let pushed = apply_to_raw(task.raw.as_ref().unwrap(), &task, now(), true);

        assert!(!pushed.contains("DUE"), "{pushed}");
    }

    /// A folded DESCRIPTION spans several physical lines; replacing only
    /// the first would leave its continuation behind as a stray line.
    #[test]
    fn replacing_a_folded_property_removes_its_continuation_lines_too() {
        let folded = "BEGIN:VCALENDAR\r\nBEGIN:VTODO\r\nUID:a\r\nSUMMARY:X\r\n\
DESCRIPTION:erste Zeile die sehr lang ist und deshalb\r\n  umgebrochen wurde\r\n\
X-EIGENES-FELD:keep-me\r\nEND:VTODO\r\nEND:VCALENDAR\r\n";
        let mut task = parse_one(folded);
        task.description = Some("kurz".to_string());

        let pushed = apply_to_raw(task.raw.as_ref().unwrap(), &task, now(), true);

        assert!(!pushed.contains("umgebrochen wurde"), "orphaned continuation: {pushed}");
        assert!(pushed.contains("DESCRIPTION:kurz"), "{pushed}");
        assert!(pushed.contains("X-EIGENES-FELD:keep-me"), "{pushed}");
    }

    #[test]
    fn a_long_line_is_folded_so_the_server_accepts_it() {
        let mut task = parse_one(NEXTCLOUD);
        task.description = Some("x".repeat(300));

        let pushed = apply_to_raw(task.raw.as_ref().unwrap(), &task, now(), true);

        assert!(pushed.split("\r\n").all(|l| l.len() <= 75), "unfolded line: {pushed}");
    }

    #[test]
    fn folding_never_splits_a_multibyte_character() {
        let mut task = parse_one(NEXTCLOUD);
        task.description = Some("üäö".repeat(60));

        let pushed = apply_to_raw(task.raw.as_ref().unwrap(), &task, now(), true);

        // Re-reading it is the real check: a byte-wise cut yields something
        // no parser accepts.
        let reparsed = parse_one(&pushed);
        assert_eq!(reparsed.description.as_deref(), Some("üäö".repeat(60).as_str()));
    }

    #[test]
    fn a_locally_created_task_becomes_a_complete_component() {
        let task = CalDavTask {
            uid: "new-1".to_string(),
            summary: "Neue Aufgabe".to_string(),
            description: None,
            status: TaskStatus::NeedsAction,
            completed_at: None,
            start: None,
            due: Some(TaskDate::Day(NaiveDate::from_ymd_opt(2026, 9, 1).unwrap())),
            categories: vec!["Arbeit".to_string()],
            unmappable_categories: Vec::new(),
            parent_uid: None,
            repeater: None,
            created: None,
            sort_order: None,
            raw: None,
        };

        let rendered = render_new(&task, now());

        let reparsed = parse_one(&rendered);
        assert_eq!(reparsed.uid, "new-1");
        assert_eq!(reparsed.summary, "Neue Aufgabe");
        assert_eq!(reparsed.due, task.due);
        assert_eq!(reparsed.categories, vec!["Arbeit"]);
    }

    #[test]
    fn a_full_round_trip_through_a_push_changes_nothing_it_should_not() {
        let task = parse_one(NEXTCLOUD);

        let pushed = apply_to_raw(task.raw.as_ref().unwrap(), &task, now(), true);
        let reparsed = parse_one(&pushed);

        assert_eq!(reparsed.uid, task.uid);
        assert_eq!(reparsed.summary, task.summary);
        assert_eq!(reparsed.description, task.description);
        assert_eq!(reparsed.status, task.status);
        assert_eq!(reparsed.due, task.due);
        assert_eq!(reparsed.categories, task.categories);
        assert_eq!(reparsed.sort_order, task.sort_order);
    }

    /// A repeating task whose Thursday was moved arrives as two
    /// components: the master, and an override carrying RECURRENCE-ID.
    /// The app reads only the master (see parse_tasks) -- so writing the
    /// master's summary and status into the override would overwrite what
    /// another client meant by that one occurrence, with values it never
    /// looked at.
    #[test]
    fn a_recurrence_override_is_passed_through_untouched() {
        let raw = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\n\
BEGIN:VTODO\r\nUID:abc\r\nSUMMARY:Müll rausbringen\r\nSTATUS:NEEDS-ACTION\r\n\
RRULE:FREQ=WEEKLY\r\nEND:VTODO\r\n\
BEGIN:VTODO\r\nUID:abc\r\nRECURRENCE-ID:20260903T080000\r\n\
SUMMARY:Müll -- diese Woche Papier\r\nSTATUS:NEEDS-ACTION\r\nEND:VTODO\r\n\
END:VCALENDAR\r\n";

        // The master as the app read it, with the summary edited here.
        let mut task = parse_one(raw);
        task.summary = "Müll rausbringen (neu)".into();
        let out = apply_to_raw(raw, &task, now(), true);

        assert!(out.contains("SUMMARY:Müll -- diese Woche Papier"), "the override lost its own summary:\n{out}");
        assert!(out.contains("SUMMARY:Müll rausbringen (neu)"), "the master was not updated:\n{out}");
        // ... and exactly once each: the override must not have collected
        // a second SUMMARY from the master.
        assert_eq!(out.matches("SUMMARY:").count(), 2, "{out}");
        assert_eq!(out.matches("DTSTAMP:").count(), 1, "only the master is stamped:\n{out}");
        assert!(out.contains("RECURRENCE-ID:20260903T080000"), "{out}");
    }
}
