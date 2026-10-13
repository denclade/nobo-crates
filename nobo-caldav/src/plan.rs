//! Deciding what a sync run has to do, without doing any of it.
//!
//! Split out from the run itself for the same reason nobo_sync::diff is:
//! this is where every data-loss decision lives, and it is worth being
//! able to test all of them without a server.
//!
//! Three inputs, exactly like a 3-way merge:
//! - `state` -- what this device knew at the end of the last sync (base)
//! - `remote` -- what the server has now
//! - `local` -- what the file has now

use super::store::TaskState;

/// One task in the local file, as far as the planner cares.
#[derive(Debug, Clone, PartialEq)]
pub struct LocalTask {
    /// `None` for a heading the user typed into the file themselves --
    /// those become new remote tasks.
    pub uid: Option<String>,
    /// Hash of the heading's org block, compared against what this device
    /// last wrote to decide whether the user edited it. Deliberately not a
    /// timestamp: the app writes those itself, so a pull would look like a
    /// user edit and the two sides would push each other back and forth
    /// forever.
    pub hash: String,
}

/// A task as the server currently has it.
#[derive(Debug, Clone, PartialEq)]
pub struct RemoteTask {
    pub uid: String,
    pub url: String,
    pub etag: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TaskAction {
    /// Fetch it and put it in the file: the server has it, this device
    /// does not. Carries the etag it was listed at -- without recording
    /// that, the next run sees the task as changed again and re-pulls it
    /// on every single sync, forever.
    Pull { uid: String, url: String, etag: String },
    /// Send the local version. The remote version is the one this device
    /// last saw, so nothing is being overwritten unseen.
    Push { uid: String, url: String, etag: String },
    /// Both sides changed. The local edit wins for the fields the app
    /// owns; everything else keeps the server's value, because the push
    /// patches the server's own component rather than replacing it (see
    /// vtodo::apply_to_raw). Reported separately so it is not invisible.
    PushOverRemoteChange { uid: String, url: String, etag: String },
    /// A heading the user wrote by hand becomes a task on the server.
    /// `local_index` points back into the `local` slice -- without it the
    /// caller cannot tell which of several new headings the generated uid
    /// belongs to.
    Create { uid: String, local_index: usize },
    /// The heading is gone from the file and was not changed remotely.
    DeleteRemote { uid: String, url: String, etag: String },
    /// The task is gone from the server and the file's copy is untouched.
    DeleteLocal { uid: String },
    /// Gone from the server, but the user had edited it since. The edit
    /// wins: the task goes back, rather than the work being thrown away
    /// on the strength of a deletion the user may not have made.
    RecreateRemote { uid: String },
}

impl TaskAction {
    /// The task this action is about.
    ///
    /// Needed because a failed action must not have its state recorded as
    /// if it had happened -- see `sync::persist`.
    pub fn uid(&self) -> &str {
        match self {
            TaskAction::Pull { uid, .. }
            | TaskAction::Push { uid, .. }
            | TaskAction::PushOverRemoteChange { uid, .. }
            | TaskAction::Create { uid, .. }
            | TaskAction::DeleteRemote { uid, .. }
            | TaskAction::DeleteLocal { uid }
            | TaskAction::RecreateRemote { uid } => uid,
        }
    }
}

/// Everything one run has to do, in no particular order.
pub fn plan(state: &[TaskState], remote: &[RemoteTask], local: &[LocalTask]) -> Vec<TaskAction> {
    let by_uid = |uid: &str| state.iter().find(|s| s.uid == uid);
    let remote_by_uid = |uid: &str| remote.iter().find(|r| r.uid == uid);
    let local_by_uid = |uid: &str| local.iter().find(|l| l.uid.as_deref() == Some(uid));

    let mut actions = Vec::new();

    // Everything the server knows about.
    for entry in remote {
        let known = by_uid(&entry.uid).filter(|s| !s.deleted);
        let here = local_by_uid(&entry.uid);

        match (known, here) {
            // Never seen: it is new to this device either way.
            (None, _) => actions.push(pull(entry)),
            (Some(base), None) => {
                // Known, but no longer in the file: the user deleted it
                // here. Only propagate that when the server's copy is
                // still the one they deleted -- otherwise somebody else
                // has changed it since and it is not theirs to remove.
                if base.etag == entry.etag {
                    actions.push(TaskAction::DeleteRemote { uid: entry.uid.clone(), url: entry.url.clone(), etag: entry.etag.clone() });
                } else {
                    actions.push(pull(entry));
                }
            }
            (Some(base), Some(current)) => {
                let remote_changed = base.etag != entry.etag;
                let local_changed = base.local_hash != current.hash;
                match (remote_changed, local_changed) {
                    (false, false) => {}
                    (true, false) => actions.push(pull(entry)),
                    (false, true) => {
                        actions.push(TaskAction::Push { uid: entry.uid.clone(), url: entry.url.clone(), etag: entry.etag.clone() })
                    }
                    (true, true) => actions.push(TaskAction::PushOverRemoteChange {
                        uid: entry.uid.clone(),
                        url: entry.url.clone(),
                        etag: entry.etag.clone(),
                    }),
                }
            }
        }
    }

    // Tasks the server no longer has.
    for base in state.iter().filter(|s| !s.deleted) {
        if remote_by_uid(&base.uid).is_some() {
            continue;
        }
        match local_by_uid(&base.uid) {
            // Gone on both sides already -- nothing to do but forget it,
            // which the caller does once the run succeeds.
            None => {}
            Some(current) if current.hash == base.local_hash => actions.push(TaskAction::DeleteLocal { uid: base.uid.clone() }),
            Some(_) => actions.push(TaskAction::RecreateRemote { uid: base.uid.clone() }),
        }
    }

    // Headings the user wrote into the file themselves.
    for (index, entry) in local.iter().enumerate().filter(|(_, l)| l.uid.is_none()) {
        // Unless a previous run already created this one and died before
        // it could write the uid back into the file.
        //
        // Creating is two writes with a gap: the PUT reaches the server,
        // and the uid reaches the file and the database only at the end of
        // the run. Android kills workers routinely, so the gap is real --
        // and the next run then saw the server's copy as an unknown remote
        // task AND the same heading as new, so it pulled one and created a
        // second. The user's one line became two tasks.
        //
        // `sync_list` writes a provisional row before the PUT: a uid, the
        // heading's hash, and no url yet. Finding one whose hash matches
        // and whose task the server actually has means the create already
        // happened -- so this run adopts it instead of doing it again.
        let already_created = state.iter().find(|s| s.url.is_empty() && !s.local_hash.is_empty() && s.local_hash == entry.hash);
        //
        // Nothing is pushed here: the loop over the server's tasks above
        // has already planned the Pull for it (the uid is in `state` with
        // no matching local heading, and its etag is empty, so it reads as
        // "changed remotely"). All that is needed is to not ALSO create
        // it.
        if already_created.is_some_and(|pending| remote_by_uid(&pending.uid).is_some()) {
            continue;
        }
        actions.push(TaskAction::Create { uid: new_uid(), local_index: index });
    }

    actions
}

fn pull(entry: &RemoteTask) -> TaskAction {
    TaskAction::Pull { uid: entry.uid.clone(), url: entry.url.clone(), etag: entry.etag.clone() }
}

/// UIDs must be globally unique across every client of the collection, so
/// a random one is the only safe choice.
fn new_uid() -> String {
    format!("{}@nobo", uuid::Uuid::new_v4())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A create that reached the server but not the file is adopted, not
    /// repeated.
    ///
    /// The PUT lands, then the run dies before the uid gets into the file
    /// or the database -- routine on Android. The next run saw the
    /// server's copy as an unknown remote task AND the same heading as
    /// new, so it pulled one and created a second: one typed line, two
    /// tasks.
    #[test]
    fn a_create_that_already_reached_the_server_is_adopted() {
        let pending = TaskState { url: String::new(), etag: String::new(), local_hash: "h-neu".to_string(), ..state("uid-1", "", "h-neu") };
        let remote = vec![RemoteTask { uid: "uid-1".to_string(), url: "https://x/uid-1.ics".to_string(), etag: "\"v1\"".to_string() }];
        let local = vec![LocalTask { uid: None, hash: "h-neu".to_string() }];

        let actions = plan(&[pending], &remote, &local);

        assert!(actions.iter().all(|a| !matches!(a, TaskAction::Create { .. })), "nothing is created a second time: {actions:?}");
        assert_eq!(actions.len(), 1);
        assert!(matches!(&actions[0], TaskAction::Pull { uid, .. } if uid == "uid-1"), "{actions:?}");
    }

    /// A provisional row whose task the server does not actually have is
    /// no reason to skip the create -- the PUT never landed.
    #[test]
    fn a_pending_row_without_a_remote_task_still_creates() {
        let pending = TaskState { url: String::new(), etag: String::new(), ..state("uid-1", "", "h-neu") };
        let local = vec![LocalTask { uid: None, hash: "h-neu".to_string() }];

        let actions = plan(&[pending], &[], &local);

        assert!(actions.iter().any(|a| matches!(a, TaskAction::Create { .. })), "{actions:?}");
    }

    fn state(uid: &str, etag: &str, hash: &str) -> TaskState {
        TaskState {
            list_id: "list-1".to_string(),
            uid: uid.to_string(),
            etag: etag.to_string(),
            url: format!("https://x/{uid}.ics"),
            local_hash: hash.to_string(),
            sort_order: Some(1000),
            position: 0,
            unmappable_categories: Vec::new(),
            deleted: false,
        }
    }

    fn remote(uid: &str, etag: &str) -> RemoteTask {
        RemoteTask { uid: uid.to_string(), url: format!("https://x/{uid}.ics"), etag: etag.to_string() }
    }

    fn local(uid: Option<&str>, hash: &str) -> LocalTask {
        LocalTask { uid: uid.map(String::from), hash: hash.to_string() }
    }

    fn only(actions: Vec<TaskAction>) -> TaskAction {
        assert_eq!(actions.len(), 1, "{actions:?}");
        actions.into_iter().next().unwrap()
    }

    /* --- the quiet case --- */

    #[test]
    fn a_task_neither_side_touched_produces_no_work() {
        let actions = plan(&[state("a", "\"v1\"", "h1")], &[remote("a", "\"v1\"")], &[local(Some("a"), "h1")]);

        assert!(actions.is_empty(), "{actions:?}");
    }

    /* --- one side changed --- */

    #[test]
    fn a_task_only_the_server_changed_is_pulled() {
        let actions = plan(&[state("a", "\"v1\"", "h1")], &[remote("a", "\"v2\"")], &[local(Some("a"), "h1")]);

        assert_eq!(only(actions), TaskAction::Pull { uid: "a".into(), url: "https://x/a.ics".into(), etag: "\"v2\"".into() });
    }

    #[test]
    fn a_task_only_the_file_changed_is_pushed() {
        let actions = plan(&[state("a", "\"v1\"", "h1")], &[remote("a", "\"v1\"")], &[local(Some("a"), "h2")]);

        assert_eq!(only(actions), TaskAction::Push { uid: "a".into(), url: "https://x/a.ics".into(), etag: "\"v1\"".into() });
    }

    #[test]
    fn a_task_this_device_has_never_seen_is_pulled() {
        let actions = plan(&[], &[remote("a", "\"v1\"")], &[]);

        assert_eq!(only(actions), TaskAction::Pull { uid: "a".into(), url: "https://x/a.ics".into(), etag: "\"v1\"".into() });
    }

    /* --- both sides changed --- */

    /// Reported as its own action rather than folded into a plain Push:
    /// the user's edit winning over someone else's is a decision, and one
    /// worth being able to count and show.
    #[test]
    fn a_task_both_sides_changed_is_pushed_but_marked_as_such() {
        let actions = plan(&[state("a", "\"v1\"", "h1")], &[remote("a", "\"v2\"")], &[local(Some("a"), "h2")]);

        assert_eq!(
            only(actions),
            TaskAction::PushOverRemoteChange { uid: "a".into(), url: "https://x/a.ics".into(), etag: "\"v2\"".into() }
        );
    }

    /* --- deletions --- */

    #[test]
    fn a_heading_deleted_from_the_file_is_deleted_on_the_server() {
        let actions = plan(&[state("a", "\"v1\"", "h1")], &[remote("a", "\"v1\"")], &[]);

        assert_eq!(only(actions), TaskAction::DeleteRemote { uid: "a".into(), url: "https://x/a.ics".into(), etag: "\"v1\"".into() });
    }

    /// The user deleted the version they had; somebody has changed it
    /// since. Removing it anyway would destroy an edit nobody here has
    /// even seen.
    #[test]
    fn a_local_deletion_does_not_remove_a_task_changed_remotely_since() {
        let actions = plan(&[state("a", "\"v1\"", "h1")], &[remote("a", "\"v2\"")], &[]);

        assert_eq!(only(actions), TaskAction::Pull { uid: "a".into(), url: "https://x/a.ics".into(), etag: "\"v2\"".into() });
    }

    #[test]
    fn a_task_deleted_on_the_server_goes_from_the_file_too() {
        let actions = plan(&[state("a", "\"v1\"", "h1")], &[], &[local(Some("a"), "h1")]);

        assert_eq!(only(actions), TaskAction::DeleteLocal { uid: "a".into() });
    }

    /// The other direction of the same rule: the user's work outweighs a
    /// deletion they did not make.
    #[test]
    fn a_remote_deletion_does_not_discard_a_local_edit() {
        let actions = plan(&[state("a", "\"v1\"", "h1")], &[], &[local(Some("a"), "h2")]);

        assert_eq!(only(actions), TaskAction::RecreateRemote { uid: "a".into() });
    }

    #[test]
    fn a_task_gone_from_both_sides_needs_no_action() {
        let actions = plan(&[state("a", "\"v1\"", "h1")], &[], &[]);

        assert!(actions.is_empty(), "{actions:?}");
    }

    /// Without the tombstone check this would be pulled again on every
    /// single sync -- the deletion would never stick.
    #[test]
    fn a_tombstoned_task_is_not_resurrected() {
        let mut tombstone = state("a", "\"v1\"", "h1");
        tombstone.deleted = true;

        let actions = plan(&[tombstone], &[], &[]);

        assert!(actions.is_empty(), "{actions:?}");
    }

    /* --- new local headings --- */

    #[test]
    fn a_heading_typed_into_the_file_becomes_a_new_remote_task() {
        let actions = plan(&[], &[], &[local(None, "h1")]);

        match only(actions) {
            TaskAction::Create { uid, local_index } => {
                assert_eq!(local_index, 0);
                assert!(uid.ends_with("@nobo"), "{uid}");
            }
            other => panic!("{other:?}"),
        }
    }

    /// Each needs its own identity, and the caller has to be able to tell
    /// which uid belongs to which heading.
    #[test]
    fn several_new_headings_get_distinct_uids_tied_to_their_positions() {
        let actions = plan(&[], &[], &[local(Some("a"), "h1"), local(None, "h2"), local(None, "h3")]);

        let creates: Vec<_> = actions
            .iter()
            .filter_map(|a| match a {
                TaskAction::Create { uid, local_index } => Some((uid.clone(), *local_index)),
                _ => None,
            })
            .collect();
        assert_eq!(creates.len(), 2);
        assert_eq!(creates[0].1, 1);
        assert_eq!(creates[1].1, 2);
        assert_ne!(creates[0].0, creates[1].0);
    }

    /* --- everything at once --- */

    #[test]
    fn a_mixed_run_plans_each_task_independently() {
        let actions = plan(
            &[state("unchanged", "\"v1\"", "h1"), state("pull", "\"v1\"", "h2"), state("push", "\"v1\"", "h3")],
            &[remote("unchanged", "\"v1\""), remote("pull", "\"v2\""), remote("push", "\"v1\""), remote("brandneu", "\"v1\"")],
            &[local(Some("unchanged"), "h1"), local(Some("pull"), "h2"), local(Some("push"), "changed"), local(None, "neu")],
        );

        assert_eq!(actions.len(), 4, "{actions:?}");
        assert!(actions.iter().any(|a| matches!(a, TaskAction::Pull { uid, .. } if uid == "pull")));
        assert!(actions.iter().any(|a| matches!(a, TaskAction::Pull { uid, .. } if uid == "brandneu")));
        assert!(actions.iter().any(|a| matches!(a, TaskAction::Push { uid, .. } if uid == "push")));
        assert!(actions.iter().any(|a| matches!(a, TaskAction::Create { .. })));
    }
}
