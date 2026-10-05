//! The one place a heading is written.
//!
//! Every heading-mutating command used to spell the same sequence out for
//! itself: parse the file from disk, resolve the target by identity,
//! clone the heading, capture its ancestors, build a block, splice it in
//! at `line_no - 1 .. last_line`, touch the ancestors, refresh the index.
//! A dozen call sites, a dozen copies, and the parts that were already
//! centralised were only centralised where somebody remembered to call
//! them -- `replace_heading_block` exists precisely to own that
//! `line_no - 1`, and the app's own main edit path did the arithmetic
//! itself anyway.
//!
//! This makes the sequence the thing you call:
//!
//! ```ignore
//! let mut edit = heading_utils::open_edit(&path, &target, &crate::core::errors::subject::heading())?;
//! let block = heading_utils::build_org_heading_block(edit.heading(), &payload);
//! edit.replace(block)?;
//! edit.touch_ancestors()?;
//! edit.commit()?;
//! ```
//!
//! Three properties fall out of that shape rather than out of discipline:
//!
//! - **One read.** The lines a block is spliced INTO are the same lines
//!   the heading's position was parsed FROM. Previously `parse_from_disk`
//!   read the file and `splice_lines` read it again, and in between a
//!   sync pull, a CalDAV pull or another editor could insert lines above
//!   the heading -- the range still fit, so nothing complained, and the
//!   block landed that many lines too high. `read_and_parse` was written
//!   to close exactly this window and only the batch paths ever used it.
//!
//! - **One write.** Everything happens in memory and lands in a single
//!   `write_lines`. `touch_ancestors` used to write the whole file once
//!   per ancestor; a subtask three levels deep meant four full rewrites,
//!   each one an atomic write with its own fsync.
//!
//! - **One conversion.** `block_range` is the only code that turns a
//!   1-based `line_no`/`last_line` into a 0-based half-open range, and it
//!   refuses a range that no longer fits instead of trimming it to size.
//!
//! What this deliberately does NOT do is decide policy. It has no opinion
//! on history snapshots, triggers, repeat handling or the index -- those
//! differ per command and stay at the call site. It owns finding the
//! region and writing it, nothing else.

use super::batch::block_range;
use super::serialize::{build_org_heading_block, read_and_parse, touch_last_activity};
use super::tree::{ancestors_of, find_subtree_end_line, subtree_line_range};
use crate::core::heading_identity;
use crate::core::models::{Heading, HeadingTarget, HeadingUpdatePayload};
use crate::core::org_parser;
use crate::utils::fs_utils;
use std::path::{Path, PathBuf};

/// One file, read once: its lines, the headings parsed from exactly those
/// lines, and the index of the one this edit is aimed at.
///
/// Mutations are applied to `lines` and nothing reaches disk until
/// `commit`. `flat` keeps describing the file as it was READ -- which is
/// what the ancestor touches want, since every ancestor's own block sits
/// strictly above the edited heading and so keeps its position no matter
/// how the edit below it changes the line count.
pub struct HeadingEdit {
    path: PathBuf,
    lines: Vec<String>,
    flat: Vec<Heading>,
    idx: usize,
    /// What the file looked like when it was read, so `commit` can tell
    /// whether anything landed in it since -- see `commit`.
    read_as: Fingerprint,
    /// Set by the first structural mutation. A second one would be
    /// computing its range from `flat`, which the first has just made
    /// stale -- see `refuse_second_mutation`.
    mutated: bool,
}

/// Line count plus a hash of the lines. Enough to catch another writer,
/// and far cheaper than keeping a second copy of the file around for the
/// length of an edit.
type Fingerprint = (usize, u64);

fn fingerprint(lines: &[String]) -> Fingerprint {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    lines.hash(&mut hasher);
    (lines.len(), hasher.finish())
}

/// Reads PATH, finds the heading TARGET names, and hands back an edit
/// positioned on it.
///
/// The target is resolved by identity, never by line number alone (see
/// `heading_identity::resolve_target`): a CUSTOM_ID first, a unique title
/// where there is no id, and the caller's line number only once the
/// heading actually sitting there proves to be the one meant.
///
/// The file's declared kind is checked here rather than at `commit`, so a
/// generated mirror is refused before the caller does any work -- and on
/// the lines just read, which are the file as it IS. Checking the lines
/// about to be WRITTEN would let a write that removes the `#+NOBO_TYPE:`
/// declaration through, which is the exact write this refuses.
///
/// SUBJECT names the thing in the error the user reads ("Überschrift",
/// "Übergeordnete Überschrift", "Quell-Überschrift", &crate::core::errors::subject::target_heading()).
pub fn open_edit(path: &Path, target: &HeadingTarget, subject: &str) -> Result<HeadingEdit, String> {
    let (lines, flat) = read_and_parse(path)?;
    fs_utils::refuse_unwritable_kind_of(&lines).map_err(|e| e.to_string())?;
    let idx = heading_identity::resolve_target(&flat, target, subject)?;
    let read_as = fingerprint(&lines);
    Ok(HeadingEdit { path: path.to_path_buf(), lines, flat, idx, read_as, mutated: false })
}

impl HeadingEdit {
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The heading being edited, as it is on disk right now.
    pub fn heading(&self) -> &Heading {
        &self.flat[self.idx]
    }

    /// Every heading in the file, from the same read -- for the callers
    /// that need context around the target (a level to derive, a subtree
    /// end to find).
    pub fn flat(&self) -> &[Heading] {
        &self.flat
    }

    pub fn idx(&self) -> usize {
        self.idx
    }

    /// Whether a structural mutation has happened. `move_subtree_before`
    /// deliberately does nothing for a drop that lands where the heading
    /// already is -- and a caller that commits regardless still rewrites
    /// the file and wakes every watcher, which is exactly what that
    /// no-op is there to avoid.
    pub fn is_mutated(&self) -> bool {
        self.mutated
    }

    /// Whether FLAT[OTHER] is a sibling of the heading being edited: same
    /// level, same parent. The question a drop next to another heading
    /// asks first -- a sibling means reordering, anything else means
    /// refiling, and the two are different commands.
    pub fn is_sibling(&self, other: usize) -> bool {
        self.flat[other].level == self.flat[self.idx].level
            && parent_line_of(&self.flat, other) == parent_line_of(&self.flat, self.idx)
    }

    /// The sibling following FLAT[OTHER], if any -- what "drop it after
    /// that one" resolves to as an anchor for `move_subtree_before`. The
    /// run ends where the parent's subtree does, so the last sibling has
    /// none, which is the `None` that means "last".
    pub fn next_sibling_of(&self, other: usize) -> Option<usize> {
        let level = self.flat[other].level;
        let parent = parent_line_of(&self.flat, other);
        (other + 1..self.flat.len())
            .take_while(|&i| self.flat[i].level >= level)
            .find(|&i| self.flat[i].level == level && parent_line_of(&self.flat, i) == parent)
    }

    /// The enclosing headings, nearest first.
    pub fn ancestors(&self) -> Vec<Heading> {
        ancestors_of(&self.flat, self.idx)
    }

    /// Replaces the heading's own block -- its heading line, drawers and
    /// body, but not its children.
    pub fn replace(&mut self, mut block: Vec<String>) -> Result<(), String> {
        self.refuse_second_mutation()?;
        let heading = self.flat[self.idx].clone();
        // The edited heading keeps its own id; the tasks a pasted
        // description turns into must not take one the file already has.
        let taken = ids_in_file_except(&self.flat, Some(self.idx));
        renew_taken_ids(&mut block, &taken, true);
        self.splice(heading.line_no, heading.last_line, block, &heading.title)
    }

    /// The same, with the block built from PAYLOAD -- the ordinary edit.
    pub fn write(&mut self, payload: &HeadingUpdatePayload) -> Result<(), String> {
        let block = build_org_heading_block(self.heading(), payload);
        self.replace(block)
    }

    /// Removes the heading and everything under it.
    pub fn delete_subtree(&mut self) -> Result<(), String> {
        self.refuse_second_mutation()?;
        let title = self.flat[self.idx].title.clone();
        let (start_line, end_line) = subtree_line_range(&self.flat, self.idx);
        self.splice(start_line, end_line, Vec::new(), &title)
    }

    /// Puts NEW_LINES directly above the heading -- where a restored
    /// sibling goes back.
    pub fn insert_before(&mut self, new_lines: Vec<String>) -> Result<(), String> {
        self.refuse_second_mutation()?;
        let heading = &self.flat[self.idx];
        if heading.line_no == 0 || heading.line_no > self.lines.len() + 1 {
            return Err(crate::t!(
                "\"{}\" liegt nicht mehr auf Zeile {} (die Datei hat {}) -- sie wurde inzwischen geändert, bitte neu laden.", "\"{}\" is no longer at line {} (the file has {}) -- it has changed since, please reload.",
                heading.title,
                heading.line_no,
                self.lines.len()
            ));
        }
        let at = heading.line_no - 1;
        self.lines.splice(at..at, new_lines);
        self.mutated = true;
        Ok(())
    }

    /// Puts NEW_LINES directly after the heading's whole subtree -- where
    /// a new child belongs.
    pub fn insert_after_subtree(&mut self, mut new_lines: Vec<String>) -> Result<(), String> {
        let taken = ids_in_file_except(&self.flat, None);
        renew_taken_ids(&mut new_lines, &taken, false);
        self.refuse_second_mutation()?;
        let after = find_subtree_end_line(&self.flat, self.idx);
        if after > self.lines.len() {
            return Err(crate::t!(
                "\"{}\" endet nicht mehr auf Zeile {after} (die Datei hat {}) -- sie wurde inzwischen geändert, bitte neu laden.", "\"{}\" no longer ends at line {after} (the file has {}) -- it has changed since, please reload.",
                self.flat[self.idx].title,
                self.lines.len()
            ));
        }
        self.lines.splice(after..after, new_lines);
        self.mutated = true;
        Ok(())
    }

    /// Moves the heading and everything under it to where ANCHOR sits --
    /// directly above that heading -- or last among its siblings when
    /// there is no anchor.
    ///
    /// Reordering only. The anchor has to be a sibling: same level, same
    /// parent. A drag that lands somewhere else is not a move but a
    /// refile, and `refile_heading` owns that, with the history entry and
    /// the trigger handling that belong to it.
    ///
    /// One mutation, not a delete followed by an insert. Both ranges are
    /// read off the same parse and the insertion point is corrected by
    /// exactly what the lift took out -- doing it as two calls would be
    /// the stale arithmetic `refuse_second_mutation` exists to stop.
    pub fn move_subtree_before(&mut self, anchor: Option<usize>) -> Result<(), String> {
        self.refuse_second_mutation()?;

        // `open_edit` has already refused a file nobody may write. This
        // is the narrower question the kinds answer separately: whether
        // the ORDER of the headings is the user's to change. It is the
        // list itself for a CalDAV list, and the cook's own text in a
        // cookbook (see FileKindSpec::reorderable).
        let kind = crate::core::file_kind::from_lines(&self.lines);
        if !crate::core::file_kind::capabilities(kind).reorderable {
            return Err(crate::t!(
                "Die Reihenfolge in dieser Datei lässt sich nicht umstellen.",
                "The order of the headings in this file is not something to rearrange."
            ));
        }

        let heading = self.flat[self.idx].clone();
        let (first, last) = subtree_line_range(&self.flat, self.idx);
        let (start, end) = block_range(&self.lines, first, last, &heading.title)?;

        let at = match anchor {
            Some(other) => {
                let sibling = self.flat[other].clone();
                if !self.is_sibling(other) {
                    return Err(crate::t!(
                        "\"{}\" und \"{}\" stehen nicht auf derselben Ebene -- zum Verschieben unter eine andere Überschrift gibt es das Einsortieren.", "\"{}\" and \"{}\" are not siblings -- moving something under a different heading is what refiling is for.",
                        heading.title,
                        sibling.title
                    ));
                }
                if sibling.line_no == 0 || sibling.line_no > self.lines.len() + 1 {
                    return Err(crate::t!(
                        "\"{}\" liegt nicht mehr auf Zeile {} (die Datei hat {}) -- sie wurde inzwischen geändert, bitte neu laden.", "\"{}\" is no longer at line {} (the file has {}) -- it has changed since, please reload.",
                        sibling.title,
                        sibling.line_no,
                        self.lines.len()
                    ));
                }
                sibling.line_no - 1
            }
            // Last among its siblings: the end of what encloses them,
            // which for a top-level heading is the end of the file.
            None => match parent_idx_of(&self.flat, self.idx) {
                Some(parent) => find_subtree_end_line(&self.flat, parent),
                None => self.lines.len(),
            },
        };

        // Dropped where it already is. The lift and the insert would
        // cancel out, but the write would still touch the file and wake
        // every watcher -- including the sync that is about to be told
        // the order changed.
        if at >= start && at <= end {
            return Ok(());
        }

        let block: Vec<String> = self.lines.splice(start..end, Vec::new()).collect();
        let at = if at > start { at - (end - start) } else { at };
        self.lines.splice(at..at, block);
        self.mutated = true;
        Ok(())
    }

    /// Bumps LAST_ACTIVITY on every enclosing heading, so finishing a
    /// subtask surfaces its project under "sort by last activity".
    ///
    /// Call after the primary mutation: the progress cookies are re-read
    /// from the lines as they then are, which is the change being reacted
    /// to.
    pub fn touch_ancestors(&mut self) -> Result<(), String> {
        let chain = self.ancestors();
        self.touch(chain)
    }

    /// The same, including the heading itself -- for a new child, where
    /// the heading in hand is the parent rather than the thing edited.
    pub fn touch_self_and_ancestors(&mut self) -> Result<(), String> {
        let mut chain = vec![self.flat[self.idx].clone()];
        chain.extend(self.ancestors());
        self.touch(chain)
    }

    /// Writes the file, once -- unless somebody else got there first.
    ///
    /// Splicing in memory and writing the whole file back is what buys the
    /// single read; it also means the write carries every line, including
    /// the ones nobody edited. So a sync pull, a CalDAV pull or another
    /// editor landing between `open_edit` and here would be overwritten
    /// wholesale rather than merged. Re-reading and comparing turns that
    /// into a refusal the user can act on ("bitte neu laden") instead of a
    /// silent loss.
    ///
    /// Not a lock: another writer could still land between this read and
    /// the rename below. It closes the window that lasts as long as the
    /// caller's own work -- building a block, stamping an id, walking an
    /// ancestor chain -- and leaves only the one inside a single function,
    /// which is the same one every other write in the app has. Same shape
    /// as sync's own `local_matches_diff_snapshot`.
    ///
    /// The re-read doubles as the write guard's look at the file, so this
    /// opens it once, not twice (see `fs_utils::write_checked`).
    pub fn commit(self) -> Result<(), String> {
        let current = fs_utils::read_lines(&self.path).map_err(|e| e.to_string())?;
        if fingerprint(&current) != self.read_as {
            return Err(crate::t!(
                "\"{}\" wurde während der Bearbeitung von außen geändert -- die Änderung wurde nicht gespeichert, bitte neu laden.", "\"{}\" was changed from outside while it was being edited -- the change was not saved, please reload.",
                self.path.display()
            ));
        }
        fs_utils::refuse_unwritable_kind_of(&current).map_err(|e| e.to_string())?;
        fs_utils::write_checked(&self.path, &self.lines).map_err(|e| e.to_string())
    }

    /// Applied nearest first: touching a shallower ancestor can change its
    /// line count, and the rest of the chain sits strictly above that
    /// point, so their captured positions survive.
    fn touch(&mut self, chain: Vec<Heading>) -> Result<(), String> {
        if chain.is_empty() {
            return Ok(());
        }

        // `chain` was captured before the primary mutation, so its
        // progress stats are stale by exactly what this is reacting to.
        // Re-parsing the lines in hand gives each `[N%]` a current value
        // without touching the disk. Line numbers are unchanged between
        // the two parses for everything above the edit, so matching by
        // line_no is safe.
        let fresh = org_parser::parse_headings_from_lines(&self.path, &self.lines);

        for mut ancestor in chain {
            ancestor.progress_stats =
                fresh.iter().find(|h| h.line_no == ancestor.line_no).and_then(|h| h.progress_stats);
            let block = touch_last_activity(&ancestor);
            self.splice(ancestor.line_no, ancestor.last_line, block, &ancestor.title)?;
        }
        Ok(())
    }

    /// The one conversion from a heading's 1-based, inclusive line range
    /// to the 0-based, half-open one a splice wants -- and the one place
    /// that range is checked against the lines actually in hand.
    fn splice(&mut self, line_no: usize, last_line: usize, block: Vec<String>, title: &str) -> Result<(), String> {
        let (start, end) = block_range(&self.lines, line_no, last_line, title)?;
        self.lines.splice(start..end, block);
        self.mutated = true;
        Ok(())
    }

    /// A second structural mutation would take its range from `flat`,
    /// which the first one has just invalidated -- and splicing a stale
    /// range writes over whatever moved into it. A caller that needs two
    /// edits in one file wants `apply_bulk_update`, which resolves them
    /// all against one parse and applies them in a safe order.
    fn refuse_second_mutation(&self) -> Result<(), String> {
        if self.mutated {
            return Err(crate::t!(
                "{}: zweite Änderung an derselben Datei in einem Vorgang -- die Zeilennummern der ersten sind dadurch überholt.", "{}: a second change to the same file in one operation -- the first has made its line numbers stale.",
                self.path.display()
            ));
        }
        Ok(())
    }
}

/// Every CUSTOM_ID the file's headings carry, but the one at EXCEPT --
/// the heading being rewritten, whose id is its own to keep.
pub fn ids_in_file_except(flat: &[Heading], except: Option<usize>) -> std::collections::HashSet<String> {
    flat.iter()
        .enumerate()
        .filter(|(i, _)| Some(*i) != except)
        .filter_map(|(_, h)| heading_identity::custom_id_of(h).map(str::to_string))
        .collect()
}

/// Gives a fresh CUSTOM_ID to every task in LINES whose id the file
/// already has (TAKEN) or that occurs twice within LINES itself.
///
/// The case it exists for: a task copied together with its drawer and
/// pasted into a description. On save it becomes a real task -- with the
/// SAME id as the original. Every write finds a task by its id first
/// (heading_identity::resolve_target), so from then on an edit, a tick or
/// a delete meant for one of the two could land on the other.
///
/// The original keeps its id -- links and history point at it -- and the
/// copy gets a new one. KEEP_FIRST spares the first heading in LINES: when
/// a heading's own block is rewritten, that one IS the original. Nothing
/// inside a code block is touched; those lines are text.
pub fn renew_taken_ids(lines: &mut [String], taken: &std::collections::HashSet<String>, keep_first: bool) {
    let in_block = org_parser::closed_block_lines(lines);
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut headings_seen = 0usize;
    for (i, line) in lines.iter_mut().enumerate() {
        if in_block[i] {
            continue;
        }
        if org_parser::parse_heading_line(line).is_some() {
            headings_seen += 1;
            continue;
        }
        let Some((key, value)) = org_parser::parse_property(line.trim_start()) else { continue };
        if !key.eq_ignore_ascii_case(heading_identity::CUSTOM_ID_PROPERTY) {
            continue;
        }
        let value = value.trim().to_string();
        if value.is_empty() || (keep_first && headings_seen <= 1) {
            seen.insert(value);
            continue;
        }
        if taken.contains(&value) || seen.contains(&value) {
            *line = format!(":{}: {}", heading_identity::CUSTOM_ID_PROPERTY, heading_identity::generate_custom_id());
        } else {
            seen.insert(value);
        }
    }
}

/// The index of the heading enclosing FLAT[IDX], if any.
fn parent_idx_of(flat: &[Heading], idx: usize) -> Option<usize> {
    let level = flat[idx].level;
    flat[..idx].iter().rposition(|h| h.level < level)
}

/// Its line number -- what two headings have to agree on to be siblings.
fn parent_line_of(flat: &[Heading], idx: usize) -> Option<usize> {
    parent_idx_of(flat, idx).map(|i| flat[i].line_no)
}

/// A target that names nothing but a line -- for the callers that restore
/// a recorded position rather than follow a heading's identity (see
/// `history::restore`). `resolve_target` gives this the plain
/// line-number behaviour, and refuses when no heading sits there.
pub fn target_at_line(line_no: usize) -> HeadingTarget {
    HeadingTarget { line_no, custom_id: None, title: None }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::file_kind::{FileKind, FileKindSpec, StateSupport};
    use tempfile::TempDir;

    const TWO_HEADINGS: &str = concat!(
        "* TODO Erste\n",
        ":PROPERTIES:\n",
        ":CUSTOM_ID: h:eins\n",
        ":END:\n",
        "* TODO Zweite\n",
        ":PROPERTIES:\n",
        ":CUSTOM_ID: h:zwei\n",
        ":END:\n",
    );

    const PROJECT: &str = concat!(
        "* Projekt                                                          :project:\n",
        "** Gruppe\n",
        ":PROPERTIES:\n",
        ":CUSTOM_ID: h:gruppe\n",
        ":END:\n",
        "*** TODO Aufgabe A\n",
        "*** TODO Aufgabe B\n",
    );

    fn write(dir: &TempDir, name: &str, content: &str) -> PathBuf {
        let path = dir.path().join(name);
        std::fs::write(&path, content).unwrap();
        path
    }

    fn renamed(title: &str) -> HeadingUpdatePayload {
        HeadingUpdatePayload { title: Some(title.to_string()), ..Default::default() }
    }

    /// The identity gate, seen from the outside: the caller's line number
    /// is stale (it points at the FIRST heading), and the write still
    /// lands on the one its CUSTOM_ID names.
    #[test]
    fn an_edit_follows_the_heading_rather_than_the_line_it_was_last_seen_on() {
        let dir = TempDir::new().unwrap();
        let path = write(&dir, "notizen.org", TWO_HEADINGS);
        let stale = HeadingTarget { line_no: 1, custom_id: Some("h:zwei".into()), title: Some("Zweite".into()) };

        let mut edit = open_edit(&path, &stale, &crate::core::errors::subject::heading()).unwrap();
        edit.write(&renamed("Zweite umbenannt")).unwrap();
        edit.commit().unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("* TODO Zweite umbenannt"), "{text}");
        assert!(text.contains("* TODO Erste"), "the heading on the stale line was not touched:\n{text}");
    }

    /// The price of splicing in memory and writing the whole file: a
    /// change that landed in between would be overwritten wholesale. It is
    /// refused instead.
    #[test]
    fn commit_refuses_a_file_that_changed_while_the_edit_was_open() {
        let dir = TempDir::new().unwrap();
        let path = write(&dir, "notizen.org", TWO_HEADINGS);
        let target = HeadingTarget { line_no: 1, custom_id: Some("h:eins".into()), title: None };

        let mut edit = open_edit(&path, &target, &crate::core::errors::subject::heading()).unwrap();
        edit.write(&renamed("Erste umbenannt")).unwrap();

        // Whatever else writes into this vault -- a sync pull, a CalDAV
        // pull, the user's own editor.
        let from_elsewhere = format!("{TWO_HEADINGS}* TODO Vom anderen Gerät\n");
        std::fs::write(&path, &from_elsewhere).unwrap();

        let err = edit.commit().unwrap_err();

        assert!(err.contains("neu laden"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), from_elsewhere, "the other write survives untouched");
    }

    /// Nothing reaches disk before `commit`, so a caller that gives up
    /// halfway leaves the file exactly as it found it.
    #[test]
    fn an_uncommitted_edit_leaves_the_file_alone() {
        let dir = TempDir::new().unwrap();
        let path = write(&dir, "notizen.org", TWO_HEADINGS);
        let target = HeadingTarget { line_no: 1, custom_id: Some("h:eins".into()), title: None };

        let mut edit = open_edit(&path, &target, &crate::core::errors::subject::heading()).unwrap();
        edit.write(&renamed("Nie gespeichert")).unwrap();
        edit.touch_ancestors().unwrap();
        drop(edit);

        assert_eq!(std::fs::read_to_string(&path).unwrap(), TWO_HEADINGS);
    }

    /// The whole ancestor chain and the edit itself land in one write --
    /// and the cookie the project carries reflects the edit, not the parse
    /// it was captured from.
    #[test]
    fn the_edit_and_every_ancestor_touch_land_together() {
        let dir = TempDir::new().unwrap();
        let path = write(&dir, "projekte.org", PROJECT);
        let target = HeadingTarget { line_no: 7, custom_id: None, title: Some("Aufgabe B".into()) };

        let mut edit = open_edit(&path, &target, &crate::core::errors::subject::heading()).unwrap();
        assert_eq!(
            edit.ancestors().iter().map(|h| h.title.as_str()).collect::<Vec<_>>(),
            vec!["Gruppe", "Projekt"],
            "nearest ancestor first"
        );
        edit.write(&HeadingUpdatePayload { state: Some("DONE".into()), ..Default::default() }).unwrap();
        edit.touch_ancestors().unwrap();
        edit.commit().unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("*** DONE Aufgabe B"), "{text}");
        assert_eq!(
            text.matches("LAST_ACTIVITY").count(),
            3,
            "the edited heading plus both ancestors, all in the same write:\n{text}"
        );
        // The cookies come from a re-parse of the mutated lines, so they
        // describe the file after this edit rather than the parse the
        // ancestors were captured from.
        assert_eq!(text.matches("[50%]").count(), 2, "both ancestors show one of two done:\n{text}");
        assert!(text.contains("*** TODO Aufgabe A"), "the sibling is untouched:\n{text}");
    }

    #[test]
    fn deleting_a_heading_takes_its_children_with_it_and_leaves_the_rest() {
        let dir = TempDir::new().unwrap();
        let path = write(&dir, "projekte.org", PROJECT);
        let target = HeadingTarget { line_no: 2, custom_id: None, title: Some("Gruppe".into()) };

        let mut edit = open_edit(&path, &target, &crate::core::errors::subject::heading()).unwrap();
        edit.delete_subtree().unwrap();
        edit.commit().unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("Aufgabe A") && !text.contains("Aufgabe B"), "{text}");
        assert!(text.contains("* Projekt"), "{text}");
    }

    #[test]
    fn insert_before_puts_the_lines_directly_above_the_heading() {
        let dir = TempDir::new().unwrap();
        let path = write(&dir, "notizen.org", TWO_HEADINGS);
        let target = HeadingTarget { line_no: 5, custom_id: Some("h:zwei".into()), title: None };

        let mut edit = open_edit(&path, &target, &crate::core::errors::subject::heading()).unwrap();
        edit.insert_before(vec!["* TODO Dazwischen".to_string()]).unwrap();
        edit.commit().unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        let at = lines.iter().position(|l| *l == "* TODO Dazwischen").unwrap();
        assert_eq!(lines[at + 1], "* TODO Zweite", "{text}");
    }

    /// A second structural mutation would take its range from a parse the
    /// first one just invalidated.
    #[test]
    fn a_second_mutation_in_the_same_edit_is_refused() {
        let dir = TempDir::new().unwrap();
        let path = write(&dir, "notizen.org", TWO_HEADINGS);
        let target = HeadingTarget { line_no: 1, custom_id: Some("h:eins".into()), title: None };

        let mut edit = open_edit(&path, &target, &crate::core::errors::subject::heading()).unwrap();
        edit.write(&renamed("Einmal")).unwrap();

        assert!(edit.write(&renamed("Zweimal")).is_err());
        assert!(edit.delete_subtree().is_err());
    }

    /* --- a pasted copy of a task does not share the original's id --- */

    const WITH_ORIGINAL: &str = concat!(
        "* TODO Projekt :project:\n",
        ":PROPERTIES:\n",
        ":CUSTOM_ID: h:projekt\n",
        ":END:\n",
        "** TODO Original\n",
        ":PROPERTIES:\n",
        ":CUSTOM_ID: h:orig\n",
        ":END:\n",
        "** TODO Notizen\n",
        ":PROPERTIES:\n",
        ":CUSTOM_ID: h:notizen\n",
        ":END:\n",
    );

    fn ids_after(path: &PathBuf) -> Vec<(String, String)> {
        let (_, flat) = read_and_parse(path).unwrap();
        flat.iter().map(|h| (h.title.clone(), heading_identity::custom_id_of(h).unwrap_or("").to_string())).collect()
    }

    /// The case that was left open: a task copied WITH its drawer and
    /// pasted into another task's description. It became a real task with
    /// the original's id -- and every write finds a task by its id first,
    /// so an edit or a delete meant for one could land on the other.
    #[test]
    fn a_pasted_copy_gets_its_own_id_and_the_original_keeps_its() {
        let dir = TempDir::new().unwrap();
        let path = write(&dir, "projects.org", WITH_ORIGINAL);
        let target = HeadingTarget { line_no: 0, custom_id: Some("h:notizen".into()), title: None };
        let pasted = "Kopie:\n* TODO Original\n:PROPERTIES:\n:CUSTOM_ID: h:orig\n:END:";

        let mut edit = open_edit(&path, &target, "t").unwrap();
        edit.write(&HeadingUpdatePayload { content: Some(pasted.into()), ..Default::default() }).unwrap();
        edit.commit().unwrap();

        let ids = ids_after(&path);
        let originals: Vec<_> = ids.iter().filter(|(t, _)| t == "Original").collect();
        assert_eq!(originals.len(), 2, "{ids:?}");
        assert_eq!(originals[0].1, "h:orig", "the original keeps its id: {ids:?}");
        assert!(!originals[1].1.is_empty() && originals[1].1 != "h:orig", "the copy gets its own: {ids:?}");
        let notizen = ids.iter().find(|(t, _)| t == "Notizen").unwrap();
        assert_eq!(notizen.1, "h:notizen", "the edited task keeps its own id too");
    }

    /// The same copy pasted twice: the two copies must differ from each
    /// other, not only from the original.
    #[test]
    fn two_copies_in_one_paste_differ_from_each_other() {
        let dir = TempDir::new().unwrap();
        let path = write(&dir, "projects.org", WITH_ORIGINAL);
        let target = HeadingTarget { line_no: 0, custom_id: Some("h:notizen".into()), title: None };
        let copy = "* TODO Kopie\n:PROPERTIES:\n:CUSTOM_ID: h:neu\n:END:";
        let pasted = format!("{copy}\n{copy}");

        let mut edit = open_edit(&path, &target, "t").unwrap();
        edit.write(&HeadingUpdatePayload { content: Some(pasted), ..Default::default() }).unwrap();
        edit.commit().unwrap();

        let copies: Vec<_> = ids_after(&path).into_iter().filter(|(t, _)| t == "Kopie").map(|(_, id)| id).collect();
        assert_eq!(copies.len(), 2);
        assert_eq!(copies[0], "h:neu", "the first is unique and keeps it");
        assert_ne!(copies[0], copies[1]);
    }

    /// Org code quoted in a block is text: an id inside it is not an id.
    #[test]
    fn an_id_inside_a_code_block_is_left_alone() {
        let mut lines: Vec<String> = ["** TODO X", "#+BEGIN_SRC org", ":CUSTOM_ID: h:orig", "#+END_SRC"].iter().map(|l| l.to_string()).collect();
        let taken: std::collections::HashSet<String> = ["h:orig".to_string()].into();

        renew_taken_ids(&mut lines, &taken, false);

        assert_eq!(lines[2], ":CUSTOM_ID: h:orig");
    }

    /* --- reordering --- */

    const THREE: &str = concat!(
        "* TODO Brot\n",
        "* TODO Milch\n",
        "* TODO Käse\n",
    );

    fn titles(text: &str) -> Vec<&str> {
        text.lines().filter(|l| l.starts_with('*')).collect()
    }

    fn move_it(path: &PathBuf, title: &str, before: Option<&str>) -> Result<(), String> {
        let target = HeadingTarget { line_no: 0, custom_id: None, title: Some(title.to_string()) };
        let mut edit = open_edit(path, &target, &crate::core::errors::subject::heading())?;
        let anchor = match before {
            Some(other) => {
                let sibling = HeadingTarget { line_no: 0, custom_id: None, title: Some(other.to_string()) };
                Some(heading_identity::resolve_target(edit.flat(), &sibling, &crate::core::errors::subject::sibling_heading())?)
            }
            None => None,
        };
        edit.move_subtree_before(anchor)?;
        edit.commit()
    }

    #[test]
    fn a_heading_moved_up_lands_directly_above_the_one_it_was_dropped_on() {
        let dir = TempDir::new().unwrap();
        let path = write(&dir, "liste.org", THREE);

        move_it(&path, "Käse", Some("Milch")).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(titles(&text), vec!["* TODO Brot", "* TODO Käse", "* TODO Milch"], "{text}");
    }

    /// Downwards the lift happens above the target, so the insertion point
    /// has to come down by what was taken out -- the arithmetic that is
    /// wrong in every implementation that does this as two edits.
    #[test]
    fn a_heading_moved_down_lands_directly_above_the_one_it_was_dropped_on() {
        let dir = TempDir::new().unwrap();
        let path = write(&dir, "liste.org", THREE);

        move_it(&path, "Brot", Some("Käse")).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(titles(&text), vec!["* TODO Milch", "* TODO Brot", "* TODO Käse"], "{text}");
    }

    #[test]
    fn no_anchor_means_last() {
        let dir = TempDir::new().unwrap();
        let path = write(&dir, "liste.org", THREE);

        move_it(&path, "Brot", None).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(titles(&text), vec!["* TODO Milch", "* TODO Käse", "* TODO Brot"], "{text}");
    }

    /// The children come along, and the drawers with them.
    #[test]
    fn a_subtree_moves_whole() {
        let dir = TempDir::new().unwrap();
        // The second group carries an id, because a `*`-line with no
        // state, no tags and no CUSTOM_ID is absorbed as a sub-note
        // rather than parsed as a heading (see org_parser).
        let path = write(
            &dir,
            "projekte.org",
            &format!("{PROJECT}** Zweite Gruppe\n:PROPERTIES:\n:CUSTOM_ID: h:zweite\n:END:\n*** TODO Aufgabe C\n"),
        );

        move_it(&path, "Zweite Gruppe", Some("Gruppe")).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            titles(&text),
            vec![
                "* Projekt                                                          :project:",
                "** Zweite Gruppe",
                "*** TODO Aufgabe C",
                "** Gruppe",
                "*** TODO Aufgabe A",
                "*** TODO Aufgabe B",
            ],
            "{text}"
        );
        assert!(text.contains(":CUSTOM_ID: h:gruppe"), "the drawer travelled with its heading:\n{text}");
    }

    /// Last among its siblings is the end of what encloses them, not the
    /// end of the file -- otherwise a subtask dropped at the bottom of its
    /// project would leave the project altogether.
    #[test]
    fn last_among_siblings_stays_inside_the_parent() {
        let dir = TempDir::new().unwrap();
        let path = write(&dir, "projekte.org", &format!("{PROJECT}* Anderes Projekt                                                  :project:\n"));

        move_it(&path, "Aufgabe A", None).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            titles(&text),
            vec![
                "* Projekt                                                          :project:",
                "** Gruppe",
                "*** TODO Aufgabe B",
                "*** TODO Aufgabe A",
                "* Anderes Projekt                                                  :project:",
            ],
            "{text}"
        );
    }

    /// Moving something under a different heading is refiling, which has
    /// its own command, its own history entry and its own trigger
    /// handling. A drag cannot be allowed to do it quietly.
    #[test]
    fn an_anchor_that_is_not_a_sibling_is_refused() {
        let dir = TempDir::new().unwrap();
        let path = write(&dir, "projekte.org", PROJECT);

        let err = move_it(&path, "Gruppe", Some("Aufgabe A")).unwrap_err();

        assert!(err.contains("Ebene") || err.contains("siblings"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), PROJECT, "nothing was written");
    }

    /// Dropped back where it came from: no write at all, because one
    /// would still touch the file and wake everything watching it.
    #[test]
    fn dropping_a_heading_where_it_already_is_writes_nothing() {
        let dir = TempDir::new().unwrap();
        let path = write(&dir, "liste.org", THREE);
        let target = HeadingTarget { line_no: 0, custom_id: None, title: Some("Milch".into()) };

        let mut edit = open_edit(&path, &target, &crate::core::errors::subject::heading()).unwrap();
        let sibling = HeadingTarget { line_no: 0, custom_id: None, title: Some("Käse".into()) };
        let anchor = heading_identity::resolve_target(edit.flat(), &sibling, &crate::core::errors::subject::sibling_heading()).unwrap();
        edit.move_subtree_before(Some(anchor)).unwrap();

        // Still open for another mutation, because none has happened.
        assert!(edit.write(&renamed("Milch 2")).is_ok());
    }

    /* --- the write guard, asked before the caller does any work --- */

    const EDIT_GENERATED: FileKind = FileKind::new("test-edit-generated");

    inventory::submit! {
        FileKindSpec {
            kind: EDIT_GENERATED,
            served_by: None,
            editable: false,
            states: StateSupport::None,
            reorderable: false,
            can_add_headings: false,
        }
    }

    /// Refused at `open_edit` rather than at `commit`: the caller would
    /// otherwise build a block, resolve an ancestor chain and stamp an id
    /// before finding out the file was never writable.
    #[test]
    fn opening_an_edit_on_a_generated_file_is_refused_up_front() {
        let dir = TempDir::new().unwrap();
        let path = write(
            &dir,
            "spiegel.org",
            &format!("#+TITLE: X\n{}\n\n* TODO Termin\n", EDIT_GENERATED.declaration()),
        );
        let target = HeadingTarget { line_no: 4, custom_id: None, title: Some("Termin".into()) };

        let err = open_edit(&path, &target, &crate::core::errors::subject::heading()).err().unwrap();

        assert!(err.contains("von nobo erzeugt"), "{err}");
    }
}
