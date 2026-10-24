//! One sync run for one list: the layer where planner, client, store,
//! order merge and org rendering meet.
//!
//! Everything the run does to the outside world goes through
//! `CalDavTransport`, so the whole algorithm -- including every branch
//! that can lose data -- is testable without a server.

use std::collections::{HashMap, HashSet};

use async_trait::async_trait;
use chrono::Utc;
use rusqlite::Connection;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::client::{self, CalDavClient, Precondition, RemoteObject, CONFLICT_MARKER};
use super::model::CalDavTask;
use super::model::ListSyncOutcome;
use super::org;
use super::plan::{self, LocalTask, RemoteTask, TaskAction};
use super::sort::{self, ServerEntry};
use super::store::{self, TaskState};
use super::vtodo;
use nobo_core::core::models::Heading;

/// What a run has to be able to ask of a server.
#[async_trait]
pub trait CalDavTransport: Send + Sync {
    async fn ctag(&self, list_url: &str) -> Result<Option<String>, String>;
    /// The objects of one component kind -- see
    /// CalDavClient::list_objects on why the kind is never assumed.
    async fn list_objects(&self, list_url: &str, component: &str) -> Result<Vec<RemoteObject>, String>;
    async fn get_object(&self, url: &str) -> Result<String, String>;
    /// Many objects at once. The default is the honest fallback -- one
    /// request each -- so a transport only implements it where the server
    /// can actually do better (see CalDavClient::get_objects).
    async fn get_objects(&self, _list_url: &str, urls: &[String]) -> Result<Vec<(String, String)>, String> {
        let mut out = Vec::with_capacity(urls.len());
        for url in urls {
            out.push((url.clone(), self.get_object(url).await?));
        }
        Ok(out)
    }
    async fn put_object(&self, url: &str, ics: &str, precondition: Precondition<'_>) -> Result<Option<String>, String>;
    async fn delete_object(&self, url: &str, expected_etag: Option<&str>) -> Result<(), String>;
    fn object_url(&self, list_url: &str, uid: &str) -> String;
}

#[async_trait]
impl CalDavTransport for CalDavClient {
    async fn ctag(&self, list_url: &str) -> Result<Option<String>, String> {
        CalDavClient::ctag(self, list_url).await
    }
    async fn list_objects(&self, list_url: &str, component: &str) -> Result<Vec<RemoteObject>, String> {
        CalDavClient::list_objects(self, list_url, component).await
    }
    async fn get_object(&self, url: &str) -> Result<String, String> {
        CalDavClient::get_object(self, url).await
    }

    async fn get_objects(&self, list_url: &str, urls: &[String]) -> Result<Vec<(String, String)>, String> {
        Ok(CalDavClient::get_objects(self, list_url, urls).await?.into_iter().map(|object| (object.url, object.body)).collect())
    }
    async fn put_object(&self, url: &str, ics: &str, precondition: Precondition<'_>) -> Result<Option<String>, String> {
        CalDavClient::put_object(self, url, ics, precondition).await
    }
    async fn delete_object(&self, url: &str, expected_etag: Option<&str>) -> Result<(), String> {
        CalDavClient::delete_object(self, url, expected_etag).await
    }
    fn object_url(&self, list_url: &str, uid: &str) -> String {
        CalDavClient::object_url(self, list_url, uid)
    }
}

/// Everything needed to sync one list.
pub struct ListTarget<'a> {
    /// What is being synced -- read out of the mirror file's own header
    /// (see caldav::registry), which is why nothing here comes from the
    /// settings any more.
    pub subscription: &'a super::model::CalDavSubscription,
    /// The org file this list is mirrored into.
    pub file: &'a std::path::Path,
}

impl ListTarget<'_> {
    /// Stable id of the subscription; keys the local state.
    fn id(&self) -> &str {
        &self.subscription.id
    }
    /// Collection URL on the server.
    fn url(&self) -> &str {
        &self.subscription.url
    }
}

/// How far a list sync has got.
///
/// `total == 0` means the size of the job is not known yet -- the listing
/// has to come back before there is anything to count.
#[derive(Debug, Clone, Copy, Default, PartialEq, serde::Serialize)]
#[cfg_attr(feature = "tauri", derive(specta::Type))]
pub struct SyncProgress {
    pub done: usize,
    pub total: usize,
}

/// Where progress goes. A trait rather than a callback type so the core
/// stays free of anything Tauri, and `()` is the sink for every caller
/// that has nobody to tell (tests included).
pub trait ProgressSink: Sync {
    fn report(&self, progress: SyncProgress);
}

impl ProgressSink for () {
    fn report(&self, _progress: SyncProgress) {}
}

/// Counts work off against a known total and reports every step.
// Atomics rather than Cells: the counters are passed by reference into
// the async reads below, and a `&Cell` held across an await makes the
// whole command's future non-Send. Nothing here is contended -- the
// atomics are for what the type system needs, not for parallelism.
struct Progress<'a> {
    sink: &'a dyn ProgressSink,
    done: AtomicUsize,
    total: AtomicUsize,
}

impl<'a> Progress<'a> {
    fn new(sink: &'a dyn ProgressSink) -> Self {
        Progress { sink, done: AtomicUsize::new(0), total: AtomicUsize::new(0) }
    }

    /// The size of the job, once the plan is known.
    fn expect(&self, total: usize) {
        self.total.store(total, Ordering::Relaxed);
        self.emit();
    }

    /// More work than the plan showed. Order is reconciled only after
    /// every task is resolved, so the pushes it needs cannot be counted
    /// up front -- and leaving them out is what made a ring sit at "0 of
    /// 1" through a few hundred requests.
    fn expect_more(&self, additional: usize) {
        self.total.fetch_add(additional, Ordering::Relaxed);
        self.emit();
    }

    fn advance(&self, steps: usize) {
        self.done.fetch_add(steps, Ordering::Relaxed);
        self.emit();
    }

    fn finish(&self) {
        self.done.store(self.total.load(Ordering::Relaxed), Ordering::Relaxed);
        self.emit();
    }

    fn emit(&self) {
        // Never past the end: an estimate that overshoots would show a
        // progress ring more than full.
        let total = self.total.load(Ordering::Relaxed);
        self.sink.report(SyncProgress { done: self.done.load(Ordering::Relaxed).min(total), total });
    }
}

/// Syncs one list: pulls what the server has, pushes what this device
/// changed, and rewrites the mirror.
///
/// Takes the mutex rather than a connection: the run does network IO
/// between its reads and its writes, and holding the app's single database
/// lock across that would stall every other part of the app for as long as
/// the server takes. Same convention as nobo_sync::orchestrator.
pub async fn sync_list(
    transport: &dyn CalDavTransport,
    db: &std::sync::Mutex<Connection>,
    target: &ListTarget<'_>,
    sink: &dyn ProgressSink,
) -> Result<ListSyncOutcome, String> {
    let progress = Progress::new(sink);
    progress.emit();
    let (state, known_ctag) = {
        let conn = lock(db)?;
        (store::get_all(&conn, target.id()).map_err(|e| e.to_string())?, store::get_ctag(&conn, target.id()).map_err(|e| e.to_string())?)
    };
    // No readable mirror is NOT an empty list (see read_local). Planning
    // against a state that remembers tasks the file can no longer show
    // would delete every one of them on the server -- which is exactly
    // what happened once the vault sync removed a mirror out from under a
    // device that still had the list subscribed.
    //
    // Fall back to "this device knows nothing about this list": every
    // remote task then looks new, gets pulled, and the mirror is written
    // again from the server. Nothing is deleted on either side. A first
    // sync (no state yet) is that same situation and always was.
    let read_as = file_fingerprint(target.file);
    let (local, mirror_lost) = match read_local(target.file) {
        Some(local) => (local, false),
        None => (Vec::new(), !state.is_empty()),
    };
    let state = if mirror_lost { Vec::new() } else { state };
    let known_ctag = if mirror_lost { None } else { known_ctag };

    // State a build without the CREATED fallback wrote down cannot be
    // trusted for order: it recorded no positions, and the sort orders it
    // did record were invented for every task that carries only CREATED --
    // which lives on the server and nowhere in the file. Neither side
    // changed, so nothing else would ever ask again. Re-read once.
    let stale_state = recorded_order(&state).is_none() && live_count(&state) > 1;

    // Fast path: the server says nothing changed, and the file is exactly
    // what this device last wrote. Skips the whole per-object round trip,
    // which is most of the cost of a sync that has nothing to do.
    let remote_ctag = transport.ctag(target.url()).await?;
    if !stale_state && remote_ctag.is_some() && remote_ctag == known_ctag && !local_has_changes(&state, &local) {
        progress.finish();
        return Ok(ListSyncOutcome::default());
    }

    let objects = transport.list_objects(target.url(), "VTODO").await?;
    // Counted before the read, not after: on a FIRST sync every object is
    // unknown and resolve_remote downloads the whole collection -- the
    // slowest part of the run -- while the ring, told nothing yet, sat at
    // 0/0 through all of it. The plan below adds what it finds on top (see
    // Progress::expect_more).
    progress.expect(unknown_objects(&state, &objects));
    let (remote, mut fetched) = resolve_remote(transport, target.url(), &state, &objects, &progress).await?;

    let local_tasks: Vec<LocalTask> = local.iter().map(|l| LocalTask { uid: l.uid.clone(), hash: l.hash.clone() }).collect();
    // Forgetting the etags is what turns "nothing changed" into "every
    // task changed remotely": with the file unchanged, the plan then reads
    // each one back from the server rather than pushing anything.
    let planning_state: Vec<TaskState> = match stale_state {
        true => state.iter().map(|s| TaskState { etag: String::new(), ..s.clone() }).collect(),
        false => state.clone(),
    };
    let actions = plan::plan(&planning_state, &remote, &local_tasks);

    // Everything the plan wants to read, read at once. Left to the action
    // loop these are one blocking round trip each, which is what made a
    // full re-read of a few hundred tasks take minutes.
    let to_pull: Vec<String> = actions
        .iter()
        .filter_map(|action| match action {
            plan::TaskAction::Pull { uid, url, .. } if !fetched.contains_key(uid) => Some(url.clone()),
            _ => None,
        })
        .collect();
    // Reading is one step per object, applying one per action, and writing
    // the file one more. Close enough to move a progress ring honestly.
    progress.expect_more(to_pull.len() + actions.len() + 1);

    // Chunked here as well as in the client: the client's batching is a
    // limit on request size, this one is what makes progress visible while
    // a few hundred objects come in.
    for batch in to_pull.chunks(client::READ_BATCH) {
        for (_url, body) in transport.get_objects(target.url(), batch).await? {
            for task in vtodo::parse_tasks(&body)? {
                fetched.insert(task.uid.clone(), task);
            }
        }
        progress.advance(batch.len());
    }

    let mut outcome = ListSyncOutcome::default();
    // Where the merged file's content comes from, keyed by uid. Starts as
    // the local copy of everything and is replaced per task by whatever
    // the run decides wins.
    let mut resolved: HashMap<String, CalDavTask> = local.iter().filter_map(|l| l.uid.clone().map(|uid| (uid, l.task.clone()))).collect();
    let mut etags: HashMap<String, String> = state.iter().map(|s| (s.uid.clone(), s.etag.clone())).collect();
    let mut urls: HashMap<String, String> = state.iter().map(|s| (s.uid.clone(), s.url.clone())).collect();
    let mut dropped: Vec<String> = Vec::new();
    let local_uids: std::collections::HashSet<String> = local.iter().filter_map(|l| l.uid.clone()).collect();
    // What this device already knew about, before the run.
    let known_uids: HashSet<String> = state.iter().map(|s| s.uid.clone()).collect();
    let first_sync = state.is_empty();

    // All at once, before any of them exists: a task created in this run
    // is invisible to the others until it does.
    let creating: HashSet<usize> = actions
        .iter()
        .filter_map(|action| match action {
            plan::TaskAction::Create { local_index, .. } => Some(*local_index),
            _ => None,
        })
        .collect();
    let placements = place_new_tasks(&local, &state, &creating);

    // uids whose action the server refused -- see the `Err` arm below.
    let mut failed: std::collections::HashSet<String> = std::collections::HashSet::new();

    // Every uid this run is about to create, written down BEFORE the PUTs.
    //
    // Otherwise a run that is killed between the PUT and the end (Android
    // does this routinely) leaves a task on the server that nothing here
    // knows about, and the next run creates it a second time -- see
    // `plan::plan`. The row carries the heading's hash and no url, which
    // is what marks it as not-yet-confirmed; `persist` overwrites it with
    // the real url and etag when the run gets that far.
    {
        let conn = lock(db)?;
        for action in &actions {
            let plan::TaskAction::Create { uid, local_index } = action else { continue };
            let Some(heading) = local.get(*local_index) else { continue };
            let provisional = TaskState {
                list_id: target.id().to_string(),
                uid: uid.clone(),
                etag: String::new(),
                url: String::new(),
                local_hash: heading.hash.clone(),
                sort_order: placements.get(local_index).copied(),
                position: 0,
                unmappable_categories: Vec::new(),
                deleted: false,
            };
            store::upsert(&conn, &provisional).map_err(|e| e.to_string())?;
        }
    }

    for action in actions {
        let result = apply(transport, target, &action, &local, &state, &local_uids, &mut fetched, &placements).await;
        match result {
            Ok(Applied::Replaced { uid, task, url, etag }) => {
                if let Some(etag) = etag {
                    etags.insert(uid.clone(), etag);
                }
                urls.insert(uid.clone(), url);
                resolved.insert(uid, *task);
            }
            Ok(Applied::Removed { uid }) => {
                resolved.remove(&uid);
                dropped.push(uid);
            }
            Err(message) => {
                // Remembered, not just reported. `resolved` still holds the
                // LOCAL version of this task, and `persist` records
                // `local_hash` from what it writes -- so recording it would
                // tell the next run "this file matches the server", and the
                // user's change would never be retried. A later edit from
                // another client then pulls the server's version over it
                // and the change is gone, with no error and no conflict
                // count. Same shape as the reorder, which already keeps its
                // old position when the push was refused.
                failed.insert(action.uid().to_string());
                outcome.errors.push(message);
            }
        }
        // Only what happened. The counters are what the settings page and
        // the list view show, so counting a refused push as `pushed: 1`
        // says the opposite of the error next to it.
        if !failed.contains(action.uid()) {
            count(&action, &known_uids, first_sync, &mut outcome);
        }
        progress.advance(1);
    }

    // Order, then the file, then what the server needs to know about it.
    let Reordered { ordered, pushed_orders, assignments, read_keys, file_written } =
        reconcile_order(transport, target, &state, &local, &resolved, &urls, &mut etags, &mut outcome, &progress, read_as.as_deref())
            .await?;

    let order_settled = pushed_orders.len() == assignments;
    {
        let conn = lock(db)?;
        // One transaction for the whole bookkeeping.
        //
        // The server and the file have already been changed by the time
        // this runs, so a half-written record is the worst of the three
        // states: some rows updated, some stale, and a ctag that may or
        // may not match either. On Android the worker holds a second
        // connection to the same file, so SQLITE_BUSY is reachable without
        // any crash at all.
        let tx = conn.unchecked_transaction().map_err(|e| e.to_string())?;
        let recorded = Recorded::of(&state);
        persist(
            &conn,
            target.id(),
            &ordered,
            &etags,
            &urls,
            &pushed_orders,
            &dropped,
            &recorded,
            &read_keys,
            order_settled,
            &failed,
            file_written,
        )?;
        // Without the ctag the next run cannot take the fast path, so it
        // is bound to look at the order again. A file this run did not get
        // to write is the same situation: the next run has to look.
        let ctag = if order_settled && file_written { remote_ctag.as_deref() } else { None };
        store::set_ctag(&conn, target.id(), ctag).map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())?;
    }
    progress.finish();
    Ok(outcome)
}

fn lock(db: &std::sync::Mutex<Connection>) -> Result<std::sync::MutexGuard<'_, Connection>, String> {
    db.lock().map_err(|_| "database lock poisoned".to_string())
}

/// Identifies a task as this device last wrote it.
///
/// The parent is part of it, even though the rendered block never shows
/// it: nesting expresses the relationship in the file, but `render_task`
/// normalizes every task to the top level -- so without this, moving a
/// subtask out from under its parent produces the same hash as before and
/// the change is invisible to both the fast path and the planner.
fn task_hash(task: &CalDavTask) -> String {
    store::hash_block(&format!("{}\n{}", task.parent_uid.as_deref().unwrap_or(""), org::render_task(task)))
}

/// Stands in for the uid of a heading the user typed by hand, until the
/// planner assigns a real one.
const PENDING_UID: &str = "pending@nobo";

/// A heading in the file, with everything the run needs from it.
struct LocalHeading {
    uid: Option<String>,
    hash: String,
    task: CalDavTask,
}

/// The mirror's current contents, or `None` when there is no mirror to
/// read -- the file is gone, unreadable, or is not this app's list file at
/// all (no `#+NOBO_TYPE: caldav-list`, so something else wrote it or a
/// half-finished write left it truncated).
///
/// The distinction is not cosmetic, it is the difference between "the user
/// emptied this list" and "this device cannot see the list right now".
/// Returning an empty Vec for both is what let a MISSING file delete every
/// task on the server: the planner reads "known in state, absent from the
/// file" as a deletion the user made (see plan.rs), and applies it
/// remotely. An empty-but-real mirror still returns `Some(vec![])` -- that
/// one IS the user clearing the list, and must keep working.
/// A cheap stand-in for "the file as it was read", so the write at the end
/// of the run can tell whether anything happened to it in between.
///
/// The run reads the file once and, minutes of network later, replaces it
/// whole. Nothing compared the two. On Android the worker runs inside the
/// app's own process and WorkManager fires while the app is open, so:
/// worker reads at T0, the user ticks a box at T1, the worker writes its
/// T0 view at T2 -- and because the state row is written from what was
/// written, the next run saw no local change either. Every other
/// whole-file writer in this codebase compares first
/// (`heading_utils::HeadingEdit::commit`, `nobo_sync`'s
/// `local_matches_diff_snapshot`).
pub(crate) fn file_fingerprint(file: &std::path::Path) -> Option<String> {
    std::fs::read_to_string(file).ok().map(|content| store::hash_block(&content))
}

fn read_local(file: &std::path::Path) -> Option<Vec<LocalHeading>> {
    let content = std::fs::read_to_string(file).ok()?;
    let lines: Vec<&str> = content.lines().collect();
    if nobo_core::core::file_kind::from_lines(&lines) != crate::LIST_KIND {
        return None;
    }
    let headings = nobo_core::core::vault::parse_headings(file, &content);
    // The file's nesting is the parent relationship, so it has to be read
    // from the list as a whole, not from each heading in isolation.
    let parents = org::parent_uids(&headings);

    headings
        .into_iter()
        .zip(parents)
        .filter_map(|(heading, parent_uid)| {
            let uid = org::uid_of(&heading);
            // A heading with no title is not a task the user meant to create.
            if uid.is_none() && heading.title.trim().is_empty() {
                return None;
            }
            let mut task = task_from(&heading, uid.clone());
            task.parent_uid = parent_uid;
            Some(LocalHeading { uid, hash: task_hash(&task), task })
        })
        .collect::<Vec<_>>()
        .into()
}

fn task_from(heading: &Heading, uid: Option<String>) -> CalDavTask {
    match org::task_from_heading(heading) {
        Some(task) => task,
        // A hand-written heading has no uid yet; give it a placeholder so
        // the rest of the pipeline can treat it like any other task.
        None => {
            // A stand-in, not the empty string: `task_from_heading`
            // treats a blank uid as no uid at all and would hand back
            // None again. The planner replaces it with a generated one.
            let mut placeholder = heading.clone();
            placeholder.properties.insert(org::CALDAV_UID_PROPERTY.to_string(), uid.unwrap_or_else(|| PENDING_UID.to_string()));
            org::task_from_heading(&placeholder).expect("a uid was just inserted")
        }
    }
}

/// Whether anything in the file differs from what this device last wrote.
///
/// Order counts as a change. Comparing only the per-task content would
/// let the fast path swallow a pure reorder -- the one edit that changes
/// no task at all -- and the new order would never reach the server.
fn local_has_changes(state: &[TaskState], local: &[LocalHeading]) -> bool {
    if local.iter().any(|l| l.uid.is_none()) {
        return true;
    }
    let live: Vec<&TaskState> = state.iter().filter(|s| !s.deleted).collect();
    if live.len() != local.len() {
        return true;
    }
    let content_changed = local.iter().any(|l| {
        let uid = l.uid.as_deref().unwrap_or_default();
        !live.iter().any(|s| s.uid == uid && s.local_hash == l.hash)
    });
    let local_order: Vec<String> = local.iter().filter_map(|l| l.uid.clone()).collect();
    let order_changed = recorded_order(state).is_some_and(|known| local_order != known);
    content_changed || order_changed
}

/// Turns the collection listing into tasks with UIDs.
///
/// A listing answers with hrefs and ETags, never UIDs. Objects this device
/// already knows are mapped through the stored URL; the rest have to be
/// downloaded -- which is no loss, since an unknown object is new and its
/// content is needed anyway.
/// How many objects the read below will actually fetch -- the ones no
/// previous run recorded. The same rule resolve_remote applies; kept next
/// to it so the two cannot answer differently.
fn unknown_objects(state: &[TaskState], objects: &[RemoteObject]) -> usize {
    objects.iter().filter(|o| !state.iter().any(|s| s.url == o.url && !s.deleted)).count()
}

async fn resolve_remote(
    transport: &dyn CalDavTransport,
    list_url: &str,
    state: &[TaskState],
    objects: &[RemoteObject],
    progress: &Progress<'_>,
) -> Result<(Vec<RemoteTask>, HashMap<String, CalDavTask>), String> {
    let mut remote = Vec::new();
    let mut fetched: HashMap<String, CalDavTask> = HashMap::new();

    let unknown: Vec<String> =
        objects.iter().filter(|o| !state.iter().any(|s| s.url == o.url && !s.deleted)).map(|o| o.url.clone()).collect();
    // Chunked, like the pull loop in sync_list and for the same reason:
    // this is the whole collection on a first sync, and one request for
    // all of it leaves the ring still while the slowest part of the run
    // happens.
    let mut bodies: HashMap<String, String> = HashMap::new();
    for batch in unknown.chunks(client::READ_BATCH) {
        bodies.extend(transport.get_objects(list_url, batch).await?);
        progress.advance(batch.len());
    }

    for object in objects {
        match state.iter().find(|s| s.url == object.url && !s.deleted) {
            Some(known) => remote.push(RemoteTask { uid: known.uid.clone(), url: object.url.clone(), etag: object.etag.clone() }),
            None => {
                // A server that answered the multiget with fewer entries
                // than asked for is not a reason to lose an object.
                let body = match bodies.get(&object.url) {
                    Some(body) => body.clone(),
                    None => transport.get_object(&object.url).await?,
                };
                for task in vtodo::parse_tasks(&body)? {
                    remote.push(RemoteTask { uid: task.uid.clone(), url: object.url.clone(), etag: object.etag.clone() });
                    fetched.insert(task.uid.clone(), task);
                }
            }
        }
    }
    Ok((remote, fetched))
}

/// Boxed: the two variants differ hugely in size otherwise, and every
/// action allocates one.
enum Applied {
    Replaced { uid: String, task: Box<CalDavTask>, url: String, etag: Option<String> },
    Removed { uid: String },
}

#[allow(clippy::too_many_arguments)]
async fn apply(
    transport: &dyn CalDavTransport,
    target: &ListTarget<'_>,
    action: &TaskAction,
    local: &[LocalHeading],
    state: &[TaskState],
    local_uids: &std::collections::HashSet<String>,
    fetched: &mut HashMap<String, CalDavTask>,
    // Where every task being created in this run goes, worked out for all
    // of them at once (see place_new_tasks).
    placements: &HashMap<usize, i64>,
) -> Result<Applied, String> {
    match action {
        TaskAction::Pull { uid, url, etag } => {
            let task = match fetched.remove(uid) {
                Some(task) => task,
                None => task_of(&transport.get_object(url).await?, uid)?,
            };
            Ok(Applied::Replaced { uid: uid.clone(), task: Box::new(task), url: url.clone(), etag: Some(etag.clone()) })
        }

        TaskAction::Push { uid, url, etag } | TaskAction::PushOverRemoteChange { uid, url, etag } => {
            let local_task = local_task(local, uid)?;
            // Fetched fresh rather than remembered: the push patches the
            // server's own component, and patching a stale copy would put
            // back whatever it has lost since (see vtodo::apply_to_raw).
            let raw = transport.get_object(url).await?;
            let remote_task = task_of(&raw, uid)?;
            let merged = merge_for_push(&local_task, &remote_task, state, uid, local_uids);
            let status_changed = local_task.status.is_finished() != remote_task.status.is_finished();
            let body = vtodo::apply_to_raw(&raw, &merged, Utc::now(), status_changed);
            let new_etag = transport.put_object(url, &body, Precondition::Version(etag)).await.map_err(|e| describe(e, uid))?;
            Ok(Applied::Replaced { uid: uid.clone(), task: Box::new(merged), url: url.clone(), etag: new_etag })
        }

        TaskAction::Create { uid, local_index } => {
            let mut task = local.get(*local_index).ok_or_else(|| format!("task {uid} vanished from the file mid-run"))?.task.clone();
            task.uid = uid.clone();
            // Placed explicitly, from where the user put it in the file.
            // Without this the task has no sort order of its own, and
            // every client -- this one included -- falls back to CREATED,
            // which says when it was made and nothing about where it goes.
            task.sort_order = placements.get(local_index).copied();
            let url = transport.object_url(target.url(), uid);
            let body = vtodo::render_new(&task, Utc::now());
            let etag = transport.put_object(&url, &body, Precondition::New).await.map_err(|e| describe(e, uid))?;
            Ok(Applied::Replaced { uid: uid.clone(), task: Box::new(task), url, etag })
        }

        TaskAction::RecreateRemote { uid } => {
            let task = local_task(local, uid)?;
            let url = transport.object_url(target.url(), uid);
            let body = vtodo::render_new(&task, Utc::now());
            let etag = transport.put_object(&url, &body, Precondition::New).await.map_err(|e| describe(e, uid))?;
            Ok(Applied::Replaced { uid: uid.clone(), task: Box::new(task), url, etag })
        }

        TaskAction::DeleteRemote { uid, url, etag } => {
            transport.delete_object(url, Some(etag)).await.map_err(|e| describe(e, uid))?;
            Ok(Applied::Removed { uid: uid.clone() })
        }

        TaskAction::DeleteLocal { uid } => Ok(Applied::Removed { uid: uid.clone() }),
    }
}

/// The version to push when both sides have something to say.
///
/// The local edit wins for every field the app shows, because that is what
/// the user just changed. What they cannot see -- alarms, parent links,
/// categories with no org tag form -- comes from the server's copy, so
/// pushing never destroys what this app merely could not display.
fn merge_for_push(
    local: &CalDavTask,
    remote: &CalDavTask,
    state: &[TaskState],
    uid: &str,
    local_uids: &std::collections::HashSet<String>,
) -> CalDavTask {
    let mut merged = local.clone();
    // The file's nesting decides the parent -- but only for a parent the
    // file could actually show. A task whose parent lives in another list
    // (or is simply not synced here) sits at the top level for want of
    // anywhere else to put it, and reading that as "the user removed the
    // relationship" would delete a link they never touched.
    if merged.parent_uid.is_none() {
        if let Some(remote_parent) = &remote.parent_uid {
            if !local_uids.contains(remote_parent) {
                merged.parent_uid = Some(remote_parent.clone());
            }
        }
    }
    merged.unmappable_categories = if remote.unmappable_categories.is_empty() {
        state.iter().find(|s| s.uid == uid).map(|s| s.unmappable_categories.clone()).unwrap_or_default()
    } else {
        remote.unmappable_categories.clone()
    };
    merged.sort_order = remote.sort_order;
    // The recurrence belongs to the server (see model::CalDavTask), so the
    // local copy never gets a say -- but it has to survive the round trip,
    // or the org file would lose the repeater on the first local edit.
    merged.repeater = remote.repeater.clone();
    merged.raw = remote.raw.clone();
    merged
}

/// What the last run recorded, for the things this one has no better
/// answer for.
struct Recorded<'a> {
    positions: HashMap<&'a str, i64>,
    sort_orders: HashMap<&'a str, i64>,
    first_free: i64,
}

impl<'a> Recorded<'a> {
    fn of(state: &'a [TaskState]) -> Self {
        let live = || state.iter().filter(|s| !s.deleted);
        let positions: HashMap<&str, i64> = live().map(|s| (s.uid.as_str(), s.position)).collect();
        let sort_orders: HashMap<&str, i64> = live().filter_map(|s| s.sort_order.map(|value| (s.uid.as_str(), value))).collect();
        let first_free = positions.values().max().map_or(0, |highest| highest + 1);
        Recorded { positions, sort_orders, first_free }
    }

    /// A task the previous run never saw has no position to keep -- and
    /// giving it its index would collide with whatever else already holds
    /// that number, leaving a recorded "order" with two entries in the
    /// same place and no defined sequence at all. Those go after
    /// everything known, ordered among themselves by the file.
    fn position_of(&self, uid: &str, index: usize) -> i64 {
        match self.positions.get(uid) {
            Some(kept) => *kept,
            None => self.first_free + index as i64,
        }
    }

    fn sort_order_of(&self, uid: &str) -> Option<i64> {
        self.sort_orders.get(uid).copied()
    }
}

/// Where each task being created goes, from where it sits in the file.
///
/// Computed for all of them together, and that is the whole point: asked
/// one at a time, every task typed in the same breath sees only the
/// neighbours that existed BEFORE the run -- so they all land on the same
/// value, and a tie is broken by uid, which is a random uuid. Six entries
/// typed in order came back shuffled.
///
/// Consecutive new entries share the gap between their nearest placed
/// neighbours, evenly. Beyond the last placed entry they step by `STEP`,
/// which leaves room for something to be moved between them later --
/// touching values would force a renumbering of the whole list on the
/// next drag.
fn place_new_tasks(local: &[LocalHeading], state: &[TaskState], creating: &HashSet<usize>) -> HashMap<usize, i64> {
    /// Same spacing as sort::renumber uses, for the same reason.
    const STEP: i64 = 1000;

    let placed_key = |index: usize| -> Option<i64> {
        if creating.contains(&index) {
            return None;
        }
        let uid = local.get(index)?.uid.as_deref()?;
        state.iter().find(|s| s.uid == uid && !s.deleted)?.sort_order
    };

    let mut placements = HashMap::new();
    let mut index = 0;
    while index < local.len() {
        if !creating.contains(&index) {
            index += 1;
            continue;
        }

        // The whole run of new entries, and what encloses it.
        let start = index;
        let mut end = index;
        while end + 1 < local.len() && creating.contains(&(end + 1)) {
            end += 1;
        }
        let count = (end - start + 1) as i64;
        let above = (0..start).rev().find_map(placed_key);
        let below = ((end + 1)..local.len()).find_map(placed_key);

        for (offset, position) in (start..=end).enumerate() {
            let step = offset as i64 + 1;
            let value = match (above, below) {
                // Share the gap: the room for one more each side of every
                // new entry, so none of them collide.
                (Some(above), Some(below)) if below - above > count => Some(above + step * (below - above) / (count + 1)),
                (Some(above), Some(_)) => Some(above + step),
                (Some(above), None) => Some(above + step * STEP),
                (None, Some(below)) => Some(below - (count - offset as i64) * STEP),
                // Nothing placed at all: CREATED answers this as well as
                // anything, and it needs no write.
                (None, None) => None,
            };
            if let Some(value) = value {
                placements.insert(position, value);
            }
        }
        index = end + 1;
    }
    placements
}

/// The sort keys of tasks this run has none for, read from the server.
///
/// A missing key is not an absent one: it usually means only that this run
/// had no reason to re-read the task. Batched, and best effort -- a task
/// whose key cannot be read is simply given one, which is what would have
/// happened anyway.
async fn read_missing_keys(
    transport: &dyn CalDavTransport,
    list_url: &str,
    uids: &[String],
    urls: &HashMap<String, String>,
) -> HashMap<String, i64> {
    if uids.is_empty() {
        return HashMap::new();
    }
    let wanted: Vec<String> = uids.iter().filter_map(|uid| urls.get(uid).cloned()).collect();

    let mut keys = HashMap::new();
    for batch in wanted.chunks(client::READ_BATCH) {
        let bodies = match transport.get_objects(list_url, batch).await {
            Ok(bodies) => bodies,
            Err(e) => {
                log::warn!("could not read sort keys: {e}");
                continue;
            }
        };
        for (_url, body) in bodies {
            for task in vtodo::parse_tasks(&body).unwrap_or_default() {
                if let Some(key) = task.sort_key() {
                    keys.insert(task.uid.clone(), key);
                }
            }
        }
    }
    keys
}

/// The current bodies of everything about to be re-sorted, in one request.
///
/// Missing entries are not an error: `push_sort_order` reads the one it
/// needs itself when it is not here.
async fn read_raw_for(
    transport: &dyn CalDavTransport,
    list_url: &str,
    assignments: &[(String, i64)],
    urls: &HashMap<String, String>,
) -> HashMap<String, String> {
    let wanted: Vec<String> = assignments.iter().filter_map(|(uid, _)| urls.get(uid).cloned()).collect();
    if wanted.is_empty() {
        return HashMap::new();
    }
    let mut raws = HashMap::new();
    for batch in wanted.chunks(client::READ_BATCH) {
        // A failed read is not a failed sync: the individual push falls
        // back to reading its own object and reports its own error.
        match transport.get_objects(list_url, batch).await {
            Ok(bodies) => raws.extend(bodies),
            Err(e) => log::warn!("could not pre-read objects for reordering: {e}"),
        }
    }
    raws
}

/// Writes one task's position back to the server, returning the version
/// the write produced.
async fn push_sort_order(
    transport: &dyn CalDavTransport,
    url: &str,
    task: &CalDavTask,
    value: i64,
    known_raw: Option<&String>,
) -> Result<Option<String>, String> {
    let raw = match known_raw {
        Some(raw) => raw.clone(),
        None => transport.get_object(url).await?,
    };
    let body = set_sort_order(&raw, value);
    // Unconditional on purpose: the value was computed from the listing
    // this run already reconciled, and a reorder losing a race is worth
    // far less than failing the run over it.
    transport.put_object(url, &body, Precondition::Any).await.map_err(|e| describe(e, &task.uid))
}

/// Replaces (or adds) `X-APPLE-SORT-ORDER`, leaving the rest of the
/// component exactly as it is -- same rule as every other write here.
fn set_sort_order(raw: &str, value: i64) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut written = false;
    for line in raw.split("\r\n").flat_map(|l| l.split('\n')) {
        let line = line.trim_end_matches('\r');
        if line.to_ascii_uppercase().starts_with("X-APPLE-SORT-ORDER") {
            out.push(format!("X-APPLE-SORT-ORDER:{value}"));
            written = true;
            continue;
        }
        if !written && line.trim().eq_ignore_ascii_case("END:VTODO") {
            out.push(format!("X-APPLE-SORT-ORDER:{value}"));
            written = true;
        }
        out.push(line.to_string());
    }
    out.join("\r\n")
}

fn local_task(local: &[LocalHeading], uid: &str) -> Result<CalDavTask, String> {
    local
        .iter()
        .find(|l| l.uid.as_deref() == Some(uid))
        .map(|l| l.task.clone())
        .ok_or_else(|| format!("task {uid} vanished from the file mid-run"))
}

fn task_of(ics: &str, uid: &str) -> Result<CalDavTask, String> {
    vtodo::parse_tasks(ics)?.into_iter().find(|t| t.uid == uid).ok_or_else(|| format!("the server's copy of {uid} has no matching VTODO"))
}

fn describe(error: String, uid: &str) -> String {
    if error == CONFLICT_MARKER {
        return format!("{uid}: changed on the server while syncing -- retried on the next run");
    }
    format!("{uid}: {error}")
}

/// Counts one applied action.
///
/// `known` is what the last run recorded -- a Pull for a uid that is not
/// in it is not a re-read but an arrival: somebody else put it there. That
/// distinction is the whole basis of the notifications (see
/// docs/background-sync.md), and it cannot be recovered from the totals
/// afterwards.
fn count(action: &TaskAction, known: &HashSet<String>, first_sync: bool, outcome: &mut ListSyncOutcome) {
    match action {
        TaskAction::Pull { uid, .. } => {
            outcome.pulled += 1;
            // Not on a first sync: everything is new there by definition,
            // and "42 new entries" for a list somebody just subscribed to
            // says nothing.
            if !first_sync && !known.contains(uid) {
                outcome.arrived += 1;
            }
        }
        TaskAction::Push { .. } => outcome.pushed += 1,
        TaskAction::PushOverRemoteChange { .. } => {
            outcome.pushed += 1;
            outcome.local_won_conflicts += 1;
        }
        TaskAction::Create { .. } | TaskAction::RecreateRemote { .. } => outcome.created += 1,
        TaskAction::DeleteRemote { .. } => outcome.deleted_remote += 1,
        TaskAction::DeleteLocal { .. } => outcome.deleted_local += 1,
    }
}

/// How many tasks the last run recorded.
fn live_count(state: &[TaskState]) -> usize {
    state.iter().filter(|s| !s.deleted).count()
}

/// The order this device last wrote the file in -- what a changed file has
/// to be compared against (see TaskState::position) -- or `None` when it
/// was never recorded.
///
/// `None` matters after a database from a build that did not store
/// positions is migrated: every row then carries the same default, and
/// reading that as an order would sort the list by uid and make the next
/// sync believe the user had rearranged it -- pushing that invented order
/// over whatever the server has. Without a recorded order there is simply
/// nothing local to defend, and the server's order wins.
fn recorded_order(state: &[TaskState]) -> Option<Vec<String>> {
    let mut live: Vec<&TaskState> = state.iter().filter(|s| !s.deleted).collect();
    if live.len() > 1 && live.iter().all(|s| s.position == live[0].position) {
        return None;
    }
    live.sort_by_key(|s| (s.position, s.uid.clone()));
    Some(live.into_iter().map(|s| s.uid.clone()).collect())
}

/// The server's own order, which is what the merge reconciles against.
fn sort_order_of(state: &[TaskState], uid: &str) -> Option<i64> {
    state.iter().find(|s| s.uid == uid).and_then(|s| s.sort_order)
}

/// What reconciling the order produced.
struct Reordered {
    /// The tasks in the sequence the file was written in -- which is the
    /// sequence the next run will read back, and therefore the one whose
    /// positions have to be recorded.
    ordered: Vec<CalDavTask>,
    /// The orders that actually reached the server.
    pushed_orders: Vec<(String, i64)>,
    /// How many were meant to (see the caller's `order_settled`).
    assignments: usize,
    /// Whether the mirror was actually replaced. `false` means somebody
    /// else changed the file while this run was on the network, so the
    /// run's own view was NOT written over it -- see `write_file`.
    file_written: bool,
    /// Sort keys read from the server for tasks this run had none for --
    /// recorded so the next run does not have to read them again.
    read_keys: HashMap<String, i64>,
}

/// Merges the three orders -- the file's, the server's, and the one this
/// device last recorded -- writes the file in the result, and pushes what
/// the server needs to know about it.
///
/// Its own step because it is one: everything above resolves individual
/// tasks, and nothing here changes what a task IS. Extracting it also
/// keeps sync_list readable as the sequence it is, rather than one
/// function with three unrelated halves.
#[allow(clippy::too_many_arguments)]
async fn reconcile_order(
    transport: &dyn CalDavTransport,
    target: &ListTarget<'_>,
    state: &[TaskState],
    local: &[LocalHeading],
    resolved: &HashMap<String, CalDavTask>,
    urls: &HashMap<String, String>,
    etags: &mut HashMap<String, String>,
    outcome: &mut ListSyncOutcome,
    progress: &Progress<'_>,
    // The file as it was at the start of the run -- see `write_file`.
    read_as: Option<&str>,
) -> Result<Reordered, String> {
    let local_order: Vec<String> = local.iter().filter_map(|l| l.uid.clone()).collect();
    let base_order: Vec<String> = recorded_order(state).unwrap_or_else(|| local_order.clone());

    // Tasks this run has no order for at all -- neither read this time nor
    // recorded before. Their order is on the server; reading it costs a
    // handful of batched requests, whereas inventing one costs a write per
    // task and overwrites whatever was really there.
    let unknown_order: Vec<String> = resolved
        .keys()
        .filter(|uid| resolved.get(*uid).and_then(|t| t.sort_key()).is_none() && sort_order_of(state, uid).is_none())
        .cloned()
        .collect();
    let read_keys = read_missing_keys(transport, target.url(), &unknown_order, urls).await;

    let server_entries: Vec<ServerEntry> = resolved
        .keys()
        .map(|uid| ServerEntry {
            uid: uid.clone(),
            // From the resolved task, not from `fetched`: applying a Pull
            // takes the task out of that map, and reading it there left
            // every freshly pulled task without an order at all.
            //
            // sort_key, not sort_order: only a dragged task carries the
            // explicit property, and every other client falls back to
            // CREATED on the same scale (see CalDavTask::sort_key).
            sort_order: resolved
                .get(uid)
                .and_then(|t| t.sort_key())
                .or_else(|| sort_order_of(state, uid))
                .or_else(|| read_keys.get(uid).copied()),
        })
        .collect();
    let merged = sort::merge(&base_order, &server_entries, &local_order);

    let merged_tasks: Vec<CalDavTask> = merged.order.iter().filter_map(|uid| resolved.get(uid).cloned()).collect();
    // The file is written as a tree, so THIS is the sequence the next run
    // will read back -- and therefore the one whose positions have to be
    // remembered.
    let ordered: Vec<CalDavTask> = org::nested(&merged_tasks).into_iter().map(|(task, _)| task.clone()).collect();
    let file_written = write_file(target, &merged_tasks, read_as)?;

    progress.expect_more(merged.assignments.len());

    // The push has to patch the server's own component, so it needs the
    // current body -- read for all of them at once rather than one round
    // trip per task ahead of each write.
    let raws = read_raw_for(transport, target.url(), &merged.assignments, urls).await;

    // Only what actually reached the server. Recording an assignment the
    // PUT rejected would file the reorder as done: the next run then sees
    // the file and the record agreeing, finds nothing to do, and the
    // order never leaves this device.
    let mut pushed_orders: Vec<(String, i64)> = Vec::new();

    for (uid, value) in &merged.assignments {
        if let (Some(task), Some(url)) = (resolved.get(uid), urls.get(uid)) {
            match push_sort_order(transport, url, task, *value, raws.get(url)).await {
                // The write changed the object, so it has a new version.
                // Storing the pre-push etag would make the next run think
                // the server had changed the task behind our back -- and
                // re-pull every task on every single sync.
                Ok(Some(new_etag)) => {
                    etags.insert(uid.clone(), new_etag);
                    pushed_orders.push((uid.clone(), *value));
                }
                Ok(None) => {
                    // No etag came back, so what this device holds is
                    // stale either way. Clearing it means the next run
                    // re-reads the task rather than assuming.
                    etags.remove(uid);
                    pushed_orders.push((uid.clone(), *value));
                }
                Err(e) => outcome.errors.push(e),
            }
        }
        progress.advance(1);
    }

    // An order that did not fully reach the server is not this device's
    // order yet. Keeping the positions it had before leaves the file and
    // the record disagreeing, which is exactly what makes the next run
    // notice the reorder again and retry it.

    Ok(Reordered { ordered, pushed_orders, assignments: merged.assignments.len(), read_keys, file_written })
}

/// Rewrites the mirror -- unless it would come out exactly as it already
/// is.
///
/// A sync run that misses the ctag fast path still usually changes
/// nothing: it looked, and the answer was "the same". Writing the file
/// anyway hands the vault sync a modification to carry, on every run, for
/// no content at all. Comparing the rendered text is the complete test --
/// order, entries and header at once -- and the file is small enough that
/// reading it costs nothing next to the round trip that just happened.
///
/// `read_as` is the fingerprint the run started from. A file that no longer
/// matches it was changed by somebody else while this run was on the
/// network, and writing the run's own view over it would silently drop
/// that change -- so the write is skipped and the caller told, which is
/// what `Ok(false)` means. See `file_fingerprint`.
fn write_file(target: &ListTarget<'_>, tasks: &[CalDavTask], read_as: Option<&str>) -> Result<bool, String> {
    let rendered = org::render_list(target.subscription, tasks);
    let current = std::fs::read_to_string(target.file).ok();
    if current.as_deref() == Some(rendered.as_str()) {
        return Ok(true);
    }

    let now = current.as_deref().map(store::hash_block);
    if now.as_deref() != read_as {
        log::warn!(
            "CalDAV: {} changed while '{}' was syncing -- not overwriting it, the next run re-reads it",
            target.file.display(),
            target.subscription.name
        );
        return Ok(false);
    }

    nobo_core::core::vault_write::write_generated(target.file, &rendered)?;
    Ok(true)
}

#[allow(clippy::too_many_arguments)]
fn persist(
    conn: &Connection,
    list_id: &str,
    tasks: &[CalDavTask],
    etags: &HashMap<String, String>,
    urls: &HashMap<String, String>,
    assignments: &[(String, i64)],
    dropped: &[String],
    recorded: &Recorded,
    // Keys read from the server this run for tasks that had none recorded.
    // Without these the same tasks are read again on every single run.
    read_keys: &HashMap<String, i64>,
    // Whether the new order reached the server. When it did not, the
    // positions of the last run are kept, so the file and the record go on
    // disagreeing and the next run notices the reorder again.
    order_settled: bool,
    // Tasks whose action the server refused. Their recorded row is left
    // exactly as it was, so the next run still sees the local change and
    // tries again.
    failed: &std::collections::HashSet<String>,
    // Whether the mirror was actually written. When it was not, the file
    // holds somebody else's newer content, so `local_hash` must not claim
    // to describe it -- everything else this run learned (etags, urls,
    // orders) still gets recorded, because it did happen.
    file_written: bool,
) -> Result<(), String> {
    for (position, task) in tasks.iter().enumerate() {
        let uid = &task.uid;
        if failed.contains(uid) {
            continue;
        }
        // sort_key, not sort_order: a task that has only CREATED still
        // has a position, and the org file does not carry CREATED. Storing
        // the explicit property alone leaves every such task without a key
        // on the next sync that does not re-fetch it -- and the merge then
        // invents an order and pushes it to the server.
        // Falls back to what was recorded before: a run that did not
        // re-read a task learns nothing new about its order, and the copy
        // it has is the local one, which carries neither the property nor
        // CREATED. Taking `None` from that would DROP a value the previous
        // run knew -- and a task with no value has to be given one, so the
        // next reorder rewrites every task that ever went quiet.
        let sort_order = assignments
            .iter()
            .find(|(u, _)| u == uid)
            .map(|(_, v)| *v)
            .or_else(|| task.sort_key())
            .or_else(|| recorded.sort_order_of(uid))
            .or_else(|| read_keys.get(uid).copied());
        let state = TaskState {
            list_id: list_id.to_string(),
            uid: uid.clone(),
            etag: etags.get(uid).cloned().unwrap_or_default(),
            url: urls.get(uid).cloned().unwrap_or_default(),
            // Hashed from what was just WRITTEN, not from what was read:
            // this is the value the next run compares the file against, so
            // it has to describe the file as it now stands.
            //
            // Empty when nothing was written: the file then holds content
            // this run never saw, and claiming to know its hash is how a
            // concurrent edit disappears. An unknown hash reads as "the
            // file differs", so the next run picks the user's version up
            // and pushes it.
            local_hash: if file_written { task_hash(task) } else { String::new() },
            sort_order,
            position: match order_settled {
                true => position as i64,
                false => recorded.position_of(uid, position),
            },
            unmappable_categories: task.unmappable_categories.clone(),
            deleted: false,
        };
        store::upsert(conn, &state).map_err(|e| e.to_string())?;
    }
    // Both sides agree these are gone, so the tombstone has nothing left
    // to protect against.
    for uid in dropped {
        store::forget(conn, list_id, uid).map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tempfile::TempDir;

    /// An in-memory CalDAV collection. Records every write so a test can
    /// assert what the server was actually told, not just what came back.
    #[derive(Default)]
    struct FakeServer {
        objects: Mutex<HashMap<String, (String, String)>>, // url -> (ics, etag)
        ctag: Mutex<Option<String>>,
        puts: Mutex<Vec<(String, String)>>, // url, body
        deletes: Mutex<Vec<String>>,
        gets: Mutex<usize>,
        multigets: Mutex<usize>,
        /// Set to reject the next conditional write, standing in for
        /// another client having got there first.
        reject_next_put: Mutex<bool>,
        /// `(path, content)` written once during the network phase --
        /// stands in for the user editing the file while a background run
        /// is out on the wire. Android's worker runs in the app's own
        /// process, so this is not hypothetical.
        edit_during_run: Mutex<Option<(std::path::PathBuf, String)>>,
    }

    impl FakeServer {
        fn with(objects: &[(&str, &str, &str)]) -> Self {
            let server = FakeServer { ctag: Mutex::new(Some("ctag-1".into())), ..Default::default() };
            for (url, ics, etag) in objects {
                server.objects.lock().unwrap().insert(url.to_string(), (ics.to_string(), etag.to_string()));
            }
            server
        }

        fn body(&self, url: &str) -> String {
            self.objects.lock().unwrap().get(url).map(|(ics, _)| ics.clone()).unwrap_or_default()
        }
    }

    #[async_trait]
    impl CalDavTransport for FakeServer {
        async fn ctag(&self, _list_url: &str) -> Result<Option<String>, String> {
            Ok(self.ctag.lock().unwrap().clone())
        }
        async fn list_objects(&self, _list_url: &str, _component: &str) -> Result<Vec<RemoteObject>, String> {
            if let Some((path, content)) = self.edit_during_run.lock().unwrap().take() {
                std::fs::write(path, content).unwrap();
            }
            let mut objects: Vec<RemoteObject> =
                self.objects.lock().unwrap().iter().map(|(url, (_, etag))| RemoteObject { url: url.clone(), etag: etag.clone() }).collect();
            objects.sort_by(|a, b| a.url.cmp(&b.url));
            Ok(objects)
        }
        async fn get_objects(&self, _list_url: &str, urls: &[String]) -> Result<Vec<(String, String)>, String> {
            *self.multigets.lock().unwrap() += 1;
            Ok(urls.iter().map(|url| (url.clone(), self.body(url))).collect())
        }

        async fn get_object(&self, url: &str) -> Result<String, String> {
            *self.gets.lock().unwrap() += 1;
            self.objects.lock().unwrap().get(url).map(|(ics, _)| ics.clone()).ok_or_else(|| format!("no such object {url}"))
        }
        async fn put_object(&self, url: &str, ics: &str, precondition: Precondition<'_>) -> Result<Option<String>, String> {
            if std::mem::take(&mut *self.reject_next_put.lock().unwrap()) {
                return Err(CONFLICT_MARKER.to_string());
            }
            // Enforced, not ignored: a fake that accepts every write is
            // blind to exactly the mistake of sending the wrong condition
            // -- which is how every reorder came to fail against the real
            // server while the tests stayed green.
            let current = self.objects.lock().unwrap().get(url).map(|(_, etag)| etag.clone());
            let allowed = match (precondition, &current) {
                (Precondition::New, Some(_)) => false,
                (Precondition::Version(expected), Some(etag)) => expected == etag,
                (Precondition::Version(_), None) => false,
                _ => true,
            };
            if !allowed {
                return Err(CONFLICT_MARKER.to_string());
            }
            self.puts.lock().unwrap().push((url.to_string(), ics.to_string()));
            let etag = format!("\"{}\"", self.puts.lock().unwrap().len() + 100);
            self.objects.lock().unwrap().insert(url.to_string(), (ics.to_string(), etag.clone()));
            Ok(Some(etag))
        }
        async fn delete_object(&self, url: &str, _expected: Option<&str>) -> Result<(), String> {
            self.deletes.lock().unwrap().push(url.to_string());
            self.objects.lock().unwrap().remove(url);
            Ok(())
        }
        fn object_url(&self, list_url: &str, uid: &str) -> String {
            format!("{}/{uid}.ics", list_url.trim_end_matches('/'))
        }
    }

    const LIST_URL: &str = "https://cloud.example.com/dav/einkauf";

    fn vtodo(uid: &str, summary: &str, extra: &str) -> String {
        format!(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VTODO\r\nUID:{uid}\r\nSUMMARY:{summary}\r\n\
STATUS:NEEDS-ACTION\r\n{extra}END:VTODO\r\nEND:VCALENDAR\r\n"
        )
    }

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        store::init_schema(&conn).unwrap();
        conn
    }

    struct Fixture {
        _dir: TempDir,
        file: std::path::PathBuf,
        conn: std::sync::Mutex<Connection>,
        subscription: super::super::model::CalDavSubscription,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = TempDir::new().unwrap();
            let file = dir.path().join("einkauf.org");
            let subscription =
                super::super::registry::subscription_for(super::super::model::CalDavKind::Tasks, "Einkauf", "dennis", LIST_URL);
            Fixture { _dir: dir, file, conn: std::sync::Mutex::new(db()), subscription }
        }

        fn target(&self) -> ListTarget<'_> {
            ListTarget { subscription: &self.subscription, file: &self.file }
        }

        async fn sync(&self, server: &FakeServer) -> ListSyncOutcome {
            sync_list(server, &self.conn, &self.target(), &()).await.unwrap()
        }

        fn content(&self) -> String {
            std::fs::read_to_string(&self.file).unwrap_or_default()
        }

        /// Header plus one entry per task heading -- string surgery on the
        /// raw file is too fragile to build a test on.
        fn blocks(&self) -> (String, Vec<String>) {
            let content = self.content();
            let mut header = String::new();
            let mut blocks: Vec<String> = Vec::new();
            for line in content.lines() {
                if line.starts_with("* ") {
                    blocks.push(String::new());
                }
                match blocks.last_mut() {
                    Some(block) => {
                        block.push_str(line);
                        block.push('\n');
                    }
                    None => {
                        header.push_str(line);
                        header.push('\n');
                    }
                }
            }
            (header, blocks)
        }

        fn write_blocks(&self, header: &str, blocks: &[String]) {
            std::fs::write(&self.file, format!("{header}{}", blocks.concat())).unwrap();
        }
    }

    /* --- first contact --- */

    #[tokio::test]
    async fn a_first_sync_writes_every_remote_task_into_the_file() {
        let fixture = Fixture::new();
        let server = FakeServer::with(&[
            ("https://cloud.example.com/dav/einkauf/a.ics", &vtodo("a", "Milch", "X-APPLE-SORT-ORDER:1000\r\n"), "\"v1\""),
            ("https://cloud.example.com/dav/einkauf/b.ics", &vtodo("b", "Brot", "X-APPLE-SORT-ORDER:2000\r\n"), "\"v1\""),
        ]);

        let outcome = fixture.sync(&server).await;

        assert_eq!(outcome.pulled, 2);
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        let content = fixture.content();
        assert!(content.contains("* TODO Milch"), "{content}");
        assert!(content.contains("* TODO Brot"), "{content}");
        // The file declares its kind, which is what makes the app treat it
        // as a list rather than an ordinary note.
        assert_eq!(nobo_core::core::file_kind::from_lines(&content.lines().collect::<Vec<_>>()), crate::LIST_KIND);
    }

    /// The whole point of the CTag: a run with nothing to do must not
    /// download every object to find that out.
    #[tokio::test]
    async fn an_unchanged_list_does_no_per_object_work_at_all() {
        let fixture = Fixture::new();
        let server = FakeServer::with(&[("https://cloud.example.com/dav/einkauf/a.ics", &vtodo("a", "Milch", ""), "\"v1\"")]);
        fixture.sync(&server).await;
        let after_first = *server.gets.lock().unwrap();

        let outcome = fixture.sync(&server).await;

        assert_eq!(outcome, ListSyncOutcome::default());
        assert_eq!(*server.gets.lock().unwrap(), after_first, "no object was fetched again");
    }

    /// ...but a local edit must still be noticed even when the server says
    /// nothing changed, or an edit made offline would never leave the
    /// device.
    #[tokio::test]
    async fn a_local_edit_is_pushed_even_when_the_ctag_is_unchanged() {
        let fixture = Fixture::new();
        let server = FakeServer::with(&[("https://cloud.example.com/dav/einkauf/a.ics", &vtodo("a", "Milch", ""), "\"v1\"")]);
        fixture.sync(&server).await;

        let edited = fixture.content().replace("* TODO Milch", "* DONE Milch");
        std::fs::write(&fixture.file, edited).unwrap();
        let outcome = fixture.sync(&server).await;

        assert_eq!(outcome.pushed, 1, "{outcome:?}");
        assert!(server.body("https://cloud.example.com/dav/einkauf/a.ics").contains("STATUS:COMPLETED"));
    }

    /* --- the two directions --- */

    #[tokio::test]
    async fn a_remote_change_is_pulled_into_the_file() {
        let fixture = Fixture::new();
        let server = FakeServer::with(&[("https://cloud.example.com/dav/einkauf/a.ics", &vtodo("a", "Milch", ""), "\"v1\"")]);
        fixture.sync(&server).await;

        server
            .objects
            .lock()
            .unwrap()
            .insert("https://cloud.example.com/dav/einkauf/a.ics".into(), (vtodo("a", "Hafermilch", ""), "\"v2\"".into()));
        *server.ctag.lock().unwrap() = Some("ctag-2".into());
        let outcome = fixture.sync(&server).await;

        assert_eq!(outcome.pulled, 1);
        assert!(fixture.content().contains("Hafermilch"), "{}", fixture.content());
    }

    #[tokio::test]
    async fn a_heading_typed_into_the_file_becomes_a_task_on_the_server() {
        let fixture = Fixture::new();
        let server = FakeServer::with(&[]);
        fixture.sync(&server).await;

        std::fs::write(&fixture.file, format!("{}* TODO Von Hand\n", fixture.content())).unwrap();
        let outcome = fixture.sync(&server).await;

        assert_eq!(outcome.created, 1, "{outcome:?}");
        // One write. The only task in a list needs no order of its own:
        // its CREATED already places it, here and in every other client.
        let puts = server.puts.lock().unwrap();
        assert_eq!(puts.len(), 1, "{puts:?}");
        assert!(puts.iter().any(|(_, body)| body.contains("SUMMARY:Von Hand")), "{puts:?}");
        // It comes back with an identity, so the next run does not create
        // it a second time.
        assert!(fixture.content().contains(":CALDAV_UID:"), "{}", fixture.content());
    }

    #[tokio::test]
    async fn creating_a_task_twice_does_not_happen() {
        let fixture = Fixture::new();
        let server = FakeServer::with(&[]);
        fixture.sync(&server).await;
        std::fs::write(&fixture.file, format!("{}* TODO Von Hand\n", fixture.content())).unwrap();
        fixture.sync(&server).await;

        server.puts.lock().unwrap().clear();
        let outcome = fixture.sync(&server).await;

        assert_eq!(outcome.created, 0, "{outcome:?}");
        assert!(server.puts.lock().unwrap().is_empty(), "nothing left to write");
    }

    /* --- the paths that can lose data --- */

    /// The user ticked something off here while somebody renamed it there.
    /// The local edit wins for what the user can see -- and it is counted,
    /// because that is a decision made on their behalf.
    #[tokio::test]
    async fn when_both_sides_changed_the_local_edit_wins_and_is_reported() {
        let fixture = Fixture::new();
        let server = FakeServer::with(&[("https://cloud.example.com/dav/einkauf/a.ics", &vtodo("a", "Milch", ""), "\"v1\"")]);
        fixture.sync(&server).await;

        std::fs::write(&fixture.file, fixture.content().replace("* TODO Milch", "* DONE Milch")).unwrap();
        server
            .objects
            .lock()
            .unwrap()
            .insert("https://cloud.example.com/dav/einkauf/a.ics".into(), (vtodo("a", "Vollmilch", ""), "\"v2\"".into()));
        *server.ctag.lock().unwrap() = Some("ctag-2".into());

        let outcome = fixture.sync(&server).await;

        assert_eq!(outcome.local_won_conflicts, 1, "{outcome:?}");
        assert!(server.body("https://cloud.example.com/dav/einkauf/a.ics").contains("STATUS:COMPLETED"));
    }

    /// The single most destructive thing this module could do: push a
    /// component rebuilt from what it understands, deleting the alarm and
    /// the parent link the server had.
    #[tokio::test]
    async fn a_push_preserves_properties_the_app_knows_nothing_about() {
        let fixture = Fixture::new();
        let extras = "RELATED-TO;RELTYPE=PARENT:parent-uid\r\nBEGIN:VALARM\r\nTRIGGER:-PT15M\r\nEND:VALARM\r\n";
        let server = FakeServer::with(&[("https://cloud.example.com/dav/einkauf/a.ics", &vtodo("a", "Milch", extras), "\"v1\"")]);
        fixture.sync(&server).await;

        std::fs::write(&fixture.file, fixture.content().replace("* TODO Milch", "* TODO Hafermilch")).unwrap();
        fixture.sync(&server).await;

        let body = server.body("https://cloud.example.com/dav/einkauf/a.ics");
        assert!(body.contains("RELATED-TO;RELTYPE=PARENT:parent-uid"), "{body}");
        assert!(body.contains("BEGIN:VALARM"), "{body}");
        assert!(body.contains("SUMMARY:Hafermilch"), "{body}");
    }

    #[tokio::test]
    async fn deleting_a_heading_deletes_the_task_on_the_server() {
        let fixture = Fixture::new();
        let server = FakeServer::with(&[
            ("https://cloud.example.com/dav/einkauf/a.ics", &vtodo("a", "Milch", ""), "\"v1\""),
            ("https://cloud.example.com/dav/einkauf/b.ics", &vtodo("b", "Brot", ""), "\"v1\""),
        ]);
        fixture.sync(&server).await;

        let (header, blocks) = fixture.blocks();
        let kept: Vec<String> = blocks.into_iter().filter(|b| !b.contains("Milch")).collect();
        assert_eq!(kept.len(), 1, "the fixture must remove exactly one task");
        fixture.write_blocks(&header, &kept);
        let outcome = fixture.sync(&server).await;

        assert_eq!(outcome.deleted_remote, 1, "{outcome:?}");
        assert_eq!(*server.deletes.lock().unwrap(), vec!["https://cloud.example.com/dav/einkauf/a.ics"]);
    }

    /// The reported incident, reproduced: a list that had synced fine went
    /// completely empty on the server -- every task deleted, the
    /// collection left behind. The mirror file had disappeared underneath
    /// the device (the vault sync propagated its deletion after the list
    /// ids changed, while that device still had the old subscription), and
    /// a missing file was read as "the user deleted everything here".
    #[tokio::test]
    async fn a_missing_mirror_file_never_deletes_anything_on_the_server() {
        let fixture = Fixture::new();
        let server = FakeServer::with(&[
            ("https://cloud.example.com/dav/einkauf/a.ics", &vtodo("a", "Milch", ""), "\"v1\""),
            ("https://cloud.example.com/dav/einkauf/b.ics", &vtodo("b", "Brot", ""), "\"v1\""),
        ]);
        fixture.sync(&server).await;

        // Gone -- not emptied by the user, gone.
        std::fs::remove_file(&fixture.file).unwrap();
        let outcome = fixture.sync(&server).await;

        assert_eq!(outcome.deleted_remote, 0, "nothing may be deleted on the server: {outcome:?}");
        assert!(server.deletes.lock().unwrap().is_empty(), "{:?}", server.deletes.lock().unwrap());
        assert_eq!(server.objects.lock().unwrap().len(), 2, "both tasks still on the server");
        // And the mirror is back, rebuilt from the server.
        assert!(fixture.content().contains("Milch"), "{}", fixture.content());
        assert!(fixture.content().contains("Brot"), "{}", fixture.content());
    }

    /// Same protection for a file that is there but is not the mirror any
    /// more -- a truncated write, or something else writing to that path.
    /// Without the type declaration it cannot be read as "these are the
    /// tasks that remain".
    #[tokio::test]
    async fn a_file_that_no_longer_declares_itself_the_mirror_deletes_nothing_either() {
        let fixture = Fixture::new();
        let server = FakeServer::with(&[("https://cloud.example.com/dav/einkauf/a.ics", &vtodo("a", "Milch", ""), "\"v1\"")]);
        fixture.sync(&server).await;

        std::fs::write(&fixture.file, "").unwrap();
        let outcome = fixture.sync(&server).await;

        assert_eq!(outcome.deleted_remote, 0, "{outcome:?}");
        assert!(server.deletes.lock().unwrap().is_empty());
        assert!(fixture.content().contains("Milch"), "rebuilt: {}", fixture.content());
    }

    /// The other half of the guard: emptying the list is still a thing the
    /// user is allowed to do. A real mirror with no tasks left in it means
    /// exactly that, and must still reach the server.
    #[tokio::test]
    async fn clearing_a_mirror_that_is_still_intact_does_delete_remotely() {
        let fixture = Fixture::new();
        let server = FakeServer::with(&[("https://cloud.example.com/dav/einkauf/a.ics", &vtodo("a", "Milch", ""), "\"v1\"")]);
        fixture.sync(&server).await;

        let (header, _) = fixture.blocks();
        fixture.write_blocks(&header, &[]);
        let outcome = fixture.sync(&server).await;

        assert_eq!(outcome.deleted_remote, 1, "{outcome:?}");
        assert_eq!(*server.deletes.lock().unwrap(), vec!["https://cloud.example.com/dav/einkauf/a.ics"]);
    }

    #[tokio::test]
    async fn a_task_deleted_on_the_server_leaves_the_file() {
        let fixture = Fixture::new();
        let server = FakeServer::with(&[("https://cloud.example.com/dav/einkauf/a.ics", &vtodo("a", "Milch", ""), "\"v1\"")]);
        fixture.sync(&server).await;

        server.objects.lock().unwrap().clear();
        *server.ctag.lock().unwrap() = Some("ctag-2".into());
        let outcome = fixture.sync(&server).await;

        assert_eq!(outcome.deleted_local, 1, "{outcome:?}");
        assert!(!fixture.content().contains("Milch"), "{}", fixture.content());
    }

    /// And it stays gone -- without the tombstone it would be pulled back
    /// on the very next run.
    #[tokio::test]
    async fn a_deletion_is_not_undone_by_the_next_sync() {
        let fixture = Fixture::new();
        let server = FakeServer::with(&[("https://cloud.example.com/dav/einkauf/a.ics", &vtodo("a", "Milch", ""), "\"v1\"")]);
        fixture.sync(&server).await;
        server.objects.lock().unwrap().clear();
        *server.ctag.lock().unwrap() = Some("ctag-2".into());
        fixture.sync(&server).await;

        let outcome = fixture.sync(&server).await;

        assert_eq!(outcome, ListSyncOutcome::default(), "{outcome:?}");
        assert!(!fixture.content().contains("Milch"));
    }

    /// A deletion the user did not make must not throw away work they did.
    #[tokio::test]
    async fn a_remote_deletion_does_not_discard_a_local_edit() {
        let fixture = Fixture::new();
        let server = FakeServer::with(&[("https://cloud.example.com/dav/einkauf/a.ics", &vtodo("a", "Milch", ""), "\"v1\"")]);
        fixture.sync(&server).await;

        std::fs::write(&fixture.file, fixture.content().replace("* TODO Milch", "* TODO Hafermilch dringend")).unwrap();
        server.objects.lock().unwrap().clear();
        *server.ctag.lock().unwrap() = Some("ctag-2".into());
        let outcome = fixture.sync(&server).await;

        assert_eq!(outcome.created, 1, "{outcome:?}");
        assert!(fixture.content().contains("Hafermilch dringend"), "{}", fixture.content());
    }

    /* --- subtasks and recurrence --- */

    #[tokio::test]
    async fn a_subtask_arrives_nested_under_its_parent() {
        let fixture = Fixture::new();
        let server = FakeServer::with(&[
            ("https://cloud.example.com/dav/einkauf/p.ics", &vtodo("p", "Einkauf planen", "X-APPLE-SORT-ORDER:1000\r\n"), "\"v1\""),
            (
                "https://cloud.example.com/dav/einkauf/c.ics",
                &vtodo("c", "Milch", "X-APPLE-SORT-ORDER:2000\r\nRELATED-TO;RELTYPE=PARENT:p\r\n"),
                "\"v1\"",
            ),
        ]);

        fixture.sync(&server).await;

        let content = fixture.content();
        assert!(content.contains("* TODO Einkauf planen"), "{content}");
        assert!(content.contains("** TODO Milch"), "{content}");
    }

    /// Re-nesting in the file has to reach the server, or dragging a
    /// subtask somewhere else is a local-only illusion.
    #[tokio::test]
    async fn moving_a_subtask_out_in_the_file_removes_the_parent_link() {
        let fixture = Fixture::new();
        let server = FakeServer::with(&[
            ("https://cloud.example.com/dav/einkauf/p.ics", &vtodo("p", "Planen", "X-APPLE-SORT-ORDER:1000\r\n"), "\"v1\""),
            (
                "https://cloud.example.com/dav/einkauf/c.ics",
                &vtodo("c", "Milch", "X-APPLE-SORT-ORDER:2000\r\nRELATED-TO;RELTYPE=PARENT:p\r\n"),
                "\"v1\"",
            ),
        ]);
        fixture.sync(&server).await;

        std::fs::write(&fixture.file, fixture.content().replace("** TODO Milch", "* TODO Milch")).unwrap();
        fixture.sync(&server).await;

        let body = server.body("https://cloud.example.com/dav/einkauf/c.ics");
        assert!(!body.contains("RELATED-TO"), "{body}");
    }

    /// The parent is in another list, so the file cannot show the
    /// relationship at all. Reading its absence as "the user removed it"
    /// would delete a link they never touched.
    #[tokio::test]
    async fn a_parent_outside_this_list_survives_a_local_edit() {
        let fixture = Fixture::new();
        let server = FakeServer::with(&[(
            "https://cloud.example.com/dav/einkauf/c.ics",
            &vtodo("c", "Milch", "RELATED-TO;RELTYPE=PARENT:woanders\r\n"),
            "\"v1\"",
        )]);
        fixture.sync(&server).await;

        std::fs::write(&fixture.file, fixture.content().replace("* TODO Milch", "* TODO Hafermilch")).unwrap();
        fixture.sync(&server).await;

        let body = server.body("https://cloud.example.com/dav/einkauf/c.ics");
        assert!(body.contains("SUMMARY:Hafermilch"), "{body}");
        assert!(body.contains("RELATED-TO;RELTYPE=PARENT:woanders"), "{body}");
    }

    /// The recurrence is the server's. A local edit must not overwrite the
    /// dates it derives from the rule.
    #[tokio::test]
    async fn editing_a_recurring_task_leaves_its_rule_and_dates_alone() {
        let fixture = Fixture::new();
        let server = FakeServer::with(&[(
            "https://cloud.example.com/dav/einkauf/a.ics",
            &vtodo("a", "Müll", "DUE;VALUE=DATE:20260825\r\nRRULE:FREQ=WEEKLY\r\n"),
            "\"v1\"",
        )]);
        fixture.sync(&server).await;

        // The interval is visible in the file, so the user can see it
        // recurs.
        assert!(fixture.content().contains("+1w"), "{}", fixture.content());

        std::fs::write(&fixture.file, fixture.content().replace("* TODO Müll", "* DONE Müll")).unwrap();
        fixture.sync(&server).await;

        let body = server.body("https://cloud.example.com/dav/einkauf/a.ics");
        assert!(body.contains("STATUS:COMPLETED"), "the completion must reach the server: {body}");
        assert!(body.contains("RRULE:FREQ=WEEKLY"), "{body}");
        assert!(body.contains("DUE;VALUE=DATE:20260825"), "{body}");
    }

    /* --- failure handling --- */

    /// One task the server refuses must not cost the run: everything else
    /// still syncs, and the failure is reported rather than thrown.
    #[tokio::test]
    async fn a_rejected_write_is_reported_without_failing_the_whole_run() {
        let fixture = Fixture::new();
        let server = FakeServer::with(&[("https://cloud.example.com/dav/einkauf/a.ics", &vtodo("a", "Milch", ""), "\"v1\"")]);
        fixture.sync(&server).await;

        std::fs::write(&fixture.file, fixture.content().replace("* TODO Milch", "* DONE Milch")).unwrap();
        *server.reject_next_put.lock().unwrap() = true;
        let outcome = fixture.sync(&server).await;

        assert_eq!(outcome.errors.len(), 1, "{outcome:?}");
        assert!(outcome.errors[0].contains("next run"), "{:?}", outcome.errors);
    }

    /// And the run after that tries again.
    ///
    /// The state row used to be written from what this run WROTE, so a
    /// refused push recorded the local version as the agreed one: the next
    /// run saw no local change, reported `pushed: 0`, and the user's edit
    /// sat there until somebody else's edit pulled the server's version
    /// over it. Silently -- no error, no conflict count.
    ///
    /// The reorder path already got this right ("a rejected reorder is
    /// retried instead of being forgotten"); the task push did not.
    #[tokio::test]
    async fn a_rejected_write_is_retried_on_the_next_run() {
        let fixture = Fixture::new();
        let server = FakeServer::with(&[("https://cloud.example.com/dav/einkauf/a.ics", &vtodo("a", "Milch", ""), "\"v1\"")]);
        fixture.sync(&server).await;

        std::fs::write(&fixture.file, fixture.content().replace("* TODO Milch", "* DONE Milch")).unwrap();
        *server.reject_next_put.lock().unwrap() = true;
        let refused = fixture.sync(&server).await;
        assert_eq!(refused.errors.len(), 1, "{refused:?}");
        assert_eq!(refused.pushed, 0, "nothing reached the server: {refused:?}");

        // Nothing else changed -- the same local edit is still waiting.
        let retried = fixture.sync(&server).await;

        assert!(retried.errors.is_empty(), "{retried:?}");
        assert_eq!(retried.pushed, 1, "the refused edit is pushed again: {retried:?}");
    }

    /// A file that changed while the run was on the network is not
    /// overwritten with the run's own stale view.
    ///
    /// The run reads the mirror once and replaces it whole minutes later.
    /// Nothing compared the two, and because the state row was written
    /// from what was written, the next run saw no local change either --
    /// so the user's edit was gone twice over. Android's worker shares the
    /// app's process, so "while the app is open" is the normal case.
    #[tokio::test]
    async fn an_edit_made_during_the_run_is_not_overwritten() {
        let fixture = Fixture::new();
        let server = FakeServer::with(&[("https://cloud.example.com/dav/einkauf/a.ics", &vtodo("a", "Milch", ""), "\"v1\"")]);
        fixture.sync(&server).await;

        // The user ticks the box while the run is listing the collection.
        let edited = fixture.content().replace("* TODO Milch", "* DONE Milch");
        *server.edit_during_run.lock().unwrap() = Some((fixture.file.clone(), edited.clone()));
        // Something has to make the run reach the write at all.
        server
            .objects
            .lock()
            .unwrap()
            .insert("https://cloud.example.com/dav/einkauf/b.ics".to_string(), (vtodo("b", "Brot", ""), "\"v1\"".to_string()));
        *server.ctag.lock().unwrap() = Some("ctag-2".into());

        fixture.sync(&server).await;

        assert!(fixture.content().contains("* DONE Milch"), "the edit made during the run survived:\n{}", fixture.content());

        // And the next run picks it up rather than treating it as settled.
        let next = fixture.sync(&server).await;
        assert!(fixture.content().contains("* DONE Milch"), "{}", fixture.content());
        assert!(next.errors.is_empty(), "{next:?}");
    }

    /* --- ordering --- */

    /// The file's order is the TREE order (a child follows its parent),
    /// while the server's is flat by sort value. Comparing the file
    /// against the flat order therefore reads as "the user reordered
    /// this" on every single sync -- and pushes the tree layout back over
    /// whatever order the server actually had.
    #[tokio::test]
    async fn nesting_is_not_mistaken_for_the_user_reordering_the_list() {
        let fixture = Fixture::new();
        // The child sorts BEFORE its parent on the server, so flat order
        // and tree order genuinely differ.
        let server = FakeServer::with(&[
            (
                "https://cloud.example.com/dav/einkauf/c.ics",
                &vtodo("c", "Milch", "X-APPLE-SORT-ORDER:1000\r\nRELATED-TO;RELTYPE=PARENT:p\r\n"),
                "\"v1\"",
            ),
            ("https://cloud.example.com/dav/einkauf/p.ics", &vtodo("p", "Planen", "X-APPLE-SORT-ORDER:2000\r\n"), "\"v1\""),
        ]);
        fixture.sync(&server).await;
        server.puts.lock().unwrap().clear();

        // Nobody touched anything, so a second run must be silent.
        let outcome = fixture.sync(&server).await;

        assert_eq!(outcome, ListSyncOutcome::default(), "{outcome:?}");
        assert!(server.puts.lock().unwrap().is_empty(), "the order was pushed back for no reason");
    }

    #[tokio::test]
    async fn the_file_follows_the_servers_order() {
        let fixture = Fixture::new();
        let server = FakeServer::with(&[
            ("https://cloud.example.com/dav/einkauf/a.ics", &vtodo("a", "Zuletzt", "X-APPLE-SORT-ORDER:3000\r\n"), "\"v1\""),
            ("https://cloud.example.com/dav/einkauf/b.ics", &vtodo("b", "Zuerst", "X-APPLE-SORT-ORDER:1000\r\n"), "\"v1\""),
        ]);

        fixture.sync(&server).await;

        let content = fixture.content();
        assert!(content.find("Zuerst").unwrap() < content.find("Zuletzt").unwrap(), "{content}");
    }

    /// The reported bug, with the list that produced it: nine entries, one
    /// of them dragged in Nextcloud, the rest carrying nothing but
    /// CREATED. Inventing values for the eight untouched ones put the list
    /// in an order no other client agreed with.
    #[tokio::test]
    async fn the_order_matches_what_nextcloud_and_tasks_org_show() {
        let fixture = Fixture::new();
        // (uid, summary, CREATED, explicit sort order)
        let list: [(&str, &str, &str, Option<i64>); 9] = [
            ("a", "Item A", "20260823T155150Z", None),
            ("b", "Item B", "20260823T155155Z", None),
            ("c", "Item C", "20260823T155201Z", None),
            ("d", "Item D", "20260823T155204Z", None),
            ("e", "Item E", "20260823T155208Z", None),
            ("f", "Item F", "20260823T155215Z", None),
            ("g", "Item G", "20260823T155221Z", None),
            // Created last, dragged to fourth place.
            ("h", "Moved item", "20260823T160042Z", Some(809193116)),
            ("i", "Earlier item", "20260810T060855Z", None),
        ];
        let objects: Vec<(String, String, String)> = list
            .iter()
            .map(|(uid, summary, created, sort)| {
                let extra = match sort {
                    Some(value) => format!("CREATED:{created}\r\nX-APPLE-SORT-ORDER:{value}\r\n"),
                    None => format!("CREATED:{created}\r\n"),
                };
                (format!("https://cloud.example.com/dav/einkauf/{uid}.ics"), vtodo(uid, summary, &extra), "\"v1\"".to_string())
            })
            .collect();
        let server = FakeServer::with(&objects.iter().map(|(u, b, e)| (u.as_str(), b.as_str(), e.as_str())).collect::<Vec<_>>());

        fixture.sync(&server).await;

        let content = fixture.content();
        let titles: Vec<&str> = content.lines().filter_map(|l| l.strip_prefix("* TODO ")).collect();
        assert_eq!(titles, ["Earlier item", "Item A", "Item B", "Moved item", "Item C", "Item D", "Item E", "Item F", "Item G"],);
    }

    /// ...and reproducing that order must not cost a single write: every
    /// task already has a position, it just is not always in a property.
    #[tokio::test]
    async fn adopting_the_servers_order_writes_nothing_back() {
        let fixture = Fixture::new();
        let server = FakeServer::with(&[
            ("https://cloud.example.com/dav/einkauf/a.ics", &vtodo("a", "Erste", "CREATED:20260823T155150Z\r\n"), "\"v1\""),
            ("https://cloud.example.com/dav/einkauf/b.ics", &vtodo("b", "Zweite", "CREATED:20260823T155155Z\r\n"), "\"v1\""),
        ]);

        fixture.sync(&server).await;

        assert!(server.puts.lock().unwrap().is_empty(), "{:?}", server.puts.lock().unwrap());
    }

    /// A progress ring is only worth having if it never goes backwards and
    /// always arrives -- a run that stops at 7/9 reads as a hang.
    #[tokio::test]
    async fn progress_only_moves_forward_and_ends_full() {
        #[derive(Default)]
        struct Recorder(Mutex<Vec<SyncProgress>>);
        impl ProgressSink for Recorder {
            fn report(&self, progress: SyncProgress) {
                self.0.lock().unwrap().push(progress);
            }
        }

        let fixture = Fixture::new();
        let server = FakeServer::with(&[
            ("https://cloud.example.com/dav/einkauf/a.ics", &vtodo("a", "Erste", "CREATED:20260823T155150Z\r\n"), "\"v1\""),
            ("https://cloud.example.com/dav/einkauf/b.ics", &vtodo("b", "Zweite", "CREATED:20260823T155155Z\r\n"), "\"v1\""),
        ]);
        let recorder = Recorder::default();

        sync_list(&server, &fixture.conn, &fixture.target(), &recorder).await.unwrap();

        let steps = recorder.0.lock().unwrap().clone();
        assert!(steps.len() > 2, "something was reported along the way: {steps:?}");
        for pair in steps.windows(2) {
            assert!(pair[1].done >= pair[0].done, "went backwards: {pair:?}");
        }
        let last = steps.last().unwrap();
        assert!(last.total > 0 && last.done == last.total, "did not end full: {last:?}");
        assert!(steps.iter().all(|s| s.done <= s.total), "overshot: {steps:?}");
    }

    /// Keeping the previous positions when a reorder did not go through
    /// must not put two tasks in the same place. A task the previous run
    /// never saw has no position to keep, and handing it its index in the
    /// new order collides with whoever already holds that number --
    /// leaving a recorded order with no defined sequence. Observed in a
    /// real database as two live rows both at position 0.
    #[test]
    fn a_task_with_no_previous_position_does_not_take_someone_elses() {
        let known = |uid: &str, position: i64| TaskState {
            list_id: "list-1".into(),
            uid: uid.into(),
            etag: "\"v1\"".into(),
            url: String::new(),
            local_hash: String::new(),
            sort_order: None,
            position,
            unmappable_categories: Vec::new(),
            deleted: false,
        };
        let state = vec![known("a", 0), known("b", 1)];
        let kept = Recorded::of(&state);

        // The newcomer sorted to the front of the new order, which is
        // exactly where "a" already sits.
        assert_eq!(kept.position_of("neu", 0), 2);
        assert_eq!(kept.position_of("a", 1), 0, "a keeps what it had");
        assert_eq!(kept.position_of("b", 2), 1);
    }

    #[test]
    fn several_newcomers_keep_the_order_the_file_gives_them() {
        let state: Vec<TaskState> = Vec::new();
        let kept = Recorded::of(&state);

        assert!(kept.position_of("erste", 0) < kept.position_of("zweite", 1));
    }

    /// A record that lost the order of most of its tasks must not answer
    /// by giving them new ones: that is a write per task for a list nobody
    /// touched. Their order is on the server, and reading it back is a
    /// handful of batched requests.
    #[tokio::test]
    async fn orders_missing_from_the_record_are_read_back_not_invented() {
        let fixture = Fixture::new();
        let objects: Vec<(String, String, String)> = (0..10)
            .map(|index| {
                (
                    format!("https://cloud.example.com/dav/einkauf/{index}.ics"),
                    vtodo(
                        &format!("u{index}"),
                        &format!("Eintrag {index}"),
                        &format!("X-APPLE-SORT-ORDER:{}\r\n", 810000000 + index * 1000),
                    ),
                    "\"v1\"".to_string(),
                )
            })
            .collect();
        let server = FakeServer::with(&objects.iter().map(|(u, b, e)| (u.as_str(), b.as_str(), e.as_str())).collect::<Vec<_>>());
        fixture.sync(&server).await;

        // The state an older build left behind: the tasks are known, their
        // order is not.
        fixture.conn.lock().unwrap().execute("UPDATE caldav_state SET sort_order = NULL", []).unwrap();
        server.puts.lock().unwrap().clear();

        // One entry moved, which is the only thing that should be written.
        let (header, mut blocks) = fixture.blocks();
        let moved = blocks.remove(9);
        blocks.insert(4, moved);
        fixture.write_blocks(&header, &blocks);

        let outcome = fixture.sync(&server).await;

        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        let puts = server.puts.lock().unwrap();
        assert_eq!(puts.len(), 1, "one write, not one per task: {:?}", puts.len());

        let content = fixture.content();
        let titles: Vec<&str> = content.lines().filter_map(|l| l.strip_prefix("* TODO ")).collect();
        assert_eq!(titles[4], "Eintrag 9");
    }

    /// The reported bug: "Hi, 1, 2, 3, 4, 5" typed in quick succession
    /// came back as "2, 4, 1, 5, Hi, 3". Asked one at a time, each new
    /// entry could only see the neighbours that existed before the run --
    /// so all six got the same value and the tie fell to their uuids.
    #[tokio::test]
    async fn entries_typed_in_one_burst_keep_the_order_they_were_typed_in() {
        let fixture = Fixture::new();
        let server = FakeServer::with(&[(
            "https://cloud.example.com/dav/einkauf/a.ics",
            &vtodo("a", "Vorher", "X-APPLE-SORT-ORDER:810000000\r\n"),
            "\"v1\"",
        )]);
        fixture.sync(&server).await;

        let typed = ["Hi", "1", "2", "3", "4", "5"];
        let mut content = fixture.content();
        for title in typed {
            content.push_str(&format!("* TODO {title}\n"));
        }
        std::fs::write(&fixture.file, &content).unwrap();

        fixture.sync(&server).await;
        // A second run, where a wrong order would surface: by then the
        // server has all six and drives the order.
        fixture.sync(&server).await;

        let content = fixture.content();
        let titles: Vec<&str> = content.lines().filter_map(|l| l.strip_prefix("* TODO ")).collect();
        assert_eq!(titles, ["Vorher", "Hi", "1", "2", "3", "4", "5"]);
    }

    /// ...and typed into a gap, they share it rather than piling onto one
    /// value.
    #[tokio::test]
    async fn entries_typed_between_two_others_share_the_room_between_them() {
        let fixture = Fixture::new();
        let server = FakeServer::with(&[
            ("https://cloud.example.com/dav/einkauf/a.ics", &vtodo("a", "Erste", "X-APPLE-SORT-ORDER:810000000\r\n"), "\"v1\""),
            ("https://cloud.example.com/dav/einkauf/b.ics", &vtodo("b", "Letzte", "X-APPLE-SORT-ORDER:810001000\r\n"), "\"v1\""),
        ]);
        fixture.sync(&server).await;

        let (header, blocks) = fixture.blocks();
        let inserted: Vec<String> = ["A", "B", "C"].iter().map(|t| format!("* TODO {t}\n")).collect();
        let mut all = vec![blocks[0].clone()];
        all.extend(inserted);
        all.push(blocks[1].clone());
        fixture.write_blocks(&header, &all);

        fixture.sync(&server).await;
        fixture.sync(&server).await;

        let content = fixture.content();
        let titles: Vec<&str> = content.lines().filter_map(|l| l.strip_prefix("* TODO ")).collect();
        assert_eq!(titles, ["Erste", "A", "B", "C", "Letzte"]);
    }

    /// A quiet task must keep the order it is known to have. A run that
    /// does not re-read it only ever sees the local copy, which carries
    /// neither the property nor CREATED -- and recording that as "no
    /// order" means the next reorder has to give every such task a value,
    /// which is one write each for a list that never changed.
    #[tokio::test]
    async fn a_task_nobody_touched_keeps_its_recorded_order() {
        let fixture = Fixture::new();
        let server = FakeServer::with(&[
            ("https://cloud.example.com/dav/einkauf/a.ics", &vtodo("a", "Erste", "X-APPLE-SORT-ORDER:810000000\r\n"), "\"v1\""),
            ("https://cloud.example.com/dav/einkauf/b.ics", &vtodo("b", "Zweite", "X-APPLE-SORT-ORDER:810001000\r\n"), "\"v1\""),
        ]);
        fixture.sync(&server).await;

        let recorded = |fixture: &Fixture| -> Vec<Option<i64>> {
            let conn = fixture.conn.lock().unwrap();
            let mut statement = conn.prepare("SELECT sort_order FROM caldav_state WHERE deleted = 0 ORDER BY position").unwrap();
            let values = statement.query_map([], |row| row.get(0)).unwrap();
            values.collect::<Result<_, _>>().unwrap()
        };
        let before = recorded(&fixture);
        assert_eq!(before, vec![Some(810000000), Some(810001000)]);

        // Something else changed, so the run goes the long way round --
        // but neither task is re-read.
        std::fs::write(&fixture.file, format!("{}* TODO Neu\n", fixture.content())).unwrap();
        fixture.sync(&server).await;

        assert_eq!(recorded(&fixture)[..2], before[..], "still known after a run that ignored them");
    }

    /// Moving one entry is one write. It used to be one per task in the
    /// list, because every reorder was sent with the condition "must not
    /// exist yet" and came back rejected -- so the values never settled
    /// and the next run had to renumber everything again.
    #[tokio::test]
    async fn moving_one_entry_writes_one_object() {
        let fixture = Fixture::new();
        let objects: Vec<(String, String, String)> = (0..12)
            .map(|index| {
                (
                    format!("https://cloud.example.com/dav/einkauf/{index}.ics"),
                    vtodo(
                        &format!("u{index}"),
                        &format!("Eintrag {index}"),
                        &format!("X-APPLE-SORT-ORDER:{}\r\n", 810000000 + index * 1000),
                    ),
                    "\"v1\"".to_string(),
                )
            })
            .collect();
        let server = FakeServer::with(&objects.iter().map(|(u, b, e)| (u.as_str(), b.as_str(), e.as_str())).collect::<Vec<_>>());
        fixture.sync(&server).await;

        // Last entry dragged to the middle.
        let (header, mut blocks) = fixture.blocks();
        let moved = blocks.remove(11);
        blocks.insert(5, moved);
        fixture.write_blocks(&header, &blocks);
        server.puts.lock().unwrap().clear();

        let outcome = fixture.sync(&server).await;

        let puts = server.puts.lock().unwrap();
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert_eq!(puts.len(), 1, "one write, not a renumbering: {:?}", puts.iter().map(|(u, _)| u).collect::<Vec<_>>());

        let content = fixture.content();
        let titles: Vec<&str> = content.lines().filter_map(|l| l.strip_prefix("* TODO ")).collect();
        assert_eq!(titles[5], "Eintrag 11", "and it stayed where it was dropped");
    }

    /// Dragging a task in the web UI has to arrive here. Nothing changed
    /// locally, so the server's order is the only one there is.
    #[tokio::test]
    async fn an_order_changed_in_another_client_is_adopted() {
        let fixture = Fixture::new();
        let server = FakeServer::with(&[
            ("https://cloud.example.com/dav/einkauf/a.ics", &vtodo("a", "Erste", "X-APPLE-SORT-ORDER:810000000\r\n"), "\"v1\""),
            ("https://cloud.example.com/dav/einkauf/b.ics", &vtodo("b", "Zweite", "X-APPLE-SORT-ORDER:810001000\r\n"), "\"v1\""),
            ("https://cloud.example.com/dav/einkauf/c.ics", &vtodo("c", "Dritte", "X-APPLE-SORT-ORDER:810002000\r\n"), "\"v1\""),
        ]);
        fixture.sync(&server).await;

        // "Dritte" dragged to the front over there.
        server.objects.lock().unwrap().insert(
            "https://cloud.example.com/dav/einkauf/c.ics".to_string(),
            (vtodo("c", "Dritte", "X-APPLE-SORT-ORDER:809999000\r\n"), "\"v2\"".to_string()),
        );
        *server.ctag.lock().unwrap() = Some("ctag-2".into());
        server.puts.lock().unwrap().clear();

        fixture.sync(&server).await;

        let content = fixture.content();
        let titles: Vec<&str> = content.lines().filter_map(|l| l.strip_prefix("* TODO ")).collect();
        assert_eq!(titles, ["Dritte", "Erste", "Zweite"]);
        assert!(server.puts.lock().unwrap().is_empty(), "and nothing was written back");
    }

    /// The reported bug: a task typed into the middle of a list came back
    /// at the very top after the sync. Its own CREATED was older than the
    /// list's sort values, and with no order of its own that is what every
    /// client sorts it by -- so it landed before everything.
    #[tokio::test]
    async fn a_new_entry_stays_where_it_was_typed_not_where_its_age_puts_it() {
        let fixture = Fixture::new();
        // Sort values far above what a CREATED of "now" would produce,
        // which is what a renumbered list looks like.
        let server = FakeServer::with(&[
            ("https://cloud.example.com/dav/einkauf/a.ics", &vtodo("a", "Erste", "X-APPLE-SORT-ORDER:810000000\r\n"), "\"v1\""),
            ("https://cloud.example.com/dav/einkauf/b.ics", &vtodo("b", "Zweite", "X-APPLE-SORT-ORDER:810001000\r\n"), "\"v1\""),
        ]);
        fixture.sync(&server).await;

        // Typed between the two, as the list view's "add" does.
        let (header, blocks) = fixture.blocks();
        fixture.write_blocks(&header, &[blocks[0].clone(), "* TODO Dazwischen\n".to_string(), blocks[1].clone()]);

        fixture.sync(&server).await;
        // A second run: the first created it, this one is where a wrong
        // position would surface.
        fixture.sync(&server).await;

        let content = fixture.content();
        let titles: Vec<&str> = content.lines().filter_map(|l| l.strip_prefix("* TODO ")).collect();
        assert_eq!(titles, ["Erste", "Dazwischen", "Zweite"]);

        // And the server was told where it goes, rather than being left to
        // guess from CREATED.
        let puts = server.puts.lock().unwrap();
        let created = puts.iter().find(|(_, body)| body.contains("SUMMARY:Dazwischen")).expect("it was created");
        // Halfway between its neighbours (810000000 and 810001000), so the
        // next move still has room either side of it.
        assert!(created.1.contains("X-APPLE-SORT-ORDER:810000500"), "{}", created.1);
    }

    /// A reorder whose write the server rejected must stay pending. Filing
    /// it as done leaves the file and the record agreeing, and the next run
    /// then finds nothing to do -- the drag never leaves this device.
    #[tokio::test]
    async fn a_rejected_reorder_is_retried_instead_of_being_forgotten() {
        let fixture = Fixture::new();
        let server = FakeServer::with(&[
            ("https://cloud.example.com/dav/einkauf/a.ics", &vtodo("a", "Erste", "CREATED:20260823T155150Z\r\n"), "\"v1\""),
            ("https://cloud.example.com/dav/einkauf/b.ics", &vtodo("b", "Zweite", "CREATED:20260823T155155Z\r\n"), "\"v1\""),
            ("https://cloud.example.com/dav/einkauf/c.ics", &vtodo("c", "Dritte", "CREATED:20260823T155201Z\r\n"), "\"v1\""),
        ]);
        fixture.sync(&server).await;

        let (header, blocks) = fixture.blocks();
        fixture.write_blocks(&header, &[blocks[2].clone(), blocks[0].clone(), blocks[1].clone()]);
        server.puts.lock().unwrap().clear();
        *server.reject_next_put.lock().unwrap() = true;

        let refused = fixture.sync(&server).await;
        assert!(!refused.errors.is_empty(), "the rejection was reported");
        server.puts.lock().unwrap().clear();

        // Nothing else changed -- the next run has only the pending
        // reorder to notice.
        let retried = fixture.sync(&server).await;

        assert!(!server.puts.lock().unwrap().is_empty(), "the reorder was tried again: {:?}", retried.errors);
    }

    /// The ring said "0 of 1" and then sat there while the reorder went
    /// out task by task. Whatever the pushes cost has to be part of the
    /// total, or the progress is a lie exactly when it matters.
    #[tokio::test]
    async fn reordering_is_part_of_the_progress_it_reports() {
        #[derive(Default)]
        struct Recorder(Mutex<Vec<SyncProgress>>);
        impl ProgressSink for Recorder {
            fn report(&self, progress: SyncProgress) {
                self.0.lock().unwrap().push(progress);
            }
        }

        let fixture = Fixture::new();
        let server = FakeServer::with(&[
            ("https://cloud.example.com/dav/einkauf/a.ics", &vtodo("a", "Erste", "CREATED:20260823T155150Z\r\n"), "\"v1\""),
            ("https://cloud.example.com/dav/einkauf/b.ics", &vtodo("b", "Zweite", "CREATED:20260823T155155Z\r\n"), "\"v1\""),
            ("https://cloud.example.com/dav/einkauf/c.ics", &vtodo("c", "Dritte", "CREATED:20260823T155201Z\r\n"), "\"v1\""),
        ]);
        fixture.sync(&server).await;

        // Drag the last entry to the front, as the list view would.
        let (header, blocks) = fixture.blocks();
        fixture.write_blocks(&header, &[blocks[2].clone(), blocks[0].clone(), blocks[1].clone()]);
        server.puts.lock().unwrap().clear();
        let recorder = Recorder::default();

        sync_list(&server, &fixture.conn, &fixture.target(), &recorder).await.unwrap();

        assert!(!server.puts.lock().unwrap().is_empty(), "the new order reached the server");
        let steps = recorder.0.lock().unwrap().clone();
        let last = steps.last().unwrap();
        assert!(last.total > 1, "the pushes are counted, not hidden behind a total of 1: {steps:?}");
        assert_eq!(last.done, last.total, "and it ends full: {last:?}");
    }

    /// A few hundred tasks read one request at a time is minutes of
    /// waiting. The protocol has calendar-multiget for exactly this, and
    /// the sync has to actually use it -- for the first sync and for the
    /// repair re-read alike.
    #[tokio::test]
    async fn tasks_are_read_in_one_request_rather_than_one_each() {
        let fixture = Fixture::new();
        let server = FakeServer::with(&[
            ("https://cloud.example.com/dav/einkauf/a.ics", &vtodo("a", "Erste", "CREATED:20260823T155150Z\r\n"), "\"v1\""),
            ("https://cloud.example.com/dav/einkauf/b.ics", &vtodo("b", "Zweite", "CREATED:20260823T155155Z\r\n"), "\"v1\""),
            ("https://cloud.example.com/dav/einkauf/c.ics", &vtodo("c", "Dritte", "CREATED:20260823T155201Z\r\n"), "\"v1\""),
        ]);

        fixture.sync(&server).await;

        assert_eq!(*server.gets.lock().unwrap(), 0, "nothing was read one at a time");
        assert_eq!(*server.multigets.lock().unwrap(), 1, "one batched read for all three");
    }

    /// The state a build without the CREATED fallback left behind: the
    /// file is in the order that build wrote, the server has not changed,
    /// and the recorded sort orders are the invented ones. Nothing about
    /// either side changed, so without noticing that the record itself is
    /// stale the list would keep its wrong order forever.
    #[tokio::test]
    async fn a_list_left_in_the_wrong_order_repairs_itself_on_the_next_sync() {
        let fixture = Fixture::new();
        let server = FakeServer::with(&[
            ("https://cloud.example.com/dav/einkauf/a.ics", &vtodo("a", "Erste", "CREATED:20260823T155150Z\r\n"), "\"v1\""),
            ("https://cloud.example.com/dav/einkauf/b.ics", &vtodo("b", "Zweite", "CREATED:20260823T155155Z\r\n"), "\"v1\""),
            ("https://cloud.example.com/dav/einkauf/c.ics", &vtodo("c", "Dritte", "CREATED:20260823T155201Z\r\n"), "\"v1\""),
        ]);
        fixture.sync(&server).await;

        // Rewind to what the old build recorded and wrote: no positions,
        // invented sort orders, and a file in that invented order.
        {
            let conn = fixture.conn.lock().unwrap();
            conn.execute("UPDATE caldav_state SET position = 0", []).unwrap();
            for (uid, value) in [("a", 3000), ("b", 2000), ("c", 1000)] {
                conn.execute("UPDATE caldav_state SET sort_order = ?1 WHERE uid = ?2", rusqlite::params![value, uid]).unwrap();
            }
        }
        let (header, blocks) = fixture.blocks();
        fixture.write_blocks(&header, &[blocks[2].clone(), blocks[1].clone(), blocks[0].clone()]);
        // Only the sequence of the blocks changed, so every per-task hash
        // still matches: nothing looks locally edited.
        fixture.conn.lock().unwrap().execute("UPDATE caldav_list_state SET ctag = 'ctag-1'", []).unwrap();
        *server.ctag.lock().unwrap() = Some("ctag-1".into());
        server.puts.lock().unwrap().clear();

        fixture.sync(&server).await;

        let content = fixture.content();
        let titles: Vec<&str> = content.lines().filter_map(|l| l.strip_prefix("* TODO ")).collect();
        assert_eq!(titles, ["Erste", "Zweite", "Dritte"], "read back from the server");
        assert!(server.puts.lock().unwrap().is_empty(), "{:?}", server.puts.lock().unwrap());
    }

    /// Sort orders that were recorded while the fallback to CREATED was
    /// missing are wrong for every task that was never dragged. They have
    /// to be corrected from the server rather than defended as the user's
    /// own arrangement -- the file was never touched, so there is nothing
    /// local to protect.
    #[tokio::test]
    async fn sort_orders_recorded_from_a_wrong_reading_heal_themselves() {
        let fixture = Fixture::new();
        let server = FakeServer::with(&[
            ("https://cloud.example.com/dav/einkauf/a.ics", &vtodo("a", "Erste", "CREATED:20260823T155150Z\r\n"), "\"v1\""),
            ("https://cloud.example.com/dav/einkauf/b.ics", &vtodo("b", "Zweite", "CREATED:20260823T155155Z\r\n"), "\"v1\""),
            ("https://cloud.example.com/dav/einkauf/c.ics", &vtodo("c", "Dritte", "CREATED:20260823T155201Z\r\n"), "\"v1\""),
        ]);
        fixture.sync(&server).await;

        // Invented values, in the reverse of the true order.
        {
            let conn = fixture.conn.lock().unwrap();
            for (uid, value) in [("a", 3000), ("b", 2000), ("c", 1000)] {
                conn.execute(
                    "UPDATE caldav_state SET sort_order = ?1 WHERE list_id = 'list-1' AND uid = ?2",
                    rusqlite::params![value, uid],
                )
                .unwrap();
            }
        }
        server.puts.lock().unwrap().clear();

        fixture.sync(&server).await;

        let content = fixture.content();
        let titles: Vec<&str> = content.lines().filter_map(|l| l.strip_prefix("* TODO ")).collect();
        assert_eq!(titles, ["Erste", "Zweite", "Dritte"]);
        assert!(server.puts.lock().unwrap().is_empty(), "{:?}", server.puts.lock().unwrap());
    }

    /// A database migrated from a build that never recorded positions:
    /// every row carries the same default. That is an absence of
    /// information, not an order the user chose -- reading it as one would
    /// push a list sorted by uid over the server's own arrangement.
    #[tokio::test]
    async fn a_migrated_database_does_not_invent_a_local_order() {
        let fixture = Fixture::new();
        let server = FakeServer::with(&[
            ("https://cloud.example.com/dav/einkauf/zzz.ics", &vtodo("zzz", "Erste", "CREATED:20260823T155150Z\r\n"), "\"v1\""),
            ("https://cloud.example.com/dav/einkauf/aaa.ics", &vtodo("aaa", "Zweite", "CREATED:20260823T155155Z\r\n"), "\"v1\""),
        ]);
        fixture.sync(&server).await;

        // What db::ensure_columns leaves behind: the column exists, and
        // every pre-existing row has its default.
        fixture.conn.lock().unwrap().execute("UPDATE caldav_state SET position = 0", []).unwrap();
        server.puts.lock().unwrap().clear();
        *server.ctag.lock().unwrap() = Some("ctag-2".into());

        fixture.sync(&server).await;

        let content = fixture.content();
        let titles: Vec<&str> = content.lines().filter_map(|l| l.strip_prefix("* TODO ")).collect();
        assert_eq!(titles, ["Erste", "Zweite"], "the server's order, not one sorted by uid");
        assert!(server.puts.lock().unwrap().is_empty(), "{:?}", server.puts.lock().unwrap());
    }

    /// Reordering the file has to reach the server, or the order is
    /// local-only and every other client disagrees with it.
    #[tokio::test]
    async fn reordering_the_file_pushes_new_sort_values() {
        let fixture = Fixture::new();
        let server = FakeServer::with(&[
            ("https://cloud.example.com/dav/einkauf/a.ics", &vtodo("a", "Erste", "X-APPLE-SORT-ORDER:1000\r\n"), "\"v1\""),
            ("https://cloud.example.com/dav/einkauf/b.ics", &vtodo("b", "Zweite", "X-APPLE-SORT-ORDER:2000\r\n"), "\"v1\""),
        ]);
        fixture.sync(&server).await;
        server.puts.lock().unwrap().clear();

        let (header, mut blocks) = fixture.blocks();
        assert_eq!(blocks.len(), 2);
        blocks.swap(0, 1);
        fixture.write_blocks(&header, &blocks);

        fixture.sync(&server).await;

        let bodies: Vec<String> = server.puts.lock().unwrap().iter().map(|(_, b)| b.clone()).collect();
        assert!(!bodies.is_empty(), "the new order was never sent");
        assert!(bodies.iter().any(|b| b.contains("X-APPLE-SORT-ORDER")), "{bodies:?}");
        let content = fixture.content();
        assert!(content.find("Zweite").unwrap() < content.find("Erste").unwrap(), "{content}");
    }

    /// A run that finds nothing changed must not touch the file: the
    /// vault sync watches it, and a rewrite it has to carry on every
    /// CalDAV poll is traffic and a conflict window for no content.
    #[tokio::test]
    async fn a_run_that_changes_nothing_leaves_the_file_alone() {
        let fixture = Fixture::new();
        let server = FakeServer::with(&[("https://cloud.example.com/dav/einkauf/a.ics", &vtodo("a", "Milch", ""), "\"v1\"")]);
        fixture.sync(&server).await;
        let before = std::fs::metadata(&fixture.file).unwrap().modified().unwrap();

        // A second run with the same server state, past the ctag fast path.
        *server.ctag.lock().unwrap() = Some("ctag-2".into());
        fixture.sync(&server).await;

        assert_eq!(std::fs::metadata(&fixture.file).unwrap().modified().unwrap(), before);
    }

    /// A first sync downloads the whole collection before it can plan
    /// anything -- the slowest part of the run. The ring used to be told
    /// the size of the job only afterwards, so it sat at 0 of 0 through
    /// all of it and then jumped.
    #[tokio::test]
    async fn the_first_sync_reports_its_size_before_downloading() {
        struct FirstReport(std::sync::Mutex<Option<SyncProgress>>);
        impl ProgressSink for FirstReport {
            fn report(&self, progress: SyncProgress) {
                let mut seen = self.0.lock().unwrap();
                // The first report with anything to count.
                if seen.is_none() && progress.total > 0 {
                    *seen = Some(progress);
                }
            }
        }

        let fixture = Fixture::new();
        let server = FakeServer::with(&[
            ("https://cloud.example.com/dav/einkauf/a.ics", &vtodo("a", "Milch", ""), "\"v1\""),
            ("https://cloud.example.com/dav/einkauf/b.ics", &vtodo("b", "Brot", ""), "\"v1\""),
            ("https://cloud.example.com/dav/einkauf/c.ics", &vtodo("c", "Käse", ""), "\"v1\""),
        ]);
        let sink = FirstReport(std::sync::Mutex::new(None));

        sync_list(&server, &fixture.conn, &fixture.target(), &sink).await.unwrap();

        let first = sink.0.lock().unwrap().expect("nothing was ever counted");
        assert_eq!(first.total, 3, "the three unknown objects have to be counted before they are read");
        assert_eq!(first.done, 0);
    }

    /// The notification rule, on the task side: a task somebody else added
    /// counts, one this device merely re-read does not, and a first sync
    /// counts nothing at all.
    #[tokio::test]
    async fn only_tasks_this_device_never_saw_count_as_arrived() {
        let fixture = Fixture::new();
        let first =
            fixture.sync(&FakeServer::with(&[("https://cloud.example.com/dav/einkauf/a.ics", &vtodo("a", "Milch", ""), "\"v1\"")])).await;
        assert_eq!(first.arrived, 0, "everything is new on a first sync");

        // Somebody adds one on the server, and the one we already have
        // changes there too.
        let server = FakeServer::with(&[
            ("https://cloud.example.com/dav/einkauf/a.ics", &vtodo("a", "Milch, 2 Liter", ""), "\"v2\""),
            ("https://cloud.example.com/dav/einkauf/b.ics", &vtodo("b", "Brot", ""), "\"v1\""),
        ]);
        // ... which the collection's ctag reflects, or the run would take
        // the fast path and never look.
        *server.ctag.lock().unwrap() = Some("ctag-2".into());

        let second = fixture.sync(&server).await;

        assert_eq!(second.pulled, 2, "both were read");
        assert_eq!(second.arrived, 1, "only the one this device had never seen");
    }
}
