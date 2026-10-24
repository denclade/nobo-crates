// SPDX-FileCopyrightText: 2026 Dennis Burgermeister
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::core::heading_identity;
use crate::core::models::{
    is_real_structure, remove_property, set_property, state_log_line, Heading, HeadingCreatePayload, HeadingUpdatePayload, ProgressStats,
    DATE_FORMAT_ACTIVITY, DATE_FORMAT_CLOSED, DATE_FORMAT_LOG, ORG_CATEGORY_PROPERTY, ORG_LOGBOOK_END, ORG_LOGBOOK_START,
    ORG_PROPERTIES_END, ORG_PROPERTIES_START, ORG_TAGS_COLUMN, ORG_TAGS_MIN_GAP,
};
use crate::core::org_parser;
use crate::core::task_utils;
use crate::utils::effort_utils::ORG_EFFORT_PROPERTY;
use crate::utils::string_utils::count_leading_stars;
use crate::utils::{effort_utils, fs_utils, timestamp_utils};
use chrono::Local;
use std::path::Path;

/// Property key stamped with the current time whenever a heading is
/// *directly* edited (its own state/title/tags/content/properties change --
/// see build_org_heading_block/build_new_heading_block).
const ORG_UPDATED_PROPERTY: &str = "UPDATED";

/// Property key stamped with the current time on the heading itself AND
/// every one of its ancestors (see touch_last_activity/touch_ancestors)
/// whenever anything changes anywhere in its subtree.
const ORG_LAST_ACTIVITY_PROPERTY: &str = "LAST_ACTIVITY";

fn pad_tags_to_column(prefix: &str, tags_str: &str) -> String {
    let prefix_len = prefix.chars().count();
    let tags_len = tags_str.chars().count();

    let available = ORG_TAGS_COLUMN.saturating_sub(prefix_len + tags_len);
    let gap = available.max(ORG_TAGS_MIN_GAP);

    format!("{}{}{}", prefix, " ".repeat(gap), tags_str)
}

pub fn format_new_heading_line(
    level: usize,
    state: &Option<String>,
    priority: Option<&str>,
    title: &str,
    tags: &[String],
    progress_stats: Option<ProgressStats>,
) -> String {
    let stars = "*".repeat(level);
    let state_part = state.as_deref().map(|s| format!("{} ", s)).unwrap_or_default();
    // Between the keyword and the title, which is where org puts it.
    let priority_part = priority.map(|p| format!("[#{p}] ")).unwrap_or_default();
    let mut prefix = format!("{} {}{}{}", stars, state_part, priority_part, title);
    if let Some(stats) = progress_stats {
        prefix.push_str(&format!(" [{}%]", stats.progress()));
    }
    if tags.is_empty() {
        return prefix;
    }

    let tags_str = format!(":{}:", tags.join(":"));
    pad_tags_to_column(&prefix, &tags_str)
}

/// Creates heading block for a completly new heading from PAYLOAD.
/// Unlike build_org_heading_block theres no previous state to handle.
///
/// LEVEL is passed separately because only the caller can know it: it is
/// derived from the parent the write actually resolved to, not from
/// anything the payload carries (see create_heading_in_file).
pub fn build_new_heading_block(payload: &HeadingCreatePayload, level: usize) -> Vec<String> {
    let mut lines = vec![format_new_heading_line(
        level,
        &payload.state,
        // Nothing creates a heading with a priority yet; org's own default
        // is none either.
        None,
        &payload.title,
        &payload.tags,
        None,
    )];

    if let Some(ts) = &payload.scheduled {
        lines.push(format!("SCHEDULED: {}", timestamp_utils::format_timestamp(ts)));
    }

    let now = Local::now().format(DATE_FORMAT_ACTIVITY).to_string();
    lines.push(ORG_PROPERTIES_START.to_string());
    // Always fresh, no consent needed -- a brand-new heading has nothing
    // pre-existing to ask about (unlike an EXISTING heading being edited,
    // see build_org_heading_block_inner's own set_custom_id parameter).
    lines.push(format!(":{}: {}", heading_identity::CUSTOM_ID_PROPERTY, heading_identity::generate_custom_id()));
    lines.push(format!(":{}: {}", ORG_UPDATED_PROPERTY, now));
    lines.push(format!(":{}: {}", ORG_LAST_ACTIVITY_PROPERTY, now));
    if let Some(minutes) = payload.effort_minutes {
        lines.push(format!(":{}: {}", ORG_EFFORT_PROPERTY, effort_utils::format_effort(minutes)));
    }
    if let Some(category) = payload.category.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
        lines.push(format!(":{}: {}", ORG_CATEGORY_PROPERTY, category));
    }
    lines.push(ORG_PROPERTIES_END.to_string());

    if let Some(content) = &payload.content {
        if !content.trim().is_empty() {
            // Exactly the same demotion the edit path applies (see
            // build_org_heading_block_inner) -- a description is always
            // the SHORT typed form, and writing it verbatim let a typed
            // "* TODO Schritt" land as a real level-1 heading: the new
            // task lost everything from that line on, and every heading
            // after the insertion point silently re-parented under it.
            lines.extend(with_ids_on_new_tasks(apply_content_heading_offset(content, level)));
        }
    }

    lines
}

/// Rewrites HEADING's own block completely unchanged. Does NOT bump
/// UPDATED or LAST_ACTIVITY. Used for reconstructing a file during
/// an AST-based sync merge where the timestamps are already correctly
/// merged and must not be overwritten.
pub fn build_org_heading_block_unaltered(heading: &Heading) -> Vec<String> {
    build_org_heading_block_inner(
        heading,
        &HeadingUpdatePayload::default(),
        false, // touch_updated = false
        false, // touch_activity = false
        false, // set_custom_id = false
    )
}

/// Rewrites a heading's block with `payload` applied -- the direct-edit
/// path, which is why it stamps both UPDATED and LAST_ACTIVITY.
///
/// Never adds a missing CUSTOM_ID: doing so on any edit bypasses the
/// one-time backfill's ask-first consent. New headings get one for free;
/// an existing one only via `stamp_custom_id_only`.
pub fn build_org_heading_block(heading: &Heading, payload: &HeadingUpdatePayload) -> Vec<String> {
    build_org_heading_block_inner(heading, payload, true, true, false)
}

/// Rewrites HEADING's own block completely unchanged except for a fresh
/// LAST_ACTIVITY stamp -- the ancestor-propagation path (see
/// touch_ancestors): a descendant changing is activity for every enclosing
/// heading, but not a direct edit of THEM, so their own UPDATED must stay
/// exactly as it was. Same "no opportunistic CUSTOM_ID" reasoning as
/// build_org_heading_block above.
pub fn touch_last_activity(heading: &Heading) -> Vec<String> {
    build_org_heading_block_inner(heading, &HeadingUpdatePayload::default(), false, true, false)
}

/// Rewrites a heading's block unchanged except for a newly added
/// CUSTOM_ID -- the consented backfill path for existing headings.
pub fn stamp_custom_id_only(heading: &Heading) -> Vec<String> {
    build_org_heading_block_inner(heading, &HeadingUpdatePayload::default(), false, false, true)
}

/// Locates the 0-indexed line within HEADING's own `line_no..=last_line`
/// range in LINES holding its `:CUSTOM_ID:` property -- inverse lookup of
/// stamp_custom_id_only above, used by commands_headings::remove_custom_ids
/// to delete exactly that one line and nothing else in the drawer. `None`
/// if the heading has no properties drawer, or none of its property lines
/// is CUSTOM_ID (a stale caller, or a heading that's since lost it) --
/// callers treat that as "nothing to remove", not an error.
pub fn find_custom_id_line(lines: &[String], heading: &Heading) -> Option<usize> {
    let start = heading.line_no.saturating_sub(1);
    let end = heading.last_line.min(lines.len());
    lines
        .get(start..end)?
        .iter()
        .position(|line| {
            org_parser::parse_property(line.trim_start())
                .is_some_and(|(key, _)| key.eq_ignore_ascii_case(heading_identity::CUSTOM_ID_PROPERTY))
        })
        .map(|relative| start + relative)
}

/// Removes HEADING's own `:CUSTOM_ID:` line (see find_custom_id_line
/// above) from LINES, and additionally its enclosing `:PROPERTIES:`/
/// `:END:` boundary lines too if that leaves the drawer with nothing else
/// in it -- an empty drawer serves no purpose, so Settings' Maintenance
/// section (commands_headings::remove_custom_ids) cleans it up as part of
/// the same action instead of leaving a pointless `:PROPERTIES:\n:END:`
/// behind. Returns whether anything was actually removed.
pub fn remove_custom_id_and_empty_drawer(lines: &mut Vec<String>, heading: &Heading) -> bool {
    let Some(idx) = find_custom_id_line(lines, heading) else { return false };
    lines.remove(idx);

    let is_marker = |line: &str, marker: &str| line.trim().eq_ignore_ascii_case(marker);
    if idx > 0 && idx < lines.len() && is_marker(&lines[idx - 1], ":PROPERTIES:") && is_marker(&lines[idx], ":END:") {
        lines.remove(idx); // the :END: line, now sitting at idx
        lines.remove(idx - 1); // :PROPERTIES:, unaffected by the removal above
    }

    true
}

/// Removes a heading's `:PROPERTIES:`/`:END:` boundary lines, but only
/// with nothing between them. Separate from
/// `remove_custom_id_and_empty_drawer`, which only cleans up a drawer it
/// just emptied itself; this acts on one that was already empty. Returns
/// whether anything was removed.
pub fn remove_drawer_if_empty(lines: &mut Vec<String>, heading: &Heading) -> bool {
    let start = heading.line_no; // 0-indexed line right after the heading
    if start + 1 >= lines.len() {
        return false;
    }

    let is_marker = |line: &str, marker: &str| line.trim().eq_ignore_ascii_case(marker);
    if !is_marker(&lines[start], ":PROPERTIES:") || !is_marker(&lines[start + 1], ":END:") {
        return false;
    }

    lines.remove(start + 1);
    lines.remove(start);
    true
}

fn build_org_heading_block_inner(
    heading: &Heading,
    payload: &HeadingUpdatePayload,
    touch_updated: bool,
    touch_activity: bool,
    set_custom_id: bool,
) -> Vec<String> {
    let mut lines = Vec::new();

    let old_state = heading.state.as_deref().unwrap_or("");
    let new_state = payload.state.as_deref().unwrap_or(old_state);
    let title = payload.title.as_deref().unwrap_or(&heading.title);
    let tags = payload.tags.as_ref().unwrap_or(&heading.tags);
    let state_opt = if new_state.is_empty() { None } else { Some(new_state.to_string()) };

    // `Some(None)` clears it, `None` keeps whatever the heading had --
    // see HeadingUpdatePayload::priority.
    let priority = match &payload.priority {
        Some(next) => next.as_deref(),
        None => heading.priority.as_deref(),
    };

    lines.push(format_new_heading_line(heading.level, &state_opt, priority, title, tags, heading.progress_stats));

    // Any finished state stamps CLOSED, not just the literal DONE -- a
    // task marked "won't do" is finished too (task_utils::is_finished,
    // FINISHED_STATES). Keying on "DONE" alone left WONT-DO and OBE
    // without a CLOSED date, and find_archivable_ranges reads a missing
    // one as "old enough": an abandoned task was archived on the next
    // run, whatever the configured delay said.
    //
    // Moving BETWEEN two finished states keeps the original stamp rather
    // than re-dating it -- same as org's own org-log-done, which fires on
    // entering the done set, not on moving around inside it.
    let mut closed_str = heading.closed.clone();
    let was_finished = task_utils::is_finished(Some(old_state));
    let is_finished = task_utils::is_finished(Some(new_state));
    if is_finished && !was_finished {
        closed_str = Some(Local::now().format(DATE_FORMAT_CLOSED).to_string());
    } else if !is_finished {
        closed_str = None;
    }

    let mut planning_parts = Vec::new();
    if let Some(c) = closed_str {
        planning_parts.push(format!("CLOSED: [{}]", c));
    }

    let scheduled = match &payload.scheduled {
        Some(v) => v.as_ref(),
        None => heading.scheduled.as_ref(),
    };
    if let Some(ts) = scheduled {
        planning_parts.push(format!("SCHEDULED: {}", timestamp_utils::format_timestamp(ts)));
    }

    let deadline = match &payload.deadline {
        Some(v) => v.as_ref(),
        None => heading.deadline.as_ref(),
    };
    if let Some(ts) = deadline {
        planning_parts.push(format!("DEADLINE: {}", timestamp_utils::format_timestamp(ts)));
    }

    if !planning_parts.is_empty() {
        lines.push(planning_parts.join(" "));
    }

    let mut properties = payload.properties.clone().unwrap_or_else(|| heading.properties.clone());

    if let Some(effort) = &payload.effort_minutes {
        match effort {
            Some(minutes) => set_property(&mut properties, ORG_EFFORT_PROPERTY, effort_utils::format_effort(*minutes)),
            None => {
                remove_property(&mut properties, ORG_EFFORT_PROPERTY);
            }
        }
    }

    let now = Local::now().format(DATE_FORMAT_ACTIVITY).to_string();
    if touch_updated {
        set_property(&mut properties, ORG_UPDATED_PROPERTY, now.clone());
    }
    if touch_activity {
        set_property(&mut properties, ORG_LAST_ACTIVITY_PROPERTY, now);
    }

    // No-op if it already has one either way
    if set_custom_id {
        heading_identity::ensure_custom_id(&mut properties);
    }

    if !properties.is_empty() {
        lines.push(ORG_PROPERTIES_START.to_string());
        // The file's own arrangement first, then whatever is new, sorted.
        //
        // Sorting everything was not laziness -- `properties` is a
        // `HashMap` and Rust randomises its iteration order per process,
        // so without a total order the merge's determinism contract
        // breaks. But it also rewrote a hand-arranged drawer on every
        // edit. Remembering the order the file had (see
        // Heading::property_order) keeps both: deterministic, and no churn
        // for a drawer nobody touched.
        let mut written: std::collections::HashSet<&String> = std::collections::HashSet::new();
        for key in &heading.property_order {
            if let Some((k, v)) = properties.get_key_value(key) {
                if written.insert(k) {
                    lines.push(format!(":{}: {}", k, v));
                    // Directly after the line they repeat, so a drawer
                    // that listed the key twice comes back listing it
                    // twice, in place -- see Heading::repeated_properties.
                    // Skipped when the key itself was removed by this
                    // edit: repeats of a property that is gone are gone
                    // with it.
                    for (repeat_key, repeat_value) in &heading.repeated_properties {
                        if repeat_key == k {
                            lines.push(format!(":{}: {}", repeat_key, repeat_value));
                        }
                    }
                }
            }
        }
        let mut rest: Vec<_> = properties.keys().filter(|k| !written.contains(k)).collect();
        rest.sort();
        for k in rest {
            lines.push(format!(":{}: {}", k, properties[k]));
        }
        lines.push(ORG_PROPERTIES_END.to_string());
    }

    let clocks = payload.clocks.clone().unwrap_or_else(|| heading.clocks.clone());

    let mut history = payload.history.clone().unwrap_or_else(|| heading.history.clone());
    if !old_state.is_empty() && old_state != new_state {
        let now = Local::now().format(DATE_FORMAT_LOG).to_string();
        history.insert(0, state_log_line(new_state, old_state, &now));
    }

    // One shared :LOGBOOK: drawer for both clock entries and the state
    // log, after :PROPERTIES: and before the body. The parser discards the
    // raw drawer markers, precisely because this is what regenerates them,
    // so writing the entries unwrapped would drop the drawer from any
    // hand-written file. Clocks first, newest first; omitted entirely when
    // both are empty.
    // Nothing in this write touches the log, so it goes back exactly as it
    // came -- entries in their original order, each note still next to the
    // state change it explains. See Heading::raw_logbook.
    let log_untouched = payload.clocks.is_none()
        && payload.history.is_none()
        && (old_state.is_empty() || old_state == new_state)
        && !heading.raw_logbook.is_empty();

    if log_untouched {
        lines.push(ORG_LOGBOOK_START.to_string());
        lines.extend(heading.raw_logbook.iter().cloned());
        lines.push(ORG_LOGBOOK_END.to_string());
    } else if !clocks.is_empty() || !history.is_empty() || !heading.logbook_extra.is_empty() {
        lines.push(ORG_LOGBOOK_START.to_string());
        lines.extend(rebuilt_logbook(&heading.raw_logbook, clocks, history, &heading.logbook_extra));
        lines.push(ORG_LOGBOOK_END.to_string());
    }

    // Drawers belonging to somebody else, verbatim and after the LOGBOOK.
    for drawer in &heading.raw_drawers {
        lines.extend(drawer.iter().cloned());
    }

    match payload.content.as_deref() {
        // Edited here, so it goes through the ghost-prefix offset -- what
        // keeps a `* Notiz` typed in the description editor part of this
        // heading's body instead of becoming a heading in its own right.
        Some(edited) => {
            if !edited.trim().is_empty() {
                lines.extend(with_ids_on_new_tasks(apply_content_heading_offset(edited, heading.level)));
            }
        }
        // Not edited: written back exactly as it was read. See
        // Heading::raw_content for why re-deriving it is not safe.
        None if !heading.raw_content.is_empty() => {
            lines.extend(heading.raw_content.lines().map(String::from));
        }
        None => {
            if !heading.content.trim().is_empty() {
                lines.extend(apply_content_heading_offset(&heading.content, heading.level));
            }
        }
    }

    lines
}

/// Gives every task that a description is about to BECOME its own
/// `CUSTOM_ID`.
///
/// A `* TODO` line typed or pasted into a description is written as a
/// real task (see `apply_content_heading_offset` -- the user's own rule:
/// a TODO keyword or a :project: tag is structure). Tasks made that way
/// were the only ones without an id: everything created through the app
/// gets one, this path did not. Without one, a task is found by its title,
/// and a title that occurs twice -- the same link pasted twice -- is
/// refused by every write (heading_identity::resolve_target). The result
/// was tasks that could be seen and not deleted, completed or moved.
///
/// Into the task's own drawer when the pasted text brought one (it
/// usually does: a copied task comes with `:CREATED:` and the like),
/// otherwise a new drawer right under the heading -- after its planning
/// line if it has one, which is where org requires the drawer to be.
/// A task that already carries an id keeps it, and nothing inside a code
/// block is touched: those lines are text.
fn with_ids_on_new_tasks(mut lines: Vec<String>) -> Vec<String> {
    let in_block = org_parser::content_block_lines(&lines);
    let is_task = |line: &str| org_parser::parse_heading_line(line).is_some_and(|parsed| is_real_structure(&parsed.state, &parsed.tags));
    let is_planning = |line: &str| {
        let t = line.trim_start();
        ["SCHEDULED:", "DEADLINE:", "CLOSED:"].iter().any(|k| t.starts_with(k))
    };
    let has_id = |line: &str| {
        org_parser::parse_property(line.trim_start())
            .is_some_and(|(key, value)| key.eq_ignore_ascii_case(heading_identity::CUSTOM_ID_PROPERTY) && !value.trim().is_empty())
    };

    // Collected first and applied from the bottom up, so no insertion
    // moves a position that is still waiting.
    let mut inserts: Vec<(usize, Vec<String>)> = Vec::new();
    for i in 0..lines.len() {
        if in_block[i] || !is_task(&lines[i]) {
            continue;
        }
        let mut at = i + 1;
        if at < lines.len() && is_planning(&lines[at]) {
            at += 1;
        }
        let id_line = format!(":{}: {}", heading_identity::CUSTOM_ID_PROPERTY, heading_identity::generate_custom_id());
        if at < lines.len() && lines[at].trim().eq_ignore_ascii_case(ORG_PROPERTIES_START) {
            let end = (at + 1..lines.len()).find(|&k| lines[k].trim().eq_ignore_ascii_case(ORG_PROPERTIES_END));
            // An unclosed drawer is left exactly as it is -- adding to it
            // would guess where the user meant it to end.
            if let Some(end) = end {
                if !lines[at + 1..end].iter().any(|l| has_id(l)) {
                    inserts.push((end, vec![id_line]));
                }
            }
        } else {
            inserts.push((at, vec![ORG_PROPERTIES_START.to_string(), id_line, ORG_PROPERTIES_END.to_string()]));
        }
    }
    for (at, new_lines) in inserts.into_iter().rev() {
        lines.splice(at..at, new_lines);
    }
    lines
}

/// The write-time half of the ghost-prefix round trip;
/// `strip_content_heading_offset` is the read-time inverse.
///
/// A description field always holds the short typed form. What lands on
/// disk gets enough extra stars that the next parse absorbs it as content
/// of whichever heading it ends up under. `host_level` is the baseline for
/// content before any real heading typed in the same block.
///
/// "Whichever heading it ends up under" is the subtle part: a real heading
/// typed earlier in the same description becomes a genuine nested subtask,
/// and org's rule makes everything after it that subtask's content. A flat
/// offset would land such a line too shallow to be absorbed at all, so
/// this tracks a running chain of the real headings seen so far and
/// computes each line's offset against the one it will nest under.
///
/// Two conservative per-line cases:
/// - No leading `*`: passed through unchanged.
/// - A `*`-run not followed by whitespace (inline emphasis, not a heading
///   attempt): gets one leading space instead of an offset. Org headings
///   require column 0, so an indented line can never be misread as one,
///   while adding stars would glue a stray run to the word.
/// The `:LOGBOOK:` body for a write that DID change the log.
///
/// Every line that was already in the drawer keeps the place it had, and
/// whatever is new goes on top -- which is where org puts a new entry
/// (`org-log-states-order-reversed`, on by default).
///
/// Rebuilding it as clocks-then-state-then-the-rest instead was a rewrite
/// of a drawer the edit had barely touched: clocking in on a task whose
/// log read "state, note, clock" wrote it back as "clock, state, note",
/// and every device got the churn. Nothing was lost, but a `- Note taken
/// on ...` could end up several lines away from the state change it
/// explains -- and RAW_LOGBOOK exists precisely to keep them together.
///
/// RAW_LOGBOOK is empty for a heading that was built in memory rather
/// than parsed; then there is no order to preserve and the typed order is
/// the only one there is.
fn rebuilt_logbook(raw_logbook: &[String], clocks: Vec<String>, history: Vec<String>, extra: &[String]) -> Vec<String> {
    let mut wanted: Vec<String> = clocks;
    wanted.extend(history);
    // Notes and anything else in the drawer this build does not model
    // (see Heading::logbook_extra). Inside the drawer, which is the point
    // -- they used to be written back below it as body text.
    wanted.extend(extra.iter().cloned());

    if raw_logbook.is_empty() {
        return wanted;
    }

    // Matched by content, and each match consumed, so a drawer with two
    // identical clock lines keeps both.
    let mut new_entries = wanted;
    let mut in_file_order: Vec<String> = Vec::with_capacity(new_entries.len());
    for line in raw_logbook {
        if let Some(index) = new_entries.iter().position(|candidate| candidate == line) {
            in_file_order.push(new_entries.remove(index));
        }
    }
    // What is left never appeared in the file: this write's own additions.
    new_entries.extend(in_file_order);
    new_entries
}

fn apply_content_heading_offset(content: &str, host_level: usize) -> Vec<String> {
    // (typed_level, saved_level) per real heading seen so far, innermost
    // last -- see the doc comment above.
    let mut real_ancestors: Vec<(usize, usize)> = Vec::new();

    // A code block's lines are text, whatever they start with: they are
    // written exactly as typed and the reader keeps them exactly as read
    // (org_parser::closed_block_lines). Demoting them was one-way -- the
    // reader never takes stars off inside a block -- so each save pushed
    // the org code in a `#+BEGIN_SRC org` a few levels deeper.
    let lines: Vec<&str> = content.lines().collect();
    let regions = org_parser::content_block_regions(&lines);
    let block_line = |i: usize| regions.iter().find(|&&(begin, end)| (begin..=end).contains(&i)).copied();

    lines
        .iter()
        .enumerate()
        .map(|(i, &line)| {
            // Into the file the org way: a block's inside escaped with
            // org's comma (org-escape-code-in-string), so a `* ` or `#+`
            // line in the code is not a heading or a marker to org -- the
            // reader takes the comma off again. The markers stay.
            if let Some((begin, end)) = block_line(i) {
                return if i == begin || i == end { line.to_string() } else { crate::core::org_syntax::escape_code_line(line) };
            }
            let typed_level = count_leading_stars(line);
            if typed_level == 0 {
                return line.to_string();
            }
            let after_stars = &line[typed_level..];
            let is_heading_attempt = after_stars.is_empty() || after_stars.starts_with(char::is_whitespace);
            if !is_heading_attempt {
                return format!(" {}", line);
            }

            // Keyword/tag detection only looks at the text AFTER the
            // stars, so it's unaffected by how many the user actually
            // typed -- safe to check on the original, not-yet-demoted
            // line. Uses the exact same predicate as org_parser's
            // read-side structural rule (models::is_real_structure) --
            // the two MUST stay in sync for the round trip to hold.
            let is_real = org_parser::parse_heading_line(line).is_some_and(|parsed| is_real_structure(&parsed.state, &parsed.tags));

            let prepend = if is_real {
                // Pop every ancestor at the same or a deeper typed level
                // -- this heading becomes their sibling (or a child of a
                // shallower one still on the stack), matching real org's
                // own relative-star-depth nesting rule, not just "always
                // nests under the last real heading found".
                while real_ancestors.last().is_some_and(|&(t, _)| t >= typed_level) {
                    real_ancestors.pop();
                }
                real_ancestors.last().map(|&(a_typed, a_saved)| a_saved - a_typed).unwrap_or(host_level)
            } else {
                // A non-real line is always absorbed into whichever
                // heading is CURRENTLY open once parsed, regardless of
                // its own typed star count -- use the nearest real
                // ancestor's own saved level directly (not the
                // shift-adjusted value above, which is for computing a
                // REAL heading's tree position, not "how much to demote
                // by so this lands deeper than its absorbing parent").
                real_ancestors.last().map(|&(_, a_saved)| a_saved).unwrap_or(host_level)
            };

            let demoted = format!("{}{}", "*".repeat(prepend), line);
            if is_real {
                real_ancestors.push((typed_level, prepend + typed_level));
            }
            demoted
        })
        .collect()
}

/// Re-levels every heading in LINES by DIFF -- a refile hands in a whole
/// extracted subtree here, headings and their content alike.
///
/// Which is why "starts with a star" is not the test: it has to be the
/// same one `apply_content_heading_offset` applies on the way in, a
/// `*`-run followed by whitespace or the line end. `*Wichtig*` at column 0
/// is emphasis, and Emacs writes it exactly like that into files this app
/// then refiles -- rewriting its leading run turns the markup into
/// `**Wichtig*`.
pub fn adjust_heading_levels(lines: &mut Vec<String>, diff: isize) {
    // Not inside a code block: those lines are text, and shifting their
    // stars rewrote the org code someone was quoting every time the task
    // holding it moved (org_parser::closed_block_lines).
    let in_block = org_parser::closed_block_lines(lines);
    for (line, in_block) in lines.iter_mut().zip(in_block) {
        if in_block {
            continue;
        }
        let stars_count = count_leading_stars(line);
        if stars_count == 0 {
            continue;
        }
        let after_stars = &line[stars_count..];
        if !(after_stars.is_empty() || after_stars.starts_with(char::is_whitespace)) {
            continue;
        }

        let new_count = (stars_count as isize + diff).max(1) as usize;
        let without_stars = line.trim_start_matches('*');
        *line = format!("{}{}", "*".repeat(new_count), without_stars);
    }
}

/// PATH as it is on disk right now, parsed -- what every command that is
/// about to WRITE resolves its target against, and what it splices into.
///
/// Deliberately not the index cache (`IndexManager::get_headings_or_parse`):
/// that is only as current as the last watcher event or explicit update,
/// and a sync pull, a CalDAV pull or an external editor can have moved
/// every line in the file since. Splicing a heading's block against a
/// stale parse overwrites whatever actually occupies those lines now --
/// the file-level half of the guarantee heading_identity::resolve_target
/// gives at the heading level.
///
/// Reads that only need heading DATA (list views, search, previews) keep
/// using the cache; writers -- and readers that go on to slice the file by
/// these line numbers, see get_heading_source_text -- pay for this. The
/// index itself is refreshed once afterwards, by the write.
pub fn parse_from_disk(path: &Path) -> Vec<Heading> {
    org_parser::extract_headings_from_file(path)
}

/// The file's lines AND the headings parsed from exactly those lines, from
/// a single read.
///
/// For every writer that needs both halves -- the batched updates, the
/// batched deletes, both refiles. `parse_from_disk` followed by
/// `read_lines` reads the file twice, and between the two reads a sync
/// pull, a CalDAV pull or another editor can rewrite it: the caller then
/// splices line numbers taken from one version of the file into the text
/// of another, which is the same corruption a stale index causes, just
/// through a much narrower window.
///
/// One read has no such window. That the two halves agree stops being a
/// matter of timing and becomes a property of how they were obtained --
/// which is what lets the splices below refuse a mismatched range outright
/// instead of clamping it (see `apply_bulk_update`).
pub fn read_and_parse(path: &Path) -> Result<(Vec<String>, Vec<Heading>), String> {
    let lines = fs_utils::read_lines(path).map_err(|e| e.to_string())?;
    let flat = org_parser::parse_headings_from_lines(path, &lines);
    Ok((lines, flat))
}

/// Complexity canaries over synthetic data, with generous thresholds --
/// not a timing budget.
///
/// Creating a nested subtask reads the file once and writes it once, no
/// matter how deep it sits -- the insert and every ancestor touch happen
/// in the same in-memory lines (see heading_utils::open_edit). It used to
/// be one full read-and-write pass PER ancestor, i.e. O(ancestors x file
/// size). Invisible on a small hand-tested project, which is why this
/// needs sizeable data.
#[cfg(test)]
mod perf {
    use super::*;
    use crate::core::heading_utils::tree::ancestors_of;
    use crate::core::models::HeadingCreatePayload;
    use crate::core::org_parser;
    use std::time::Instant;
    use tempfile::TempDir;

    const LEAVES_PER_GROUP: usize = 1000;
    const GROUP_COUNT: usize = 3;

    /// One project, GROUP_COUNT groups, LEAVES_PER_GROUP leaf tasks each --
    /// ~3000 headings total, each with a property drawer and a body line,
    /// so the file is genuinely large (hundreds of KB), not just many
    /// small headings.
    fn write_large_project_file(path: &Path) {
        let mut content = String::from("* Project\n:PROPERTIES:\n:CUSTOM_ID: project-root\n:END:\n");
        for g in 0..GROUP_COUNT {
            content.push_str(&format!("** Group {g}\n:PROPERTIES:\n:CUSTOM_ID: group-{g}\n:END:\n"));
            for t in 0..LEAVES_PER_GROUP {
                content.push_str(&format!(
                    "*** TODO Task {g}-{t}\n:PROPERTIES:\n:CUSTOM_ID: task-{g}-{t}\n:END:\nSome descriptive body text for this task.\n"
                ));
            }
        }
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn perf_creating_a_subtask_in_a_large_project_file() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("projects.org");
        write_large_project_file(&path);
        let file_size = std::fs::metadata(&path).unwrap().len();

        let lines = fs_utils::read_lines(&path).unwrap();
        let flat = org_parser::parse_headings_from_lines(&path, &lines);
        // Target the LAST group -- its own subtree-end scan (find_subtree_end_line)
        // and touch_ancestors' single re-parse both still have to account for
        // everything before it, so this isn't an artificially cheap "first
        // heading in the file" case.
        let parent_idx = flat.iter().position(|h| h.title == format!("Group {}", GROUP_COUNT - 1)).unwrap();

        let payload = HeadingCreatePayload {
            file_path: path.to_string_lossy().to_string(),
            title: "New subtask".to_string(),
            state: Some("TODO".to_string()),
            tags: vec![],
            parent: Some(crate::core::models::HeadingTarget { line_no: flat[parent_idx].line_no, custom_id: None, title: None }),
            scheduled: None,
            effort_minutes: None,
            content: None,
            category: None,
        };
        let new_lines = build_new_heading_block(&payload, flat[parent_idx].level + 1);

        let chain_len = ancestors_of(&flat, parent_idx).len() + 1;

        let start = Instant::now();
        let mut edit = crate::core::heading_utils::open_edit(
            &path,
            &crate::core::heading_utils::target_at_line(flat[parent_idx].line_no),
            "Übergeordnete Überschrift",
        )
        .unwrap();
        edit.insert_after_subtree(new_lines).unwrap();
        edit.touch_self_and_ancestors().unwrap();
        edit.commit().unwrap();
        let elapsed = start.elapsed();

        eprintln!(
            "perf_creating_a_subtask_in_a_large_project_file: {elapsed:?} for a {} heading / {file_size} byte file, {} ancestors touched",
            GROUP_COUNT * LEAVES_PER_GROUP + GROUP_COUNT + 1,
            chain_len,
        );

        // Correctness check, not just timing -- the new heading actually
        // landed, and the touched ancestors' LAST_ACTIVITY got stamped.
        let final_content = std::fs::read_to_string(&path).unwrap();
        assert!(final_content.contains("New subtask"));
        assert!(final_content.matches(ORG_LAST_ACTIVITY_PROPERTY).count() >= chain_len);

        // Generous: (1 insert + N ancestor splices) full read+write passes
        // of a few-hundred-KB file should never realistically approach this.
        assert!(elapsed.as_secs() < 5, "creating one subtask took {elapsed:?} -- investigate for a complexity regression (see this module's own doc comment on the O(ancestors x file_size) cost shape)");
    }
}
