//! Bringing two versions of the same file back together.
//!
//! The vault sync needs this the moment a file changed on two devices
//! between one sync and the next: it has the common ancestor, the local
//! text and the remote text, and has to end up with one file -- or admit
//! that it cannot and leave the decision to the user.
//!
//! # The contract
//!
//! Whoever implements [`MergeHandler`] promises four things, and a sync
//! that uses one may rely on all four:
//!
//! 1. **No side effects.** Text in, text out. No filesystem, no network,
//!    no clock, no randomness -- which is also what makes a merge
//!    testable and what lets it run on whichever thread the caller has.
//! 2. **Deterministic.** The same three inputs give the same output on
//!    every device and in every version. A merge that decided differently
//!    on two machines would have them overwrite each other forever, each
//!    one "fixing" what the other just wrote.
//! 3. **Failure means the user decides.** `Err` is a normal outcome, not
//!    a bug: it puts the file into manual conflict resolution with both
//!    versions intact. A handler must never return a half-merged file it
//!    is unsure about -- silent data loss is the one outcome worse than
//!    asking.
//! 4. **Both inputs survive or neither.** Whatever comes back contains
//!    every local and every remote change, or the handler returns `Err`.
//!
//! # Deliberately not a chain
//!
//! One handler decides, not a list of them in priority order. Two
//! handlers that both claim a file would need an ordering, the ordering
//! would decide user data, and nothing about a file says which of them
//! ought to win. The sync feature picks exactly one, and that one is free
//! to branch on the path internally -- which is what [`OrgAstMerge`] does
//! for `.org` versus everything else.
//!
//! # What a handler must cope with
//!
//! - **No common ancestor.** A file created on both devices independently
//!   arrives with an empty base; that is a normal first sync, not an
//!   error.
//! - **Text only.** The caller reads files as UTF-8, so anything binary
//!   never reaches here -- it fails earlier and lands in manual
//!   resolution.
//! - **Large files.** Merging parses three snapshots, so it costs three
//!   parses of one file. The caller only merges files that genuinely
//!   changed on both sides, and that is what keeps this off the hot path
//!   entirely.

use std::collections::{HashMap, HashSet};

/// What one file's three versions look like to a handler.
///
/// A struct rather than four arguments, because the next thing anyone
/// wants to hand a merge (the file kind, a hint from the provider) can be
/// added here without touching every caller and every implementation --
/// and because three same-typed strings in a row are three chances to
/// swap two of them.
pub struct MergeInput<'a> {
    /// Vault-relative, as the sync knows it. For deciding how to read the
    /// file, never for touching it: a handler does no IO (see the module
    /// doc).
    pub path: &'a str,
    /// The last version both sides agreed on. Empty when there is none --
    /// a file created independently on two devices.
    pub base: &'a str,
    pub local: &'a str,
    pub remote: &'a str,
}

/// How a sync resolves a file that changed on both sides.
///
/// One implementation today ([`OrgAstMerge`]); the trait is here because
/// the *consumer* is the variable part. A second sync feature -- another
/// provider, another conflict model -- takes the handler it wants instead
/// of reimplementing three-way merging, and the vault sync keeps this
/// one.
pub trait MergeHandler: Send + Sync {
    /// For logs and conflict messages: which merge produced this, or
    /// refused to.
    fn name(&self) -> &'static str;

    /// The merged file, or why the user has to decide.
    ///
    /// See the module doc for what an implementation promises. The error
    /// is shown to the user next to both versions, so it says what could
    /// not be decided, not which function returned early.
    fn merge(&self, input: &MergeInput) -> Result<String, String>;
}

/// What nobo merges with: org files heading by heading, everything else
/// line by line.
///
/// Splitting on the extension rather than on the file kind
/// (`core::file_kind`) is deliberate: the kind lives inside the file, and
/// reading it would mean parsing a fourth version of something this is
/// already holding three copies of. Anything that parses as org gets the
/// AST merge; anything that does not is text, and text is what diffy is
/// for.
pub struct OrgAstMerge;

impl MergeHandler for OrgAstMerge {
    fn name(&self) -> &'static str {
        "org-ast"
    }

    fn merge(&self, input: &MergeInput) -> Result<String, String> {
        if input.path.ends_with(".org") {
            merge_org_ast(input.path, input.base, input.local, input.remote)
        } else {
            merge_text(input.base, input.local, input.remote)
        }
    }
}

/// The handler the vault sync uses. A `const` rather than a lookup: there
/// is one, choosing it is not a runtime decision, and a sync that wants
/// another one names it instead.
pub const DEFAULT: &OrgAstMerge = &OrgAstMerge;

use crate::core::heading_identity;
use crate::core::models::Heading;
use diffy::MergeOptions;

/// Create AST for each variant and tries to merge them heading by heading.
pub(crate) fn merge_org_ast(path: &str, base_text: &str, local_text: &str, remote_text: &str) -> Result<String, String> {
    let path_obj = std::path::Path::new(path);
    let base_flat = crate::core::org_parser::parse_headings_from_lines(path_obj, base_text.lines());
    let local_flat = crate::core::org_parser::parse_headings_from_lines(path_obj, local_text.lines());
    let remote_flat = crate::core::org_parser::parse_headings_from_lines(path_obj, remote_text.lines());

    // Fallback: No headings, just merge text
    if base_flat.is_empty() && local_flat.is_empty() && remote_flat.is_empty() {
        return merge_text(base_text, local_text, remote_text);
    }

    // Heading matching needs a stable identity across all three
    // snapshots. A title-based key drifts the moment a title changes on
    // one side, and the key-list merge below cannot tell that apart from a
    // deletion plus an unrelated addition -- silently dropping whichever
    // edit does not survive the misattribution.
    //
    // So this errors out the moment any heading lacks a CUSTOM_ID, and the
    // caller falls back to manual resolution. Once backfilled, every file
    // gets safe auto-merge.
    let all_have_custom_id = base_flat.iter().chain(&local_flat).chain(&remote_flat).all(heading_identity::has_custom_id);
    if !all_have_custom_id {
        // English, and not through `t!`, like every message in this
        // module -- see `merge_field`'s doc comment: a merge failure never
        // reaches the user as text. It becomes a manual conflict, which
        // the UI describes in its own localised words; this string is
        // only ever read in a log.
        return Err("cannot auto-merge: not every heading has a CUSTOM_ID yet (see heading_identity::has_custom_id)".to_string());
    }

    // #+TITLE etc.
    let get_preamble = |text: &str, flat: &[Heading]| -> String {
        if let Some(first) = flat.first() {
            let p = text.lines().take(first.line_no - 1).collect::<Vec<_>>().join("\n");
            if p.is_empty() {
                String::new()
            } else {
                format!("{}\n", p)
            }
        } else {
            text.to_string()
        }
    };

    let merged_preamble =
        merge_text(&get_preamble(base_text, &base_flat), &get_preamble(local_text, &local_flat), &get_preamble(remote_text, &remote_flat))?;

    // Unique key per heading -- CUSTOM_ID only (guaranteed present on
    // every heading here, checked above), via heading_identity::
    // capture_identity for consistency with the rest of the codebase's own
    // single "how do we identify a heading across a time gap" mechanism
    // (core::history::restore uses the exact same one). Computed once per
    // heading and reused for both the lookup maps and the key-list text
    // below, instead of recomputing it a second time per heading.
    let key_of = |h: &Heading| -> String {
        match heading_identity::capture_identity(h) {
            heading_identity::HeadingIdentity::CustomId(id) => format!("id:{id}"),
            heading_identity::HeadingIdentity::Title(_) => unreachable!("checked above: every heading has a CUSTOM_ID"),
        }
    };
    let base_keyed: Vec<(String, &Heading)> = base_flat.iter().map(|h| (key_of(h), h)).collect();
    let local_keyed: Vec<(String, &Heading)> = local_flat.iter().map(|h| (key_of(h), h)).collect();
    let remote_keyed: Vec<(String, &Heading)> = remote_flat.iter().map(|h| (key_of(h), h)).collect();

    let base_map: HashMap<&str, &Heading> = base_keyed.iter().map(|(k, h)| (k.as_str(), *h)).collect();
    let local_map: HashMap<&str, &Heading> = local_keyed.iter().map(|(k, h)| (k.as_str(), *h)).collect();
    let remote_map: HashMap<&str, &Heading> = remote_keyed.iter().map(|(k, h)| (k.as_str(), *h)).collect();

    // Merge all keys (= get positions of headings)
    let base_keys = base_keyed.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join("\n");
    let local_keys = local_keyed.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join("\n");
    let remote_keys = remote_keyed.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join("\n");

    let merged_keys_text = merge_text(&base_keys, &local_keys, &remote_keys)?;

    let mut final_lines = Vec::new();
    if !merged_preamble.is_empty() {
        final_lines.push(merged_preamble.trim_end().to_string());
    }

    // Helper to extract the original block
    let extract_block = |text: &str, heading: &Heading| -> Vec<String> {
        text.lines()
            .skip(heading.line_no.saturating_sub(1))
            .take(heading.last_line.saturating_sub(heading.line_no) + 1)
            .map(String::from)
            .collect()
    };

    // Merge content
    for line in merged_keys_text.lines() {
        let key = line.trim();

        if key.is_empty() {
            continue;
        }

        let b_opt = base_map.get(key).copied();
        let l_opt = local_map.get(key).copied();
        let r_opt = remote_map.get(key).copied();

        let block = match (b_opt, l_opt, r_opt) {
            (Some(b), Some(l), Some(r)) => {
                let b_text = extract_block(base_text, b).join("\n");
                let l_text = extract_block(local_text, l).join("\n");
                let r_text = extract_block(remote_text, r).join("\n");

                if l_text == r_text {
                    extract_block(local_text, l) // Both are the same, use local
                } else if b_text == l_text {
                    extract_block(remote_text, r) // Only remote changed, use remote
                } else if b_text == r_text {
                    extract_block(local_text, l) // Only local changed, use local
                } else {
                    // Both sides changed content -> Merge headings
                    let merged_heading = merge_headings(b, l, r)?;
                    crate::core::heading_utils::build_org_heading_block_unaltered(&merged_heading)
                }
            }
            // Present on both sides but not in base -- reachable when
            // both devices refiled the same heading into this file, which
            // does not regenerate its id. Identical content is
            // unambiguous; otherwise there is no base to merge against,
            // and faking one out of local reads as "local did not change"
            // for every field, discarding its edits. Fail instead, so the
            // file falls back to manual resolution.
            (None, Some(l), Some(r)) => {
                let l_text = extract_block(local_text, l);
                let r_text = extract_block(remote_text, r);
                if l_text.join("\n") == r_text.join("\n") {
                    l_text
                } else {
                    return Err(format!(
                        "merge conflict: heading \"{key}\" exists on both sides with different content but no common base"
                    ));
                }
            }
            // Task exists only on one device - use it
            (_, Some(l), None) => extract_block(local_text, l),
            (_, None, Some(r)) => extract_block(remote_text, r),

            (_, None, None) => continue,
        };

        final_lines.extend(block);
    }

    Ok(final_lines.join("\n") + "\n")
}

pub fn merge_headings(base: &Heading, local: &Heading, remote: &Heading) -> Result<Heading, String> {
    // Every field, spelled out, with no `..Default::default()` -- unlike
    // the fixtures, which is the point. A field added to `Heading` needs a
    // decision here (merge it? take local's? refuse?), and the compiler
    // refusing to build this literal is how that decision gets made
    // instead of missed.
    Ok(Heading {
        id: local.id.clone(),
        file_path: local.file_path.clone(),
        line_no: local.line_no,
        last_line: local.last_line,
        level: local.level,
        children: local.children.clone(),

        // Will be recalculated with `annotate_progress`
        progress_stats: local.progress_stats,

        // merge default fields (State, Title, Dates, Effort)
        state: merge_field(&base.state, &local.state, &remote.state, "state")?,
        priority: merge_field(&base.priority, &local.priority, &remote.priority, "priority")?,
        title: merge_field(&base.title, &local.title, &remote.title, "title")?,
        scheduled: merge_field(&base.scheduled, &local.scheduled, &remote.scheduled, "scheduled")?,
        deadline: merge_field(&base.deadline, &local.deadline, &remote.deadline, "deadline")?,
        // Derived from the merged content, not merged itself -- it is a
        // read of a line that lives in `content` (see org_parser's
        // first_bare_active_timestamp), so merging it separately could
        // contradict the text it was read from.
        occurrence: None,
        closed: merge_field(&base.closed, &local.closed, &remote.closed, "closed")?,
        effort_minutes: merge_field(&base.effort_minutes, &local.effort_minutes, &remote.effort_minutes, "effort")?,

        // merge complex fields
        tags: merge_tags(&base.tags, &local.tags, &remote.tags),
        properties: merge_properties(&base.properties, &local.properties, &remote.properties)?,
        history: merge_history(&base.history, &local.history, &remote.history),
        clocks: merge_clocks(&base.clocks, &local.clocks, &remote.clocks)?,
        // Neither is interpreted here, so neither can be merged line by
        // line -- three-way on the whole bucket, refusing when both sides
        // changed it differently, like every other field above.
        logbook_extra: merge_field(&base.logbook_extra, &local.logbook_extra, &remote.logbook_extra, "logbook notes")?,
        raw_drawers: merge_field(&base.raw_drawers, &local.raw_drawers, &remote.raw_drawers, "drawers")?,
        repeated_properties: merge_field(
            &base.repeated_properties,
            &local.repeated_properties,
            &remote.repeated_properties,
            "repeated properties",
        )?,

        // Will be recalculated with `annotate_clocked_time`
        own_clocked_minutes: local.own_clocked_minutes,
        total_clocked_minutes: local.total_clocked_minutes,

        // Merge text content (This will fail fast if there is a conflict)
        content: merge_text(&base.content, &local.content, &remote.content)?,
        // Deliberately dropped: the merged body is new text, so there is
        // no verbatim original to write back and the normal ghost-prefix
        // offset applies (see Heading::raw_content).
        raw_content: String::new(),
        // Likewise: the merged log is assembled from both sides, so there
        // is no single original to write back.
        raw_logbook: Vec::new(),
        // Local's arrangement, for the keys it still has -- the merged set
        // may add keys from remote, which then sort in after them.
        property_order: local.property_order.clone(),
    })
}

/// Three-way merge for a scalar field, refusing rather than picking.
///
/// The last arm used to be `local.clone()` -- "conflict: both sides have
/// changed, use local". That is silent data loss, and it contradicted
/// three things at once: this module's own contract ("Both inputs survive
/// or neither"), `docs/architecture.md`, and `docs/sync.md` ("Conflicts
/// are never auto-resolved"). Twelve lines further down the diffy path
/// already said so out loud: "We actively fail here so the orchestrator
/// can delegate to the UI instead of silently eating user data."
///
/// Verified before the change: base `* TODO Buy milk`, local `* DONE Buy
/// oat milk`, remote `* WAITING Buy soy milk` merged to the local one and
/// uploaded it as the winner, with no conflict ever reported.
///
/// `what` names the field in the message. English, like every other
/// message in this module and like the `log::warn!` in
/// `sync::orchestrator` that prints it: a merge failure never reaches the
/// user as text -- it puts the file into manual conflict resolution, and
/// the UI writes its own words for that. Only a log reader sees this.
fn merge_field<T: PartialEq + Clone>(base: &T, local: &T, remote: &T, what: &str) -> Result<T, String> {
    if local == remote {
        Ok(local.clone()) // Both have the same value
    } else if base == local {
        Ok(remote.clone()) // local did not change, remote did, use remote
    } else if base == remote {
        Ok(local.clone()) // remote did not change, local did, use local
    } else {
        Err(format!("merge conflict: \"{what}\" was changed differently on both devices"))
    }
}

/// 3-Way set merge for tags
/// No real conflicts, we just combine both sets
fn merge_tags(base: &[String], local: &[String], remote: &[String]) -> Vec<String> {
    let base_set: HashSet<_> = base.iter().collect();
    let local_set: HashSet<_> = local.iter().collect();
    let remote_set: HashSet<_> = remote.iter().collect();

    let mut final_tags = Vec::new();
    let all_tags: HashSet<_> = local_set.union(&remote_set).copied().collect();

    for tag in all_tags {
        let in_b = base_set.contains(tag);
        let in_l = local_set.contains(tag);
        let in_r = remote_set.contains(tag);

        if (in_l && in_r) || (!in_b && in_l) || (!in_b && in_r) {
            final_tags.push((*tag).clone());
        }
    }

    final_tags.sort(); // Consistent sort order
    final_tags
}

/// 3-Way map merge for :PROPERTIES:
/// Properties this app stamps on every write, which therefore diverge on
/// two devices even when nobody touched the same thing.
///
/// `serialize.rs` writes `:UPDATED:` and `:LAST_ACTIVITY:` to the second
/// on every edit, and `touch_ancestors` does it for every enclosing
/// heading. Refusing on those would turn the documented must-merge case
/// (`title_rename_on_one_side_and_unrelated_field_edit_on_the_other_both_survive`)
/// into a hard conflict and switch the AST auto-merge off in practice --
/// so they are resolved deterministically instead, and only they.
///
/// Newest wins, compared as text: the stamp is `YYYY-MM-DD Day HH:MM:SS`,
/// where lexicographic order and chronological order agree (the weekday
/// sits between two fields that already decide it).
const STAMPED_PROPERTIES: [&str; 2] = ["UPDATED", "LAST_ACTIVITY"];

fn merge_properties(
    base: &HashMap<String, String>,
    local: &HashMap<String, String>,
    remote: &HashMap<String, String>,
) -> Result<HashMap<String, String>, String> {
    let mut final_props = HashMap::new();

    let mut all_property_keys = HashSet::new();
    for property_key in base.keys().chain(local.keys()).chain(remote.keys()) {
        all_property_keys.insert(property_key);
    }

    for key in all_property_keys {
        let base_val = base.get(key);
        let local_val = local.get(key);
        let remote_val = remote.get(key);

        if local_val == remote_val {
            if let Some(val) = local_val {
                final_props.insert(key.clone(), val.clone());
            }
        } else if base_val == local_val {
            if let Some(val) = remote_val {
                final_props.insert(key.clone(), val.clone());
            }
        } else if base_val == remote_val {
            if let Some(val) = local_val {
                final_props.insert(key.clone(), val.clone());
            }
        } else if STAMPED_PROPERTIES.iter().any(|stamped| key.eq_ignore_ascii_case(stamped)) {
            // Not a conflict, a clock: whichever device edited last is the
            // answer, and both devices compute the same one. See
            // STAMPED_PROPERTIES.
            if let Some(val) = local_val.max(remote_val) {
                final_props.insert(key.clone(), val.clone());
            }
        } else {
            // A value the user wrote, changed differently on both sides.
            // This used to keep the local one and upload it as the winner.
            return Err(format!("merge conflict: property \"{key}\" was changed differently on both devices"));
        }
    }
    Ok(final_props)
}

/// History (Logbook) is append-only. Merge the lists and remove duplicates
fn merge_history(base: &[String], local: &[String], remote: &[String]) -> Vec<String> {
    let mut final_history = Vec::new();
    let mut seen = HashSet::new();

    for entry in base.iter().chain(local.iter()).chain(remote.iter()) {
        if seen.insert(entry.clone()) {
            final_history.push(entry.clone());
        }
    }

    final_history
}

/// A CLOCK line's start-timestamp prefix (e.g. `"[2026-08-17 Mon 13:00]"`,
/// bracket included), used as its identity for merge_clocks below --
/// `None` for a line that doesn't parse as a CLOCK entry at all.
fn clock_key(line: &str) -> Option<&str> {
    let end = line.find(']')?;
    if !line.trim_start().starts_with(crate::utils::clock_utils::CLOCK_PREFIX) {
        return None;
    }
    Some(&line[..=end])
}

/// 3-way merge for clock entries, keyed by start timestamp. A plain
/// union-dedup is unsafe here: an open entry one side later closes differs
/// only in its trailing text, so both survive as a ghost timer alongside
/// the real one.
///
/// Each key resolves with the same 3-way logic as properties -- equal
/// sides win outright, otherwise whichever side changed wins, and local
/// breaks a three-way tie.
fn merge_clocks(base: &[String], local: &[String], remote: &[String]) -> Result<Vec<String>, String> {
    // Lines with no parseable key (hand-edited/exotic) can't be reasoned
    // about positionally -- kept via plain union-dedup, like merge_history,
    // rather than silently dropped.
    let mut unkeyed = Vec::new();
    let mut seen_unkeyed = HashSet::new();
    for line in base.iter().chain(local.iter()).chain(remote.iter()) {
        if clock_key(line).is_none() && seen_unkeyed.insert(line.clone()) {
            unkeyed.push(line.clone());
        }
    }

    fn by_key(lines: &[String]) -> HashMap<&str, &String> {
        lines.iter().filter_map(|l| clock_key(l).map(|k| (k, l))).collect()
    }
    let base_by_key = by_key(base);
    let local_by_key = by_key(local);
    let remote_by_key = by_key(remote);

    let mut all_keys: HashSet<&str> = HashSet::new();
    all_keys.extend(base_by_key.keys());
    all_keys.extend(local_by_key.keys());
    all_keys.extend(remote_by_key.keys());

    let mut merged: Vec<String> = Vec::new();
    for key in all_keys {
        let b = base_by_key.get(key);
        let l = local_by_key.get(key);
        let r = remote_by_key.get(key);

        let chosen = if l == r {
            l.copied()
        } else if b == l {
            r.copied()
        } else if b == r {
            l.copied()
        } else {
            // The same clock entry, closed or corrected differently on
            // both devices. Tracked time is data the user typed as much as
            // a title is, and silently keeping one side is how an hour
            // disappears.
            return Err(format!("merge conflict: clock entry \"{key}\" was changed differently on both devices"));
        };

        if let Some(line) = chosen {
            merged.push(line.clone());
        }
    }

    // Newest first -- clock entries have an objective chronological order
    // (unlike merge_history's prose, which keeps first-seen order), and
    // that's also this app's own prepend convention for new entries.
    merged.sort_by(|a, b| clock_key(b).cmp(&clock_key(a)));
    merged.extend(unkeyed);
    Ok(merged)
}

pub fn merge_text(base: &str, local: &str, remote: &str) -> Result<String, String> {
    if local == remote {
        return Ok(local.to_string());
    }

    // diffy relies on clean line endings for proper diffing
    let format_nl = |s: &str| {
        if s.is_empty() || s.ends_with('\n') {
            s.to_string()
        } else {
            format!("{}\n", s)
        }
    };

    let base_nl = format_nl(base);
    let local_nl = format_nl(local);
    let remote_nl = format_nl(remote);

    let options = MergeOptions::new();
    match options.merge(&base_nl, &local_nl, &remote_nl) {
        Ok(merged_string) => Ok(merged_string),
        Err(e) => {
            // An actual conflict! We actively fail here so the orchestrator
            // can delegate to the UI instead of silently eating user data.
            Err(format!("diffy conflict: {:?}", e))
        }
    }
}

/* --------- Tests -------- */

#[cfg(test)]
mod tests {
    use super::*;

    fn make_heading(id: &str, title: &str) -> Heading {
        Heading {
            id: id.to_string(),
            file_path: "test.org".to_string(),
            line_no: 1,
            last_line: 1,
            level: 1,
            priority: None,
            state: None,
            title: title.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn test_merge_text_clean() {
        let base = "Zeile 1\nZeile 2\nZeile 3\nZeile 4\nZeile 5";
        let local = "Zeile 1 Lokal\nZeile 2\nZeile 3\nZeile 4\nZeile 5";
        let remote = "Zeile 1\nZeile 2\nZeile 3\nZeile 4\nZeile 5 Remote";

        let merged = merge_text(base, local, remote).unwrap();

        assert_eq!(merged, "Zeile 1 Lokal\nZeile 2\nZeile 3\nZeile 4\nZeile 5 Remote\n");
    }

    #[test]
    fn test_merge_text_conflict() {
        let base = "Zeile 1\nZeile 2\nZeile 3";
        let local = "Zeile 1\nKonflikt Lokal\nZeile 3";
        let remote = "Zeile 1\nKonflikt Remote\nZeile 3";

        let merged = merge_text(base, local, remote);

        // This must fail actively so the UI can take over
        assert!(merged.is_err());
    }

    #[test]
    fn test_merge_headings_logic() {
        let mut base = make_heading("id1", "Base Title");
        base.state = Some("TODO".to_string());

        let mut local = base.clone();
        local.title = "Local Title".to_string();

        let mut remote = base.clone();
        remote.state = Some("DONE".to_string());

        let merged = merge_headings(&base, &local, &remote).unwrap();

        assert_eq!(merged.title, "Local Title");
        assert_eq!(merged.state, Some("DONE".to_string()));
        assert_eq!(merged.id, "id1");
    }

    /* --- merge_org_ast: structural safety -- must never bleed content
    across headings, corrupt siblings, or silently drop an edit --- */

    // Every task needs a real TODO state -- otherwise only "Task A" (the
    // first heading in the file) stays real structure, and B/C get
    // absorbed as its own content instead of remaining separate,
    // addressable headings (org_parser's rule: state or :project: tag
    // required once something else already opened the heading before
    // them).
    fn org_fixture(a_id: &str, b_id: &str, c_id: &str, a_extra: &str, b_extra: &str, c_extra: &str) -> String {
        format!(
            "* TODO Task A{a_extra}\n:PROPERTIES:\n:CUSTOM_ID: {a_id}\n:END:\n\
             * TODO Task B{b_extra}\n:PROPERTIES:\n:CUSTOM_ID: {b_id}\n:END:\n\
             * TODO Task C{c_extra}\n:PROPERTIES:\n:CUSTOM_ID: {c_id}\n:END:\n"
        )
    }

    /// Deliberately changed: this used to assert that local's title won
    /// and remote's was gone from the output.
    ///
    /// That was the silent loss this module's own contract rules out
    /// ("Both inputs survive or neither"). Two people renaming the same
    /// task is precisely what manual resolution is for, and the
    /// orchestrator already falls back to it on `Err`.
    #[test]
    fn a_heading_renamed_differently_on_both_sides_is_a_conflict() {
        let base = org_fixture("aaa", "bbb", "ccc", "", "", "");
        let local = org_fixture("aaa", "bbb", "ccc", "", " Local Edit", "");
        let remote = org_fixture("aaa", "bbb", "ccc", "", " Remote Edit", "");

        let err = merge_org_ast("test.org", &base, &local, &remote).unwrap_err();

        assert!(err.contains("merge conflict"), "{err}");
    }

    /// The structural half of the test above, kept: merging one heading
    /// must not disturb a single byte of its siblings.
    ///
    /// B's two edits are chosen so they genuinely merge (a rename on one
    /// side, a state change on the other), which is what lets the merge
    /// reach the emit loop at all.
    #[test]
    fn merging_one_heading_leaves_untouched_siblings_byte_identical() {
        let base = org_fixture("aaa", "bbb", "ccc", "", "", "");
        let local = org_fixture("aaa", "bbb", "ccc", "", " Local Edit", "");
        let remote = base.replace("* TODO Task B", "* DONE Task B");

        let merged = merge_org_ast("test.org", &base, &local, &remote).unwrap();

        assert!(merged.contains("* TODO Task A\n:PROPERTIES:\n:CUSTOM_ID: aaa\n:END:"), "Task A survives untouched: {merged}");
        assert!(merged.contains("* TODO Task C\n:PROPERTIES:\n:CUSTOM_ID: ccc\n:END:"), "Task C survives untouched: {merged}");
        assert!(merged.contains("Task B Local Edit"), "the rename survives: {merged}");
        assert!(merged.contains("* DONE Task B"), "and so does the state change: {merged}");
    }

    /// The exact bug this module's CUSTOM_ID gate exists to prevent: with a
    /// stable id, a title rename on one side and an unrelated field edit
    /// on the other side of the SAME heading must both survive -- neither
    /// silently discarded because the title change looked like "this
    /// heading was deleted and a different one was added".
    #[test]
    fn title_rename_on_one_side_and_unrelated_field_edit_on_the_other_both_survive() {
        let base = org_fixture("aaa", "bbb", "ccc", "", "", "");
        let local = org_fixture("aaa", "bbb", "ccc", "", " Renamed", "");
        let remote_text = base.replace("* TODO Task B\n:PROPERTIES:", "* NEXT Task B\n:PROPERTIES:");
        // ^ remote changes B's OWN state marker (simulated directly via
        // text substitution, since org_fixture doesn't expose a state
        // knob) -- an edit to a DIFFERENT field than the title.

        let merged = merge_org_ast("test.org", &base, &local, &remote_text).unwrap();

        assert!(merged.contains("Task B Renamed"), "local's title rename survived: {merged}");
        assert!(merged.contains("NEXT"), "remote's unrelated state edit also survived, not silently dropped: {merged}");
        assert!(merged.contains("* TODO Task A"), "A untouched: {merged}");
        assert!(merged.contains("* TODO Task C"), "C untouched: {merged}");
    }

    #[test]
    fn refuses_to_auto_merge_when_any_heading_lacks_a_custom_id() {
        // Task C has no CUSTOM_ID at all -- must refuse the WHOLE file's
        // auto-merge (falls back to manual conflict resolution in the
        // caller) rather than silently using a fragile title-based key for
        // just that one heading. Task C needs a real state to stay a
        // separate heading at all (org_parser's rule: state or :project:
        // tag required once something else already opened the heading
        // before it) -- otherwise it'd just be absorbed as Task A's own
        // content, and there'd be no second heading to lack a CUSTOM_ID.
        let base = "* Task A\n:PROPERTIES:\n:CUSTOM_ID: aaa\n:END:\n* TODO Task C\n";
        let local = "* Task A Renamed\n:PROPERTIES:\n:CUSTOM_ID: aaa\n:END:\n* TODO Task C\n";
        let remote = "* Task A\n:PROPERTIES:\n:CUSTOM_ID: aaa\n:END:\n* TODO Task C Edited\n";

        assert!(merge_org_ast("test.org", base, local, remote).is_err());
    }

    #[test]
    fn a_heading_with_no_common_base_and_differing_content_on_both_sides_is_a_conflict_not_a_silent_pick() {
        // Same CUSTOM_ID appears in local AND remote but not in base (e.g.
        // both devices independently refiled the same heading in, see the
        // branch's own doc comment) -- with genuinely different content on
        // each side, there's no principled way to 3-way-merge it, so this
        // must fail rather than silently prefer one side and discard the
        // other's tags/properties/state.
        let base = "* Task A\n:PROPERTIES:\n:CUSTOM_ID: aaa\n:END:\n";
        let local = "* Task A\n:PROPERTIES:\n:CUSTOM_ID: aaa\n:END:\n* Task Z :personal:\n:PROPERTIES:\n:CUSTOM_ID: zzz\n:END:\n";
        let remote = "* Task A\n:PROPERTIES:\n:CUSTOM_ID: aaa\n:END:\n* Task Z Different :work:\n:PROPERTIES:\n:CUSTOM_ID: zzz\n:END:\n";

        let result = merge_org_ast("test.org", base, local, remote);
        assert!(result.is_err(), "must not silently fabricate a merge for a heading with no common base: {result:?}");
    }

    #[test]
    fn a_heading_with_no_common_base_but_identical_content_on_both_sides_merges_cleanly() {
        // Same scenario as above, but both sides happened to converge on
        // IDENTICAL content -- genuinely unambiguous, must succeed.
        let base = "* Task A\n:PROPERTIES:\n:CUSTOM_ID: aaa\n:END:\n";
        let same_new_heading = "* Task Z :personal:\n:PROPERTIES:\n:CUSTOM_ID: zzz\n:END:\n";
        let local = format!("{base}{same_new_heading}");
        let remote = format!("{base}{same_new_heading}");

        let merged = merge_org_ast("test.org", base, &local, &remote).unwrap();
        assert!(merged.contains("Task Z"));
    }

    #[test]
    fn three_headings_where_only_the_middle_one_actually_changed_on_either_side_is_a_pure_no_op_for_the_others() {
        let base = org_fixture("aaa", "bbb", "ccc", "", "", "");
        // Local changes ONLY B; remote is byte-identical to base.
        let local = org_fixture("aaa", "bbb", "ccc", "", " Local Only", "");

        let merged = merge_org_ast("test.org", &base, &local, &base).unwrap();

        assert!(merged.contains("Task A\n"));
        assert!(merged.contains("Task B Local Only"));
        assert!(merged.contains("Task C\n"));
        // Exactly 3 headings in, 3 headings out -- nothing duplicated or dropped.
        assert_eq!(merged.matches("CUSTOM_ID").count(), 3);
    }

    /* --- merge_clocks --- */

    const OPEN: &str = "CLOCK: [2026-08-17 Mon 18:00]";
    const CLOSED_LOCAL: &str = "CLOCK: [2026-08-17 Mon 18:00]--[2026-08-17 Mon 18:30] =>  0:30";
    const CLOSED_REMOTE: &str = "CLOCK: [2026-08-17 Mon 18:00]--[2026-08-17 Mon 19:00] =>  1:00";
    const OLDER_CLOSED: &str = "CLOCK: [2026-08-16 Sun 09:00]--[2026-08-16 Sun 09:15] =>  0:15";

    #[test]
    fn unchanged_entry_present_on_all_three_sides_survives_as_is() {
        let lines = vec![CLOSED_LOCAL.to_string()];
        let merged = merge_clocks(&lines, &lines, &lines).unwrap();
        assert_eq!(merged, vec![CLOSED_LOCAL.to_string()]);
    }

    #[test]
    fn an_open_clock_closed_on_only_one_side_collapses_to_the_closed_entry_not_a_ghost_duplicate() {
        let base = vec![OPEN.to_string()];
        let local = vec![OPEN.to_string()]; // local left it running
        let remote = vec![CLOSED_REMOTE.to_string()]; // remote stopped it

        let merged = merge_clocks(&base, &local, &remote).unwrap();
        assert_eq!(merged, vec![CLOSED_REMOTE.to_string()], "remote's close wins, no leftover open duplicate");
    }

    #[test]
    fn an_open_clock_closed_on_the_other_side_also_collapses_correctly() {
        let base = vec![OPEN.to_string()];
        let local = vec![CLOSED_LOCAL.to_string()]; // local stopped it
        let remote = vec![OPEN.to_string()]; // remote left it running

        let merged = merge_clocks(&base, &local, &remote).unwrap();
        assert_eq!(merged, vec![CLOSED_LOCAL.to_string()], "local's close wins over remote's still-open copy");
    }

    /// Deliberately changed: this used to assert that local silently won.
    ///
    /// Tracked time is data the user entered, and half an hour vanishing
    /// because two devices stopped the same timer differently is exactly
    /// what this module's contract ("Both inputs survive or neither")
    /// promises will not happen. The file now goes to manual resolution
    /// with both versions intact.
    #[test]
    fn both_sides_closing_the_same_clock_differently_is_a_conflict() {
        let base = vec![OPEN.to_string()];
        let local = vec![CLOSED_LOCAL.to_string()];
        let remote = vec![CLOSED_REMOTE.to_string()];

        let err = merge_clocks(&base, &local, &remote).unwrap_err();

        assert!(err.contains("merge conflict"), "{err}");
    }

    #[test]
    fn new_timers_started_independently_on_both_sides_are_both_kept() {
        let base: Vec<String> = vec![];
        let local = vec![CLOSED_LOCAL.to_string()];
        let remote = vec![OLDER_CLOSED.to_string()];

        let mut merged = merge_clocks(&base, &local, &remote).unwrap();
        merged.sort();
        let mut expected = vec![CLOSED_LOCAL.to_string(), OLDER_CLOSED.to_string()];
        expected.sort();
        assert_eq!(merged, expected, "neither side's independently-started timer is dropped");
    }

    #[test]
    fn result_is_sorted_newest_first_by_start_timestamp() {
        let base: Vec<String> = vec![];
        let local = vec![CLOSED_LOCAL.to_string(), OLDER_CLOSED.to_string()];
        let remote = vec![];

        let merged = merge_clocks(&base, &local, &remote).unwrap();
        assert_eq!(merged, vec![CLOSED_LOCAL.to_string(), OLDER_CLOSED.to_string()], "2026-08-17 sorts before 2026-08-16");
    }

    #[test]
    fn a_heading_with_no_clocks_anywhere_merges_to_an_empty_list() {
        let empty: Vec<String> = vec![];
        assert_eq!(merge_clocks(&empty, &empty, &empty).unwrap(), Vec::<String>::new());
    }

    #[test]
    fn unrelated_headings_own_clocks_are_never_touched_by_a_merge_elsewhere() {
        // merge_clocks operates on ONE heading's own list at a time (see
        // merge_headings, which calls it once per matched heading) -- this
        // just confirms a heading with its own stable, unchanged clock list
        // isn't affected by what CLOSED_LOCAL/CLOSED_REMOTE represent for a
        // *different* heading in the same file.
        let other_headings_clock = vec![OLDER_CLOSED.to_string()];
        let merged = merge_clocks(&other_headings_clock, &other_headings_clock, &other_headings_clock).unwrap();
        assert_eq!(merged, other_headings_clock);
    }

    #[test]
    fn a_malformed_unkeyed_line_is_preserved_via_plain_union_dedup_not_silently_dropped() {
        let base = vec!["CLOCK: garbage".to_string()];
        let local = vec!["CLOCK: garbage".to_string()];
        let remote = vec!["CLOCK: garbage".to_string(), CLOSED_LOCAL.to_string()];

        let merged = merge_clocks(&base, &local, &remote).unwrap();
        assert!(merged.contains(&"CLOCK: garbage".to_string()));
        assert!(merged.contains(&CLOSED_LOCAL.to_string()));
        assert_eq!(merged.iter().filter(|l| l.as_str() == "CLOCK: garbage").count(), 1, "deduped, not one per side");
    }

    // ---- ADVERSARIAL VERIFICATION (temporary) ----
    const B: &str = "* TODO Buy milk\n:PROPERTIES:\n:CUSTOM_ID: h1\n:END:\n";
    const L: &str = "* DONE Buy oat milk\n:PROPERTIES:\n:CUSTOM_ID: h1\n:END:\n";
    const R: &str = "* WAITING Buy soy milk\n:PROPERTIES:\n:CUSTOM_ID: h1\n:END:\n";

    #[test]
    fn adv_case_a_state_and_title_divergence() {
        let out = merge_org_ast("t.org", B, L, R);
        println!("CASE A => {:?}", out);
    }

    #[test]
    fn adv_case_b_delete_vs_modify() {
        let base = "* TODO One\n:PROPERTIES:\n:CUSTOM_ID: h1\n:END:\nold body\n* TODO Two\n:PROPERTIES:\n:CUSTOM_ID: h2\n:END:\n";
        let local = "* TODO Two\n:PROPERTIES:\n:CUSTOM_ID: h2\n:END:\n";
        let remote = "* TODO One\n:PROPERTIES:\n:CUSTOM_ID: h1\n:END:\nBRAND NEW BODY\n* TODO Two\n:PROPERTIES:\n:CUSTOM_ID: h2\n:END:\n";
        let out = merge_org_ast("t.org", base, local, remote);
        println!("CASE B => {:?}", out);
    }

    #[test]
    fn adv_case_c_property_divergence() {
        let base = "* TODO One\n:PROPERTIES:\n:CUSTOM_ID: h1\n:FOO: base\n:END:\nbody\n";
        let local = "* TODO One\n:PROPERTIES:\n:CUSTOM_ID: h1\n:FOO: local\n:END:\nbody local\n";
        let remote = "* TODO One\n:PROPERTIES:\n:CUSTOM_ID: h1\n:FOO: remote\n:END:\nbody\n";
        let out = merge_org_ast("t.org", base, local, remote);
        println!("CASE C => {:?}", out);
    }
}
