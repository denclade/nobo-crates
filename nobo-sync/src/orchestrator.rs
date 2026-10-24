// SPDX-FileCopyrightText: 2026 Dennis Burgermeister
// SPDX-License-Identifier: GPL-3.0-or-later

//! Ties crypto (Module 1), local state (Module 2), and a provider
//! (Module 3) together into one actual sync run. The one thing this file
//! deliberately does NOT do: auto-resolve a `Conflict` -- those are
//! collected and handed back untouched, left for Module 6's UI and a real
//! user decision on the next sync.

use serde::Serialize;
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use super::crypto::{self, MasterKey};
use super::diff::{compute_diff, SyncAction};
use super::index::{IndexEntry, SyncIndex};
use super::model::{ConflictInfo, SyncSummary};
use super::providers::{IndexVersion, SyncProvider};
use super::{hashing, rescan, store};
use nobo_core::core::merge::{MergeHandler, MergeInput};

/// How many times to silently retry one file transfer before giving up and
/// surfacing the error to the user (1 initial attempt + this many retries).
const MAX_TRANSIENT_RETRIES: u32 = 3;

/// Whether `msg` is a transient network hiccup worth retrying, as opposed
/// to a persistent failure -- wrong credentials, undecryptable data, a
/// forbidden resource -- where an identical retry just fails again.
///
/// String-matched, since every provider method collapses its error to a
/// `String` at the trait boundary. Narrow substrings over a broad
/// default: a wrong "recoverable" verdict costs a few seconds, a wrong
/// "permanent" one bothers the user for something a retry would have
/// fixed.
fn is_recoverable_sync_error(msg: &str) -> bool {
    const RECOVERABLE_MARKERS: &[&str] = &[
        "failed to fill whole buffer", // truncated HTTP/TLS body stream mid-transfer
        "unexpected eof",
        "unexpected end of file",
        "connection reset",
        "connection closed",
        "broken pipe",
        "timed out",
        // `nobo_http::describe_request_error`'s own prefixes, in both
        // languages it can produce them in -- missing one turns a
        // transient network blip into a failed sync that never retries.
        "zeitüberschreitung",
        "timed out -- the server",
        "server nicht erreichbar",
        "server unreachable",
    ];
    let lower = msg.to_lowercase();
    RECOVERABLE_MARKERS.iter().any(|marker| lower.contains(marker))
}

/// Whether `msg` means a Download's blob is genuinely gone remotely, as
/// opposed to a network or permission failure. Not "recoverable": an
/// identical lookup would 404 forever, so the right response is for this
/// device to drop the stale reference and continue.
///
/// This arises because the index is re-uploaded once, after every action:
/// a device killed between deleting a file and that upload leaves the
/// remote index listing a deleted blob. Without this, that one stale entry
/// hard-aborts every other device's entire sync.
fn is_missing_remotely_error(msg: &str) -> bool {
    msg.contains("not found remotely")
}

#[derive(Debug, Clone, Copy, Serialize)]
#[cfg_attr(feature = "tauri", derive(specta::Type))]
#[serde(rename_all = "snake_case")]
pub enum SyncPhase {
    Uploading,
    Downloading,
    DeletingRemote,
    DeletingLocal,
    Settling,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "tauri", derive(specta::Type))]
pub struct SyncProgress {
    // u32, not usize -- specta forbids exporting 64-bit-precision integer
    // types to TypeScript (JS numbers can't represent them exactly); a
    // sync run's file count never gets remotely close to u32::MAX anyway.
    pub current: u32,
    pub total: u32,
    pub path: String,
    pub phase: SyncPhase,
}

/// Held for the duration of one sync run so a second call (this device
/// polling too eagerly, or a manual "sync now" while one's already in
/// flight) fails fast instead of racing the first one's own reads/writes
/// to `sync_state`/the vault.
pub struct SyncLockGuard(Arc<AtomicBool>);

impl Drop for SyncLockGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

pub fn acquire_lock(flag: &Arc<AtomicBool>) -> Result<SyncLockGuard, String> {
    if flag.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_err() {
        return Err("a sync is already running".to_string());
    }
    Ok(SyncLockGuard(flag.clone()))
}

fn now() -> String {
    chrono::Local::now().to_rfc3339()
}

/// Resolves a `ConflictInfo` by rewriting local `sync_state` so the NEXT
/// `run_sync` call produces a plain Upload/Download/DeleteRemote for this
/// path instead of another Conflict -- this function itself does no
/// network I/O or file transfer, the actual resolution happens on the
/// following sync (kept this way so a resolution decision doesn't have to
/// duplicate run_sync's own upload/download logic here too).
pub fn resolve_conflict(conn: &rusqlite::Connection, conflict: &ConflictInfo, keep_local: bool) -> Result<(), String> {
    match (&conflict.local_hash, &conflict.remote_hash, keep_local) {
        // Deletion-vs-edit, deletion wins -- next diff: local_deleted=true,
        // remote unchanged relative to this new base -> DeleteRemote.
        (None, Some(remote_hash), true) => store::set_base_hash(conn, &conflict.path, remote_hash).map_err(|e| e.to_string()),
        // Deletion-vs-edit, the edit wins -- drop local tracking entirely
        // so the next diff treats it as "never seen locally, exists
        // remotely" -> Download (there's no locally-deleted state that
        // ALSO has real content to converge on, so this is simpler and
        // just as correct as trying to un-delete the tombstone in place).
        (None, Some(_), false) => store::remove(conn, &conflict.path).map_err(|e| e.to_string()),
        // Content-vs-content, local wins -- next diff: local looks
        // changed (vs. new base = remote_hash), remote doesn't -> Upload.
        (Some(_), Some(remote_hash), true) => store::set_base_hash(conn, &conflict.path, remote_hash).map_err(|e| e.to_string()),
        // Content-vs-content, remote wins -- next diff: local looks
        // unchanged (vs. new base = its own current hash), remote doesn't -> Download.
        (Some(local_hash), Some(_), false) => store::set_base_hash(conn, &conflict.path, local_hash).map_err(|e| e.to_string()),
        // Edit-vs-deletion (remote side deleted it), the local edit wins --
        // reset to "never synced" so the next diff sees a plain new local
        // file -> Upload, re-establishing it remotely.
        (Some(_), None, true) => store::clear_base_hash(conn, &conflict.path).map_err(|e| e.to_string()),
        // Edit-vs-deletion, the remote deletion wins -- next diff: local
        // looks unchanged (vs. new base = its own current hash), remote
        // still has nothing -> DeleteLocal.
        (Some(local_hash), None, false) => store::set_base_hash(conn, &conflict.path, local_hash).map_err(|e| e.to_string()),
        (None, None, _) => Err(format!("{} is not actually a conflict (both sides deleted it)", conflict.path)),
    }
}

async fn fetch_blob_by_hash(
    provider: &dyn SyncProvider,
    master_key: Option<&MasterKey>,
    work_dir: &Path,
    content_hash: &str,
) -> Result<String, String> {
    tokio::fs::create_dir_all(work_dir).await.map_err(|e| e.to_string())?;
    let temp = tempfile::NamedTempFile::new_in(work_dir).map_err(|e| e.to_string())?;
    provider.download_file(content_hash, temp.path()).await?;

    let bytes = if let Some(mk) = master_key {
        let file_key = crypto::derive_file_key(mk, content_hash);
        let temp_path = temp.path().to_path_buf();
        tokio::task::spawn_blocking(move || -> Result<Vec<u8>, String> {
            let mut reader = std::fs::File::open(&temp_path).map_err(|e| e.to_string())?;
            let mut plaintext = Vec::new();
            crypto::decrypt_stream(&file_key, &mut reader, &mut plaintext)?;
            Ok(plaintext)
        })
        .await
        .map_err(|e| format!("decryption task panicked: {e}"))??
    } else {
        tokio::fs::read(temp.path()).await.map_err(|e| e.to_string())?
    };

    // Strict, not lossy: a lossy conversion replaces invalid bytes with
    // U+FFFD, mangling any binary attachment beyond recovery -- and the
    // line-based merge below often finds no conflict in mangled binary
    // data and returns Ok, which the caller then writes to disk and
    // re-uploads. Failing here falls back to manual resolution for exactly
    // the files automatic merging was never sound for.
    String::from_utf8(bytes)
        .map_err(|e| format!("blob {content_hash} is not valid UTF-8 text (likely a binary file) -- cannot auto-merge: {e}"))
}

/// orchestrates the 3 way merge
async fn auto_merge_conflict(
    provider: &dyn SyncProvider,
    db: &StdMutex<rusqlite::Connection>,
    vault_root: &Path,
    master_key: Option<&MasterKey>,
    work_dir: &Path,
    path: &str,
    remote_hash: &str,
) -> Result<Option<String>, String> {
    // Find hash of last common base
    let base_hash = {
        let conn = store::lock(db)?;
        store::get(&conn, path).ok().flatten().and_then(|r| r.base_hash)
    };

    let Some(base_h) = base_hash else {
        return Ok(None); // Without a base we can't do a 3way merge
    };

    // Load all three versions -- base and remote are independent blob
    // fetches, fetched concurrently rather than one after the other.
    let (base_text, remote_text) = tokio::try_join!(
        fetch_blob_by_hash(provider, master_key, work_dir, &base_h),
        fetch_blob_by_hash(provider, master_key, work_dir, remote_hash),
    )?;
    // A failed read here must NOT silently become an empty string --
    // merge_org_ast would then see zero local headings and conclude local
    // deleted everything, producing a "merge" that's actually just
    // remote's content, which then gets written straight over the real
    // (successfully-existing, just-not-read) local file. Propagating the
    // error instead makes this fall back to manual conflict resolution,
    // same as any other unmergeable case.
    let local_text =
        tokio::fs::read_to_string(vault_root.join(path)).await.map_err(|e| format!("failed to read local {path} for merge: {e}"))?;

    // The one place this feature merges anything. Which handler that is
    // belongs to the sync, not to the merge (see core::merge): another
    // sync feature would name a different one here and nothing else in
    // the file would change.
    let input = MergeInput { path, base: &base_text, local: &local_text, remote: &remote_text };
    nobo_core::core::merge::DEFAULT.merge(&input).map(Some)
}

/// Whether PATH on disk still holds exactly what this run's own rescan
/// recorded, i.e. the content the diff was computed from.
///
/// `rescan_local_state` runs once at the start of a sync, and everything
/// after it is decided against that snapshot -- but the user keeps editing
/// while the run is in flight. A `Download` or `DeleteLocal` decided before
/// an edit existed would destroy it without ever noticing: the next rescan
/// then sees local matching base again, so nothing is left to detect.
///
/// `expected` is the row's recorded `local_hash`, or `None` for a path that
/// was not tracked at all -- where a file appearing since means the same
/// thing.
fn local_matches_diff_snapshot(vault_root: &Path, path: &str, expected: Option<&str>) -> bool {
    match hashing::hash_file(&vault_root.join(path)) {
        Ok(current) => expected == Some(current.as_str()),
        // Unreadable or absent -- consistent only with having been absent
        // when the diff was computed.
        Err(_) => expected.is_none(),
    }
}

pub async fn run_sync(
    provider: &dyn SyncProvider,
    db: &StdMutex<rusqlite::Connection>,
    vault_root: &Path,
    // `None` only ever for WebDAV's explicit unencrypted opt-out (see
    // index.rs's module doc comment) -- PocketBase logins always derive a
    // real master key.
    master_key: Option<&MasterKey>,
    work_dir: &Path,
    mut on_progress: impl FnMut(SyncProgress),
) -> Result<SyncSummary, String> {
    tokio::fs::create_dir_all(work_dir).await.map_err(|e| format!("failed to create sync work dir: {e}"))?;

    // rescan_local_state hashes and writes a row per vault file
    // synchronously, which blocks this worker thread for a large vault.
    // block_in_place rather than spawn_blocking, which would require the
    // arguments to be 'static -- a signature change rippling through every
    // call site.
    tokio::task::block_in_place(|| {
        let conn = store::lock(db)?;
        rescan::rescan_local_state(&conn, vault_root)
    })?;

    // INDEX_VERSION is what THIS run's own download_index saw -- captured
    // here and passed back to upload_index at the very end, unchanged, so
    // that final write is checked against what this run actually started
    // from rather than a value re-derived right before writing (see
    // providers::IndexVersion's own doc comment on why that distinction is
    // the whole point: it's what catches two sync runs merely overlapping,
    // not just two uploads landing within milliseconds of each other).
    let (remote_bytes, index_version) = match provider.download_index().await? {
        Some((bytes, version)) => (Some(bytes), version),
        None => (None, IndexVersion::Absent),
    };
    let local_rows = {
        let conn = store::lock(db)?;
        store::get_all(&conn).map_err(|e| e.to_string())?
    };

    let remote_index = match remote_bytes {
        Some(bytes) => match master_key {
            // Far more often a password mismatch -- this remote already
            // holds data encrypted under a different one -- than real
            // corruption, so it is worth naming directly rather than
            // reading as a data-integrity problem.
            Some(mk) => super::index::decrypt(mk, &bytes).map_err(|e| {
                nobo_core::t!(
                    "Entschlüsselung des Sync-Index fehlgeschlagen -- das Passwort passt nicht zu den bereits auf dem Server gespeicherten Daten (z.B. wenn dieser Account vorher mit einem anderen Passwort synchronisiert wurde). Entweder das ursprüngliche Passwort verwenden, oder den vorhandenen sync_index-Eintrag serverseitig löschen und neu synchronisieren. ({e})",
                    "Could not decrypt the sync index -- the password does not match the data already on the server (for example if this account was synced with a different one before). Either use the original password, or delete the existing sync_index record on the server and sync again. ({e})"
                )
            })?,
            None => serde_json::from_slice(&bytes).map_err(|e| format!("failed to parse remote index: {e}"))?,
        },
        // No index record on the remote at all.
        //
        // For a first run that is correct and the empty index is exactly
        // right. But it is also what a deleted record, a restored backup,
        // a 404 on the WebDAV root and an expired PocketBase token look
        // like from here (see providers::pocketbase::find_index_record) --
        // and diffing a synced vault against an empty index turns every
        // previously synced, locally unchanged file into a DeleteLocal.
        // Which then publishes the emptied index, so every other device
        // does the same on its next tick. That is the one loss in this
        // codebase with the whole fleet as its blast radius.
        //
        // A non-null `base_hash` is proof that this vault has synced
        // successfully against THIS remote identity before -- and only
        // this identity, because `store::ensure_remote_identity` wipes
        // `sync_state` whenever it changes. So its presence means the
        // index is missing, not absent.
        None if local_rows.iter().any(|row| row.base_hash.is_some()) => {
            // Prefixed with a stable marker, like `sync-index-conflict:`
            // and `sync-auth-expired:` already are. The settings page has
            // to recognise this case to offer "republish", and matching on
            // the prose would have stopped working the moment the prose
            // gained a second language.
            return Err(nobo_core::t!(
                "sync-index-missing: Auf dem Server ist kein Sync-Index vorhanden, dieses Gerät hat aber schon erfolgreich mit ihm synchronisiert. \
                 Das deutet auf einen serverseitig gelöschten Eintrag, ein zurückgespieltes Backup oder eine abgelaufene Anmeldung hin -- \
                 die Synchronisierung wurde abgebrochen, damit keine Dateien gelöscht werden. \
                 Bitte zuerst die Anmeldung prüfen. Ist der Index wirklich weg, kann dieser Tresor unter \
                 Einstellungen -> Synchronisierung neu veröffentlicht werden.",
                "sync-index-missing: There is no sync index on the server, but this device has already synced with it successfully. \
                 That points at a record deleted server-side, a restored backup, or an expired session -- \
                 syncing was stopped so that no files are deleted. \
                 Please check the sign-in first. If the index really is gone, this vault can be republished \
                 under Settings -> Sync."
            ));
        }
        None => SyncIndex::default(),
    };

    // What the diff below is computed from, kept so a destructive action can
    // re-check it right before it acts -- see local_matches_diff_snapshot.
    let diff_snapshot: HashMap<String, Option<String>> = local_rows.iter().map(|r| (r.path.clone(), r.local_hash.clone())).collect();

    let actions = compute_diff(&local_rows, &remote_index.to_remote_entries());
    let mut working_index = remote_index;
    let mut summary = SyncSummary::default();
    // A property of the server, visible from the version it just handed
    // back -- see the field's doc comment on why it is reported and not
    // only logged by the provider.
    summary.index_unprotected = index_version == IndexVersion::Unversioned;
    let total = actions.len() as u32;
    // Local sync_state changes for Upload/Download/DeleteRemote/DeleteLocal
    // are collected here instead of committed immediately -- see this
    // function's own doc comment further down (right before they're
    // finally applied) for why: now that upload_index can genuinely fail
    // (a real cross-device race, not just a hypothetical -- see
    // providers::IndexVersion), committing them mid-loop would let a later
    // failed upload_index leave sync_state claiming something is synced
    // that the remote index was never actually told about.
    let mut pending_state_updates: Vec<PendingSyncState> = Vec::new();
    // (path, content_hash) of blobs a DeleteRemote wants gone, applied
    // only after `upload_index` -- see where this is drained.
    let mut pending_blob_deletes: Vec<(String, String)> = Vec::new();

    for (i, action) in actions.into_iter().enumerate() {
        match action {
            SyncAction::Upload { path } => {
                on_progress(SyncProgress { current: i as u32, total, path: path.clone(), phase: SyncPhase::Uploading });
                let mut attempt = 1;
                let entry = loop {
                    match upload_one(provider, vault_root, master_key, work_dir, &path).await {
                        Ok(entry) => break entry,
                        Err(e) if attempt < MAX_TRANSIENT_RETRIES && is_recoverable_sync_error(&e) => {
                            log::warn!("sync: upload of {path} failed (attempt {attempt}/{MAX_TRANSIENT_RETRIES}), retrying: {e}");
                            tokio::time::sleep(std::time::Duration::from_millis(400 * attempt as u64)).await;
                            attempt += 1;
                        }
                        Err(e) => return Err(e),
                    }
                };
                pending_state_updates.push(PendingSyncState::MarkSynced { path: path.clone(), hash: entry.content_hash.clone() });
                working_index.upsert(entry);
                summary.uploaded += 1;
            }
            SyncAction::Download { path } => {
                on_progress(SyncProgress { current: i as u32, total, path: path.clone(), phase: SyncPhase::Downloading });
                // Overwrites the local file wholesale, so it must not act on
                // a snapshot the user has edited past since this run started.
                if !local_matches_diff_snapshot(vault_root, &path, diff_snapshot.get(&path).and_then(|h| h.as_deref())) {
                    log::warn!(
                        "sync: {path} changed locally while this run was in flight -- skipping the download, the next run re-diffs it"
                    );
                    summary.skipped_locally_changed += 1;
                    continue;
                }
                let Some(entry) = working_index.find(&path).cloned() else {
                    return Err(format!("index has no entry for {path}, but a download was expected"));
                };
                let mut attempt = 1;
                let mut missing_remotely = false;
                loop {
                    match download_one(provider, vault_root, master_key, work_dir, &entry).await {
                        Ok(()) => break,
                        Err(e) if is_missing_remotely_error(&e) => {
                            log::warn!(
                                "sync: {path} (content_hash {}) is gone remotely, dropping the stale index entry: {e}",
                                entry.content_hash
                            );
                            missing_remotely = true;
                            break;
                        }
                        Err(e) if attempt < MAX_TRANSIENT_RETRIES && is_recoverable_sync_error(&e) => {
                            log::warn!("sync: download of {path} failed (attempt {attempt}/{MAX_TRANSIENT_RETRIES}), retrying: {e}");
                            tokio::time::sleep(std::time::Duration::from_millis(400 * attempt as u64)).await;
                            attempt += 1;
                        }
                        Err(e) => return Err(e),
                    }
                }
                if missing_remotely {
                    // Marked, not dropped.
                    //
                    // Dropping the entry looks like a local decision and
                    // is not: this index gets published, and a device that
                    // still HAS the file reads the vanished entry as
                    // "deleted elsewhere" and unlinks its copy -- which by
                    // then is the only one. That is how a plain rename
                    // used to end in an empty vault.
                    //
                    // Marking keeps the path, tells a device that holds
                    // the content to upload it again (see diff::diff_one)
                    // and leaves this device's own sync_state untouched,
                    // so the entry is simply skipped here until somebody
                    // heals it. A cheap 404 per sync until then is a much
                    // better trade than a destructive prune.
                    working_index.mark_blob_missing(&path);
                    summary.skipped_missing += 1;
                } else {
                    pending_state_updates.push(PendingSyncState::MarkSynced { path: path.clone(), hash: entry.content_hash.clone() });
                    summary.downloaded += 1;
                }
            }
            SyncAction::DeleteRemote { path } => {
                on_progress(SyncProgress { current: i as u32, total, path: path.clone(), phase: SyncPhase::DeletingRemote });
                if let Some(entry) = working_index.find(&path).cloned() {
                    working_index.remove(&path);
                    // Deferred until after the index that stops referencing
                    // this blob has actually been published -- see where
                    // `pending_blob_deletes` is drained.
                    pending_blob_deletes.push((path.clone(), entry.content_hash));
                }
                pending_state_updates.push(PendingSyncState::Remove { path: path.clone() });
                summary.deleted_remote += 1;
            }
            SyncAction::DeleteLocal { path } => {
                on_progress(SyncProgress { current: i as u32, total, path: path.clone(), phase: SyncPhase::DeletingLocal });
                // Same in-flight-edit guard as Download above -- a file
                // recreated or edited since the rescan is not the file the
                // remote deletion was decided against.
                if !local_matches_diff_snapshot(vault_root, &path, diff_snapshot.get(&path).and_then(|h| h.as_deref())) {
                    log::warn!(
                        "sync: {path} changed locally while this run was in flight -- skipping the local delete, the next run re-diffs it"
                    );
                    summary.skipped_locally_changed += 1;
                    continue;
                }
                match tokio::fs::remove_file(vault_root.join(&path)).await {
                    Ok(()) => {
                        pending_state_updates.push(PendingSyncState::Remove { path: path.clone() });
                        summary.deleted_local += 1;
                    }
                    Err(e) => {
                        // Neither counted nor forgotten. The snapshot check
                        // just above proved the file was there, so this is
                        // a permission problem, a read-only mount or a
                        // Windows lock -- and even a concurrent delete is
                        // not this run's achievement. Discarding the error
                        // and dropping the row anyway is what turned a
                        // remote deletion back into an upload.
                        log::warn!("sync: could not delete {path} locally -- keeping its sync_state row: {e}");
                        summary.failed_local_deletes += 1;
                    }
                }
            }
            SyncAction::ResolveSame { path } => {
                // Doesn't depend on THIS run's own upload_index succeeding
                // -- the remote already independently has this exact
                // content, so there's nothing new being published here to
                // wait on. Safe to commit immediately, unlike the four
                // deferred cases above.
                on_progress(SyncProgress { current: i as u32, total, path: path.clone(), phase: SyncPhase::Settling });
                if let Some(entry) = working_index.find(&path) {
                    let hash = entry.content_hash.clone();
                    let conn = store::lock(db)?;
                    store::mark_synced(&conn, &path, &hash).map_err(|e| e.to_string())?;
                }
            }
            SyncAction::Forget { path } => {
                // Purely local bookkeeping cleanup for a path neither side
                // cares about anymore -- no remote effect to wait on either.
                let conn = store::lock(db)?;
                store::remove(&conn, &path).map_err(|e| e.to_string())?;
            }
            SyncAction::Conflict { path, local_hash, remote_hash } => {
                let mut auto_merged = false;

                // Only works if both sides have the file, if a side deleted - let the user decide
                if let (Some(_), Some(r_hash)) = (&local_hash, &remote_hash) {
                    on_progress(SyncProgress { current: i as u32, total, path: path.clone(), phase: SyncPhase::Settling });

                    match auto_merge_conflict(provider, db, vault_root, master_key, work_dir, &path, r_hash).await {
                        Ok(Some(merged_text)) => {
                            let abs_path = vault_root.join(&path);
                            // Atomic, like every other write into the vault:
                            // this replaces a file the user may have open,
                            // and a truncate-then-fill that loses the race
                            // with a backgrounded app leaves neither side of
                            // the merge behind. Unguarded on purpose -- a
                            // generated mirror belonging to another device is
                            // a legitimate thing to receive.
                            let written = {
                                let abs_path = abs_path.clone();
                                tokio::task::spawn_blocking(move || nobo_core::utils::fs_utils::write_atomic(&abs_path, &merged_text))
                                    .await
                                    .map_err(|e| format!("write task panicked: {e}"))
                                    .and_then(|r| r.map_err(|e| e.to_string()))
                            };
                            if let Err(e) = written {
                                log::error!("sync: failed to write auto-merged file {}: {}", path, e);
                            } else {
                                // Upload th merged file
                                let mut attempt = 1;
                                let entry_result = loop {
                                    match upload_one(provider, vault_root, master_key, work_dir, &path).await {
                                        Ok(entry) => break Ok(entry),
                                        Err(e) if attempt < MAX_TRANSIENT_RETRIES && is_recoverable_sync_error(&e) => {
                                            log::warn!("sync: upload of auto-merged {path} failed (attempt {attempt}/{MAX_TRANSIENT_RETRIES}), retrying: {e}");
                                            tokio::time::sleep(std::time::Duration::from_millis(400 * attempt as u64)).await;
                                            attempt += 1;
                                        }
                                        Err(e) => break Err(e),
                                    }
                                };

                                match entry_result {
                                    Ok(entry) => {
                                        pending_state_updates
                                            .push(PendingSyncState::MarkSynced { path: path.clone(), hash: entry.content_hash.clone() });
                                        working_index.upsert(entry);
                                        summary.uploaded += 1;
                                        auto_merged = true;
                                        log::info!("sync: Auto-merged conflict for {}", path);
                                    }
                                    Err(e) => {
                                        return Err(format!("failed to upload auto-merged file {}: {}", path, e));
                                    }
                                }
                            }
                        }
                        Ok(None) => {
                            // No base hash, merge not possible
                        }
                        Err(e) => {
                            log::warn!("sync: auto-merge failed for {}, falling back to manual conflict: {}", path, e);
                        }
                    }
                }

                // Something failed / we have a deleted file or something, let the user decide
                if !auto_merged {
                    summary.conflicts.push(ConflictInfo { path, local_hash, remote_hash });
                }
            }
        }
    }

    // Only re-upload the index when something actually moved -- an
    // all-conflicts or fully-quiet run would otherwise still pay for a
    // pointless round trip. Exactly the same condition under which
    // PENDING_STATE_UPDATES can be non-empty (every action that pushes to
    // it also increments one of these counters), so this gate doubles as
    // "is there anything pending to commit at all."
    if summary.uploaded + summary.downloaded + summary.deleted_remote + summary.deleted_local + summary.skipped_missing > 0 {
        let encoded = match master_key {
            Some(mk) => super::index::encrypt(mk, &working_index)?,
            None => serde_json::to_vec(&working_index).map_err(|e| format!("failed to serialize index: {e}"))?,
        };
        // On failure -- most often another device's sync landing first --
        // the pending state updates below are never applied, leaving local
        // sync_state as it was. A retry recomputes the same diff against
        // the now-current index. Committing them first and finding out the
        // index never landed would let a later sync delete a file locally
        // that was only ever this device's unpublished upload.
        provider.upload_index(&encoded, &index_version).await?;
    }

    // Blobs go last, and only when nothing points at them any more.
    //
    // Storage is deduplicated by content hash (`providers/pocketbase` and
    // `providers/webdav` both key on `{root}/files/{hash}`), so deleting
    // the blob of a path deletes the CONTENT, which another path may still
    // be using. Two everyday cases hit this: a rename, which is a Create
    // plus a Delete whose Create dedupes onto the very blob the Delete is
    // about to remove; and two byte-identical files, which in a fresh
    // vault includes nobo's own empty `inbox.org` and `actions.org`.
    //
    // Ordering matters as much as the check. Deleting before the index is
    // published leaves a window in which the published index still names a
    // blob that is already gone -- which is what another device turns into
    // `skipped_missing`.
    //
    // A leftover blob is garbage that costs storage; a wrongly deleted one
    // is the file itself. So a failure here is logged and the run still
    // counts as successful.
    //
    // One race is left: another device can dedupe onto this hash between
    // our publish and this delete. Closing it needs a server-side refcount
    // or a mark-and-sweep against the published index.
    for (path, hash) in pending_blob_deletes {
        if working_index.hash_referenced_elsewhere(&hash, &path) {
            log::info!("sync: keeping the stored copy of {path} -- another entry still points at {hash}");
            continue;
        }
        if let Err(e) = provider.delete_file(&hash).await {
            log::warn!("sync: could not remove the stored copy of {path} ({hash}): {e}");
        }
    }

    {
        let conn = store::lock(db)?;
        for update in pending_state_updates {
            match update {
                PendingSyncState::MarkSynced { path, hash } => store::mark_synced(&conn, &path, &hash).map_err(|e| e.to_string())?,
                PendingSyncState::Remove { path } => store::remove(&conn, &path).map_err(|e| e.to_string())?,
            }
        }
    }

    Ok(summary)
}

/// A local `sync_state` change that may only take effect once the remote
/// index reflecting it has actually been published.
enum PendingSyncState {
    MarkSynced { path: String, hash: String },
    Remove { path: String },
}

async fn upload_one(
    provider: &dyn SyncProvider,
    vault_root: &Path,
    master_key: Option<&MasterKey>,
    work_dir: &Path,
    path: &str,
) -> Result<IndexEntry, String> {
    let abs_path = vault_root.join(path);
    let plaintext = tokio::fs::read(&abs_path).await.map_err(|e| format!("failed to read {}: {e}", abs_path.display()))?;
    let size = plaintext.len() as u64;
    let content_hash = hashing::hash_bytes(&plaintext);

    let Some(master_key) = master_key else {
        // Unencrypted opt-out (WebDAV only, see index.rs) -- upload the
        // plaintext file directly, no temp copy or FEK needed at all.
        provider.upload_file(&content_hash, &abs_path).await?;
        return Ok(IndexEntry { path: path.to_string(), content_hash, wrapped_key: None, size, updated_at: now(), blob_missing: false });
    };

    // Deterministic, not random: content-addressed dedup plus a random
    // FEK per upload orphans any file sharing a content hash with an
    // earlier one. See derive_file_key.
    let file_key = crypto::derive_file_key(master_key, &content_hash);
    let temp = tempfile::NamedTempFile::new_in(work_dir).map_err(|e| format!("failed to create temp file: {e}"))?;
    let temp_path = temp.path().to_path_buf();

    // encrypt_stream is synchronous CPU+IO work -- run on a blocking
    // thread so it doesn't stall the async runtime while hashing/
    // encrypting a large attachment.
    let file_key_for_blocking = file_key.clone();
    tokio::task::spawn_blocking(move || -> Result<(), String> {
        let mut reader = std::io::Cursor::new(plaintext);
        let mut writer = std::fs::File::create(&temp_path).map_err(|e| e.to_string())?;
        crypto::encrypt_stream(&file_key_for_blocking, &mut reader, &mut writer)
    })
    .await
    .map_err(|e| format!("encryption task panicked: {e}"))??;

    let encrypted_len = std::fs::metadata(temp.path()).map(|m| m.len()).unwrap_or(0);
    log::info!("upload_one: {path} encrypted to {encrypted_len} bytes (plaintext size={size}, content_hash={content_hash})");

    provider.upload_file(&content_hash, temp.path()).await?;

    let wrapped_key = crypto::wrap_key(master_key, &file_key)?;
    Ok(IndexEntry { path: path.to_string(), content_hash, wrapped_key: Some(wrapped_key), size, updated_at: now(), blob_missing: false })
}

async fn download_one(
    provider: &dyn SyncProvider,
    vault_root: &Path,
    master_key: Option<&MasterKey>,
    work_dir: &Path,
    entry: &IndexEntry,
) -> Result<(), String> {
    let abs_path = vault_root.join(&entry.path);
    if let Some(parent) = abs_path.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
    }

    let (Some(master_key), Some(wrapped_key)) = (master_key, entry.wrapped_key.as_ref()) else {
        // Unencrypted opt-out -- download straight to the real path, no
        // temp copy or FEK needed.
        return provider.download_file(&entry.content_hash, &abs_path).await;
    };

    let temp = tempfile::NamedTempFile::new_in(work_dir).map_err(|e| format!("failed to create temp file: {e}"))?;
    provider.download_file(&entry.content_hash, temp.path()).await?;
    let downloaded_len = std::fs::metadata(temp.path()).map(|m| m.len()).unwrap_or(0);
    log::info!(
        "download_one: {} downloaded {downloaded_len} bytes (index says size={}, content_hash={})",
        entry.path,
        entry.size,
        entry.content_hash
    );

    let file_key = crypto::unwrap_key(master_key, wrapped_key)?;
    log::info!("download_one: {} unwrap_key succeeded", entry.path);
    let temp_path = temp.path().to_path_buf();
    let target_path = abs_path.clone();
    tokio::task::spawn_blocking(move || -> Result<(), String> {
        let mut reader = std::fs::File::open(&temp_path).map_err(|e| e.to_string())?;
        let mut plaintext = Vec::new();
        crypto::decrypt_stream(&file_key, &mut reader, &mut plaintext)?;
        // Bytes rather than text: a vault holds attachments too. Atomic
        // for the same reason as every other vault write -- an interrupted
        // download must leave the previous version intact, not a truncated
        // file that the next scan then hashes and uploads as the truth.
        nobo_core::utils::fs_utils::write_atomic_bytes(&target_path, &plaintext).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| format!("decryption task panicked: {e}"))??;

    Ok(())
}

/// Fetches and decrypts the remote side of one path for a conflict
/// preview; the local side is already on disk. Read-only -- nothing local,
/// no `sync_state` and not the index. `Ok(None)` when the remote index no
/// longer has an entry, which is not an error, just nothing to preview.
pub async fn fetch_remote_content(
    provider: &dyn SyncProvider,
    master_key: Option<&MasterKey>,
    work_dir: &Path,
    path: &str,
) -> Result<Option<String>, String> {
    let Some((bytes, _version)) = provider.download_index().await? else { return Ok(None) };
    let remote_index: SyncIndex = match master_key {
        Some(mk) => super::index::decrypt(mk, &bytes)?,
        None => serde_json::from_slice(&bytes).map_err(|e| format!("failed to parse remote index: {e}"))?,
    };
    let Some(entry) = remote_index.find(path) else { return Ok(None) };

    tokio::fs::create_dir_all(work_dir).await.map_err(|e| format!("failed to create sync work dir: {e}"))?;
    let temp = tempfile::NamedTempFile::new_in(work_dir).map_err(|e| format!("failed to create temp file: {e}"))?;
    provider.download_file(&entry.content_hash, temp.path()).await?;

    let bytes = match (master_key, entry.wrapped_key.as_ref()) {
        (Some(master_key), Some(wrapped_key)) => {
            let file_key = crypto::unwrap_key(master_key, wrapped_key)?;
            let temp_path = temp.path().to_path_buf();
            tokio::task::spawn_blocking(move || -> Result<Vec<u8>, String> {
                let mut reader = std::fs::File::open(&temp_path).map_err(|e| e.to_string())?;
                let mut plaintext = Vec::new();
                crypto::decrypt_stream(&file_key, &mut reader, &mut plaintext)?;
                Ok(plaintext)
            })
            .await
            .map_err(|e| format!("decryption task panicked: {e}"))??
        }
        // Unencrypted opt-out (see index.rs's module doc comment) -- the
        // downloaded bytes already ARE the plaintext.
        _ => std::fs::read(temp.path()).map_err(|e| e.to_string())?,
    };

    Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::SyncProvider;
    use async_trait::async_trait;
    use rusqlite::Connection;
    use std::collections::HashMap;
    use std::sync::Mutex as StdSyncMutex;
    use tempfile::TempDir;

    /// In-memory stand-in for a real provider -- validates the
    /// orchestration logic itself (upload/download/delete sequencing,
    /// index round-tripping) without any network involved; the real HTTP
    /// contracts are covered separately in providers::webdav/pocketbase's
    /// own tests. The index is tracked as (bytes, monotonic version
    /// counter) and upload_index enforces the same conditional-write
    /// semantics a real provider does (see providers::IndexVersion's own
    /// doc comment) -- letting tests here exercise the FULL cross-device
    /// race end-to-end at the orchestrator level, not just per-provider.
    #[derive(Default)]
    struct MockProvider {
        index: StdSyncMutex<Option<(Vec<u8>, u64)>>,
        files: StdSyncMutex<HashMap<String, Vec<u8>>>,
        /// When > 0, download_file fails with a recoverable-looking error
        /// and decrements this instead of actually downloading -- lets
        /// tests simulate N transient network hiccups before a real
        /// transfer succeeds, exercising run_sync's retry loop end-to-end.
        flaky_downloads_remaining: StdSyncMutex<u32>,
    }

    #[async_trait]
    impl SyncProvider for MockProvider {
        async fn download_index(&self) -> Result<Option<(Vec<u8>, IndexVersion)>, String> {
            Ok(self.index.lock().unwrap().clone().map(|(bytes, v)| (bytes, IndexVersion::At(v.to_string()))))
        }
        async fn upload_index(&self, encrypted_blob: &[u8], expected_version: &IndexVersion) -> Result<(), String> {
            let mut guard = self.index.lock().unwrap();
            let current_version = match guard.as_ref() {
                Some((_, v)) => IndexVersion::At(v.to_string()),
                None => IndexVersion::Absent,
            };
            if &current_version != expected_version {
                return Err("sync-index-conflict: another device updated the index first, retry the sync".to_string());
            }
            let next_version = guard.as_ref().map(|(_, v)| v + 1).unwrap_or(1);
            *guard = Some((encrypted_blob.to_vec(), next_version));
            Ok(())
        }
        async fn upload_file(&self, content_hash: &str, local_path: &Path) -> Result<(), String> {
            let bytes = tokio::fs::read(local_path).await.map_err(|e| e.to_string())?;
            self.files.lock().unwrap().insert(content_hash.to_string(), bytes);
            Ok(())
        }
        async fn download_file(&self, content_hash: &str, local_path: &Path) -> Result<(), String> {
            {
                let mut remaining = self.flaky_downloads_remaining.lock().unwrap();
                if *remaining > 0 {
                    *remaining -= 1;
                    return Err("failed to fill whole buffer".to_string());
                }
            }
            let bytes = self.files.lock().unwrap().get(content_hash).cloned().ok_or(format!("file {content_hash} not found remotely"))?;
            tokio::fs::write(local_path, bytes).await.map_err(|e| e.to_string())
        }
        async fn delete_file(&self, content_hash: &str) -> Result<(), String> {
            self.files.lock().unwrap().remove(content_hash);
            Ok(())
        }
        async fn list_remote_files(&self) -> Result<Vec<String>, String> {
            Ok(self.files.lock().unwrap().keys().cloned().collect())
        }
        fn label(&self) -> String {
            "mock".to_string()
        }
        fn account_label(&self) -> String {
            "mock@example.com".to_string()
        }
        async fn remote_storage_bytes(&self) -> Result<u64, String> {
            Ok(self.files.lock().unwrap().values().map(|v| v.len() as u64).sum())
        }
    }

    fn db() -> StdSyncMutex<Connection> {
        let conn = Connection::open_in_memory().unwrap();
        store::init_schema(&conn).unwrap();
        StdSyncMutex::new(conn)
    }

    fn master_key() -> MasterKey {
        crypto::derive_master_key("hunter2", &crypto::generate_salt()).unwrap()
    }

    /// Wraps a MockProvider and, the FIRST time its own download_index is
    /// called, ALSO publishes a phantom (but validly encrypted, so a later
    /// retry can actually parse it) index update straight to the inner
    /// provider -- simulating a genuinely different device's own sync
    /// completing in the gap between THIS run's read and its own eventual
    /// write. Delegates everything else straight through. Exists to
    /// reproduce the real reported bug end-to-end through the actual
    /// run_sync entry point, not just at the provider level.
    struct RacingProvider<'a> {
        inner: MockProvider,
        master: &'a MasterKey,
        raced: StdSyncMutex<bool>,
    }

    #[async_trait]
    impl<'a> SyncProvider for RacingProvider<'a> {
        async fn download_index(&self) -> Result<Option<(Vec<u8>, IndexVersion)>, String> {
            let result = self.inner.download_index().await?;
            let should_race = {
                let mut raced = self.raced.lock().unwrap();
                let should_race = !*raced;
                *raced = true;
                should_race
            };
            if should_race {
                let phantom_bytes = super::super::index::encrypt(self.master, &SyncIndex::default()).unwrap();
                let current_version = match self.inner.index.lock().unwrap().as_ref() {
                    Some((_, v)) => IndexVersion::At(v.to_string()),
                    None => IndexVersion::Absent,
                };
                self.inner.upload_index(&phantom_bytes, &current_version).await.unwrap();
            }
            Ok(result)
        }
        async fn upload_index(&self, encrypted_blob: &[u8], expected_version: &IndexVersion) -> Result<(), String> {
            self.inner.upload_index(encrypted_blob, expected_version).await
        }
        async fn upload_file(&self, content_hash: &str, local_path: &Path) -> Result<(), String> {
            self.inner.upload_file(content_hash, local_path).await
        }
        async fn download_file(&self, content_hash: &str, local_path: &Path) -> Result<(), String> {
            self.inner.download_file(content_hash, local_path).await
        }
        async fn delete_file(&self, content_hash: &str) -> Result<(), String> {
            self.inner.delete_file(content_hash).await
        }
        async fn list_remote_files(&self) -> Result<Vec<String>, String> {
            self.inner.list_remote_files().await
        }
        fn label(&self) -> String {
            "racing-mock".to_string()
        }
        fn account_label(&self) -> String {
            self.inner.account_label()
        }
        async fn remote_storage_bytes(&self) -> Result<u64, String> {
            self.inner.remote_storage_bytes().await
        }
    }

    /// Writes CONTENT to a vault file the moment `download_index` is
    /// called -- i.e. after `run_sync`'s own rescan already recorded the
    /// file's previous hash, but before any action is applied. Exactly the
    /// window a user creating a subtask while a periodic sync happens to be
    /// running lands in.
    struct EditsLocallyMidRunProvider<'a> {
        inner: MockProvider,
        file: std::path::PathBuf,
        content: &'a str,
        edited: StdSyncMutex<bool>,
    }

    #[async_trait]
    impl<'a> SyncProvider for EditsLocallyMidRunProvider<'a> {
        async fn download_index(&self) -> Result<Option<(Vec<u8>, IndexVersion)>, String> {
            let result = self.inner.download_index().await?;
            let mut edited = self.edited.lock().unwrap();
            if !*edited {
                *edited = true;
                std::fs::write(&self.file, self.content).unwrap();
            }
            Ok(result)
        }
        async fn upload_index(&self, blob: &[u8], expected: &IndexVersion) -> Result<(), String> {
            self.inner.upload_index(blob, expected).await
        }
        async fn upload_file(&self, hash: &str, local_path: &Path) -> Result<(), String> {
            self.inner.upload_file(hash, local_path).await
        }
        async fn download_file(&self, hash: &str, local_path: &Path) -> Result<(), String> {
            self.inner.download_file(hash, local_path).await
        }
        async fn delete_file(&self, hash: &str) -> Result<(), String> {
            self.inner.delete_file(hash).await
        }
        async fn list_remote_files(&self) -> Result<Vec<String>, String> {
            self.inner.list_remote_files().await
        }
        fn label(&self) -> String {
            "mid-run-edit-mock".to_string()
        }
        fn account_label(&self) -> String {
            self.inner.account_label()
        }
        async fn remote_storage_bytes(&self) -> Result<u64, String> {
            self.inner.remote_storage_bytes().await
        }
    }

    /// A local edit landing while a sync is already in flight must not be
    /// destroyed by an action decided before it existed.
    ///
    /// The rescan runs once, at the start; everything after is decided
    /// against that snapshot. A Download overwriting the file would take
    /// the edit with it -- and leave nothing to detect afterwards, since
    /// the next rescan then finds local matching base again.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_local_edit_during_a_run_is_not_overwritten_by_a_download() {
        let vault = TempDir::new().unwrap();
        let work = TempDir::new().unwrap();
        let file = vault.path().join("projects.org");
        let local_db = db();
        let master = master_key();

        // Both sides agree on the starting content.
        std::fs::write(&file, "* Projekt\n").unwrap();
        let inner = MockProvider::default();
        run_sync(&inner, &local_db, vault.path(), Some(&master), work.path(), |_| {}).await.unwrap();

        // Another device publishes a change, so this device has a Download
        // pending for that path.
        {
            let other_vault = TempDir::new().unwrap();
            let other_work = TempDir::new().unwrap();
            let other_db = db();
            let other_file = other_vault.path().join("projects.org");
            std::fs::write(&other_file, "* Projekt\n").unwrap();
            run_sync(&inner, &other_db, other_vault.path(), Some(&master), other_work.path(), |_| {}).await.unwrap();
            std::fs::write(&other_file, "* Projekt\n** TODO Von anderem Gerät\n").unwrap();
            run_sync(&inner, &other_db, other_vault.path(), Some(&master), other_work.path(), |_| {}).await.unwrap();
        }

        // ... and the user adds a subtask here, mid-run.
        let provider = EditsLocallyMidRunProvider {
            inner,
            file: file.clone(),
            content: "* Projekt\n** TODO Gerade angelegt\n",
            edited: StdSyncMutex::new(false),
        };
        let summary = run_sync(&provider, &local_db, vault.path(), Some(&master), work.path(), |_| {}).await.unwrap();

        assert_eq!(summary.downloaded, 0, "the download was decided against content that no longer exists");
        assert_eq!(summary.skipped_locally_changed, 1);
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "* Projekt\n** TODO Gerade angelegt\n",
            "the just-created subtask must survive"
        );

        // And the next run reconciles it properly rather than losing it:
        // both sides changed since base, so this is a real conflict.
        let next = run_sync(&provider, &local_db, vault.path(), Some(&master), work.path(), |_| {}).await.unwrap();
        assert!(next.conflicts.len() == 1 || next.uploaded == 1, "must resolve into a real conflict or an upload, not silence: {next:?}");
        assert!(std::fs::read_to_string(&file).unwrap().contains("Gerade angelegt"));
    }

    /// The exact real-world sequence this whole fix exists for: both
    /// devices read the SAME (empty, first-ever-sync) index, device A
    /// completes a full sync in the meantime (publishing a real update),
    /// and device B -- still holding onto what it originally saw --
    /// finally tries to publish its own, now-stale-based index. That must
    /// be refused, not silently clobber A's just-published state (the
    /// literal bug: two devices, no sync errors shown, different content
    /// ends up on each).
    #[tokio::test(flavor = "multi_thread")]
    async fn two_overlapping_syncs_the_second_upload_is_refused_not_silently_clobbered() {
        let provider = MockProvider::default();
        let master = master_key();

        // Device B "reads" the index at the start of its own sync run.
        let stale_version = provider.download_index().await.unwrap().map(|(_, v)| v).unwrap_or(IndexVersion::Absent);
        assert_eq!(stale_version, IndexVersion::Absent, "nothing synced yet");

        // Device A completes a full, real sync in the meantime.
        let vault_a = TempDir::new().unwrap();
        let work_a = TempDir::new().unwrap();
        std::fs::write(vault_a.path().join("a.org"), "from device A").unwrap();
        run_sync(&provider, &db(), vault_a.path(), Some(&master), work_a.path(), |_| {}).await.unwrap();
        let version_after_a = provider.index.lock().unwrap().clone().unwrap().1;

        // Device B now tries to publish an index built from its own
        // already-stale view.
        let err = provider.upload_index(b"device B's own stale-based index", &stale_version).await.unwrap_err();
        assert!(err.contains("sync-index-conflict"), "got: {err}");

        // A's own upload is untouched -- not overwritten by B's stale attempt.
        let (stored_bytes, stored_version) = provider.index.lock().unwrap().clone().unwrap();
        assert_ne!(stored_bytes, b"device B's own stale-based index");
        assert_eq!(stored_version, version_after_a);
    }

    /// Same race, but reproduced through the real run_sync entry point
    /// (via RacingProvider) rather than calling the provider directly --
    /// confirms a genuine cross-device race surfaces as a real Err the
    /// caller can act on (e.g. retry), never as a silent success.
    #[tokio::test(flavor = "multi_thread")]
    async fn run_sync_surfaces_a_genuine_index_race_as_an_error_not_a_silent_success() {
        let vault = TempDir::new().unwrap();
        let work = TempDir::new().unwrap();
        std::fs::write(vault.path().join("a.org"), "hello").unwrap();

        let master = master_key();
        let provider = RacingProvider { inner: MockProvider::default(), master: &master, raced: StdSyncMutex::new(false) };

        let result = run_sync(&provider, &db(), vault.path(), Some(&master), work.path(), |_| {}).await;
        let err = result.unwrap_err();
        assert!(err.contains("sync-index-conflict"), "got: {err}");
    }

    /// The second, related bug this fix also has to close: once
    /// upload_index can genuinely fail, a naive implementation that marks
    /// sync_state as "synced" for each file AS IT PROCESSES actions (before
    /// the index publishing that fact has actually landed) would let a
    /// later sync see "already synced, but the remote doesn't have it" and
    /// DELETE the file -- even though the only thing that ever happened is
    /// this device's own index publish losing a race, not any real remote
    /// deletion. A failed run must leave local state exactly as if the run
    /// never happened, so a retry safely reconverges.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_index_upload_does_not_mark_files_synced_or_risk_deleting_them_later() {
        let vault = TempDir::new().unwrap();
        let work = TempDir::new().unwrap();
        std::fs::write(vault.path().join("a.org"), "unpublished edit").unwrap();

        let master = master_key();
        let provider = RacingProvider { inner: MockProvider::default(), master: &master, raced: StdSyncMutex::new(false) };
        let db = db();

        let result = run_sync(&provider, &db, vault.path(), Some(&master), work.path(), |_| {}).await;
        assert!(result.is_err(), "the index publish lost the race and must fail loudly");

        // The blob itself may well have been uploaded (upload_file has no
        // version check, only the index does) -- but local tracking must
        // NOT claim this path is durably synced, since the index update
        // that would make that true never actually landed.
        {
            let conn = db.lock().unwrap();
            let row = store::get(&conn, "a.org").unwrap();
            assert!(row.is_none_or(|r| r.base_hash.is_none()), "must not be marked synced when its own index publish failed");
        }

        // Concretely, the danger this guards against: a later sync must
        // not see "already synced, remote doesn't have it" and delete a
        // file that was only ever this device's own still-unpublished edit.
        let retry = run_sync(&provider, &db, vault.path(), Some(&master), work.path(), |_| {}).await.unwrap();
        assert_eq!(retry.deleted_local, 0, "must not delete a file whose index publish simply hadn't happened yet");
        assert_eq!(retry.uploaded, 1, "the retry re-publishes it correctly instead");
        assert!(vault.path().join("a.org").exists());
        assert_eq!(std::fs::read_to_string(vault.path().join("a.org")).unwrap(), "unpublished edit");
    }

    /// End-to-end confirmation that a conflict is fully recoverable, not a
    /// permanent wedge: retrying after a lost race converges cleanly, and a
    /// SECOND device downloading afterward sees the correct, complete state
    /// -- nothing was ever silently dropped by the race or its recovery.
    #[tokio::test(flavor = "multi_thread")]
    async fn retrying_after_a_lost_index_race_converges_and_a_second_device_sees_everything() {
        let vault = TempDir::new().unwrap();
        let work = TempDir::new().unwrap();
        std::fs::write(vault.path().join("a.org"), "content from the racing device").unwrap();

        let master = master_key();
        let provider = RacingProvider { inner: MockProvider::default(), master: &master, raced: StdSyncMutex::new(false) };
        let db_a = db();

        assert!(run_sync(&provider, &db_a, vault.path(), Some(&master), work.path(), |_| {}).await.is_err());
        let retry = run_sync(&provider, &db_a, vault.path(), Some(&master), work.path(), |_| {}).await.unwrap();
        assert_eq!(retry.uploaded, 1);

        let vault_c = TempDir::new().unwrap();
        let work_c = TempDir::new().unwrap();
        let summary_c = run_sync(&provider, &db(), vault_c.path(), Some(&master), work_c.path(), |_| {}).await.unwrap();
        assert_eq!(summary_c.downloaded, 1);
        assert_eq!(std::fs::read_to_string(vault_c.path().join("a.org")).unwrap(), "content from the racing device");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn uploads_a_new_local_file() {
        let vault = TempDir::new().unwrap();
        let work = TempDir::new().unwrap();
        std::fs::write(vault.path().join("a.org"), "hello world").unwrap();

        let provider = MockProvider::default();
        let db = db();
        let master = master_key();

        let summary = run_sync(&provider, &db, vault.path(), Some(&master), work.path(), |_| {}).await.unwrap();

        assert_eq!(summary.uploaded, 1);
        assert_eq!(provider.files.lock().unwrap().len(), 1);
        assert!(provider.index.lock().unwrap().is_some());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_second_device_downloads_what_the_first_uploaded() {
        let vault_a = TempDir::new().unwrap();
        let work_a = TempDir::new().unwrap();
        std::fs::write(vault_a.path().join("a.org"), "shared content").unwrap();

        let provider = MockProvider::default();
        let master = master_key();

        run_sync(&provider, &db(), vault_a.path(), Some(&master), work_a.path(), |_| {}).await.unwrap();

        // Second "device": fresh empty vault + fresh sync_state, same
        // provider (simulating the same remote account) and same master
        // key (simulating the same password).
        let vault_b = TempDir::new().unwrap();
        let work_b = TempDir::new().unwrap();
        let summary = run_sync(&provider, &db(), vault_b.path(), Some(&master), work_b.path(), |_| {}).await.unwrap();

        assert_eq!(summary.downloaded, 1);
        assert_eq!(std::fs::read_to_string(vault_b.path().join("a.org")).unwrap(), "shared content");
    }

    /// A connection dropping mid-transfer must not abort the whole sync;
    /// a couple of transient hiccups per file are absorbed.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_transient_download_failure_is_retried_and_recovers() {
        let vault_a = TempDir::new().unwrap();
        let work_a = TempDir::new().unwrap();
        std::fs::write(vault_a.path().join("a.org"), "shared content").unwrap();

        let provider = MockProvider::default();
        let master = master_key();
        run_sync(&provider, &db(), vault_a.path(), Some(&master), work_a.path(), |_| {}).await.unwrap();

        // Fail twice with a "stream cut short" style error before letting
        // the download really go through -- comfortably within
        // MAX_TRANSIENT_RETRIES.
        *provider.flaky_downloads_remaining.lock().unwrap() = 2;

        let vault_b = TempDir::new().unwrap();
        let work_b = TempDir::new().unwrap();
        let summary = run_sync(&provider, &db(), vault_b.path(), Some(&master), work_b.path(), |_| {}).await.unwrap();

        assert_eq!(summary.downloaded, 1);
        assert_eq!(std::fs::read_to_string(vault_b.path().join("a.org")).unwrap(), "shared content");
    }

    /// A persistently-failing transfer (more failures than the retry
    /// budget allows) must still surface as a real error -- the retry loop
    /// is a safety net for transient hiccups, not a way to silently hide a
    /// genuinely broken sync.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_persistently_failing_download_still_errors_out() {
        let vault_a = TempDir::new().unwrap();
        let work_a = TempDir::new().unwrap();
        std::fs::write(vault_a.path().join("a.org"), "shared content").unwrap();

        let provider = MockProvider::default();
        let master = master_key();
        run_sync(&provider, &db(), vault_a.path(), Some(&master), work_a.path(), |_| {}).await.unwrap();

        *provider.flaky_downloads_remaining.lock().unwrap() = 100;

        let vault_b = TempDir::new().unwrap();
        let work_b = TempDir::new().unwrap();
        let result = run_sync(&provider, &db(), vault_b.path(), Some(&master), work_b.path(), |_| {}).await;

        assert!(result.is_err());
    }

    /// A blob gone while the remote index still lists it -- the shape a
    /// crash between deleting and re-uploading the index leaves behind.
    /// One such stale reference must not abort another device's entire
    /// sync.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_stale_index_entry_pointing_at_a_deleted_blob_is_dropped_not_fatal() {
        let vault_a = TempDir::new().unwrap();
        let work_a = TempDir::new().unwrap();
        std::fs::write(vault_a.path().join("gone.org"), "will vanish remotely").unwrap();
        std::fs::write(vault_a.path().join("fine.org"), "unaffected").unwrap();

        let provider = MockProvider::default();
        let master = master_key();
        run_sync(&provider, &db(), vault_a.path(), Some(&master), work_a.path(), |_| {}).await.unwrap();

        // Simulate the blob having disappeared server-side (crash-truncated
        // delete, admin cleanup, whatever) while the index -- which B is
        // about to download -- still references it. Content-addressed, so
        // the stored hash is deterministic from the plaintext, same as
        // upload_one computes it -- no need to reverse-engineer it from the
        // (encrypted, unreadable) blob itself.
        let gone_hash = hashing::hash_bytes(b"will vanish remotely");
        assert!(provider.files.lock().unwrap().remove(&gone_hash).is_some());

        let vault_b = TempDir::new().unwrap();
        let work_b = TempDir::new().unwrap();
        let summary = run_sync(&provider, &db(), vault_b.path(), Some(&master), work_b.path(), |_| {}).await.unwrap();

        // The whole point: NOT an error, and the unrelated file still came
        // through.
        assert_eq!(summary.skipped_missing, 1, "the missing blob should be counted, not silently swallowed");
        assert!(!vault_b.path().join("gone.org").exists());
        assert!(vault_b.path().join("fine.org").exists(), "one dead entry must not stop the rest of the sync");

        // A third device hits the same wall, and that is now the correct
        // behaviour.
        //
        // This used to assert the opposite -- that B had "healed" the
        // index by dropping the entry. It had not: B never had the file,
        // and the pruned index it published is what makes the device that
        // DOES have it delete its only copy (see
        // index::IndexEntry::blob_missing). Being skipped on every device
        // until a holder re-uploads is the fail-safe reading, and
        // `a_missing_blob_is_healed_by_a_device_that_still_has_the_file`
        // covers the healing half.
        let vault_c = TempDir::new().unwrap();
        let work_c = TempDir::new().unwrap();
        let summary_c = run_sync(&provider, &db(), vault_c.path(), Some(&master), work_c.path(), |_| {}).await.unwrap();
        assert_eq!(summary_c.skipped_missing, 1, "still skipped, not resurrected and not deleted for everyone");
        assert!(vault_c.path().join("fine.org").exists());
    }

    #[test]
    fn recoverable_error_classifier_matches_transient_network_failures() {
        assert!(is_recoverable_sync_error("failed to fill whole buffer"));
        assert!(is_recoverable_sync_error("Connection reset by peer"));
        assert!(is_recoverable_sync_error("Server nicht erreichbar -- URL und Internetverbindung prüfen"));
        assert!(is_recoverable_sync_error("Zeitüberschreitung -- der Server antwortet nicht"));
    }

    #[test]
    fn recoverable_error_classifier_rejects_persistent_failures() {
        assert!(!is_recoverable_sync_error("Anmeldung fehlgeschlagen: E-Mail oder Passwort ist falsch."));
        assert!(!is_recoverable_sync_error("stream decryption failed: wrong key, corrupted data, or truncated file"));
        assert!(!is_recoverable_sync_error("failed to download abc123: HTTP 404 Not Found"));
    }

    /// The exact incident this regression-tests: two different paths with
    /// IDENTICAL content (e.g. two freshly-created empty template files)
    /// share a content_hash, so content-addressed dedup (see
    /// providers::upload_file) only ever actually stores ONE of their
    /// ciphertexts. With a random-per-upload FEK (the bug), the OTHER
    /// file's index entry wrapped a key that never matched the ciphertext
    /// genuinely on the server -- `unwrap_key` succeeded (it's a valid
    /// wrap of *some* key) but `decrypt_stream` then failed on every
    /// download, forever, for that path specifically.
    #[tokio::test(flavor = "multi_thread")]
    async fn two_files_with_identical_content_both_download_correctly() {
        let vault_a = TempDir::new().unwrap();
        let work_a = TempDir::new().unwrap();
        std::fs::write(vault_a.path().join("actions.org"), "").unwrap();
        std::fs::write(vault_a.path().join("inbox.org"), "").unwrap();

        let provider = MockProvider::default();
        let master = master_key();

        let summary = run_sync(&provider, &db(), vault_a.path(), Some(&master), work_a.path(), |_| {}).await.unwrap();
        assert_eq!(summary.uploaded, 2);
        // Content-addressed dedup: both empty files hash the same, so only
        // one ciphertext should actually be stored.
        assert_eq!(provider.files.lock().unwrap().len(), 1);

        let vault_b = TempDir::new().unwrap();
        let work_b = TempDir::new().unwrap();
        let summary = run_sync(&provider, &db(), vault_b.path(), Some(&master), work_b.path(), |_| {}).await.unwrap();

        assert_eq!(summary.downloaded, 2);
        assert!(vault_b.path().join("actions.org").exists());
        assert!(vault_b.path().join("inbox.org").exists());
    }

    /// WebDAV's opt-in unencrypted mode (see index.rs's module doc comment
    /// and nobo_sync::commands's `sync_login_webdav`) -- `master_key: None`
    /// end-to-end, for a user who already keeps a plaintext org vault
    /// synced via their own Nextcloud/WebDAV and doesn't want nobo's
    /// encryption layered on top of it.
    #[tokio::test(flavor = "multi_thread")]
    async fn unencrypted_mode_round_trips_and_stores_plaintext_on_the_server() {
        let vault_a = TempDir::new().unwrap();
        let work_a = TempDir::new().unwrap();
        std::fs::write(vault_a.path().join("a.org"), "plain content").unwrap();

        let provider = MockProvider::default();
        run_sync(&provider, &db(), vault_a.path(), None, work_a.path(), |_| {}).await.unwrap();

        // The whole point: bytes on the "server" are the real content, not
        // ciphertext -- readable by any other WebDAV/Nextcloud client.
        let stored = provider.files.lock().unwrap().values().next().unwrap().clone();
        assert_eq!(stored, b"plain content");
        let index_bytes = provider.index.lock().unwrap().clone().unwrap().0;
        assert!(String::from_utf8(index_bytes).unwrap().contains("a.org"), "index is plain JSON, not ciphertext");

        let vault_b = TempDir::new().unwrap();
        let work_b = TempDir::new().unwrap();
        let summary = run_sync(&provider, &db(), vault_b.path(), None, work_b.path(), |_| {}).await.unwrap();

        assert_eq!(summary.downloaded, 1);
        assert_eq!(std::fs::read_to_string(vault_b.path().join("a.org")).unwrap(), "plain content");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn deletion_propagates_to_a_second_device() {
        let vault_a = TempDir::new().unwrap();
        let work_a = TempDir::new().unwrap();
        let file_a = vault_a.path().join("a.org");
        std::fs::write(&file_a, "to be deleted").unwrap();

        let provider = MockProvider::default();
        let master = master_key();
        let db_a = db();
        run_sync(&provider, &db_a, vault_a.path(), Some(&master), work_a.path(), |_| {}).await.unwrap();

        let vault_b = TempDir::new().unwrap();
        let work_b = TempDir::new().unwrap();
        let db_b = db();
        run_sync(&provider, &db_b, vault_b.path(), Some(&master), work_b.path(), |_| {}).await.unwrap();
        assert!(vault_b.path().join("a.org").exists());

        // Device A deletes the file and syncs again.
        std::fs::remove_file(&file_a).unwrap();
        let summary_a = run_sync(&provider, &db_a, vault_a.path(), Some(&master), work_a.path(), |_| {}).await.unwrap();
        assert_eq!(summary_a.deleted_remote, 1);

        // Device B syncs and the file disappears there too -- NOT
        // resurrected, which is the entire reason diff.rs exists.
        let summary_b = run_sync(&provider, &db_b, vault_b.path(), Some(&master), work_b.path(), |_| {}).await.unwrap();
        assert_eq!(summary_b.deleted_local, 1);
        assert!(!vault_b.path().join("a.org").exists());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_second_sync_with_no_changes_uploads_nothing() {
        let vault = TempDir::new().unwrap();
        let work = TempDir::new().unwrap();
        std::fs::write(vault.path().join("a.org"), "hello").unwrap();

        let provider = MockProvider::default();
        let db = db();
        let master = master_key();

        run_sync(&provider, &db, vault.path(), Some(&master), work.path(), |_| {}).await.unwrap();
        let second = run_sync(&provider, &db, vault.path(), Some(&master), work.path(), |_| {}).await.unwrap();

        assert_eq!(second.uploaded, 0);
        assert_eq!(second.downloaded, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn conflicting_edits_are_reported_not_auto_resolved() {
        let vault_a = TempDir::new().unwrap();
        let work_a = TempDir::new().unwrap();
        let file_a = vault_a.path().join("a.org");
        std::fs::write(&file_a, "original").unwrap();

        let provider = MockProvider::default();
        let master = master_key();
        let db_a = db();
        run_sync(&provider, &db_a, vault_a.path(), Some(&master), work_a.path(), |_| {}).await.unwrap();

        let vault_b = TempDir::new().unwrap();
        let work_b = TempDir::new().unwrap();
        let db_b = db();
        run_sync(&provider, &db_b, vault_b.path(), Some(&master), work_b.path(), |_| {}).await.unwrap();

        // Both devices edit the same file differently before syncing again.
        std::fs::write(&file_a, "edited on device A").unwrap();
        run_sync(&provider, &db_a, vault_a.path(), Some(&master), work_a.path(), |_| {}).await.unwrap();

        std::fs::write(vault_b.path().join("a.org"), "edited on device B").unwrap();
        let summary_b = run_sync(&provider, &db_b, vault_b.path(), Some(&master), work_b.path(), |_| {}).await.unwrap();

        // "a.org" has no org headings at all (both edits are plain text,
        // not `* Task ...` lines), so merge_org_ast's own empty-headings
        // fallback (merge_text on the whole file) applies -- a single-line
        // edit on both sides is a genuine, overlapping diffy conflict
        // (Err), which auto_merge_conflict's caller treats the same as any
        // other unmergeable case: fall back to manual resolution.
        assert_eq!(summary_b.conflicts.len(), 1);
        assert_eq!(summary_b.conflicts[0].path, "a.org");
        assert!(summary_b.conflicts[0].local_hash.is_some());
        // Untouched -- device B's own edit must survive locally until the
        // user actually resolves the conflict.
        assert_eq!(std::fs::read_to_string(vault_b.path().join("a.org")).unwrap(), "edited on device B");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn resolving_a_conflict_keep_local_uploads_it_on_the_next_sync() {
        let vault_a = TempDir::new().unwrap();
        let work_a = TempDir::new().unwrap();
        let file_a = vault_a.path().join("a.org");
        std::fs::write(&file_a, "original").unwrap();

        let provider = MockProvider::default();
        let master = master_key();
        let db_a = db();
        run_sync(&provider, &db_a, vault_a.path(), Some(&master), work_a.path(), |_| {}).await.unwrap();

        let vault_b = TempDir::new().unwrap();
        let work_b = TempDir::new().unwrap();
        let db_b = db();
        run_sync(&provider, &db_b, vault_b.path(), Some(&master), work_b.path(), |_| {}).await.unwrap();

        std::fs::write(&file_a, "edited on A").unwrap();
        run_sync(&provider, &db_a, vault_a.path(), Some(&master), work_a.path(), |_| {}).await.unwrap();
        std::fs::write(vault_b.path().join("a.org"), "edited on B").unwrap();
        let summary_b = run_sync(&provider, &db_b, vault_b.path(), Some(&master), work_b.path(), |_| {}).await.unwrap();
        assert_eq!(summary_b.conflicts.len(), 1);

        {
            let conn = db_b.lock().unwrap();
            resolve_conflict(&conn, &summary_b.conflicts[0], true).unwrap();
        }
        let summary_b2 = run_sync(&provider, &db_b, vault_b.path(), Some(&master), work_b.path(), |_| {}).await.unwrap();
        assert_eq!(summary_b2.uploaded, 1);
        assert!(summary_b2.conflicts.is_empty());

        // Device A picks up B's now-authoritative content.
        let summary_a2 = run_sync(&provider, &db_a, vault_a.path(), Some(&master), work_a.path(), |_| {}).await.unwrap();
        assert_eq!(summary_a2.downloaded, 1);
        assert_eq!(std::fs::read_to_string(&file_a).unwrap(), "edited on B");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn resolving_a_conflict_keep_remote_downloads_it_on_the_next_sync() {
        let vault_a = TempDir::new().unwrap();
        let work_a = TempDir::new().unwrap();
        let file_a = vault_a.path().join("a.org");
        std::fs::write(&file_a, "original").unwrap();

        let provider = MockProvider::default();
        let master = master_key();
        let db_a = db();
        run_sync(&provider, &db_a, vault_a.path(), Some(&master), work_a.path(), |_| {}).await.unwrap();

        let vault_b = TempDir::new().unwrap();
        let work_b = TempDir::new().unwrap();
        let db_b = db();
        run_sync(&provider, &db_b, vault_b.path(), Some(&master), work_b.path(), |_| {}).await.unwrap();

        std::fs::write(&file_a, "edited on A").unwrap();
        run_sync(&provider, &db_a, vault_a.path(), Some(&master), work_a.path(), |_| {}).await.unwrap();
        std::fs::write(vault_b.path().join("a.org"), "edited on B").unwrap();
        let summary_b = run_sync(&provider, &db_b, vault_b.path(), Some(&master), work_b.path(), |_| {}).await.unwrap();
        assert_eq!(summary_b.conflicts.len(), 1);

        {
            let conn = db_b.lock().unwrap();
            resolve_conflict(&conn, &summary_b.conflicts[0], false).unwrap();
        }
        let summary_b2 = run_sync(&provider, &db_b, vault_b.path(), Some(&master), work_b.path(), |_| {}).await.unwrap();
        assert_eq!(summary_b2.downloaded, 1);
        assert_eq!(std::fs::read_to_string(vault_b.path().join("a.org")).unwrap(), "edited on A");
    }

    /// The bug this whole conflict case exists to prevent: without it, a
    /// remote deletion would have silently destroyed B's unsynced local
    /// edit via an automatic DeleteLocal (caught by this test failing
    /// before the fix, see diff.rs's matching regression test).
    #[tokio::test(flavor = "multi_thread")]
    async fn resolving_a_remote_deletion_vs_local_edit_conflict_keep_local_reuploads_it() {
        let vault_a = TempDir::new().unwrap();
        let work_a = TempDir::new().unwrap();
        let file_a = vault_a.path().join("a.org");
        std::fs::write(&file_a, "original").unwrap();

        let provider = MockProvider::default();
        let master = master_key();
        let db_a = db();
        run_sync(&provider, &db_a, vault_a.path(), Some(&master), work_a.path(), |_| {}).await.unwrap();

        let vault_b = TempDir::new().unwrap();
        let work_b = TempDir::new().unwrap();
        let db_b = db();
        run_sync(&provider, &db_b, vault_b.path(), Some(&master), work_b.path(), |_| {}).await.unwrap();

        // A deletes; B edits -- then both sync, in the order that produces
        // the conflict on B (B still has to see A's deletion attempt fail
        // to auto-apply because B's own edit raced it).
        std::fs::remove_file(&file_a).unwrap();
        run_sync(&provider, &db_a, vault_a.path(), Some(&master), work_a.path(), |_| {}).await.unwrap();
        std::fs::write(vault_b.path().join("a.org"), "edited on B").unwrap();
        let summary_b = run_sync(&provider, &db_b, vault_b.path(), Some(&master), work_b.path(), |_| {}).await.unwrap();
        assert_eq!(summary_b.conflicts.len(), 1);
        assert!(summary_b.conflicts[0].local_hash.is_some(), "B has real local content, not a deletion");
        assert!(summary_b.conflicts[0].remote_hash.is_none(), "A deleted it, so there's no remote content hash");

        // B decides its own edit (the "local" side here) wins.
        {
            let conn = db_b.lock().unwrap();
            resolve_conflict(&conn, &summary_b.conflicts[0], true).unwrap();
        }
        let summary_b2 = run_sync(&provider, &db_b, vault_b.path(), Some(&master), work_b.path(), |_| {}).await.unwrap();
        assert_eq!(summary_b2.uploaded, 1);
        assert!(std::fs::exists(vault_b.path().join("a.org")).unwrap());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn resolving_a_remote_deletion_vs_local_edit_conflict_keep_remote_deletes_it_locally() {
        let vault_a = TempDir::new().unwrap();
        let work_a = TempDir::new().unwrap();
        let file_a = vault_a.path().join("a.org");
        std::fs::write(&file_a, "original").unwrap();

        let provider = MockProvider::default();
        let master = master_key();
        let db_a = db();
        run_sync(&provider, &db_a, vault_a.path(), Some(&master), work_a.path(), |_| {}).await.unwrap();

        let vault_b = TempDir::new().unwrap();
        let work_b = TempDir::new().unwrap();
        let db_b = db();
        run_sync(&provider, &db_b, vault_b.path(), Some(&master), work_b.path(), |_| {}).await.unwrap();

        std::fs::remove_file(&file_a).unwrap();
        run_sync(&provider, &db_a, vault_a.path(), Some(&master), work_a.path(), |_| {}).await.unwrap();
        let file_b = vault_b.path().join("a.org");
        std::fs::write(&file_b, "edited on B").unwrap();
        let summary_b = run_sync(&provider, &db_b, vault_b.path(), Some(&master), work_b.path(), |_| {}).await.unwrap();
        assert_eq!(summary_b.conflicts.len(), 1);

        // B decides A's deletion wins instead.
        {
            let conn = db_b.lock().unwrap();
            resolve_conflict(&conn, &summary_b.conflicts[0], false).unwrap();
        }
        let summary_b2 = run_sync(&provider, &db_b, vault_b.path(), Some(&master), work_b.path(), |_| {}).await.unwrap();
        assert_eq!(summary_b2.deleted_local, 1);
        assert!(!file_b.exists());
    }

    /* --- auto-merge structural safety -- must never corrupt/bleed content
    across headings, and must fail closed (manual conflict) rather than
    guess whenever it can't safely reconcile something --- */

    fn encrypted_blob(master: &MasterKey, content: &str) -> (String, Vec<u8>) {
        let hash = hashing::hash_bytes(content.as_bytes());
        let file_key = crypto::derive_file_key(master, &hash);
        let mut encrypted = Vec::new();
        crypto::encrypt_stream(&file_key, &mut std::io::Cursor::new(content.as_bytes().to_vec()), &mut encrypted).unwrap();
        (hash, encrypted)
    }

    // Every task needs a real state -- otherwise only the first ("Task A")
    // stays real structure and B/C get absorbed as its own content
    // instead of remaining separate, addressable headings (org_parser's
    // rule: state or :project: tag required once something else already
    // opened the heading before them).
    const THREE_HEADINGS: &str = "* TODO Task A\n:PROPERTIES:\n:CUSTOM_ID: aaa\n:END:\n* TODO Task B\n:PROPERTIES:\n:CUSTOM_ID: bbb\n:END:\n* TODO Task C\n:PROPERTIES:\n:CUSTOM_ID: ccc\n:END:\n";

    /// The core structural-safety guarantee end-to-end: two devices edit
    /// TWO DIFFERENT headings in the same file (both already carrying a
    /// CUSTOM_ID); auto-merge must combine both edits and leave the third,
    /// untouched heading exactly as it was -- not corrupted, not dropped,
    /// not duplicated.
    #[tokio::test(flavor = "multi_thread")]
    async fn auto_merge_combines_edits_to_different_headings_and_preserves_the_untouched_third() {
        let vault_a = TempDir::new().unwrap();
        let work_a = TempDir::new().unwrap();
        let file_a = vault_a.path().join("projects.org");
        std::fs::write(&file_a, THREE_HEADINGS).unwrap();

        let provider = MockProvider::default();
        let master = master_key();
        let db_a = db();
        run_sync(&provider, &db_a, vault_a.path(), Some(&master), work_a.path(), |_| {}).await.unwrap();

        let vault_b = TempDir::new().unwrap();
        let work_b = TempDir::new().unwrap();
        let db_b = db();
        run_sync(&provider, &db_b, vault_b.path(), Some(&master), work_b.path(), |_| {}).await.unwrap();

        // Device A edits heading A only, syncs.
        let edited_by_a = THREE_HEADINGS.replace("* TODO Task A\n", "* TODO Task A Edited By A\n");
        std::fs::write(&file_a, &edited_by_a).unwrap();
        run_sync(&provider, &db_a, vault_a.path(), Some(&master), work_a.path(), |_| {}).await.unwrap();

        // Device B, still on the original base, independently edits
        // heading B only, then syncs into A's already-published edit.
        let file_b = vault_b.path().join("projects.org");
        let edited_by_b = THREE_HEADINGS.replace("* TODO Task B\n", "* TODO Task B Edited By B\n");
        std::fs::write(&file_b, &edited_by_b).unwrap();
        let summary_b = run_sync(&provider, &db_b, vault_b.path(), Some(&master), work_b.path(), |_| {}).await.unwrap();

        assert!(summary_b.conflicts.is_empty(), "should auto-merge cleanly, not surface a manual conflict: {:?}", summary_b.conflicts);
        assert_eq!(summary_b.uploaded, 1);

        let merged = std::fs::read_to_string(&file_b).unwrap();
        assert!(merged.contains("Task A Edited By A"), "device A's edit survived: {merged}");
        assert!(merged.contains("Task B Edited By B"), "device B's own edit survived: {merged}");
        assert!(merged.contains("Task C\n"), "untouched Task C survived: {merged}");
        assert_eq!(merged.matches("CUSTOM_ID").count(), 3, "still exactly 3 headings -- nothing duplicated or dropped: {merged}");

        // Device A picks up the merged result too -- both sides converge
        // on the SAME combined content, nothing left stuck one-sided.
        let summary_a2 = run_sync(&provider, &db_a, vault_a.path(), Some(&master), work_a.path(), |_| {}).await.unwrap();
        assert_eq!(summary_a2.downloaded, 1);
        let final_a = std::fs::read_to_string(&file_a).unwrap();
        assert!(final_a.contains("Task A Edited By A") && final_a.contains("Task B Edited By B") && final_a.contains("Task C\n"));
    }

    /// A file whose headings predate the CUSTOM_ID backfill must NOT get
    /// the risky title-based auto-merge -- it should fall back to exactly
    /// the same manual-conflict behavior sync had before auto-merge
    /// existed at all, leaving the local edit untouched pending a real
    /// decision.
    #[tokio::test(flavor = "multi_thread")]
    async fn files_without_custom_id_fall_back_to_manual_conflict_instead_of_auto_merging() {
        let vault_a = TempDir::new().unwrap();
        let work_a = TempDir::new().unwrap();
        let file_a = vault_a.path().join("legacy.org");
        std::fs::write(&file_a, "* Task A\n* Task B\n").unwrap();

        let provider = MockProvider::default();
        let master = master_key();
        let db_a = db();
        run_sync(&provider, &db_a, vault_a.path(), Some(&master), work_a.path(), |_| {}).await.unwrap();

        let vault_b = TempDir::new().unwrap();
        let work_b = TempDir::new().unwrap();
        let db_b = db();
        run_sync(&provider, &db_b, vault_b.path(), Some(&master), work_b.path(), |_| {}).await.unwrap();

        std::fs::write(&file_a, "* Task A Edited\n* Task B\n").unwrap();
        run_sync(&provider, &db_a, vault_a.path(), Some(&master), work_a.path(), |_| {}).await.unwrap();

        let file_b = vault_b.path().join("legacy.org");
        std::fs::write(&file_b, "* Task A\n* Task B Edited\n").unwrap();
        let summary_b = run_sync(&provider, &db_b, vault_b.path(), Some(&master), work_b.path(), |_| {}).await.unwrap();

        assert_eq!(summary_b.conflicts.len(), 1, "no CUSTOM_ID anywhere -- must fall back to manual resolution, not guess");
        assert_eq!(
            std::fs::read_to_string(&file_b).unwrap(),
            "* Task A\n* Task B Edited\n",
            "local edit untouched pending manual resolution"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn fetch_blob_by_hash_rejects_non_utf8_content_instead_of_lossily_mangling_it() {
        let work = TempDir::new().unwrap();
        let provider = MockProvider::default();
        let master = master_key();

        // Not valid UTF-8 (a lone 0xFF/0xFE byte and continuation bytes
        // with no leading byte) -- stands in for a binary attachment.
        let binary_bytes: Vec<u8> = vec![0xFF, 0xFE, 0x00, 0x01, 0x02, 0x80, 0x81];
        let hash = hashing::hash_bytes(&binary_bytes);
        let file_key = crypto::derive_file_key(&master, &hash);
        let mut encrypted = Vec::new();
        crypto::encrypt_stream(&file_key, &mut std::io::Cursor::new(binary_bytes), &mut encrypted).unwrap();
        provider.files.lock().unwrap().insert(hash.clone(), encrypted);

        let result = fetch_blob_by_hash(&provider, Some(&master), work.path(), &hash).await;
        assert!(result.is_err(), "binary/non-UTF8 content must be rejected outright, not silently lossy-converted: {result:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn auto_merge_conflict_fails_safely_when_the_local_file_cant_be_read() {
        let vault = TempDir::new().unwrap();
        let work = TempDir::new().unwrap();
        let provider = MockProvider::default();
        let master = master_key();
        let db = db();

        let (base_hash, base_blob) = encrypted_blob(&master, "base content");
        let (remote_hash, remote_blob) = encrypted_blob(&master, "remote content");
        provider.files.lock().unwrap().insert(base_hash.clone(), base_blob);
        provider.files.lock().unwrap().insert(remote_hash.clone(), remote_blob);

        {
            let conn = db.lock().unwrap();
            store::mark_synced(&conn, "a.org", &base_hash).unwrap();
        }

        // "a.org" is a DIRECTORY, not a file -- tokio::fs::read_to_string
        // must fail on it, and that failure must propagate as an Err, not
        // silently become an empty string (which would make merge_org_ast
        // think local deleted everything and overwrite the real file with
        // remote's content alone).
        std::fs::create_dir(vault.path().join("a.org")).unwrap();

        let result = auto_merge_conflict(&provider, &db, vault.path(), Some(&master), work.path(), "a.org", &remote_hash).await;
        assert!(result.is_err(), "a local read failure must surface as Err: {result:?}");
    }

    #[test]
    fn lock_prevents_a_concurrent_second_acquire() {
        let flag = Arc::new(AtomicBool::new(false));
        let _first = acquire_lock(&flag).unwrap();
        assert!(acquire_lock(&flag).is_err());
    }

    #[test]
    fn lock_releases_on_drop() {
        let flag = Arc::new(AtomicBool::new(false));
        {
            let _guard = acquire_lock(&flag).unwrap();
        }
        assert!(acquire_lock(&flag).is_ok());
    }

    /* --- the delete paths (see index::IndexEntry::blob_missing) --- */

    /// A plain rename, which sync sees as Create + Delete.
    ///
    /// `compute_diff` walks paths in BTreeSet order, so `z.org` -> `a.org`
    /// produces `[Upload a.org, DeleteRemote z.org]`. Storage is
    /// deduplicated by content hash, so the upload lands on the blob that
    /// already exists and the delete then removed it -- while the index
    /// being published in the same run pointed `a.org` straight at it.
    /// Device B downloaded nothing, pruned the entry, published that, and
    /// device A -- the only holder -- deleted its local copy on the next
    /// run.
    ///
    /// Direction matters: `a.org` -> `z.org` sorts the other way and heals
    /// itself, which is why this went unnoticed.
    #[tokio::test(flavor = "multi_thread")]
    async fn renaming_a_file_to_an_earlier_sorting_name_keeps_its_content() {
        let vault_a = TempDir::new().unwrap();
        let work_a = TempDir::new().unwrap();
        std::fs::write(vault_a.path().join("z.org"), "important note").unwrap();

        let provider = MockProvider::default();
        let master = master_key();
        let db_a = db();
        run_sync(&provider, &db_a, vault_a.path(), Some(&master), work_a.path(), |_| {}).await.unwrap();
        assert_eq!(provider.files.lock().unwrap().len(), 1);

        std::fs::rename(vault_a.path().join("z.org"), vault_a.path().join("a.org")).unwrap();
        run_sync(&provider, &db_a, vault_a.path(), Some(&master), work_a.path(), |_| {}).await.unwrap();

        assert_eq!(
            provider.files.lock().unwrap().len(),
            1,
            "the blob the published index points at must survive the delete half of the rename"
        );

        // A second device gets the renamed file, rather than an index
        // entry with nothing behind it.
        let vault_b = TempDir::new().unwrap();
        let work_b = TempDir::new().unwrap();
        let db_b = db();
        let summary_b = run_sync(&provider, &db_b, vault_b.path(), Some(&master), work_b.path(), |_| {}).await.unwrap();
        assert_eq!(summary_b.downloaded, 1);
        assert_eq!(summary_b.skipped_missing, 0);
        assert_eq!(std::fs::read_to_string(vault_b.path().join("a.org")).unwrap(), "important note");

        // And the holder still has it after B has published its view.
        let summary_a = run_sync(&provider, &db_a, vault_a.path(), Some(&master), work_a.path(), |_| {}).await.unwrap();
        assert_eq!(summary_a.deleted_local, 0, "nothing on this device was deleted");
        assert!(vault_a.path().join("a.org").exists());
    }

    /// Two byte-identical files share one blob -- which for a fresh vault
    /// includes nobo's own empty `inbox.org` and `actions.org`. Deleting
    /// one used to delete the other's content with it, independently of
    /// sort order.
    #[tokio::test(flavor = "multi_thread")]
    async fn deleting_one_of_two_identical_files_leaves_the_other_readable() {
        let vault_a = TempDir::new().unwrap();
        let work_a = TempDir::new().unwrap();
        std::fs::write(vault_a.path().join("actions.org"), "").unwrap();
        std::fs::write(vault_a.path().join("inbox.org"), "").unwrap();

        let provider = MockProvider::default();
        let master = master_key();
        let db_a = db();
        run_sync(&provider, &db_a, vault_a.path(), Some(&master), work_a.path(), |_| {}).await.unwrap();
        assert_eq!(provider.files.lock().unwrap().len(), 1, "both files dedupe onto one blob");

        std::fs::remove_file(vault_a.path().join("inbox.org")).unwrap();
        run_sync(&provider, &db_a, vault_a.path(), Some(&master), work_a.path(), |_| {}).await.unwrap();

        assert_eq!(provider.files.lock().unwrap().len(), 1, "actions.org still needs that blob");

        let vault_b = TempDir::new().unwrap();
        let work_b = TempDir::new().unwrap();
        let db_b = db();
        let summary_b = run_sync(&provider, &db_b, vault_b.path(), Some(&master), work_b.path(), |_| {}).await.unwrap();
        assert_eq!(summary_b.skipped_missing, 0);
        assert!(vault_b.path().join("actions.org").exists());
        assert!(!vault_b.path().join("inbox.org").exists(), "the deletion still propagates");
    }

    /// The blob really is gone -- the entry is kept and marked, and the
    /// device that still holds the content republishes it, rather than
    /// every device deleting its copy.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_missing_blob_is_healed_by_a_device_that_still_has_the_file() {
        let vault_a = TempDir::new().unwrap();
        let work_a = TempDir::new().unwrap();
        std::fs::write(vault_a.path().join("a.org"), "shared content").unwrap();

        let provider = MockProvider::default();
        let master = master_key();
        let db_a = db();
        run_sync(&provider, &db_a, vault_a.path(), Some(&master), work_a.path(), |_| {}).await.unwrap();

        // Storage loses the blob, the index still names it.
        provider.files.lock().unwrap().clear();

        // A device without the file cannot download it -- and must not
        // decide on everyone's behalf that the file is gone.
        let vault_b = TempDir::new().unwrap();
        let work_b = TempDir::new().unwrap();
        let db_b = db();
        let summary_b = run_sync(&provider, &db_b, vault_b.path(), Some(&master), work_b.path(), |_| {}).await.unwrap();
        assert_eq!(summary_b.skipped_missing, 1);

        // The holder re-uploads it.
        let summary_a = run_sync(&provider, &db_a, vault_a.path(), Some(&master), work_a.path(), |_| {}).await.unwrap();
        assert_eq!(summary_a.uploaded, 1, "the device that has the content republishes it");
        assert_eq!(summary_a.deleted_local, 0, "and does not delete its own copy");
        assert!(vault_a.path().join("a.org").exists());

        // Now B can have it.
        let summary_b2 = run_sync(&provider, &db_b, vault_b.path(), Some(&master), work_b.path(), |_| {}).await.unwrap();
        assert_eq!(summary_b2.downloaded, 1);
        assert_eq!(std::fs::read_to_string(vault_b.path().join("a.org")).unwrap(), "shared content");
    }

    /// A vanished remote index is not "everything was deleted".
    ///
    /// Reachable without a server fault: an expired PocketBase token
    /// returns 200 with an empty item list, a restored backup predates the
    /// record, and `run_sync`'s own error text used to advise deleting it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_vanished_remote_index_aborts_instead_of_deleting_the_vault() {
        let vault = TempDir::new().unwrap();
        let work = TempDir::new().unwrap();
        for name in ["a.org", "b.org", "c.org"] {
            std::fs::write(vault.path().join(name), format!("content of {name}")).unwrap();
        }

        let provider = MockProvider::default();
        let master = master_key();
        let conn = db();
        run_sync(&provider, &conn, vault.path(), Some(&master), work.path(), |_| {}).await.unwrap();

        // The index record disappears server-side.
        *provider.index.lock().unwrap() = None;

        let err = run_sync(&provider, &conn, vault.path(), Some(&master), work.path(), |_| {}).await.unwrap_err();

        // The marker, not the sentence: the sentence depends on the
        // machine's configured language.
        assert!(err.contains("sync-index-missing:"), "{err}");
        for name in ["a.org", "b.org", "c.org"] {
            assert!(vault.path().join(name).exists(), "{name} must still be there");
        }

        // And the way out: republish, and everything is uploaded again.
        {
            let c = super::store::lock(&conn).unwrap();
            super::store::clear_all_base_hashes(&c).unwrap();
        }
        let summary = run_sync(&provider, &conn, vault.path(), Some(&master), work.path(), |_| {}).await.unwrap();
        assert_eq!(summary.uploaded, 3);
        assert_eq!(summary.deleted_local, 0);
    }
}
