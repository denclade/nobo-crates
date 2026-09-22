use crate::core::models::{DirEntry, FileEntry, Note, Heading};
use crate::core::org_parser::extract_headings_from_file;
use crate::utils::fs_utils;
use crate::utils::note_utils::parse_note_record;
use crate::core::cost::{Heavy, Scale};
use notify::{Config as NotifyConfig, Event, RecommendedWatcher, RecursiveMode, Watcher};
use rayon::prelude::*;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant};

/// Whoever wants to hear that the index changed.
///
/// The index is told about files by the scan, the watcher and every
/// command that writes; what it does not know is who is looking at it.
/// Under Tauri that is the web view, reached by an `index-updated`
/// event; under a Qt host (harbour-nobo) it is a signal on the main
/// thread. One trait, so the core says "changed" and never "emit".
///
/// Called from whichever thread noticed the change -- the watcher's
/// burst thread, the startup scan, a command's async worker -- so an
/// implementation has to be safe to call from anywhere and must not
/// block: a listener that waits for the UI would stall the watcher.
pub trait IndexListener: Send + Sync {
    fn index_updated(&self);
}

/// The event name a Tauri host emits for [`IndexListener::index_updated`].
///
/// Here rather than in each host crate, although the core emits nothing
/// itself: the string is a contract with the frontend (`listen(...)` in
/// `useIndexUpdatedRefetch.ts` and three more), and several crates carry
/// their own listener for their own Tauri commands. One `const` is what
/// keeps them from drifting apart -- a renamed event would otherwise go
/// unnoticed in whichever copy was missed until the UI quietly stopped
/// refreshing.
pub const INDEX_UPDATED_EVENT: &str = "index-updated";

// What the index does that is worth counting (see core::cost). The
// sentences are the point: a number is only readable next to what it
// should have been.
inventory::submit! {
    Heavy {
        id: "vault-scan",
        what: "walks the vault and reads and parses every org file in it",
        scale: Scale::Vault,
        expected: "once at startup, and again only when the vault path itself changes",
    }
}

inventory::submit! {
    Heavy {
        id: "file-index",
        what: "reads and parses one file back into the index",
        scale: Scale::File,
        expected: "once per file this app writes, plus once per burst of changes from outside",
    }
}

inventory::submit! {
    Heavy {
        id: "fulltext-search",
        what: "scans the cached body of every org file for the query",
        scale: Scale::Vault,
        expected: "once per search, and the frontend debounces typing",
    }
}

#[derive(Default, Clone)]
pub struct AppIndex {
    /// Every file and directory under the vault, key is the file_path
    pub files: HashMap<String, FileEntry>,
    /// Contains all note files, note files need to be in the denote format
    pub notes: HashMap<String, Note>,
    /// Caches full content of all `.org` files for fast, memory-based full-text search.
    /// Kept separate from `Note` to avoid sending massive payloads to the frontend.
    pub org_file_bodies: HashMap<String, String>,
    pub headings: HashMap<String, Vec<Heading>>,
    pub tag_counts: HashMap<String, usize>,
}

impl AppIndex {
    /// How many NOTES carry each tag.
    ///
    /// Per note, not per occurrence: a tag repeated in one filename is
    /// still one note using it, and `apply_tag_delta` below has to agree
    /// with this exactly or the two paths drift.
    pub fn rebuild_tag_counts(&mut self) {
        let mut counts = HashMap::new();
        for note in self.notes.values() {
            for tag in note.tags.iter().collect::<HashSet<_>>() {
                *counts.entry(tag.clone()).or_insert(0) += 1;
            }
        }
        self.tag_counts = counts;
    }

    /// Adjusts the counts for one note changing, instead of recounting the
    /// whole vault.
    ///
    /// `update` and `remove_file` called `rebuild_tag_counts` -- a full
    /// pass over every note, with a `String` allocation per tag, inside
    /// the write lock the whole design exists to keep short. A sync pull
    /// touching 500 files paid for 500 full recounts, and `remove_file`
    /// paid even for files that were never notes.
    ///
    /// A key that falls to zero is REMOVED, not left at zero:
    /// `tag_utils::search` does not filter on count, so a leftover entry
    /// would keep offering a tag nobody uses any more, forever.
    fn apply_tag_delta(&mut self, before: Option<&Note>, after: Option<&Note>) {
        let tags_of = |note: Option<&Note>| -> HashSet<String> {
            note.map(|n| n.tags.iter().cloned().collect()).unwrap_or_default()
        };
        let old = tags_of(before);
        let new = tags_of(after);

        for tag in old.difference(&new) {
            if let Some(count) = self.tag_counts.get_mut(tag) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    self.tag_counts.remove(tag);
                }
            }
        }
        for tag in new.difference(&old) {
            *self.tag_counts.entry(tag.clone()).or_insert(0) += 1;
        }
    }
}

pub struct IndexManager {
    pub index: Arc<RwLock<AppIndex>>,
    /// The current vault root, for computing relative paths and hidden
    /// states during incremental watcher updates. Multiple vaults are not
    /// supported.
    root: RwLock<Option<PathBuf>>,
    /// The live filesystem watcher, held only so switching vaults can stop
    /// it -- see `start_watcher`.
    watcher: Mutex<Option<RecommendedWatcher>>,
    /// Paths changed while a `scan_directory` is walking the disk.
    ///
    /// `Some` exactly while a scan is in flight. The scan builds a fresh
    /// `AppIndex` from a snapshot and swaps it in wholesale, so without
    /// this everything `update` inserted meanwhile was lost -- and, worse,
    /// everything `remove_file` deleted came BACK, because the walk had
    /// seen those files. A deleted note reappearing in the list, the
    /// search and the agenda.
    ///
    /// Both production callers run alongside live commands: the startup
    /// scan is a detached thread and `start_watcher` only runs after it,
    /// and the post-sync rescan runs after every sync that changed
    /// anything. On mobile there is no watcher to put it right later.
    scan_journal: Mutex<Option<Vec<(PathBuf, bool)>>>,
}

impl IndexManager {
    pub fn new() -> Self {
        Self {
            index: Arc::new(RwLock::new(AppIndex::default())),
            root: RwLock::new(None),
            watcher: Mutex::new(None),
            scan_journal: Mutex::new(None),
        }
    }

    /// Performs a single recursive disk walk to build the entire in-memory index.
    /// Subsequent directory listings are served from RAM; only watcher updates touch the disk.
    pub fn scan_directory(&self, dir: &Path) {
        let _cost = crate::core::cost::timed("vault-scan");
        let dir = &fs_utils::canonicalize_or_self(dir);

        if let Ok(mut root) = self.root.write() {
            *root = Some(dir.to_path_buf());
        }

        // Armed before the walk: from here until the swap below, every
        // `update` and `remove_file` also records what it did, so the
        // fresh index can be brought up to date instead of silently
        // undoing them. See `scan_journal`.
        if let Ok(mut journal) = self.scan_journal.lock() {
            *journal = Some(Vec::new());
        }

        let entries = fs_utils::get_file_entries_recursive(dir);

        let results: Vec<(FileEntry, Option<Note>, Vec<Heading>, Option<String>)> = entries
            .into_par_iter()
            .map(|file_entry| {
                if is_org_file(&file_entry) {
                    let path = PathBuf::from(&file_entry.path);
                    let content = std::fs::read_to_string(&path).unwrap_or_default();
                    let note = parse_note_record(&path);
                    let headings = crate::core::org_parser::parse_headings_from_lines(&path, content.lines());
                    (file_entry, note, headings, Some(content))
                } else {
                    (file_entry, None, Vec::new(), None)
                }
            })
            .collect();

        let mut new_index = AppIndex::default();

        for (file_entry, note_opt, headings, content) in results {
            let path_str = file_entry.path.clone();
            new_index.files.insert(path_str.clone(), file_entry);

            if !headings.is_empty() {
                new_index.headings.insert(path_str.clone(), headings);
            }
            if let Some(content) = &content {
                new_index.org_file_bodies.insert(path_str.clone(), content.clone());
            }
            if let Some(note) = note_opt {
                new_index.notes.insert(path_str, note);
            }
        }

        new_index.rebuild_tag_counts();

        if let Ok(mut write_lock) = self.index.write() {
            *write_lock = new_index;
            log::info!("Index initialized: {} files, {} notes, {} heading files",
                     write_lock.files.len(), write_lock.notes.len(), write_lock.headings.len());
        }

        // Replayed against the file system as it is NOW, which is the
        // point: a file the walk saw and something deleted since must not
        // come back, and a file written since must not be missing.
        let journal = self.scan_journal.lock().ok().and_then(|mut j| j.take()).unwrap_or_default();
        for (path, written) in journal {
            if written {
                self.update(&path, None);
            } else {
                self.remove_file(&path);
            }
        }
    }

    /// Records a change for a scan in flight to replay, if there is one.
    fn journal(&self, path: &Path, written: bool) {
        if let Ok(mut journal) = self.scan_journal.lock() {
            if let Some(entries) = journal.as_mut() {
                entries.push((path.to_path_buf(), written));
            }
        }
    }

    /// Stops the previous filesystem watcher and keeps the new one alive.
    ///
    /// Dropping a `notify` watcher closes its sender, so the thread parked
    /// in `rx.recv()` gets an `Err` and ends by itself -- there is no
    /// `select` on a `std::sync::mpsc` channel to build a shutdown signal
    /// out of. Without this a vault switch left the old watcher running
    /// and writing the OLD vault's files into the index (`build_file_entry`
    /// does not check root membership), so its tasks turned up in
    /// `get_all_actions` and a running timer could resolve to a file
    /// outside the vault -- plus a thread and a recursive inotify tree per
    /// switch.
    fn replace_watcher(&self, watcher: RecommendedWatcher) {
        if let Ok(mut slot) = self.watcher.lock() {
            *slot = Some(watcher);
        }
    }

    /// Incrementally updates the index for a single created or modified path.
    /// Called from the file system watcher
    pub fn update_file(&self, path: &Path) {
        self.update(path, None);
    }

    /// The same for a caller that has just written the file and therefore
    /// already knows what is in it.
    ///
    /// Saves the read -- which is not why it matters. The read happens
    /// while every reader of the index is waiting (see `update`), and a
    /// command that writes a file always has the content in hand.
    pub fn update_file_with(&self, path: &Path, content: &str) {
        self.update(path, Some(content));
    }

    /// Reads and parses FIRST, takes the lock LAST.
    ///
    /// Costs, on a 300-file vault (2 MB): one `update_file` is 272 µs, of
    /// which the read is about 29 µs. A `scan_directory` over the same
    /// vault is 13 ms -- which is why whoever knows what changed says so
    /// (see `notify_files_changed`).
    ///
    /// Everything in here used to happen inside the write lock: the read,
    /// the parse, the filename analysis. While that ran, every reader
    /// blocked -- the search, the notes list, `Vault::headings`, every
    /// feature asking for its own files. Parsing is the expensive part of
    /// this app (131 ms for a 3 MB file, against 0.2 ms for the read), so
    /// that was the whole latency spike, handed to everyone at once. The
    /// lock now holds only the inserts.
    fn update(&self, path: &Path, written: Option<&str>) {
        let _cost = crate::core::cost::timed("file-index");
        let path = &fs_utils::canonicalize_or_self(path);
        let root = self.root.read().ok().and_then(|r| r.clone());
        let Some(file_entry) = fs_utils::build_file_entry(path, root.as_deref()) else { return };
        if crate::utils::temp_file_utils::is_temp_file(&file_entry.name) {
            return;
        }

        self.journal(path, true);

        let path_str = file_entry.path.clone();
        let parsed = is_org_file(&file_entry).then(|| {
            let content = match written {
                Some(content) => content.to_string(),
                None => std::fs::read_to_string(path).unwrap_or_default(),
            };
            let headings = crate::core::org_parser::parse_headings_from_lines(path, content.lines());
            (content, headings, parse_note_record(path))
        });

        if let Ok(mut write_lock) = self.index.write() {
            write_lock.files.insert(path_str.clone(), file_entry);

            if let Some((content, headings, note)) = parsed {
                if !headings.is_empty() {
                    write_lock.headings.insert(path_str.clone(), headings);
                } else {
                    write_lock.headings.remove(&path_str);
                }

                write_lock.org_file_bodies.insert(path_str.clone(), content);
                let previous = match note {
                    Some(note) => write_lock.notes.insert(path_str.clone(), note),
                    None => write_lock.notes.remove(&path_str),
                };
                let current = write_lock.notes.get(&path_str).cloned();
                write_lock.apply_tag_delta(previous.as_ref(), current.as_ref());
            }

            log::debug!("Index updated for: {}", path.display());
        }
    }

    pub fn get_headings(&self, path: &Path) -> Option<Vec<Heading>> {
        let path = &fs_utils::canonicalize_or_self(path);
        let guard = self.index.read().ok()?;
        let path_str = path.to_string_lossy().to_string();

        if let Some(headings) = guard.headings.get(&path_str) {
            return Some(headings.clone());
        }

        guard.headings.iter()
            .find(|(k, _)| PathBuf::from(*k) == *path)
            .map(|(_, h)| h.clone())
    }

    pub fn get_headings_or_parse(&self, path: &Path) -> Vec<Heading> {
        self.get_headings(path).unwrap_or_else(|| {
            if path.exists() { extract_headings_from_file(path) } else { Vec::new() }
        })
    }

    /// Filter first, clone second.
    ///
    /// `flat_map(|v| v.clone())` cloned every heading in the vault and
    /// filtered afterwards -- measured at 10k notes / 60k headings: 145 ms
    /// and 335 MB per call, all of it under the read lock, which blocks
    /// every `update` and `remove_file` for the duration. And the frontend
    /// calls this out of every mutation (`headingStore`'s `refetch`, from
    /// a dozen places), undebounced.
    ///
    /// `.flatten().filter().cloned()` clones what is actually returned.
    pub fn all_headings_with_state(&self) -> Vec<Heading> {
        let guard = match self.index.read() { Ok(i) => i, Err(_) => return Vec::new() };
        guard.headings.values().flatten().filter(|h| h.state.is_some()).cloned().collect()
    }

    /// Returns all headings across the vault, including those without a `TODO` state
    /// (e.g., untriaged Inbox items).
    pub fn all_headings(&self) -> Vec<Heading> {
        let guard = match self.index.read() { Ok(i) => i, Err(_) => return Vec::new() };
        guard.headings.values().flatten().cloned().collect()
    }

    /// Returns all headings grouped by file.
    pub fn headings_by_file(&self) -> HashMap<String, Vec<Heading>> {
        match self.index.read() {
            Ok(guard) => guard.headings.clone(),
            Err(_) => HashMap::new(),
        }
    }

    /// The indexed org files whose text KEEP accepts, as copies.
    ///
    /// The predicate runs under the read lock, so only what matched is
    /// copied -- asking for "the cookbooks" costs the cookbooks, not the
    /// vault. It sees the text and nothing else on purpose: anything that
    /// reads the index again from in here would take the same lock a
    /// second time (see `Vault`, the only caller).
    pub fn org_file_bodies_where(&self, keep: impl Fn(&str) -> bool) -> Vec<(String, String)> {
        match self.index.read() {
            Ok(guard) => guard
                .org_file_bodies
                .iter()
                .filter(|(_, text)| keep(text))
                .map(|(path, text)| (path.clone(), text.clone()))
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Scans the index to find the active running clock (an open `CLOCK: [start]` entry
    /// without an end time). Resolves directly from the data source to ensure consistency.
    /// TODO: If everything runs in the app there SHOULD be only one clock, but from
    /// file perspective there could be any number of clocks. Maybe add a maintenance
    /// command to find multiple open clocks and close them? Notify the user about that?
    /// Which heading currently has an open CLOCK entry, as a file path and
    /// a target that survives the file shifting (see
    /// heading_identity::target_of) -- the cache is fine for FINDING it,
    /// but whoever writes to it afterwards has to re-resolve.
    pub fn find_active_clock(&self) -> Option<(String, crate::core::models::HeadingTarget)> {
        let guard = self.index.read().ok()?;
        for (path, headings) in guard.headings.iter() {
            for heading in headings {
                let has_open_clock = heading.clocks.iter().any(|line| {
                    crate::utils::clock_utils::parse_clock_line(line)
                        .map(|entry| entry.end.is_none())
                        .unwrap_or(false)
                });
                if has_open_clock {
                    return Some((path.clone(), crate::core::heading_identity::target_of(heading)));
                }
            }
        }
        None
    }

    pub fn all_notes(&self) -> Vec<Note> {
        match self.index.read() {
            Ok(index) => index.notes.values().cloned().collect(),
            Err(_) => Vec::new(),
        }
    }

    pub fn get_note(&self, path_str: &str) -> Option<Note> {
        self.index.read().ok()?.notes.get(path_str).cloned()
    }

    pub fn get_note_by_identifier(&self, identifier: &str) -> Option<Note> {
        self.index.read().ok()?.notes.values().find(|n| n.identifier == identifier).cloned()
    }

    /// Removes a file from the index. Since the file is already deleted from disk, it canonicalizes
    /// the parent directory and re-appends the filename to ensure path matching works.
    pub fn remove_file(&self, path: &Path) {
        let path = &match (path.parent(), path.file_name()) {
            (Some(parent), Some(name)) => fs_utils::canonicalize_or_self(parent).join(name),
            _ => path.to_path_buf(),
        };
        self.journal(path, false);

        let path_str = path.to_string_lossy().into_owned();
        if let Ok(mut write_lock) = self.index.write() {
            write_lock.files.remove(&path_str);
            let removed = write_lock.notes.remove(&path_str);
            write_lock.org_file_bodies.remove(&path_str);
            write_lock.headings.remove(&path_str);
            // Only when a note actually went: this used to recount every
            // tag in the vault even for files that were never notes.
            if removed.is_some() {
                write_lock.apply_tag_delta(removed.as_ref(), None);
            }
            log::debug!("File removed from index: {}", path_str);
        }
    }

    /// The `.org` files under BASE_DIR that are not Denote notes, read
    /// off the index instead of the disk.
    ///
    /// `note_utils::find_unpromoted_org_files` answers the same question
    /// authoritatively, and is what the promote dialog uses. This one is
    /// for the two search commands, which asked it on every keystroke:
    /// there it walked the whole vault AND read every unpromoted file to
    /// find a `#+TITLE:`, on the main thread, for a list the index
    /// already has -- `files` holds every file, `notes` only the ones
    /// with a Denote identity, and `org_file_bodies` holds the text the
    /// title comes from.
    ///
    /// Being index-backed also makes it consistent with the notes it is
    /// merged with: both are as current as the last scan, rather than one
    /// of them being a moment fresher than the other.
    pub fn unpromoted_org_files(&self, base_dir: &str) -> Vec<crate::core::models::UnpromotedOrgFile> {
        let base_dir = fs_utils::canonicalize_or_self(Path::new(base_dir)).to_string_lossy().into_owned();
        let guard = match self.index.read() { Ok(i) => i, Err(_) => return Vec::new() };
        let mut files: Vec<crate::core::models::UnpromotedOrgFile> = guard
            .files
            .values()
            .filter(|file| is_org_file(file) && file.path.starts_with(&base_dir))
            .filter(|file| !guard.notes.contains_key(&file.path))
            .map(|file| crate::core::models::UnpromotedOrgFile {
                title: guard
                    .org_file_bodies
                    .get(&file.path)
                    .and_then(|body| crate::utils::note_utils::title_from_text(body))
                    .unwrap_or_else(|| file.name.trim_end_matches(".org").to_string()),
                path: file.path.clone(),
            })
            .collect();
        files.sort_by(|a, b| a.path.cmp(&b.path));
        files
    }

    /// In-memory full-text search over all `.org` files. `extra_notes` (e.g. unpromoted notes)
    pub fn search_notes_fulltext(&self, base_dir: &str, query: &str, extra_notes: &[Note]) -> Vec<crate::core::models::FullTextSearchResult> {
        let _cost = crate::core::cost::timed("fulltext-search");
        let base_dir = fs_utils::canonicalize_or_self(Path::new(base_dir)).to_string_lossy().into_owned();
        let guard = match self.index.read() { Ok(i) => i, Err(_) => return Vec::new() };
        // Chained, not collected: the vault's notes are borrowed straight
        // out of the index for the duration of the search rather than
        // cloned into a Vec first (which was hundreds of MB on a large
        // vault, per keystroke).
        let notes = guard.notes.values().chain(extra_notes.iter());
        crate::utils::search_utils::search_notes_fulltext(notes, &guard.org_file_bodies, &base_dir, query)
    }

    fn create_dir_entry(file: &FileEntry, notes: &HashMap<String, Note>, bodies: &HashMap<String, String>) -> DirEntry {
        DirEntry {
            path: file.path.clone(),
            name: file.name.clone(),
            is_dir: file.is_dir,
            note: if file.is_dir { None } else { notes.get(&file.path).cloned() },
            // Read off the body the index already holds -- the declaration
            // lives in the file (see core::favorites), and re-reading every
            // file to render a sidebar would defeat the point of the cache.
            favorite: bodies
                .get(&file.path)
                .is_some_and(|text| crate::core::favorites::is_favorite_text(text)),
        }
    }

    /// Returns direct children of `dir` from the in-memory index.
    /// Hidden files/folders are filtered out unless `show_hidden` is true.
    pub fn directory_contents(&self, dir: &Path, show_hidden: bool) -> Vec<DirEntry> {
        let dir = &fs_utils::canonicalize_or_self(dir);
        let guard = match self.index.read() { Ok(i) => i, Err(_) => return Vec::new() };
        guard.files.values()
            .filter(|f| Path::new(&f.path).parent() == Some(dir.as_path()))
            .filter(|f| show_hidden || (!f.is_hidden && !f.is_in_hidden_dir))
            .map(|f| Self::create_dir_entry(f, &guard.notes, &guard.org_file_bodies))
            .collect()
    }

    /// Returns all nested descendants of `dir` from the in-memory index.
    /// Hidden files/folders are filtered out unless `show_hidden` is true.
    pub fn directory_contents_recursive(&self, dir: &Path, show_hidden: bool) -> Vec<DirEntry> {
        let dir = &fs_utils::canonicalize_or_self(dir);
        let guard = match self.index.read() { Ok(i) => i, Err(_) => return Vec::new() };
        guard.files.values()
            .filter(|f| Path::new(&f.path) != dir.as_path() && Path::new(&f.path).starts_with(dir))
            .filter(|f| show_hidden || (!f.is_hidden && !f.is_in_hidden_dir))
            .map(|f| Self::create_dir_entry(f, &guard.notes, &guard.org_file_bodies))
            .collect()
    }

    /// Every favourited file in the vault, newest note first (see
    /// core::favorites for where the mark lives).
    pub fn favorite_files(&self) -> Vec<DirEntry> {
        let guard = match self.index.read() { Ok(i) => i, Err(_) => return Vec::new() };
        let mut entries: Vec<DirEntry> = guard.files.values()
            .filter(|f| !f.is_dir)
            .map(|f| Self::create_dir_entry(f, &guard.notes, &guard.org_file_bodies))
            .filter(|entry| entry.favorite)
            .collect();
        // Same order the browser's default sort uses, so the section does
        // not present the same notes in a different sequence than the list
        // right below it.
        entries.sort_by(|a, b| match (a.note.as_ref(), b.note.as_ref()) {
            (Some(x), Some(y)) => y.identifier.cmp(&x.identifier),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => a.name.cmp(&b.name),
        });
        entries
    }

    /// Returns all files under `base` as relative `/`-separated paths.
    pub fn files_relative_to(&self, base: &Path) -> Vec<String> {
        let base = &fs_utils::canonicalize_or_self(base);
        let guard = match self.index.read() { Ok(i) => i, Err(_) => return Vec::new() };
        guard.files.values()
            .filter(|f| !f.is_dir)
            .filter_map(|f| {
                let rel = Path::new(&f.path).strip_prefix(base).ok()?;
                Some(rel.components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/"))
            })
            .collect()
    }

    /// Checks if `dir` matches the currently watched root.
    fn is_current_root(&self, dir: &Path) -> bool {
        let canonical = fs_utils::canonicalize_or_self(dir);
        self.root.read().ok()
            .and_then(|r| r.clone())
            .is_some_and(|root| root == canonical)
    }

    /// Performs a full rescan and starts the filesystem watcher for `dir`.
    ///
    /// Skips execution if `dir` is already the current root. This prevents
    /// redundant full-vault rescans and watcher thread leaks when saving
    /// unrelated config changes (e.g., theme toggles).
    ///
    /// Tells `listener` once the scan is done, so the UI immediately
    /// reflects the new vault contents, and again after every burst the
    /// watcher sees from then on.
    pub fn reindex_and_watch(self: &Arc<Self>, listener: Arc<dyn IndexListener>, dir: &Path) {
        if self.is_current_root(dir) {
            return;
        }
        let canonical = fs_utils::canonicalize_or_self(dir);
        self.scan_directory(&canonical);
        listener.index_updated();
        start_watcher(listener, self.clone(), canonical);
    }
}

fn is_org_file(file: &FileEntry) -> bool {
    !file.is_dir && file.name.ends_with(".org")
}

/* --------- Test -------- */

#[cfg(test)]
mod watcher_bursts {
    use super::*;
    use notify::event::{CreateKind, EventKind, ModifyKind, RemoveKind};

    fn event(kind: EventKind, paths: &[&str]) -> notify::Result<Event> {
        Ok(Event { kind, paths: paths.iter().map(PathBuf::from).collect(), attrs: Default::default() })
    }

    fn written(paths: &[&str]) -> notify::Result<Event> {
        event(EventKind::Modify(ModifyKind::Any), paths)
    }

    /// The reason this exists: one `fs::write` is several events, and the
    /// app writes the file it is about to index itself. Five events, one
    /// reparse.
    #[test]
    fn one_file_written_many_times_is_indexed_once() {
        let mut burst = Burst::default();
        for _ in 0..5 {
            burst.add(written(&["/vault/notiz.org"]));
        }

        assert_eq!(burst.changes.len(), 1);
        assert_eq!(burst.changes[Path::new("/vault/notiz.org")], Change::Written);
    }

    /// Last one wins, and here it has to: reparsing a file that was
    /// deleted a moment later would read a file that is not there.
    #[test]
    fn a_file_written_and_then_deleted_is_a_deletion() {
        let mut burst = Burst::default();
        burst.add(written(&["/vault/weg.org"]));
        burst.add(event(EventKind::Remove(RemoveKind::File), &["/vault/weg.org"]));

        assert_eq!(burst.changes[Path::new("/vault/weg.org")], Change::Gone);
    }

    /// And the other way round -- a rename arrives as remove + create.
    #[test]
    fn a_file_deleted_and_then_written_is_a_change() {
        let mut burst = Burst::default();
        burst.add(event(EventKind::Remove(RemoveKind::File), &["/vault/neu.org"]));
        burst.add(event(EventKind::Create(CreateKind::File), &["/vault/neu.org"]));

        assert_eq!(burst.changes[Path::new("/vault/neu.org")], Change::Written);
    }

    /// A sync writing many files is still one round through the frontend,
    /// but every file is indexed.
    #[test]
    fn many_files_in_one_burst_stay_many_files() {
        let mut burst = Burst::default();
        burst.add(written(&["/vault/a.org", "/vault/b.org"]));
        burst.add(written(&["/vault/c.org"]));

        assert_eq!(burst.changes.len(), 3);
    }

    /// Opening a file changes nothing, and must not wake the app up.
    #[test]
    fn an_event_that_changed_nothing_is_not_a_change() {
        let mut burst = Burst::default();
        burst.add(event(EventKind::Access(notify::event::AccessKind::Read), &["/vault/gelesen.org"]));

        assert!(burst.is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_file(dir: &Path, name: &str, content: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, content).unwrap();
        path
    }

    /* --- tag counts (see AppIndex::apply_tag_delta) --- */

    fn note_file(dir: &Path, id: &str, title: &str, tags: &[&str]) -> PathBuf {
        let name = if tags.is_empty() {
            format!("{id}--{title}.org")
        } else {
            format!("{id}--{title}__{}.org", tags.join("_"))
        };
        write_file(dir, &name, "#+TITLE: x\n")
    }

    #[test]
    fn adding_and_changing_a_note_keeps_the_tag_counts_exact() {
        let dir = TempDir::new().unwrap();
        let manager = IndexManager::new();
        note_file(dir.path(), "20260101T000000", "eins", &["rust", "org"]);
        note_file(dir.path(), "20260101T000001", "zwei", &["rust"]);
        manager.scan_directory(dir.path());

        let counts = |m: &IndexManager| m.index.read().unwrap().tag_counts.clone();
        assert_eq!(counts(&manager).get("rust"), Some(&2));
        assert_eq!(counts(&manager).get("org"), Some(&1));

        // The incremental path has to land on exactly what a full recount
        // would produce.
        let renamed = note_file(dir.path(), "20260101T000002", "drei", &["org", "notizen"]);
        manager.update_file(&renamed);

        let incremental = counts(&manager);
        let mut recounted = manager.index.read().unwrap().clone();
        recounted.rebuild_tag_counts();
        assert_eq!(incremental, recounted.tag_counts);
    }

    /* --- unpromoted files (see IndexManager::unpromoted_org_files) --- */

    /// The index-backed listing has to answer what the disk walk answers,
    /// because the search page merges the two lists into one result set.
    #[test]
    fn the_index_lists_the_same_unpromoted_files_the_disk_walk_finds() {
        let dir = TempDir::new().unwrap();
        let manager = IndexManager::new();
        note_file(dir.path(), "20260101T000000", "eine-notiz", &[]);
        write_file(dir.path(), "lose.org", "#+TITLE: Lose Datei\n* Etwas\n");
        write_file(dir.path(), "ohne-titel.org", "* Nur eine Überschrift\n");
        write_file(dir.path(), "keine-org-datei.md", "# egal\n");
        manager.scan_directory(dir.path());

        let from_index = manager.unpromoted_org_files(&dir.path().to_string_lossy());
        let from_disk = crate::utils::note_utils::find_unpromoted_org_files(dir.path());

        assert_eq!(from_index, from_disk);
        assert_eq!(from_index.len(), 2, "the note and the .md file are not in it: {from_index:?}");
        assert_eq!(from_index[0].title, "Lose Datei", "the #+TITLE: is used when there is one");
        assert_eq!(from_index[1].title, "ohne-titel", "and the filename when there is not");
    }

    /// A count that falls to zero has to disappear -- `tag_utils::search`
    /// does not filter on it, so a leftover would keep suggesting a tag
    /// nobody uses.
    #[test]
    fn the_last_note_with_a_tag_takes_the_tag_with_it() {
        let dir = TempDir::new().unwrap();
        let manager = IndexManager::new();
        let only = note_file(dir.path(), "20260101T000000", "eins", &["einmalig"]);
        manager.scan_directory(dir.path());
        assert_eq!(manager.index.read().unwrap().tag_counts.get("einmalig"), Some(&1));

        std::fs::remove_file(&only).unwrap();
        manager.remove_file(&only);

        assert!(
            !manager.index.read().unwrap().tag_counts.contains_key("einmalig"),
            "a zero count must be removed, not kept: {:?}",
            manager.index.read().unwrap().tag_counts
        );
    }

    /* --- scan_directory vs. concurrent changes (see scan_journal) --- */

    /// A file deleted while a scan is walking the disk must stay deleted.
    ///
    /// The scan builds a fresh index from its own snapshot and swaps it in
    /// wholesale, so without the journal the walk's view won -- and a note
    /// the user had just deleted came BACK, into the list, the search and
    /// the agenda. Both production callers run alongside live commands,
    /// and on mobile there is no watcher to put it right later.
    ///
    /// Cannot fail spuriously: if the scan happens to finish before the
    /// deletion, `remove_file` simply applies to the new index and the
    /// assertion holds for the ordinary reason.
    #[test]
    fn a_file_deleted_during_a_scan_does_not_come_back() {
        let dir = TempDir::new().unwrap();
        for i in 0..400 {
            write_file(dir.path(), &format!("note-{i}.org"), "* TODO Etwas\n:PROPERTIES:\n:CUSTOM_ID: h:x\n:END:\n");
        }
        let doomed = write_file(dir.path(), "weg.org", "* TODO Verschwindet\n");

        let manager = Arc::new(IndexManager::new());
        let scanning = {
            let manager = Arc::clone(&manager);
            let root = dir.path().to_path_buf();
            std::thread::spawn(move || manager.scan_directory(&root))
        };

        std::fs::remove_file(&doomed).unwrap();
        manager.remove_file(&doomed);
        scanning.join().unwrap();

        let key = fs_utils::canonicalize_or_self(&doomed).to_string_lossy().into_owned();
        assert!(
            !manager.index.read().unwrap().files.contains_key(&key),
            "the deleted file must not be resurrected by the scan"
        );
    }

    /* --- is_current_root (reindex_and_watch's own guard) --- */

    #[test]
    fn is_current_root_is_false_before_any_scan() {
        let tmp = TempDir::new().unwrap();
        let manager = IndexManager::new();
        assert!(!manager.is_current_root(tmp.path()));
    }

    #[test]
    fn is_current_root_is_true_for_the_dir_just_scanned() {
        let tmp = TempDir::new().unwrap();
        let manager = IndexManager::new();
        manager.scan_directory(tmp.path());
        assert!(manager.is_current_root(tmp.path()));
    }

    #[test]
    fn is_current_root_is_true_even_via_a_non_canonical_but_equivalent_path() {
        let tmp = TempDir::new().unwrap();
        let manager = IndexManager::new();
        manager.scan_directory(tmp.path());

        let junk_path = tmp.path().join(".");
        assert!(manager.is_current_root(&junk_path));
    }

    #[test]
    fn is_current_root_is_false_for_a_genuinely_different_directory() {
        let tmp_a = TempDir::new().unwrap();
        let tmp_b = TempDir::new().unwrap();
        let manager = IndexManager::new();
        manager.scan_directory(tmp_a.path());

        assert!(!manager.is_current_root(tmp_b.path()));
    }

    /* --- scan_directory --- */

    #[test]
    fn scan_populates_files_notes_and_headings_together() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "20260101T000000--cookbook.org", "* TODO Buy eggs\n");
        write_file(tmp.path(), "plain.txt", "not a note");
        std::fs::create_dir(tmp.path().join("sub")).unwrap();

        let manager = IndexManager::new();
        manager.scan_directory(tmp.path());
        let guard = manager.index.read().unwrap();

        // Every entry (note, plain file, dir) lands in `files` ...
        assert_eq!(guard.files.len(), 3);
        // ... but only the denote-shaped .org file becomes a Note/Heading.
        assert_eq!(guard.notes.len(), 1);
        assert_eq!(guard.headings.len(), 1);
    }

    #[test]
    fn scan_includes_hidden_files_and_directories() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join(".attachments")).unwrap();
        let photo = fs_utils::canonicalize_or_self(&write_file(&tmp.path().join(".attachments"), "photo.png", "pixels"));

        let manager = IndexManager::new();
        manager.scan_directory(tmp.path());
        let guard = manager.index.read().unwrap();

        let dir_entry = guard.files.values().find(|f| f.name == ".attachments").unwrap();
        assert!(dir_entry.is_hidden);
        assert!(!dir_entry.is_in_hidden_dir);

        let file_entry = guard.files.get(&photo.to_string_lossy().to_string()).unwrap();
        assert!(!file_entry.is_hidden);
        assert!(file_entry.is_in_hidden_dir);
    }

    #[test]
    fn directory_contents_matches_even_via_a_non_canonical_but_equivalent_path() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "top.txt", "x");

        let manager = IndexManager::new();
        manager.scan_directory(tmp.path());

        let junk_path = tmp.path().join(".");
        let names: Vec<String> =
            manager.directory_contents(&junk_path, false).into_iter().map(|e| e.name).collect();
        assert!(names.contains(&"top.txt".to_string()));
    }

    #[test]
    fn remove_file_matches_the_index_entry_even_via_a_non_canonical_path() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), "plain.txt", "x");

        let manager = IndexManager::new();
        manager.scan_directory(tmp.path());
        assert!(!manager.index.read().unwrap().files.is_empty());

        // The file is gone by the time removal is reported, so the full
        // path cannot be canonicalized -- it does not resolve
        std::fs::remove_file(&path).unwrap();
        let junk_path = tmp.path().join(".").join("plain.txt");
        manager.remove_file(&junk_path);

        assert!(manager.index.read().unwrap().files.is_empty());
    }

    #[test]
    fn scan_reads_modified_at_for_every_file() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "plain.txt", "x");

        let manager = IndexManager::new();
        manager.scan_directory(tmp.path());
        let guard = manager.index.read().unwrap();

        let entry = guard.files.values().find(|f| f.name == "plain.txt").unwrap();
        assert!(entry.modified_at.is_some());
    }

    /* --- update_file / remove_file --- */

    #[test]
    fn update_file_adds_to_files_and_remove_file_clears_it() {
        let tmp = TempDir::new().unwrap();
        let path = fs_utils::canonicalize_or_self(&write_file(tmp.path(), "plain.txt", "x"));

        let manager = IndexManager::new();
        manager.update_file(&path);
        assert!(manager.index.read().unwrap().files.contains_key(&path.to_string_lossy().to_string()));

        manager.remove_file(&path);
        assert!(!manager.index.read().unwrap().files.contains_key(&path.to_string_lossy().to_string()));
    }

    #[test]
    fn update_file_indexes_hidden_files_but_still_ignores_temp_files() {
        let tmp = TempDir::new().unwrap();
        let hidden = fs_utils::canonicalize_or_self(&write_file(tmp.path(), ".hidden", "x"));
        let temp = fs_utils::canonicalize_or_self(&write_file(tmp.path(), "#note.org#", "x"));

        let manager = IndexManager::new();
        manager.update_file(&hidden);
        manager.update_file(&temp);

        assert!(manager.index.read().unwrap().files.contains_key(&hidden.to_string_lossy().to_string()));
        assert!(!manager.index.read().unwrap().files.contains_key(&temp.to_string_lossy().to_string()));
    }

    #[test]
    fn update_file_indexes_files_inside_a_hidden_ancestor_dir() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join(".attachments")).unwrap();
        let attachment = fs_utils::canonicalize_or_self(&write_file(&tmp.path().join(".attachments"), "photo.png", "pixels"));

        let manager = IndexManager::new();
        manager.scan_directory(tmp.path());
        manager.update_file(&attachment);

        let guard = manager.index.read().unwrap();
        let entry = guard.files.get(&attachment.to_string_lossy().to_string()).unwrap();
        assert!(!entry.is_hidden);
        assert!(entry.is_in_hidden_dir);
    }

    /* --- directory_contents / directory_contents_recursive --- */

    #[test]
    fn directory_contents_lists_direct_children_only() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "top.txt", "x");
        std::fs::create_dir(tmp.path().join("sub")).unwrap();
        write_file(&tmp.path().join("sub"), "nested.txt", "y");

        let manager = IndexManager::new();
        manager.scan_directory(tmp.path());

        let names: Vec<String> = manager.directory_contents(tmp.path(), false).into_iter().map(|e| e.name).collect();
        assert!(names.contains(&"top.txt".to_string()));
        assert!(names.contains(&"sub".to_string()));
        assert!(!names.contains(&"nested.txt".to_string()));
    }

    #[test]
    fn directory_contents_excludes_hidden_entries() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "top.txt", "x");
        write_file(tmp.path(), ".hidden", "x");
        std::fs::create_dir(tmp.path().join(".attachments")).unwrap();

        let manager = IndexManager::new();
        manager.scan_directory(tmp.path());

        let names: Vec<String> = manager.directory_contents(tmp.path(), false).into_iter().map(|e| e.name).collect();
        assert!(names.contains(&"top.txt".to_string()));
        assert!(!names.contains(&".hidden".to_string()));
        assert!(!names.contains(&".attachments".to_string()));
    }

    #[test]
    fn directory_contents_includes_hidden_entries_when_show_hidden_is_set() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "top.txt", "x");
        std::fs::create_dir(tmp.path().join(".attachments")).unwrap();

        let manager = IndexManager::new();
        manager.scan_directory(tmp.path());

        let names: Vec<String> = manager.directory_contents(tmp.path(), true).into_iter().map(|e| e.name).collect();
        assert!(names.contains(&".attachments".to_string()));
    }

    #[test]
    fn directory_contents_recursive_excludes_files_inside_a_hidden_dir() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join(".attachments")).unwrap();
        write_file(&tmp.path().join(".attachments"), "photo.png", "pixels");

        let manager = IndexManager::new();
        manager.scan_directory(tmp.path());

        let names: Vec<String> = manager.directory_contents_recursive(tmp.path(), false).into_iter().map(|e| e.name).collect();
        assert!(!names.contains(&"photo.png".to_string()));
    }

    #[test]
    fn files_relative_to_includes_hidden_files_for_link_completion() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join(".attachments")).unwrap();
        write_file(&tmp.path().join(".attachments"), "photo.png", "pixels");

        let manager = IndexManager::new();
        manager.scan_directory(tmp.path());

        let rels = manager.files_relative_to(tmp.path());
        assert!(rels.contains(&".attachments/photo.png".to_string()));
    }

    #[test]
    fn directory_contents_recursive_includes_nested_and_excludes_self() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join("sub")).unwrap();
        write_file(&tmp.path().join("sub"), "nested.txt", "y");

        let manager = IndexManager::new();
        manager.scan_directory(tmp.path());

        let entries = manager.directory_contents_recursive(tmp.path(), false);
        let names: Vec<String> = entries.iter().map(|e| e.name.clone()).collect();
        assert!(names.contains(&"nested.txt".to_string()));
        assert!(entries.iter().all(|e| e.path != tmp.path().to_string_lossy()));
    }

    #[test]
    fn directory_contents_joins_note_metadata_for_files_only() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "20260101T000000--cookbook.org", "* TODO Buy eggs\n");
        std::fs::create_dir(tmp.path().join("sub")).unwrap();

        let manager = IndexManager::new();
        manager.scan_directory(tmp.path());

        let entries = manager.directory_contents(tmp.path(), false);
        let note_entry = entries.iter().find(|e| e.name == "20260101T000000--cookbook.org").unwrap();
        let dir_entry = entries.iter().find(|e| e.name == "sub").unwrap();

        assert!(note_entry.note.is_some());
        assert!(dir_entry.note.is_none());
    }

    /* --- files_relative_to --- */

    #[test]
    fn files_relative_to_excludes_directories_and_joins_with_slash() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join("sub")).unwrap();
        write_file(&tmp.path().join("sub"), "b.png", "y");
        write_file(tmp.path(), "a.org", "x");

        let manager = IndexManager::new();
        manager.scan_directory(tmp.path());

        let mut rels = manager.files_relative_to(tmp.path());
        rels.sort();
        assert_eq!(rels, vec!["a.org".to_string(), "sub/b.png".to_string()]);
    }

    /* --- note_bodies / search_notes_fulltext --- */

    #[test]
    fn scan_caches_note_body_for_fulltext_search() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "20260101T000000--cookbook.org", "* TODO Buy eggs\nRemember to also buy flour\n");

        let manager = IndexManager::new();
        manager.scan_directory(tmp.path());

        let results = manager.search_notes_fulltext(&tmp.path().to_string_lossy(), "flour", &[]);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].occurrences, 1);
    }

    #[test]
    fn scan_also_caches_body_for_non_denote_org_files() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), "actions.org", "* TODO something with flour\n");

        let manager = IndexManager::new();
        manager.scan_directory(tmp.path());
        let guard = manager.index.read().unwrap();

        assert_eq!(guard.org_file_bodies.get(&path.to_string_lossy().into_owned()).map(String::as_str), Some("* TODO something with flour\n"));
        assert!(guard.notes.is_empty(), "still not a recognized Denote note");
    }

    #[test]
    fn update_file_refreshes_cached_body_and_remove_file_clears_it() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), "20260101T000000--cookbook.org", "* TODO Buy eggs\n");

        let manager = IndexManager::new();
        manager.scan_directory(tmp.path());
        assert!(manager.search_notes_fulltext(&tmp.path().to_string_lossy(), "flour", &[]).is_empty());

        std::fs::write(&path, "* TODO Buy eggs\nAnd flour\n").unwrap();
        manager.update_file(&path);
        assert_eq!(manager.search_notes_fulltext(&tmp.path().to_string_lossy(), "flour", &[]).len(), 1);

        manager.remove_file(&path);
        assert!(manager.index.read().unwrap().org_file_bodies.is_empty());
    }

    /// What a command that has just written the file hands over -- the
    /// index takes it as given rather than reading the file back. The
    /// editor's save goes this way, which is also the only thing that
    /// keeps the index current on mobile, where there is no watcher.
    #[test]
    fn update_file_with_indexes_what_the_caller_wrote() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), "20260101T000000--notiz.org", "* TODO Alt\n");

        let manager = IndexManager::new();
        manager.scan_directory(tmp.path());

        let written = "* TODO Neu\nMit Mehl\n";
        std::fs::write(&path, written).unwrap();
        manager.update_file_with(&path, written);

        assert_eq!(manager.search_notes_fulltext(&tmp.path().to_string_lossy(), "mehl", &[]).len(), 1);
        assert_eq!(manager.get_headings_or_parse(&path)[0].title, "Neu");
    }

    /* --- all_headings / all_headings_with_state --- */

    #[test]
    fn all_headings_includes_stateless_headings_all_headings_with_state_excludes() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "actions.org", "* TODO Buy eggs\n* A project heading :project:\n");

        let manager = IndexManager::new();
        manager.scan_directory(tmp.path());

        assert_eq!(manager.all_headings().len(), 2);
        assert_eq!(manager.all_headings_with_state().len(), 1);
    }

    mod perf {
        use super::*;
        use std::time::Instant;

        const SYNTHETIC_FILE_COUNT: usize = 3000;

        fn write_synthetic_vault(root: &Path, count: usize) {
            for i in 0..count {
                // Spread across subdirectories (100 per folder)
                let sub = root.join(format!("folder-{}", i / 100));
                std::fs::create_dir_all(&sub).unwrap();
                std::fs::write(
                    sub.join(format!("2026010{}T00000{}--note-{i}.org", i % 9, i % 10)),
                    format!("* TODO Task {i}\n:PROPERTIES:\n:CUSTOM_ID: synthetic-{i}\n:END:\nSome body text.\n"),
                )
                .unwrap();
            }
        }

        #[test]
        fn perf_scan_directory_with_several_thousand_files() {
            let tmp = TempDir::new().unwrap();
            write_synthetic_vault(tmp.path(), SYNTHETIC_FILE_COUNT);

            let manager = IndexManager::new();
            let start = Instant::now();
            manager.scan_directory(tmp.path());
            let elapsed = start.elapsed();
            eprintln!("perf_scan_directory_with_several_thousand_files: {elapsed:?} for {SYNTHETIC_FILE_COUNT} files");

            assert_eq!(manager.index.read().unwrap().notes.len(), SYNTHETIC_FILE_COUNT);
            // Generous: a full parallel scan+parse of 3000 small files should
            // never realistically approach this on any machine running CI.
            assert!(elapsed.as_secs() < 10, "scan_directory took {elapsed:?} for {SYNTHETIC_FILE_COUNT} files -- investigate for a complexity regression");
        }

        #[test]
        fn perf_directory_contents_is_not_quadratic_in_total_vault_size() {
            let tmp = TempDir::new().unwrap();
            write_synthetic_vault(tmp.path(), SYNTHETIC_FILE_COUNT);

            let manager = IndexManager::new();
            manager.scan_directory(tmp.path());
            let one_folder = tmp.path().join("folder-0");

            let start = Instant::now();
            for _ in 0..200 {
                let entries = manager.directory_contents(&one_folder, false);
                assert_eq!(entries.len(), 100);
            }
            let elapsed = start.elapsed();
            eprintln!("perf_directory_contents_is_not_quadratic_in_total_vault_size: {elapsed:?} for 200 calls over a {SYNTHETIC_FILE_COUNT}-file vault");

            assert!(elapsed.as_secs() < 5, "200 directory_contents calls took {elapsed:?} -- investigate for a complexity regression");
        }
    }
}

/// How long the watcher waits for the file system to go quiet before it
/// believes a burst is over.
///
/// The events are not the point -- what changed is. One `fs::write` is
/// several inotify events, and the app writes a file and then indexes it
/// itself, so the watcher's copy is duplicate work by definition. Worse,
/// every single event used to emit `index-updated`, and half a dozen
/// screens refetch on that: one keystroke that reached the autosave could
/// cost three or four full rounds through the frontend.
///
/// Short enough that a save still feels immediate (a person does not
/// perceive it), long enough to swallow the burst behind one write.
///
/// Measured here (Linux, tmpfs): one `fs::write` produces four inotify
/// events, two of which say "this file changed". Twenty written mirror
/// files produce eighty. Before this, each of those was a read, a parse
/// and an `index-updated` -- and half a dozen screens refetch on that
/// event.
const SETTLE: Duration = Duration::from_millis(150);

/// And an upper bound on the waiting, for the case the file system does
/// NOT go quiet: a sync writing a hundred mirror files keeps the stream
/// alive for seconds, and the vault must not sit un-indexed for all of it.
const LONGEST_BURST: Duration = Duration::from_secs(1);

/// What one file was last seen doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Change {
    Written,
    Gone,
}

/// A burst of watcher events, reduced to what it means.
///
/// Keyed by path, last one wins: a file written five times is one reparse,
/// and a file written and then deleted is a deletion -- reparsing it first
/// would only read a file that is not there any more.
#[derive(Default)]
struct Burst {
    changes: BTreeMap<PathBuf, Change>,
}

impl Burst {
    fn add(&mut self, event: notify::Result<Event>) {
        let Ok(Event { kind, paths, .. }) = event else { return };
        let change = if kind.is_remove() {
            Change::Gone
        } else if kind.is_create() || kind.is_modify() {
            Change::Written
        } else {
            // Access, "other", and the rest: nothing changed on disk.
            return;
        };
        for path in paths {
            self.changes.insert(path, change);
        }
    }

    fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    /// Brings the index in line with the burst. `true` when anything
    /// actually happened, which is what decides whether the frontend is
    /// told.
    fn apply(&self, manager: &IndexManager) -> bool {
        for (path, change) in &self.changes {
            match change {
                Change::Written => manager.update_file(path),
                Change::Gone => manager.remove_file(path),
            }
        }
        !self.is_empty()
    }
}

pub fn start_watcher(listener: Arc<dyn IndexListener>, manager: Arc<IndexManager>, dir_to_watch: PathBuf) {
    // Created out here, not inside the thread, so the manager can hold it
    // and stop it on a vault switch -- see `IndexManager::replace_watcher`.
    let (tx, rx) = std::sync::mpsc::channel();
    let mut watcher = match RecommendedWatcher::new(tx, NotifyConfig::default()) {
        Ok(watcher) => watcher,
        Err(e) => {
            // Logged, not panicked. The release profile sets
            // `panic = "abort"`, so an `expect` here is not a dead
            // background thread but a dead app -- and the likeliest cause
            // is a large vault exhausting `max_user_watches`, i.e. exactly
            // the user this must not crash on. Without a watcher the app
            // still works; it just needs the file browser refreshed by
            // hand, like on mobile, where there is no watcher at all.
            log::error!("could not start the filesystem watcher: {e}");
            return;
        }
    };

    if let Err(e) = watcher.watch(&dir_to_watch, RecursiveMode::Recursive) {
        log::error!("could not watch {}: {e}", dir_to_watch.display());
        return;
    }

    manager.replace_watcher(watcher);

    thread::spawn(move || {
        // One burst at a time: block until something happens, then keep
        // collecting until the file system is quiet for SETTLE (or the
        // burst has gone on for LONGEST_BURST), then do the work once and
        // tell the frontend once.
        while let Ok(first) = rx.recv() {
            let mut burst = Burst::default();
            burst.add(first);

            let until = Instant::now() + LONGEST_BURST;
            loop {
                let left = until.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    break;
                }
                match rx.recv_timeout(SETTLE.min(left)) {
                    Ok(next) => burst.add(next),
                    Err(_) => break,
                }
            }

            if burst.apply(&manager) {
                listener.index_updated();
            }
        }
        // Reached when the watcher was dropped (a vault switch replaced
        // it), which is the whole shutdown mechanism.
        log::debug!("filesystem watcher stopped");
    });
}

/// Tells the app that files in the vault changed underneath it -- named,
/// not "something somewhere".
///
/// Mobile has no filesystem watcher (see docs/mobile.md), so whatever
/// writes into the vault has to say so itself; nothing else would notice.
/// Every command that writes a mirror file goes through here rather than
/// repeating the pair of calls.
///
/// Named files, because a writer always knows what it wrote. This used to
/// rescan the whole vault -- walk every directory, read and reparse every
/// org file -- because one calendar mirror had been rewritten. The
/// difference is not subtle, and it grows with the vault: on a 300-file
/// vault, 13 ms for the rescan against 272 µs for the one file that
/// changed.
///
/// A path that is gone is a removal; the caller does not have to say
/// which of the two it was, and for a sync that both wrote and deleted
/// files it would be a list of two kinds for nothing.
///
/// The frontend hears one `index-updated` for the whole set. Half a dozen
/// screens refetch on it (see e.g. src/features/review/state.ts), so one event per
/// file would be one refetch round per file.
///
/// In the core, and generic over the runtime, because the callers are
/// plugin crates now (see crates/nobo-caldav and friends): it is the one
/// thing they used to reach into the app for.
///
/// Only call it when something actually changed -- an empty list is
/// nothing to tell anyone about, and a pass where every feed answered 304
/// must not wake the app (see e.g. nobo_calendar's `wrote_anything`).
pub fn notify_files_changed(
    listener: &dyn IndexListener,
    index_manager: &IndexManager,
    paths: impl IntoIterator<Item = PathBuf>,
) {
    let mut any = false;
    for path in paths {
        if path.exists() {
            index_manager.update_file(&path);
        } else {
            index_manager.remove_file(&path);
        }
        any = true;
    }
    if any {
        listener.index_updated();
    }
}

/// The same for a single file, which is what most callers have.
pub fn notify_file_changed(
    listener: &dyn IndexListener,
    index_manager: &IndexManager,
    path: &Path,
) {
    notify_files_changed(listener, index_manager, [path.to_path_buf()]);
}
