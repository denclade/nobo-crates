// SPDX-FileCopyrightText: 2026 Dennis Burgermeister
// SPDX-License-Identifier: GPL-3.0-or-later

//! What the sync says to the frontend.
//!
//! Separate from the machinery on purpose: these types appear in command
//! signatures, so they exist in every build -- including one compiled
//! without the `sync` feature, where the commands are still there and
//! answer that they are not (see this feature's module doc).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "tauri", derive(specta::Type))]
pub struct ConflictInfo {
    pub path: String,
    /// Either field is `None` exactly when that side deleted the file
    /// while the other independently changed it.
    pub local_hash: Option<String>,
    pub remote_hash: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
#[cfg_attr(feature = "tauri", derive(specta::Type))]
pub struct SyncSummary {
    pub uploaded: u32,
    pub downloaded: u32,
    pub deleted_remote: u32,
    pub deleted_local: u32,
    /// A Download action whose index entry pointed at a blob that no
    /// longer exists remotely (see `is_missing_remotely_error`'s doc
    /// comment) -- dropped from this device's own tracking rather than
    /// aborting the whole sync. Surfaced separately from the counts above
    /// so the user can tell "everything's fine" apart from "something was
    /// silently given up on," even though no action is needed from them --
    /// the corrected index this run uploads is the actual fix, this is
    /// just visibility into that having happened.
    pub skipped_missing: u32,
    /// A destructive action left unapplied because the local file changed
    /// after this run's own rescan, so the diff no longer described it --
    /// see `local_matches_diff_snapshot`. Nothing is lost and nothing is
    /// needed from the user: the next run re-diffs against the new content
    /// and either uploads it or reports a real conflict.
    pub skipped_locally_changed: u32,
    /// A remote deletion that could not be applied locally -- a permission
    /// problem, a read-only mount, a file locked by another program.
    ///
    /// Its `sync_state` row is deliberately kept. Dropping it (which is
    /// what used to happen, because the delete's own error was discarded)
    /// made the next rescan see an untracked file, record it with
    /// `base_hash = NULL`, and UPLOAD it -- undoing the deletion on every
    /// device, from a run that had reported success. Unlike
    /// `skipped_locally_changed` this DOES need the user: nothing will fix
    /// it on its own.
    pub failed_local_deletes: u32,
    /// Needing a manual decision -- untouched on both sides, will show up
    /// as a Conflict again on the next sync until the user resolves it
    /// (see `resolve_conflict` and Module 6's UI).
    pub conflicts: Vec<ConflictInfo>,
    /// The server handed back no usable validator for the index, so this
    /// run's index write went out unconditional and the two-device
    /// lost-update protection was not in force (see
    /// `IndexVersion::Unversioned`).
    ///
    /// Reported rather than only logged: it is a property of the SERVER,
    /// not of this run, so it holds for every future sync against it too,
    /// and the user is the only one who can act on it (a different server,
    /// or one not behind an ETag-rewriting proxy). Nothing was lost here
    /// -- this says the net is missing, not that anyone fell.
    pub index_unprotected: bool,
}

/// What this feature keeps in `settings.json`, under its own key.
///
/// Its own section rather than fields in `Config` (see
/// `config::section`): both are answers about syncing, and a build
/// without it has neither to give.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "tauri", derive(specta::Type))]
#[serde(rename = "SyncSettings")]
pub struct Settings {
    /// Whether the startup restore may look at the OS keychain at all.
    /// The login material itself always lives there, never in this file,
    /// but gating the restore on the keychain entry alone means resetting
    /// settings does not turn sync off -- the entry survives
    /// independently and sync reactivates on the next launch. This file
    /// is what a reset wipes, so it holds the intent.
    #[serde(default)]
    pub remembered: bool,
    /// RFC 3339 timestamp of the last sync that completed without
    /// throwing (see src/features/sync/status.ts's `run`), shown on the
    /// sync settings page as "last synced ...". `None` if this account
    /// has never synced on this device. Here rather than in localStorage:
    /// everything else this app persists across restarts lives in
    /// settings.json, and localStorage is a separate WebView-managed
    /// store the backup/reset story knows nothing about.
    #[serde(default)]
    pub last_synced_at: Option<String>,
}
