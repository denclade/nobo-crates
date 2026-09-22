use crate::core::heading_identity;
use crate::core::heading_utils;
use crate::core::indexer::IndexManager;
use crate::core::models::{Heading, HeadingTarget, HeadingUpdatePayload};
use crate::core::task_utils;
use std::path::Path;

pub const ORG_TRIGGER_PROPERTY: &str = "TRIGGER";
pub const ORG_BLOCKER_PROPERTY: &str = "BLOCKER";

#[derive(Debug, Clone, PartialEq)]
pub enum Finder {
    NextSibling,
    PreviousSibling,
    Siblings,
    Parent,
    FirstChild,
    Children,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    SetTodo(String),
    ClearTodo,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Condition {
    NoSiblingWithTodo(String),
    SourceTodoIn(Vec<String>),
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct TriggerSpec {
    pub conditions: Vec<Condition>,
    pub finders: Vec<Finder>,
    pub actions: Vec<Action>,
}

impl TriggerSpec {
    pub fn is_empty(&self) -> bool {
        self.finders.is_empty() || self.actions.is_empty()
    }
}

/* ---------- Parser ---------- */

/// Split `foo!(A,B)` in ("foo", ["A", "B"]). Without brackets: ("foo", []).
fn split_call(token: &str) -> (String, Vec<String>) {
    let trimmed = token.trim_end_matches('!');
    match trimmed.find('(') {
        Some(open) if trimmed.ends_with(')') => {
            let name = trimmed[..open].trim_end_matches('!').to_string();
            let args = trimmed[open + 1..trimmed.len() - 1]
                .split(',')
                .map(|a| a.trim().to_string())
                .filter(|a| !a.is_empty())
                .collect();
            (name, args)
        }
        _ => (trimmed.to_string(), Vec::new()),
    }
}

fn parse_finder(name: &str) -> Option<Finder> {
    match name {
        "next-sibling" => Some(Finder::NextSibling),
        "previous-sibling" | "prev-sibling" => Some(Finder::PreviousSibling),
        "siblings" => Some(Finder::Siblings),
        "parent" => Some(Finder::Parent),
        "first-child" => Some(Finder::FirstChild),
        "children" => Some(Finder::Children),
        _ => None,
    }
}

fn parse_action(name: &str, args: &[String]) -> Option<Action> {
    match name {
        "todo" => match args.first() {
            Some(state) if !state.is_empty() => Some(Action::SetTodo(state.clone())),
            _ => Some(Action::ClearTodo),
        },
        _ => None,
    }
}

fn parse_condition(name: &str, args: &[String]) -> Option<Condition> {
    match name {
        "if-no-sibling-todo" => args.first().map(|s| Condition::NoSiblingWithTodo(s.clone())),
        "if-todo" => Some(Condition::SourceTodoIn(args.to_vec())),
        _ => None,
    }
}

/// Parses TRIGGER in org-edna-Stil.
/// Example: `next-sibling todo!(NEXT)`
/// With condition: `if-no-sibling-todo(NEXT) next-sibling todo!(NEXT)`
pub fn parse_trigger(raw: &str) -> TriggerSpec {
    let mut spec = TriggerSpec::default();

    for token in raw.split_whitespace() {
        let (name, args) = split_call(token);

        // Conditions are prefixed with "if-",
        // Actions contain "!" every thing else is a finder.
        if name.starts_with("if-") {
            if let Some(c) = parse_condition(&name, &args) {
                spec.conditions.push(c);
            }
        } else if token.contains('!') {
            if let Some(a) = parse_action(&name, &args) {
                spec.actions.push(a);
            }
        } else if let Some(f) = parse_finder(&name) {
            spec.finders.push(f);
        }
    }

    spec
}

/// Write trigger in org-edna style
pub fn format_trigger(spec: &TriggerSpec) -> String {
    let mut parts: Vec<String> = Vec::new();

    for c in &spec.conditions {
        parts.push(match c {
            Condition::NoSiblingWithTodo(s) => format!("if-no-sibling-todo({})", s),
            Condition::SourceTodoIn(states) => format!("if-todo({})", states.join(",")),
        });
    }
    for f in &spec.finders {
        parts.push(match f {
            Finder::NextSibling => "next-sibling".into(),
            Finder::PreviousSibling => "previous-sibling".into(),
            Finder::Siblings => "siblings".into(),
            Finder::Parent => "parent".into(),
            Finder::FirstChild => "first-child".into(),
            Finder::Children => "children".into(),
        });
    }
    for a in &spec.actions {
        parts.push(match a {
            Action::SetTodo(s) => format!("todo!({})", s),
            Action::ClearTodo => "todo!()".into(),
        });
    }

    parts.join(" ")
}

/* ---------- Finder ---------- */

/// Index area after idx
fn subtree_end_idx(flat: &[Heading], idx: usize) -> usize {
    let level = flat[idx].level;
    let mut j = idx + 1;
    while j < flat.len() && flat[j].level > level {
        j += 1;
    }
    j
}

fn parent_idx(flat: &[Heading], idx: usize) -> Option<usize> {
    let level = flat[idx].level;
    if level <= 1 {
        return None;
    }
    (0..idx).rev().find(|&i| flat[i].level < level)
}

fn sibling_indices(flat: &[Heading], idx: usize) -> Vec<usize> {
    let level = flat[idx].level;
    let parent = parent_idx(flat, idx);
    let (start, end) = match parent {
        Some(p) => (p + 1, subtree_end_idx(flat, p)),
        None => (0, flat.len()),
    };
    
    (start..end)
        .filter(|&i| i != idx && flat[i].level == level)
        .collect()
}

/// Where a finder points, seen from IDX.
///
/// `next-sibling` and `previous-sibling` walk PAST finished headings. They
/// name a direction, not a position, so "the next one along" is the next
/// one that is still open. That is what "give this project its next
/// step" has to mean once tasks get done out of order or one in between
/// is cancelled -- the only reading under which Auto-NEXT keeps its
/// promise that a project always has a visible next step. Read literally,
/// the trigger fired at a task crossed off last week, refused to reopen
/// it, and left the project with no next step at all.
///
/// The set finders (`siblings`, `children`) and the fixed ones (`parent`,
/// `first-child`) stay literal: "the first child" does not become "the
/// second" because the first is done. evaluate_trigger keeps finished
/// targets out of those instead.
pub fn resolve_finder(flat: &[Heading], idx: usize, finder: &Finder) -> Vec<usize> {
    match finder {
        Finder::NextSibling => sibling_indices(flat, idx)
            .into_iter()
            .find(|&i| i > idx && !task_utils::is_finished(flat[i].state.as_deref()))
            .into_iter()
            .collect(),
        Finder::PreviousSibling => sibling_indices(flat, idx)
            .into_iter()
            .filter(|&i| i < idx && !task_utils::is_finished(flat[i].state.as_deref()))
            .last()
            .into_iter()
            .collect(),
        Finder::Siblings => sibling_indices(flat, idx),
        Finder::Parent => parent_idx(flat, idx).into_iter().collect(),
        Finder::FirstChild => {
            let level = flat[idx].level;
            (idx + 1 < flat.len() && flat[idx + 1].level == level + 1)
                .then_some(idx + 1)
                .into_iter()
                .collect()
        }
        Finder::Children => {
            let level = flat[idx].level;
            let end = subtree_end_idx(flat, idx);
            (idx + 1..end).filter(|&i| flat[i].level == level + 1).collect()
        }
    }
}

/* ---------- Conditions & Actions ---------- */

fn condition_holds(flat: &[Heading], idx: usize, cond: &Condition) -> bool {
    match cond {
        Condition::NoSiblingWithTodo(state) => sibling_indices(flat, idx)
            .into_iter()
            .all(|i| flat[i].state.as_deref() != Some(state.as_str())),
        Condition::SourceTodoIn(states) => match flat[idx].state.as_deref() {
            Some(s) => states.iter().any(|w| w == s),
            None => states.is_empty(),
        },
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlannedChange {
    pub target_line_no: usize,
    pub new_state: Option<String>,
}

/// Analyzes the trigger of src and returns the planed changes
/// `flat` must be the state AFTER the src-change
pub fn evaluate_trigger(flat: &[Heading], source_line_no: usize) -> Vec<PlannedChange> {
    let Some(idx) = flat.iter().position(|h| h.line_no == source_line_no) else {
        return Vec::new();
    };

    let Some(raw) = inherited_trigger(flat, idx) else {
        return Vec::new();
    };

    let spec = parse_trigger(raw);
    if spec.is_empty() {
        return Vec::new();
    }

    if !spec.conditions.iter().all(|c| condition_holds(flat, idx, c)) {
        return Vec::new();
    }

    let mut targets: Vec<usize> = spec
        .finders
        .iter()
        .flat_map(|f| resolve_finder(flat, idx, f))
        .collect();
    targets.sort_unstable();
    targets.dedup();

    targets
        .into_iter()
        // A heading that is already finished is never a target, whichever
        // finder named it: `children` and `siblings` hand back everything
        // at their level, done or not, and "give this project its next
        // step" can never mean "reopen something that was done". The
        // sibling finders already walk past finished ones (see
        // resolve_finder); this is the net under the rest.
        .filter(|&t| !task_utils::is_finished(flat[t].state.as_deref()))
        .flat_map(|t| {
            spec.actions.iter().map(move |a| PlannedChange {
                target_line_no: flat[t].line_no,
                new_state: match a {
                    Action::SetTodo(s) => Some(s.clone()),
                    Action::ClearTodo => None,
                },
            })
        })
        .collect()
}

/// The TRIGGER that applies to the heading at IDX: its own, or the
/// nearest one an ancestor carries.
///
/// Org property inheritance, which is what org-edna itself uses when
/// TRIGGER is in `org-use-property-inheritance` -- and without it the
/// feature both frontends offer does not work at all. "Auto-NEXT" is a
/// switch on the PROJECT, and a project's trigger has to be what fires
/// when a task under it is completed; read off the source alone, a
/// project's TRIGGER could only ever fire when the project itself
/// changed state, which is not what the switch says it does.
///
/// The source stays the source: `next-sibling` is resolved against the
/// task that changed, not against the project the trigger was read from.
/// That is what makes the inherited spelling mean "when something under
/// me is finished, move to the one after it".
fn inherited_trigger(flat: &[Heading], idx: usize) -> Option<&String> {
    let mut at = Some(idx);
    while let Some(i) = at {
        if let Some(raw) = flat[i].properties.get(ORG_TRIGGER_PROPERTY) {
            return Some(raw);
        }
        at = parent_idx(flat, i);
    }
    None
}

/// Re-evaluates the TRIGGER property of the heading at SOURCE_LINE_NO (after
/// its own change has already been written) and applies any resulting state
/// changes to the affected headings.
pub fn apply_triggers(
    path: &Path,
    source_line_no: usize,
    index_manager: &IndexManager,
) -> Result<(), String> {
    // The file as it is on disk, not the index cache. The caller has just
    // written its own edit, and the cache is only as current as the last
    // watcher event -- which on mobile never comes at all. Same rule every
    // other writer follows, see heading_utils::parse_from_disk.
    let flat = heading_utils::parse_from_disk(path);
    let changes = evaluate_trigger(&flat, source_line_no);

    // Each target is carried as an IDENTITY, not as the line number it
    // happens to sit on. Every write in the loop below rewrites a block
    // and moves every line after it -- and a state change grows one (a
    // LOGBOOK note, a CLOSED date), so the shift is real, not theoretical.
    // The second target's captured position is wrong by the time its turn
    // comes; its CUSTOM_ID is not.
    let targets: Vec<(HeadingTarget, Option<String>)> = changes
        .iter()
        .filter_map(|change| {
            flat.iter()
                .find(|h| h.line_no == change.target_line_no)
                .map(|h| (heading_identity::target_of(h), change.new_state.clone()))
        })
        .collect();

    for (target, new_state) in targets {
        // A target that no longer resolves is skipped rather than treated
        // as an error: the user's own edit is already on disk, and a
        // trigger that cannot find its heading has simply not fired. What
        // must never happen is the third option -- writing against the
        // stale number anyway, which is the case this whole shape exists
        // to rule out. Each iteration opens its own edit, so every write
        // is resolved against the file as the previous one left it.
        let mut edit = match heading_utils::open_edit(path, &target, &crate::core::errors::subject::heading()) {
            Ok(edit) => edit,
            Err(e) => {
                log::warn!("Trigger-Ziel in {} nicht mehr auffindbar -- übersprungen ({e})", path.display());
                continue;
            }
        };
        if edit.heading().state == new_state {
            continue;
        }

        let payload = HeadingUpdatePayload {
            state: Some(new_state.clone().unwrap_or_default()),
            ..Default::default()
        };

        edit.write(&payload)?;
        edit.commit()?;
        index_manager.update_file(path);
    }

    Ok(())
}

/* ------ Test ------- */
#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::models::Heading;

    fn heading(line_no: usize, level: usize, title: &str, state: Option<&str>) -> Heading {
        Heading {
            id: format!("test:{}", line_no),
            file_path: "test.org".to_string(),
            line_no,
            last_line: line_no,
            level,
            state: state.map(String::from),
            title: title.to_string(),
            ..Default::default()
        }
    }

    fn heading_with_prop(
        line_no: usize,
        level: usize,
        title: &str,
        state: Option<&str>,
        key: &str,
        value: &str,
    ) -> Heading {
        let mut heading = heading(line_no, level, title, state);
        heading.properties.insert(key.to_string(), value.to_string());
        heading
    }

    /// Auto-NEXT as both frontends offer it: the switch is on the
    /// PROJECT, and finishing a task under it moves the next one up.
    #[test]
    fn a_projects_trigger_fires_for_a_task_under_it() {
        let flat = vec![
            heading_with_prop(1, 1, "Projekt", None, ORG_TRIGGER_PROPERTY,
                              "if-no-sibling-todo(NEXT) next-sibling todo!(NEXT)"),
            heading(2, 2, "A", Some("DONE")),
            heading(3, 2, "B", Some("TODO")),
            heading(4, 2, "C", Some("TODO")),
        ];

        let changes = evaluate_trigger(&flat, 2);

        assert_eq!(changes, vec![PlannedChange { target_line_no: 3, new_state: Some("NEXT".into()) }]);
    }

    /// The task's own trigger still wins: inheritance only fills a gap.
    #[test]
    fn a_task_with_its_own_trigger_keeps_it() {
        let flat = vec![
            heading_with_prop(1, 1, "Projekt", None, ORG_TRIGGER_PROPERTY, "next-sibling todo!(NEXT)"),
            heading_with_prop(2, 2, "A", Some("DONE"), ORG_TRIGGER_PROPERTY, "next-sibling todo!(TODO)"),
            heading(3, 2, "B", Some("TODO")),
        ];

        let changes = evaluate_trigger(&flat, 2);

        assert_eq!(changes, vec![PlannedChange { target_line_no: 3, new_state: Some("TODO".into()) }]);
    }

    /// Completing out of order: the cancelled task in between is walked
    /// past, not reopened, and the next OPEN one gets the NEXT -- which is
    /// what the switch's own hint promises ("die naechste offene").
    #[test]
    fn a_finished_sibling_is_walked_past_rather_than_reopened() {
        let flat = vec![
            heading_with_prop(1, 1, "Projekt", None, ORG_TRIGGER_PROPERTY,
                              "if-no-sibling-todo(NEXT) next-sibling todo!(NEXT)"),
            heading(2, 2, "A", Some("DONE")),
            heading(3, 2, "B", Some("WONT-DO")),
            heading(4, 2, "C", Some("TODO")),
        ];

        let changes = evaluate_trigger(&flat, 2);

        assert_eq!(changes, vec![PlannedChange { target_line_no: 4, new_state: Some("NEXT".into()) }]);
    }

    /// Nothing open after it means nothing to move to. Nothing before it
    /// is reached for either: no wrap-around, and certainly no reopening.
    #[test]
    fn nothing_open_after_it_means_nothing_happens() {
        let flat = vec![
            heading_with_prop(1, 1, "Projekt", None, ORG_TRIGGER_PROPERTY,
                              "if-no-sibling-todo(NEXT) next-sibling todo!(NEXT)"),
            heading(2, 2, "A", Some("DONE")),
            heading(3, 2, "B", Some("DONE")),
            heading(4, 2, "C", Some("WONT-DO")),
        ];

        assert!(evaluate_trigger(&flat, 2).is_empty());
    }

    /// The guarantee underneath, on a finder that names finished headings
    /// too: `children` hands back every child, and the done one must not
    /// come back.
    #[test]
    fn a_finished_heading_is_never_a_target() {
        let flat = vec![
            heading_with_prop(1, 1, "Projekt", Some("DONE"), ORG_TRIGGER_PROPERTY, "children todo!(NEXT)"),
            heading(2, 2, "A", Some("DONE")),
            heading(3, 2, "B", Some("TODO")),
        ];

        let changes = evaluate_trigger(&flat, 1);

        assert_eq!(changes, vec![PlannedChange { target_line_no: 3, new_state: Some("NEXT".into()) }]);
    }

    #[test]
    fn previous_sibling_walks_past_finished_ones() {
        let flat = vec![
            heading(1, 1, "P", None),
            heading(2, 2, "A", Some("TODO")),
            heading(3, 2, "B", Some("DONE")),
            heading(4, 2, "C", Some("DONE")),
        ];
        // idx 3 = C; B (idx 2) is done and walked past, A (idx 1) is the answer
        assert_eq!(resolve_finder(&flat, 3, &Finder::PreviousSibling), vec![1]);
    }

    #[test]
    fn next_sibling_skips_own_children() {
        // P(L1) > A(L2) > A.child(L3), B(L2)
        let flat = vec![
            heading(1, 1, "P", None),
            heading(2, 2, "A", Some("DONE")),
            heading(3, 3, "A.child", Some("TODO")),
            heading(4, 2, "B", Some("TODO")),
        ];
        // idx 1 = A; next sibling is B (idx 3), not A.child (idx 2)
        assert_eq!(resolve_finder(&flat, 1, &Finder::NextSibling), vec![3]);
    }

    #[test]
    fn evaluate_returns_nothing_without_trigger_property() {
        let flat = vec![
            heading(1, 1, "P", None),
            heading(2, 2, "A", Some("DONE")),
        ];
        assert!(evaluate_trigger(&flat, 2).is_empty());
    }

    #[test]
    fn evaluate_promotes_next_sibling_to_next() {
        let flat = vec![
            heading(1, 1, "P", None),
            heading_with_prop(2, 2, "A", Some("DONE"), ORG_TRIGGER_PROPERTY, "next-sibling todo!(NEXT)"),
            heading(3, 2, "B", Some("TODO")),
        ];
        let changes = evaluate_trigger(&flat, 2);
        assert_eq!(changes, vec![PlannedChange {
            target_line_no: 3,
            new_state: Some("NEXT".to_string()),
        }]);
    }

    #[test]
    fn evaluate_respects_no_sibling_condition() {
        let flat = vec![
            heading(1, 1, "P", None),
            heading_with_prop(2, 2, "A", Some("DONE"), ORG_TRIGGER_PROPERTY,
                              "if-no-sibling-todo(NEXT) next-sibling todo!(NEXT)"),
            heading(3, 2, "B", Some("TODO")),
            heading(4, 2, "C", Some("NEXT")),
        ];
        assert!(evaluate_trigger(&flat, 2).is_empty());
    }

    #[test]
    fn parent_finder_returns_enclosing_heading() {
        let flat = vec![
            heading(1, 1, "P", None),
            heading(2, 2, "A", Some("DONE")),
        ];
        assert_eq!(resolve_finder(&flat, 1, &Finder::Parent), vec![0]);
    }

    #[test]
    fn children_finder_returns_direct_children_only() {
        let flat = vec![
            heading(1, 1, "P", None),
            heading(2, 2, "A", None),
            heading(3, 3, "A.child", None),
            heading(4, 2, "B", None),
        ];
        assert_eq!(resolve_finder(&flat, 0, &Finder::Children), vec![1, 3]);
    }
}
/* ------ apply_triggers against the file, not the cache ------- */
#[cfg(test)]
mod apply_tests {
    use super::*;
    use tempfile::TempDir;

    const VAULT: &str = "\
* DONE Projekt
:PROPERTIES:
:CUSTOM_ID: p1
:TRIGGER: children todo!(NEXT)
:END:
** TODO Eins
:PROPERTIES:
:CUSTOM_ID: c1
:END:
** TODO Zwei
:PROPERTIES:
:CUSTOM_ID: c2
:END:
";

    /// The trigger reads the file, not the index cache.
    ///
    /// The cache is only as current as the last watcher event -- and on
    /// mobile there is no watcher at all (docs/mobile.md). Here something
    /// else has prepended two lines since the last scan, so every heading
    /// sits two lines lower than the index believes. Reading the cache
    /// means either finding nothing at the source line (the trigger
    /// silently never fires) or, worse, writing each target's new state at
    /// the position it used to have.
    #[test]
    fn apply_triggers_reads_the_file_on_disk_not_a_stale_index() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("projekt.org");
        std::fs::write(&path, VAULT).unwrap();

        let index = IndexManager::new();
        index.scan_directory(dir.path());

        // An external edit the index has not seen: everything moves down.
        std::fs::write(&path, format!("#+TITLE: Projekt\n\n{VAULT}")).unwrap();

        apply_triggers(&path, 3, &index).unwrap();

        let result = std::fs::read_to_string(&path).unwrap();
        assert!(result.contains("** NEXT Eins"), "first child triggered:\n{result}");
        assert!(result.contains("** NEXT Zwei"), "second child triggered too:\n{result}");
        assert!(result.starts_with("#+TITLE: Projekt"), "the external edit survived:\n{result}");
        for id in ["p1", "c1", "c2"] {
            assert_eq!(result.matches(&format!(":CUSTOM_ID: {id}")).count(), 1, "{id} exactly once:\n{result}");
        }
    }
}
