// SPDX-FileCopyrightText: 2026 Dennis Burgermeister
// SPDX-License-Identifier: GPL-3.0-or-later

//! Attachment ownership and references. Only files in managed attachment
//! directories are collected; a normal file link never transfers ownership.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};

use super::indexer::IndexManager;
use super::models::FileSnapshot;
use super::models::HistoryEntryKind;
use super::trash::Policy;
use super::vault_write;
use crate::utils::ToCmdResult;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::sync::Mutex;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, specta::Type)]
pub struct OrphanedAttachment {
    pub path: String,
    #[specta(type = specta_typescript::Number)]
    pub size_bytes: u64,
    /// Identifies the exact bytes reviewed, so a replacement at the same
    /// path is not removed by an older preview.
    pub revision: String,
}

#[derive(Debug, Serialize, specta::Type)]
pub struct AttachmentCleanupResult {
    pub removed: u32,
    pub skipped: u32,
}

inventory::submit! {
    super::cost::Heavy {
        id: "orphaned-attachments",
        what: "walks attachment folders, checks all saved references and fingerprints unused files",
        scale: super::cost::Scale::Vault,
        expected: "on an explicit maintenance scan and again before its selected files are recycled",
    }
}

/// File targets, including Org search suffixes and percent-encoded paths.
/// Scan raw text deliberately: links in drawers, examples and drafts still
/// count as uses, even when the renderer does not display them as links.
pub fn references(owner: &Path, text: &str) -> HashSet<PathBuf> {
    let mut found = HashSet::new();
    // Org also accepts paths without the explicit file: scheme.
    for (_, rest) in text.match_indices("[[").map(|(at, marker)| (marker, &text[at + 2..])) {
        if let Some(end) = rest.find(']') {
            let target = &rest[..end];
            if !target.contains(':') && target.contains('/') {
                found.extend(references(owner, &format!("[[file:{target}]]")));
            }
        }
    }
    let mut rest = text;
    while let Some(at) = rest.find("file:") {
        rest = &rest[at + 5..];
        let bracketed = text[..text.len() - rest.len() - 5].ends_with("[[");
        let end =
            rest.find(|c: char| c == ']' || c == '>' || c == '\n' || c == '\r' || (!bracketed && c.is_whitespace())).unwrap_or(rest.len());
        let raw = rest[..end].split("::").next().unwrap_or("");
        if !raw.is_empty() {
            let mut bytes = Vec::new();
            let input = raw.as_bytes();
            let mut i = 0;
            while i < input.len() {
                if input[i] == b'%' && i + 2 < input.len() {
                    if let Some(byte) = std::str::from_utf8(&input[i + 1..i + 3]).ok().and_then(|s| u8::from_str_radix(s, 16).ok()) {
                        bytes.push(byte);
                        i += 3;
                        continue;
                    }
                }
                bytes.push(input[i]);
                i += 1;
            }
            if let Ok(decoded) = String::from_utf8(bytes) {
                let path = Path::new(&decoded);
                let path = if path.is_absolute() { path.to_path_buf() } else { owner.parent().unwrap_or(Path::new(".")).join(path) };
                found.insert(normalize(&path));
            }
        }
        rest = &rest[end..];
    }
    found
}

pub(crate) fn normalize(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                result.pop();
            }
            _ => result.push(part.as_os_str()),
        }
    }
    result
}

/// Includes hidden and ignored Org files, plus #file.org# drafts. Any walk
/// or read error aborts collection: an incomplete reference scan cannot
/// prove that a file is unused. Symlinks are followed only inside ROOT.
pub(crate) fn live_references(root: &Path) -> io::Result<HashSet<PathBuf>> {
    let root = fs::canonicalize(root)?;
    let mut dirs = vec![root.clone()];
    let mut visited = HashSet::new();
    let mut found = HashSet::new();
    while let Some(dir) = dirs.pop() {
        let dir = fs::canonicalize(dir)?;
        if !dir.starts_with(&root) || !visited.insert(dir.clone()) {
            continue;
        }
        for entry in fs::read_dir(&dir)? {
            let path = entry?.path();
            if path.file_name().is_some_and(|n| n == ".git") {
                continue;
            }
            let canonical = fs::canonicalize(&path)?;
            if !canonical.starts_with(&root) {
                continue;
            }
            let meta = fs::metadata(&path)?;
            if meta.is_dir() {
                dirs.push(path);
            } else if meta.is_file() {
                let name = path.file_name().unwrap_or_default().to_string_lossy().to_lowercase();
                if [".org", ".org#", ".org_archive", ".org_archive#", ".md", ".md#", ".txt", ".txt#"]
                    .iter()
                    .any(|ext| name.ends_with(ext))
                {
                    let text = fs::read_to_string(&path)?;
                    found.extend(references(&path, &text));
                }
            }
        }
    }
    // A reference via a symlink protects the real file as well.
    let aliases: Vec<_> = found.iter().filter_map(|p| fs::canonicalize(p).ok()).collect();
    found.extend(aliases);
    Ok(found)
}

fn managed(root: &Path, configured: &str, owner: &Path, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(root) else { return false };
    if relative.components().any(|c| c.as_os_str() == ".git") {
        return false;
    }
    let default = relative.parent().is_some_and(|p| p.components().any(|c| c.as_os_str() == ".attachments"));
    let configured = Path::new(configured);
    let folder = if configured.is_absolute() { configured.to_path_buf() } else { owner.parent().unwrap_or(root).join(configured) };
    let folder = normalize(&folder);
    let owned = folder.starts_with(root) && folder != root && path.starts_with(&folder);
    if !default && !owned {
        return false;
    }
    // Do not unlink symlinks or anything reached through a symlink. This
    // also rules out an attachment directory redirected outside the vault.
    let mut cursor = root.to_path_buf();
    for component in relative.components() {
        cursor.push(component);
        if fs::symlink_metadata(&cursor).is_ok_and(|m| m.file_type().is_symlink()) {
            return false;
        }
    }
    path.is_file()
}

pub(crate) fn orphaned(root: &Path, configured: &str, changes: &[FileSnapshot]) -> io::Result<Vec<PathBuf>> {
    let mut candidates = HashSet::new();
    for change in changes {
        let owner = Path::new(&change.path);
        let before = references(owner, change.before.as_deref().unwrap_or(""));
        let after = references(owner, change.after.as_deref().unwrap_or(""));
        candidates.extend(before.difference(&after).filter(|p| managed(root, configured, owner, p)).cloned());
    }
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    let live = live_references(root)?;
    let mut result: Vec<_> = candidates.difference(&live).cloned().collect();
    result.sort();
    Ok(result)
}

/// Every unused file in the default or currently configured attachment
/// folders, including folders left behind after their note was deleted.
/// Hidden/ignored notes, archives and persisted drafts still protect files.
pub fn find_orphaned(policy: &Policy) -> io::Result<Vec<OrphanedAttachment>> {
    let _cost = super::cost::timed("orphaned-attachments");
    let root = fs::canonicalize(&policy.root)?;
    let live = live_references(&root)?;
    let mut dirs = vec![root.clone()];
    let mut folders = HashSet::new();
    let mut files = Vec::new();
    let configured = Path::new(&policy.attachments_dir);
    while let Some(dir) = dirs.pop() {
        // Relative attachment paths resolve against a note's directory.
        // Checking every directory also finds attachments whose owning
        // note no longer exists, without depending on the index cache.
        let folder = normalize(&if configured.is_absolute() { configured.to_path_buf() } else { dir.join(configured) });
        if folder.starts_with(&root) && folder != root {
            folders.insert(folder);
        }
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if entry.file_name() == ".git" {
                continue;
            }
            let kind = entry.file_type()?;
            // Candidate files must never cross a symlink, even one whose
            // destination happens to be inside the vault today.
            if kind.is_symlink() {
                continue;
            }
            if kind.is_dir() {
                if entry.file_name() == ".attachments" {
                    folders.insert(path.clone());
                }
                dirs.push(path);
            } else if kind.is_file() && !crate::utils::temp_file_utils::is_temp_file(&entry.file_name().to_string_lossy()) {
                files.push(path);
            }
        }
    }
    let mut result = Vec::new();
    for path in files {
        if live.contains(&path) || !path.ancestors().skip(1).any(|p| folders.contains(p)) {
            continue;
        }
        let before = fs::metadata(&path)?;
        let mut file = fs::File::open(&path)?;
        let mut hasher = blake3::Hasher::new();
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let count = file.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            hasher.update(&buffer[..count]);
        }
        let after = fs::metadata(&path)?;
        if before.len() != after.len() || before.modified()? != after.modified()? {
            return Err(io::Error::other(format!("{} changed during the attachment scan", path.display())));
        }
        result.push(OrphanedAttachment {
            path: path.to_string_lossy().into_owned(),
            size_bytes: after.len(),
            revision: hasher.finalize().to_hex().to_string(),
        });
    }
    result.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(result)
}

/// Rechecks both references and content before deleting reviewed files.
/// Newly linked/replaced files and paths outside managed folders are
/// skipped. Each removed file gets its own independently restorable entry.
pub fn recycle_orphaned(
    policy: &Policy,
    selected: &[OrphanedAttachment],
    index: &IndexManager,
    db: &Mutex<Connection>,
) -> Result<AttachmentCleanupResult, String> {
    vault_write::with_vault_lock(&policy.root, || {
        let current: HashMap<_, _> = find_orphaned(policy).to_cmd()?.into_iter().map(|f| (f.path.clone(), f)).collect();
        let mut result = AttachmentCleanupResult { removed: 0, skipped: 0 };
        let mut seen = HashSet::new();
        for candidate in selected {
            if !seen.insert(&candidate.path) {
                continue;
            }
            if current.get(&candidate.path) != Some(candidate) {
                result.skipped += 1;
                continue;
            }
            let path = Path::new(&candidate.path);
            super::trash::retain_attachment_for_deleted_entries(policy, path, db)?;
            vault_write::recorded_with_policy(
                path,
                Some(policy.clone()),
                |_| HistoryEntryKind::FileDeleted { filename: crate::utils::fs_utils::filename_of(path) },
                index,
                db,
                || fs::remove_file(path).to_cmd(),
            )?;
            result.removed += 1;
        }
        Ok(result)
    })
}

#[cfg(test)]
mod tests;
