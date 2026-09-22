//! Pure 3-way diff logic -- deliberately DB- and network-free (takes plain
//! data in, returns plain data out) so every case in the table below has a
//! direct, fast unit test with no sqlite/HTTP involved. See docs/sync.md
//! for the case table this implements; `base_hash` (content hash as of the
//! last successful sync) is what makes this different from -- and correct
//! where -- a naive local-vs-remote-only comparison would resurrect
//! deliberate deletions.

use super::store::SyncStateRow;
use std::collections::{BTreeSet, HashMap};

#[derive(Debug, Clone, PartialEq)]
pub struct RemoteEntry {
    pub path: String,
    pub content_hash: String,
    /// The index still lists this path, but its blob is gone from storage
    /// -- see `index::IndexEntry::blob_missing`.
    pub blob_missing: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SyncAction {
    Upload { path: String },
    Download { path: String },
    DeleteRemote { path: String },
    DeleteLocal { path: String },
    /// Both sides changed to genuinely different content, which needs a
    /// user decision -- last-write-wins would quietly lose edits.
    ///
    /// Carries both hashes so resolving needs no second lookup. A side's
    /// hash is `None` exactly when it deleted the file while the other
    /// changed it; never both `None`, which is a Forget, and never both
    /// equal, which is a ResolveSame.
    Conflict { path: String, local_hash: Option<String>, remote_hash: Option<String> },
    /// Both sides changed since the last sync, but converged on the SAME
    /// content (e.g. the same edit synced from two devices before either
    /// saw the other's upload) -- not a real conflict, just needs its
    /// `base_hash` brought up to date with no actual transfer.
    ResolveSame { path: String },
    /// Tracked locally but deleted again before it was ever synced, and
    /// never existed remotely either -- nothing to propagate, just drop
    /// the sync_state row.
    Forget { path: String },
}

impl SyncAction {
    pub fn path(&self) -> &str {
        match self {
            SyncAction::Upload { path }
            | SyncAction::Download { path }
            | SyncAction::DeleteRemote { path }
            | SyncAction::DeleteLocal { path }
            | SyncAction::Conflict { path, .. }
            | SyncAction::ResolveSame { path }
            | SyncAction::Forget { path } => path,
        }
    }
}

pub fn compute_diff(local: &[SyncStateRow], remote: &[RemoteEntry]) -> Vec<SyncAction> {
    let remote_map: HashMap<&str, &RemoteEntry> =
        remote.iter().map(|e| (e.path.as_str(), e)).collect();
    let local_map: HashMap<&str, &SyncStateRow> = local.iter().map(|r| (r.path.as_str(), r)).collect();

    let mut all_paths: BTreeSet<&str> = BTreeSet::new();
    all_paths.extend(local_map.keys());
    all_paths.extend(remote_map.keys());

    let mut actions = Vec::with_capacity(all_paths.len());

    for path in all_paths {
        let row = local_map.get(path).copied();
        let remote = remote_map.get(path).copied();

        let Some(action) = diff_one(path, row, remote) else { continue };
        actions.push(action);
    }

    actions
}

fn diff_one(path: &str, row: Option<&SyncStateRow>, remote: Option<&RemoteEntry>) -> Option<SyncAction> {
    let path = path.to_string();
    let remote_hash = remote.map(|e| e.content_hash.as_str());
    let blob_missing = remote.is_some_and(|e| e.blob_missing);

    match (row, remote_hash) {
        // Never tracked locally at all -- a file that exists only remotely.
        (None, Some(_)) => Some(SyncAction::Download { path }),

        (Some(row), remote_hash) => {
            let base = row.base_hash.as_deref();
            let local_hash = row.local_hash.as_deref();
            let local_deleted = row.deleted_at.is_some();
            let local_changed = local_hash != base;
            let remote_changed = remote_hash != base;

            match remote_hash {
                None => {
                    // Gone (or never present) remotely.
                    if base.is_none() {
                        // Never synced -- either a genuine new local file
                        // (upload it) or one that was created and deleted
                        // again before ever syncing (nothing to do).
                        if local_deleted { Some(SyncAction::Forget { path }) } else { Some(SyncAction::Upload { path }) }
                    } else if local_deleted {
                        // Both sides now agree it's gone -- just stop tracking it.
                        Some(SyncAction::Forget { path })
                    } else if local_changed {
                        // Remote deleted it, but there's an unsynced local
                        // edit -- must NOT auto-delete that edit (that's
                        // real data loss, and the whole reason this isn't
                        // just "missing on one side -> assume the other
                        // side is right"). A real conflict, symmetric with
                        // the "local deleted, remote edited" case below.
                        Some(SyncAction::Conflict { path, local_hash: local_hash.map(String::from), remote_hash: None })
                    } else {
                        // Previously synced, unchanged locally, gone
                        // remotely: another device deleted it and there is
                        // nothing local worth keeping.
                        Some(SyncAction::DeleteLocal { path })
                    }
                }
                Some(remote_h) => {
                    if local_deleted {
                        if remote_changed {
                            // Deleted here, edited elsewhere -- a real conflict,
                            // not an automatic "deletion wins" or "edit wins".
                            Some(SyncAction::Conflict { path, local_hash: None, remote_hash: Some(remote_h.to_string()) })
                        } else {
                            Some(SyncAction::DeleteRemote { path })
                        }
                    } else if local_changed && remote_changed {
                        if local_hash == Some(remote_h) {
                            Some(SyncAction::ResolveSame { path })
                        } else {
                            Some(SyncAction::Conflict {
                                path,
                                local_hash: local_hash.map(String::from),
                                remote_hash: Some(remote_h.to_string()),
                            })
                        }
                    } else if local_changed {
                        Some(SyncAction::Upload { path })
                    } else if remote_changed {
                        Some(SyncAction::Download { path })
                    } else if blob_missing && local_hash == Some(remote_h) {
                        // In sync by hash, but the blob behind the entry is
                        // gone from storage -- this device holds the only
                        // copy of content the index claims exists. Upload
                        // it again; without this the entry stays broken
                        // forever, because "unchanged on both sides" is
                        // otherwise nothing to do.
                        Some(SyncAction::Upload { path })
                    } else {
                        None // Unchanged on both sides -- nothing to do.
                    }
                }
            }
        }

        (None, None) => unreachable!("path can't be in the union of local/remote paths with neither present"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(path: &str, base: Option<&str>, local: Option<&str>, deleted: bool) -> SyncStateRow {
        SyncStateRow {
            path: path.to_string(),
            base_hash: base.map(String::from),
            local_hash: local.map(String::from),
            deleted_at: if deleted { Some("2026-01-01T00:00:00Z".to_string()) } else { None },
        }
    }

    fn remote(path: &str, hash: &str) -> RemoteEntry {
        RemoteEntry { blob_missing: false, path: path.to_string(), content_hash: hash.to_string() }
    }

    #[test]
    fn unchanged_both_sides_is_a_no_op() {
        let local = vec![row("a.org", Some("h1"), Some("h1"), false)];
        let remote = vec![remote("a.org", "h1")];
        assert_eq!(compute_diff(&local, &remote), vec![]);
    }

    #[test]
    fn new_local_file_uploads() {
        let local = vec![row("a.org", None, Some("h1"), false)];
        assert_eq!(compute_diff(&local, &[]), vec![SyncAction::Upload { path: "a.org".into() }]);
    }

    #[test]
    fn new_remote_file_never_seen_locally_downloads() {
        let remote = vec![remote("a.org", "h1")];
        assert_eq!(compute_diff(&[], &remote), vec![SyncAction::Download { path: "a.org".into() }]);
    }

    #[test]
    fn local_edit_uploads() {
        let local = vec![row("a.org", Some("h1"), Some("h2"), false)];
        let remote = vec![remote("a.org", "h1")];
        assert_eq!(compute_diff(&local, &remote), vec![SyncAction::Upload { path: "a.org".into() }]);
    }

    #[test]
    fn remote_edit_downloads() {
        let local = vec![row("a.org", Some("h1"), Some("h1"), false)];
        let remote = vec![remote("a.org", "h2")];
        assert_eq!(compute_diff(&local, &remote), vec![SyncAction::Download { path: "a.org".into() }]);
    }

    /// A file deleted locally must not come back just because the remote
    /// index still lists its old hash.
    #[test]
    fn local_deletion_of_synced_file_propagates_as_delete_remote_not_a_download() {
        let local = vec![row("a.org", Some("h1"), None, true)];
        let remote = vec![remote("a.org", "h1")];
        assert_eq!(compute_diff(&local, &remote), vec![SyncAction::DeleteRemote { path: "a.org".into() }]);
    }

    #[test]
    fn remote_deletion_of_synced_file_propagates_as_delete_local() {
        let local = vec![row("a.org", Some("h1"), Some("h1"), false)];
        assert_eq!(compute_diff(&local, &[]), vec![SyncAction::DeleteLocal { path: "a.org".into() }]);
    }

    #[test]
    fn local_deletion_racing_a_remote_edit_is_a_conflict() {
        let local = vec![row("a.org", Some("h1"), None, true)];
        let remote = vec![remote("a.org", "h2")];
        assert_eq!(
            compute_diff(&local, &remote),
            vec![SyncAction::Conflict { path: "a.org".into(), local_hash: None, remote_hash: Some("h2".into()) }]
        );
    }

    #[test]
    fn divergent_edits_on_both_sides_are_a_conflict() {
        let local = vec![row("a.org", Some("h1"), Some("h2"), false)];
        let remote = vec![remote("a.org", "h3")];
        assert_eq!(
            compute_diff(&local, &remote),
            vec![SyncAction::Conflict { path: "a.org".into(), local_hash: Some("h2".into()), remote_hash: Some("h3".into()) }]
        );
    }

    /// The mirror-image bug: a remote deletion must NOT silently destroy an
    /// unsynced local edit -- caught this as a real bug while building
    /// resolve_conflict (see orchestrator.rs's own test with the same name
    /// pattern), where the old logic produced DeleteLocal here instead.
    #[test]
    fn remote_deletion_racing_a_local_edit_is_a_conflict_not_an_auto_delete() {
        let local = vec![row("a.org", Some("h1"), Some("h2"), false)];
        assert_eq!(
            compute_diff(&local, &[]),
            vec![SyncAction::Conflict { path: "a.org".into(), local_hash: Some("h2".into()), remote_hash: None }]
        );
    }

    #[test]
    fn both_sides_converging_on_the_same_new_content_resolves_without_conflict() {
        let local = vec![row("a.org", Some("h1"), Some("h2"), false)];
        let remote = vec![remote("a.org", "h2")];
        assert_eq!(compute_diff(&local, &remote), vec![SyncAction::ResolveSame { path: "a.org".into() }]);
    }

    #[test]
    fn never_synced_file_deleted_before_first_sync_is_forgotten_not_uploaded() {
        let local = vec![row("a.org", None, None, true)];
        assert_eq!(compute_diff(&local, &[]), vec![SyncAction::Forget { path: "a.org".into() }]);
    }

    #[test]
    fn synced_file_deleted_on_both_sides_is_forgotten() {
        let local = vec![row("a.org", Some("h1"), None, true)];
        assert_eq!(compute_diff(&local, &[]), vec![SyncAction::Forget { path: "a.org".into() }]);
    }

    #[test]
    fn multiple_files_are_diffed_independently() {
        let local = vec![
            row("unchanged.org", Some("h1"), Some("h1"), false),
            row("edited.org", Some("h1"), Some("h2"), false),
        ];
        let remote = vec![remote("unchanged.org", "h1"), remote("edited.org", "h1"), remote("new-remote.org", "h9")];

        let mut actions = compute_diff(&local, &remote);
        actions.sort_by(|a, b| a.path().cmp(b.path()));

        assert_eq!(
            actions,
            vec![
                SyncAction::Upload { path: "edited.org".into() },
                SyncAction::Download { path: "new-remote.org".into() },
            ]
        );
    }
}
