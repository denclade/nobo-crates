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
    // Org's implicit file links include a basename without a slash. Fuzzy
    // heading links can produce false positives here; keeping an extra
    // attachment is preferable to collecting a real one.
    for (at, _) in text.match_indices("[[") {
        let rest = &text[at + 2..];
        if let Some(end) = rest.find(']') {
            add_target(owner, &rest[..end], &mut found);
        }
    }
    let mut rest = text;
    while let Some(at) = rest.find("file:") {
        rest = &rest[at + 5..];
        let prefix = &text[..text.len() - rest.len() - 5];
        let bracketed = prefix.ends_with("[[") || prefix.ends_with('<');
        let end =
            rest.find(|c: char| c == ']' || c == '>' || c == '\n' || c == '\r' || (!bracketed && c.is_whitespace())).unwrap_or(rest.len());
        add_target(owner, &format!("file:{}", &rest[..end]), &mut found);
        rest = &rest[end..];
    }
    // Inline Markdown links/images and reference definitions. Parse even
    // unused definitions and examples: this is a conservative liveness
    // scan, not a renderer. Balanced parentheses and backslash escapes are
    // valid in Markdown file destinations.
    for (at, _) in text.match_indices("](") {
        add_markdown_target(owner, &text[at + 2..], &mut found);
    }
    for (at, _) in text.match_indices("]:") {
        let line = text[..at].rsplit('\n').next().unwrap_or("").trim_start();
        if line.starts_with('[') {
            add_markdown_target(owner, &text[at + 2..], &mut found);
        }
    }
    found
}

fn add_markdown_target(owner: &Path, text: &str, found: &mut HashSet<PathBuf>) {
    let text = text.trim_start();
    if let Some(angled) = text.strip_prefix('<') {
        if let Some(end) = angled.find('>') {
            add_target(owner, &angled[..end], found);
        }
        return;
    }
    let mut depth = 0usize;
    let mut escaped = false;
    let mut end = text.len();
    for (at, ch) in text.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' => escaped = true,
            '(' => depth += 1,
            ')' if depth > 0 => depth -= 1,
            ')' => {
                end = at;
                break;
            }
            ch if ch.is_whitespace() => {
                end = at;
                break;
            }
            _ => {}
        }
    }
    let raw = &text[..end];
    add_target(owner, raw, found);
    // Keep the literal spelling too, for files with a real backslash.
    let mut unescaped = String::new();
    let mut chars = raw.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\' && chars.peek().is_some_and(char::is_ascii_punctuation) {
            unescaped.push(chars.next().unwrap());
        } else {
            unescaped.push(ch);
        }
    }
    add_target(owner, &unescaped, found);
}

fn add_target(owner: &Path, target: &str, found: &mut HashSet<PathBuf>) {
    let explicit_file = target.starts_with("file:");
    let target = target.strip_prefix("file:").unwrap_or(target);
    let target = target.split("::").next().unwrap_or("");
    // Ignore non-file schemes and Org internal/custom-ID links. Windows
    // drive letters are accepted on Windows by Path itself.
    if target.is_empty() || target.starts_with(['#', '*']) || (!explicit_file && target.contains(':') && !Path::new(target).is_absolute()) {
        return;
    }
    let mut spellings = vec![target.to_owned()];
    let mut bytes = Vec::new();
    let input = target.as_bytes();
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
        spellings.push(decoded);
    }
    // A Markdown fragment does not belong to the filename. Keeping both
    // spellings also protects a literal '#' in an Org file link.
    let without_fragments: Vec<_> = spellings.iter().filter_map(|s| s.split_once('#').map(|(path, _)| path.to_owned())).collect();
    spellings.extend(without_fragments);
    for spelling in spellings {
        let path = Path::new(&spelling);
        let path = if path.is_absolute() { path.to_path_buf() } else { owner.parent().unwrap_or(Path::new(".")).join(path) };
        found.insert(normalize(&path));
    }
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
                if [".org", ".org#", ".org_archive", ".org_archive#", ".md", ".md#", ".txt", ".txt#"].iter().any(|ext| name.ends_with(ext))
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

/// Source documents and application state never become owned attachments,
/// even if a configured folder happens to contain them.
fn protected_document(path: &Path) -> bool {
    let name = path.file_name().unwrap_or_default().to_string_lossy().to_lowercase();
    name.starts_with('.')
        || crate::utils::temp_file_utils::is_temp_file(&name)
        || [".org", ".org_archive", ".md", ".txt"].iter().any(|ext| name.ends_with(ext))
        || path.components().any(|c| c.as_os_str() == ".git")
        || path.components().any(|c| c.as_os_str() == ".nobo")
            && !path.parent().is_some_and(|p| p.components().any(|c| c.as_os_str() == ".attachments"))
}

fn configured_folder(root: &Path, configured: &Path, owner_dir: &Path) -> Option<PathBuf> {
    // '.' or '..' means a note directory, not a dedicated attachment
    // folder. In particular, resolving it against every nested directory
    // must not classify the entire vault as disposable attachments.
    if !matches!(configured.components().next_back(), Some(Component::Normal(_))) {
        return None;
    }
    let folder = normalize(&if configured.is_absolute() { configured.to_path_buf() } else { owner_dir.join(configured) });
    (folder.starts_with(root) && folder != root).then_some(folder)
}

fn configured_gtd_files(root: &Path) -> HashSet<PathBuf> {
    let Some(config) = crate::config::settings::Config::load() else { return HashSet::new() };
    if config.vault_root().ok().map(|p| crate::utils::fs_utils::canonicalize_or_self(&p)).as_deref() != Some(root) {
        return HashSet::new();
    }
    [config.inbox_file_path, config.actions_file_path, config.projects_file_path, config.someday_file_path]
        .iter()
        .filter(|p| !p.is_empty())
        .map(|p| {
            let p = Path::new(p);
            normalize(&if p.is_absolute() { p.to_path_buf() } else { root.join(p) })
        })
        .collect()
}

fn managed(root: &Path, configured: &str, owner: &Path, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(root) else { return false };
    if protected_document(path) || configured_gtd_files(root).contains(path) {
        return false;
    }
    let default = relative.parent().is_some_and(|p| p.components().any(|c| c.as_os_str() == ".attachments"));
    let configured = Path::new(configured);
    let owned = configured_folder(root, configured, owner.parent().unwrap_or(root)).is_some_and(|folder| path.starts_with(folder));
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
    let protected_gtd = configured_gtd_files(&root);
    while let Some(dir) = dirs.pop() {
        // Relative attachment paths resolve against a note's directory.
        // Checking every directory also finds attachments whose owning
        // note no longer exists, without depending on the index cache.
        if let Some(folder) = configured_folder(&root, configured, &dir) {
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
        if protected_document(&path)
            || protected_gtd.contains(&path)
            || live.contains(&path)
            || !path.ancestors().skip(1).any(|p| folders.contains(p))
        {
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
