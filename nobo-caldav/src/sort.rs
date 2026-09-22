//! Merging list order between the org file and the server.
//!
//! CalDAV has no inherent order for VTODOs. Apple invented
//! `X-APPLE-SORT-ORDER` for it and Nextcloud Tasks honors it, so that
//! property is the only ordering the two sides can agree on. On this side
//! the order is physical: the sequence of headings in the org file.
//!
//! The merge rule, in one sentence: **whoever moved something wins, and if
//! the user moved something locally, the file wins.** Local order is the
//! most recent deliberate act; silently replacing it with the server's
//! would undo a drag the user just performed.

/// One task as the server currently has it.
#[derive(Debug, Clone, PartialEq)]
pub struct ServerEntry {
    pub uid: String,
    /// `None` for a task the server never assigned one to -- common for
    /// tasks created by clients that don't write the property.
    pub sort_order: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SortMerge {
    /// The uids in the order the file should end up in.
    pub order: Vec<String>,
    /// `X-APPLE-SORT-ORDER` values that have to be pushed, and only those:
    /// a value already consistent with the merged order is left alone, so
    /// moving one task does not rewrite the whole list on the server.
    pub assignments: Vec<(String, i64)>,
    /// Whether the local file order drove the result -- i.e. whether the
    /// user rearranged the file since the last run. What decides which of
    /// the three orders wins (see the merge below); the caller writes the
    /// file either way, and write_file decides for itself whether that is
    /// a change at all.
    pub local_reordered: bool,
}

/// Distance between generated values. Wide enough that inserting between
/// two neighbours normally finds room without renumbering anything else.
const STEP: i64 = 1000;

/// Merges the three orders.
///
/// - `base_order`: uids in the order recorded at the last sync.
/// - `server`: what the server has now.
/// - `local_order`: uids in the order the file has now.
pub fn merge(base_order: &[String], server: &[ServerEntry], local_order: &[String]) -> SortMerge {
    let known_remotely: std::collections::HashSet<&str> = server.iter().map(|e| e.uid.as_str()).collect();

    // What should be in the file afterwards: everything the server has,
    // plus anything the file has that the server has never seen (created
    // here, not pushed yet). A task in `base` that is now in neither was
    // deleted and simply falls out.
    let locally_new: Vec<&String> = local_order
        .iter()
        .filter(|u| !known_remotely.contains(u.as_str()) && !base_order.contains(u))
        .collect();
    let survives = |uid: &String| known_remotely.contains(uid.as_str()) || locally_new.contains(&uid);

    let local_live: Vec<String> = local_order.iter().filter(|u| survives(u)).cloned().collect();

    // Only tasks BOTH sides still know can say anything about whether the
    // user reordered: an addition or a deletion shifts positions without
    // anyone having dragged anything.
    let local_common: Vec<&String> = local_live.iter().filter(|u| base_order.contains(u)).collect();
    let base_common: Vec<&String> =
        base_order.iter().filter(|u| survives(u) && local_order.contains(u)).collect();
    let local_reordered = local_common != base_common;

    // `None` means the server has no opinion about where the task goes --
    // NOT that it belongs last. Sorting those to the end would drag a task
    // the user just placed in the middle down to the bottom.
    let mut placed: Vec<&ServerEntry> = server.iter().filter(|e| e.sort_order.is_some()).collect();
    placed.sort_by(|a, b| a.sort_order.cmp(&b.sort_order).then_with(|| a.uid.cmp(&b.uid)));
    let placed_order: Vec<String> = placed.into_iter().map(|e| e.uid.clone()).collect();

    let order = if local_reordered {
        // The file drives; anything it does not mention slots in at the
        // position the server gives it.
        insert_missing(&local_live, &placed_order)
    } else {
        // The server drives -- but only for what it has actually placed.
        // Everything else keeps the position the file gives it.
        insert_missing(&placed_order, &local_live)
    };
    // Whatever neither sequence mentioned (unplaced remotely AND absent
    // from the file) goes last, in a stable order.
    let mut order = order;
    let mut leftover: Vec<String> = server
        .iter()
        .map(|e| e.uid.clone())
        .filter(|uid| !order.contains(uid))
        .collect();
    leftover.sort();
    order.extend(leftover);

    let assignments = assign_values(&order, server);
    SortMerge { order, assignments, local_reordered }
}

/// Adds every uid of `from` that `into` lacks, each directly after
/// whichever of `from`'s earlier entries `into` already has.
///
/// Appending them all at the end would be simpler but wrong in both
/// directions: a remote task another client placed mid-list would jump to
/// the bottom, and a task the user just typed into the middle of the file
/// would be moved away from where they put it.
fn insert_missing(into: &[String], from: &[String]) -> Vec<String> {
    let mut out = into.to_vec();
    for (index, uid) in from.iter().enumerate() {
        if out.contains(uid) {
            continue;
        }
        let anchor = from[..index].iter().rev().find_map(|prev| out.iter().position(|o| o == prev));
        match anchor {
            Some(position) => out.insert(position + 1, uid.clone()),
            None => out.insert(0, uid.clone()),
        }
    }
    out
}

/// Values to push so the server's order matches `order`.
///
/// Keeps every value that is already strictly increasing in the merged
/// order and only invents new ones where it has to -- moving one task
/// should be one PUT, not a rewrite of the whole list. Falls back to
/// renumbering everything when the existing values leave no room.
fn assign_values(order: &[String], server: &[ServerEntry]) -> Vec<(String, i64)> {
    let current: std::collections::HashMap<&str, Option<i64>> =
        server.iter().map(|e| (e.uid.as_str(), e.sort_order)).collect();
    let existing: Vec<Option<i64>> =
        order.iter().map(|uid| current.get(uid.as_str()).copied().flatten()).collect();

    // Two ways to decide which values to leave alone, because neither wins
    // everywhere. The longest increasing run re-places exactly the entry
    // that moved -- one drag, one write -- but where several runs tie it
    // can keep the one that leaves no room below it. The left-to-right
    // walk is the opposite: it keeps early values, which is what saves the
    // case of two neighbours swapping. Cheapest answer wins; both are a
    // pass over the list.
    let cheapest = [longest_increasing(&existing), rising_prefix(&existing)]
        .into_iter()
        .filter_map(|keep| assign_around(order, &existing, &keep))
        .min_by_key(Vec::len);

    cheapest.unwrap_or_else(|| renumber(order, server))
}

/// Values for every position not in `keep`, or `None` when one of them has
/// nowhere to go -- then the caller renumbers instead.
fn assign_around(
    order: &[String],
    existing: &[Option<i64>],
    keep: &std::collections::HashSet<usize>,
) -> Option<Vec<(String, i64)>> {
    let mut assignments = Vec::new();
    let mut last: Option<i64> = None;

    for (index, uid) in order.iter().enumerate() {
        if keep.contains(&index) {
            last = existing[index];
            continue;
        }

        // Between the previous value and the next one that is staying, so
        // nothing after this has to move either.
        let next_fixed = ((index + 1)..order.len())
            .filter(|ahead| keep.contains(ahead))
            .find_map(|ahead| existing[ahead])
            .filter(|value| last.is_none_or(|previous| *value > previous));

        let value = match (last, next_fixed) {
            (Some(previous), Some(next)) if next - previous > 1 => previous + (next - previous) / 2,
            (Some(_), Some(_)) => return None,
            (Some(previous), None) => previous + STEP,
            (None, Some(next)) if next > STEP => next - STEP,
            (None, Some(_)) => return None,
            (None, None) => STEP,
        };
        assignments.push((uid.clone(), value));
        last = Some(value);
    }

    Some(assignments)
}

/// Indices a left-to-right walk would leave alone: every value that is
/// still larger than the last one kept.
fn rising_prefix(values: &[Option<i64>]) -> std::collections::HashSet<usize> {
    let mut keep = std::collections::HashSet::new();
    let mut last: Option<i64> = None;
    for (index, value) in values.iter().enumerate() {
        if let Some(value) = value {
            if last.is_none_or(|previous| *value > previous) {
                keep.insert(index);
                last = Some(*value);
            }
        }
    }
    keep
}

/// Indices of the longest strictly increasing run of values, in order.
///
/// Positions without a value of their own are never part of it -- they
/// need one assigned either way.
fn longest_increasing(values: &[Option<i64>]) -> std::collections::HashSet<usize> {
    let candidates: Vec<(usize, i64)> =
        values.iter().enumerate().filter_map(|(index, value)| value.map(|v| (index, v))).collect();

    // Patience sorting: `ends[length - 1]` is the position in `candidates`
    // that ends the shortest-tailed increasing run of that length so far.
    let mut ends: Vec<usize> = Vec::new();
    let mut previous: Vec<Option<usize>> = vec![None; candidates.len()];

    for (position, (_, value)) in candidates.iter().enumerate() {
        let slot = ends.partition_point(|end| candidates[*end].1 < *value);
        previous[position] = slot.checked_sub(1).map(|earlier| ends[earlier]);
        match ends.get_mut(slot) {
            Some(end) => *end = position,
            None => ends.push(position),
        }
    }

    let mut keep = std::collections::HashSet::new();
    let mut cursor = ends.last().copied();
    while let Some(position) = cursor {
        keep.insert(candidates[position].0);
        cursor = previous[position];
    }
    keep
}

/// Last resort: every task gets a fresh, evenly spaced value. Costs one
/// PUT per task, which is why it only happens when the existing values
/// have no gap left to place something in.
///
/// Anchored at the list's own lowest value rather than at zero. These
/// values share a scale with `CREATED` (seconds since 2001-01-01), so
/// numbering from 1 would place the whole list decades before anything
/// another client adds later.
fn renumber(order: &[String], server: &[ServerEntry]) -> Vec<(String, i64)> {
    let base = server.iter().filter_map(|e| e.sort_order).min().unwrap_or(STEP);
    order
        .iter()
        .enumerate()
        .map(|(index, uid)| (uid.clone(), base + index as i64 * STEP))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uids(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    fn server(entries: &[(&str, Option<i64>)]) -> Vec<ServerEntry> {
        entries.iter().map(|(uid, sort_order)| ServerEntry { uid: uid.to_string(), sort_order: *sort_order }).collect()
    }

    fn order_of(merge: &SortMerge) -> Vec<&str> {
        merge.order.iter().map(String::as_str).collect()
    }

    /// The value each task ends up with on the server: the pushed one
    /// where there is one, otherwise what it already had. Strictly
    /// increasing is the only real correctness condition -- otherwise the
    /// server hands back a different order than the file has.
    fn effective_values(merge: &SortMerge, remote: &[ServerEntry]) -> Vec<i64> {
        merge
            .order
            .iter()
            .map(|uid| {
                merge
                    .assignments
                    .iter()
                    .find(|(u, _)| u == uid)
                    .map(|(_, v)| *v)
                    .or_else(|| remote.iter().find(|e| &e.uid == uid).and_then(|e| e.sort_order))
                    .unwrap_or_else(|| panic!("{uid} ended up with no value at all"))
            })
            .collect()
    }

    /* --- who wins --- */

    /// Nobody touched the file, so a drag performed in Nextcloud or
    /// Reminders is simply the newer truth.
    #[test]
    fn a_server_side_reorder_is_adopted_when_the_file_was_not_touched() {
        let base = uids(&["a", "b", "c"]);
        let remote = server(&[("a", Some(3000)), ("b", Some(1000)), ("c", Some(2000))]);

        let merged = merge(&base, &remote, &base);

        assert_eq!(order_of(&merged), ["b", "c", "a"]);
        assert!(!merged.local_reordered);
        assert!(merged.assignments.is_empty(), "nothing to push: {:?}", merged.assignments);
    }

    /// The user just dragged something here; replacing that with the
    /// server's order would undo the act they performed a second ago.
    #[test]
    fn a_local_reorder_wins_and_is_pushed() {
        let base = uids(&["a", "b", "c"]);
        let remote = server(&[("a", Some(1000)), ("b", Some(2000)), ("c", Some(3000))]);

        let merged = merge(&base, &remote, &uids(&["c", "a", "b"]));

        assert_eq!(order_of(&merged), ["c", "a", "b"]);
        assert!(merged.local_reordered);
        assert!(!merged.assignments.is_empty());
    }

    #[test]
    fn when_both_sides_moved_something_the_file_wins() {
        let base = uids(&["a", "b", "c"]);
        let remote = server(&[("a", Some(3000)), ("b", Some(1000)), ("c", Some(2000))]);

        let merged = merge(&base, &remote, &uids(&["c", "b", "a"]));

        assert_eq!(order_of(&merged), ["c", "b", "a"]);
        assert!(merged.local_reordered);
    }

    /* --- additions and deletions --- */

    /// An addition or a deletion shifts every position after it. Reading
    /// that as "the user reordered the list" would push a pointless
    /// renumbering of everything on every single new task.
    #[test]
    fn a_new_task_alone_does_not_count_as_a_local_reorder() {
        let base = uids(&["a", "b"]);
        let remote = server(&[("a", Some(1000)), ("b", Some(2000)), ("c", Some(1500))]);

        let merged = merge(&base, &remote, &uids(&["a", "b"]));

        assert!(!merged.local_reordered);
        assert_eq!(order_of(&merged), ["a", "c", "b"], "server position honored");
    }

    #[test]
    fn a_deleted_task_alone_does_not_count_as_a_local_reorder() {
        let base = uids(&["a", "b", "c"]);
        let remote = server(&[("a", Some(1000)), ("c", Some(3000))]);

        let merged = merge(&base, &remote, &uids(&["a", "c"]));

        assert!(!merged.local_reordered);
        assert_eq!(order_of(&merged), ["a", "c"]);
    }

    #[test]
    fn a_task_gone_from_the_server_drops_out_of_the_order() {
        let base = uids(&["a", "b", "c"]);
        let remote = server(&[("a", Some(1000)), ("c", Some(3000))]);

        let merged = merge(&base, &remote, &uids(&["a", "b", "c"]));

        assert_eq!(order_of(&merged), ["a", "c"]);
    }

    /// Appending every new remote task would be simpler and wrong: one
    /// deliberately placed in the middle from another client would jump to
    /// the bottom the first time this device saw it.
    #[test]
    fn a_new_remote_task_lands_where_the_server_put_it_even_when_the_file_wins() {
        let base = uids(&["a", "b"]);
        let remote = server(&[("a", Some(1000)), ("neu", Some(1500)), ("b", Some(2000))]);

        let merged = merge(&base, &remote, &uids(&["b", "a"]));

        assert!(merged.local_reordered);
        // Local order b,a kept; "neu" follows "a", which precedes it on
        // the server.
        assert_eq!(order_of(&merged), ["b", "a", "neu"]);
    }

    #[test]
    fn a_remote_task_that_sorts_first_lands_at_the_top() {
        let base = uids(&["a", "b"]);
        let remote = server(&[("neu", Some(500)), ("a", Some(1000)), ("b", Some(2000))]);

        let merged = merge(&base, &remote, &uids(&["b", "a"]));

        assert_eq!(order_of(&merged), ["neu", "b", "a"]);
    }

    /* --- what actually gets pushed --- */

    /// The point of not simply renumbering: moving one task should be one
    /// PUT, not a rewrite of the entire list on the server.
    #[test]
    fn moving_one_task_pushes_only_that_task() {
        let base = uids(&["a", "b", "c", "d"]);
        let remote = server(&[("a", Some(1000)), ("b", Some(2000)), ("c", Some(3000)), ("d", Some(4000))]);

        let merged = merge(&base, &remote, &uids(&["a", "c", "b", "d"]));

        // Which of the two swapped tasks gets the new value is the
        // algorithm's choice -- the guarantee is that only ONE does, and
        // that the result still reads back in the merged order.
        assert_eq!(merged.assignments.len(), 1, "{:?}", merged.assignments);
        assert!(effective_values(&merged, &remote).windows(2).all(|w| w[0] < w[1]));
    }

    /// Two adjacent values with a swap still resolve in a single push --
    /// the fallback below is genuinely a last resort, not the common case.
    #[test]
    fn swapping_the_last_two_needs_no_renumbering() {
        let base = uids(&["a", "b"]);
        let remote = server(&[("a", Some(5)), ("b", Some(6))]);

        let merged = merge(&base, &remote, &uids(&["b", "a"]));

        assert_eq!(merged.assignments.len(), 1, "{:?}", merged.assignments);
        assert!(effective_values(&merged, &remote).windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn a_task_the_server_never_placed_gets_a_value() {
        let base = uids(&["a"]);
        let remote = server(&[("a", Some(1000)), ("neu", None)]);

        let merged = merge(&base, &remote, &uids(&["a"]));

        assert_eq!(order_of(&merged), ["a", "neu"], "unplaced tasks sort last");
        assert_eq!(merged.assignments, vec![("neu".to_string(), 2000)]);
    }

    /// Values assigned must be strictly increasing in the merged order, or
    /// the server would hand back a different order than the file has.
    #[test]
    fn the_pushed_values_reproduce_the_merged_order() {
        let base = uids(&["a", "b", "c"]);
        let remote = server(&[("a", Some(1000)), ("b", Some(1001)), ("c", Some(1002))]);

        let merged = merge(&base, &remote, &uids(&["c", "b", "a"]));

        let effective = effective_values(&merged, &remote);
        assert!(effective.windows(2).all(|w| w[0] < w[1]), "not increasing: {effective:?}");
    }

    /// Something has to go BETWEEN two adjacent values, and there is no
    /// integer there. Renumbering the list is the only way out, and it
    /// must still produce the merged order.
    #[test]
    fn a_gap_with_no_room_falls_back_to_renumbering_the_whole_list() {
        let base = uids(&["a", "b"]);
        // "neu" has no value of its own and belongs between a and b.
        let remote = server(&[("a", Some(1000)), ("b", Some(1001)), ("neu", None)]);

        let merged = merge(&base, &remote, &uids(&["a", "neu", "b"]));

        assert_eq!(order_of(&merged), ["a", "neu", "b"]);
        assert_eq!(merged.assignments.len(), 3, "every task renumbered: {:?}", merged.assignments);
        assert!(effective_values(&merged, &remote).windows(2).all(|w| w[0] < w[1]));
    }

    /// A task created here has no server position yet. Dropping it out of
    /// the merged order would lose the place the user typed it into --
    /// and, since the caller writes the file from this order, the task
    /// itself.
    #[test]
    fn a_locally_created_task_keeps_the_position_it_was_typed_into() {
        let base = uids(&["a", "b"]);
        let remote = server(&[("a", Some(1000)), ("b", Some(2000))]);

        let merged = merge(&base, &remote, &uids(&["a", "lokal-neu", "b"]));

        assert_eq!(order_of(&merged), ["a", "lokal-neu", "b"]);
        assert!(!merged.local_reordered, "inserting is not reordering");
        let assigned = merged.assignments.iter().find(|(u, _)| u == "lokal-neu").expect("needs a value");
        assert!((1000..2000).contains(&assigned.1), "must land between a and b, got {}", assigned.1);
    }

    /// `None` means "the server has no opinion", not "put it last" -- a
    /// task another client created without the property would otherwise be
    /// dragged to the bottom away from where the user placed it.
    #[test]
    fn an_unplaced_remote_task_keeps_its_local_position() {
        let base = uids(&["a", "b"]);
        let remote = server(&[("a", Some(1000)), ("b", Some(2000)), ("ohne-wert", None)]);

        let merged = merge(&base, &remote, &uids(&["a", "ohne-wert", "b"]));

        assert_eq!(order_of(&merged), ["a", "ohne-wert", "b"]);
    }

    /// Only when the file has nothing to say about it either.
    #[test]
    fn an_unplaced_remote_task_the_file_does_not_know_goes_last() {
        let base = uids(&["a"]);
        let remote = server(&[("a", Some(1000)), ("ohne-wert", None)]);

        let merged = merge(&base, &remote, &uids(&["a"]));

        assert_eq!(order_of(&merged), ["a", "ohne-wert"]);
    }

    /* --- degenerate input --- */

    #[test]
    fn an_empty_list_stays_empty() {
        let merged = merge(&[], &[], &[]);
        assert!(merged.order.is_empty());
        assert!(merged.assignments.is_empty());
        assert!(!merged.local_reordered);
    }

    #[test]
    fn a_first_sync_with_no_base_takes_the_server_order() {
        let remote = server(&[("b", Some(1000)), ("a", Some(2000))]);

        let merged = merge(&[], &remote, &[]);

        assert_eq!(order_of(&merged), ["b", "a"]);
        assert!(merged.assignments.is_empty());
    }

    /// Two tasks with the same value must not make the order depend on
    /// whatever sequence the server happened to answer in.
    #[test]
    fn equal_values_are_broken_deterministically() {
        let remote = server(&[("b", Some(1000)), ("a", Some(1000))]);

        let first = merge(&[], &remote, &[]);
        let second = merge(&[], &server(&[("a", Some(1000)), ("b", Some(1000))]), &[]);

        assert_eq!(first.order, second.order);
        assert_eq!(order_of(&first), ["a", "b"]);
    }
}
