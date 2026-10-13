use crate::core::models::FileEntry;
use crate::utils::temp_file_utils::is_temp_file;
use chrono::{DateTime, Utc};
use ignore::WalkBuilder;
use rayon::prelude::*;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use std::{fs, io};

/// Canonicalizes `dir`, falling back to it unchanged when that fails --
/// the directory does not exist, or the file was just deleted and its path
/// no longer resolves.
///
/// The index is canonical-keyed throughout, so comparing a raw path
/// against it silently matches nothing on any symlink, trailing-slash or
/// case difference -- invisible when reading the two paths side by side.
/// The index and search apply this to their own inputs rather than
/// trusting every caller to remember it.
pub fn canonicalize_or_self(dir: &Path) -> PathBuf {
    fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf())
}

/// Creates DIR if it does not exist yet, returning whether it had to.
///
/// The configured vault can be gone at startup for ordinary reasons -- an
/// external drive not mounted yet, a folder moved or removed, a synced
/// directory not restored on a fresh install. Recreating it beats starting
/// up against a path nothing can be read from or written to, where every
/// later operation fails one by one with no obvious cause.
pub fn ensure_dir_exists(dir: &Path) -> Result<bool, String> {
    if dir.is_dir() {
        return Ok(false);
    }
    fs::create_dir_all(dir).map(|()| true).map_err(|e| format!("failed to create {}: {e}", dir.display()))
}

/// Basename for a path, falling back to the full path if it has none
/// (shouldn't normally happen for a real file path, but History labels are
/// display-only, so this degrades gracefully instead of erroring). Generic
/// over `&Path`/`&str`/`&String` so every history-recording command site
/// can call it directly on whatever path type it already has in hand.
pub fn filename_of<P: AsRef<Path>>(path: P) -> String {
    let path = path.as_ref();
    path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_else(|| path.to_string_lossy().into_owned())
}

/// Whether NAME (a bare filename, no path) is a Unix-style dotfile --
/// this entry's *own* name, e.g. `.attachments` itself (see
/// `has_hidden_ancestor` for "lives inside a hidden dir" instead).
pub fn is_hidden(name: &str) -> bool {
    name.starts_with('.')
}

fn is_hidden_component(c: std::path::Component) -> bool {
    c.as_os_str().to_str().is_some_and(|s| s.starts_with('.') && s != "." && s != "..")
}

/// Whether any component of RELATIVE_PATH *before its last* (the entry's own
/// name, checked separately via `is_hidden`) is a dotfile/dotdir -- e.g.
/// true for `.attachments/photo.png` even though `photo.png` isn't hidden
/// itself. RELATIVE_PATH must already be relative to the vault root (an
/// unrelated dot-component earlier in the absolute path -- e.g. a test's own
/// `TempDir` named `/tmp/.tmpXXXXXX` -- must not count).
pub fn has_hidden_ancestor(relative_path: &Path) -> bool {
    let mut components: Vec<_> = relative_path.components().collect();
    components.pop();
    components.into_iter().any(is_hidden_component)
}

/// Creates DIR if needed, resolves a collision-free path for FILENAME inside
/// it (a numeric suffix on conflict, never overwriting), then hands that
/// final path to WRITE to actually create the file. Shared by
/// commands_notes::copy_attachment_file (`fs::copy` from a real OS path,
/// e.g. an OS-level drag-and-drop) and save_attachment_bytes (`fs::write`,
/// e.g. a pasted clipboard image) -- the editor's attach-a-file flow.
pub fn place_attachment(dir: &Path, filename: &str, write: impl FnOnce(&Path) -> io::Result<()>) -> io::Result<String> {
    fs::create_dir_all(dir)?;
    let dest = unique_path_in(dir, filename);
    write(&dest)?;
    Ok(dest.to_string_lossy().into_owned())
}

fn unique_path_in(dir: &Path, filename: &str) -> PathBuf {
    let candidate = dir.join(filename);
    if !candidate.exists() {
        return candidate;
    }

    let stem = Path::new(filename).file_stem().and_then(|s| s.to_str()).unwrap_or("file");
    let ext = Path::new(filename).extension().and_then(|e| e.to_str());
    let mut n = 2;
    loop {
        let name = match ext {
            Some(e) => format!("{stem}-{n}.{e}"),
            None => format!("{stem}-{n}"),
        };
        let candidate = dir.join(&name);
        if !candidate.exists() {
            return candidate;
        }
        n += 1;
    }
}

fn system_time_to_iso(t: SystemTime) -> String {
    DateTime::<Utc>::from(t).to_rfc3339()
}

/*
Get all entries in the current directory DIR.
Does not check subdirectories! Used for the settings' arbitrary-path
autocomplete (get_path_suggestions), which browses the whole filesystem, not
just the notes vault -- so unlike directory listings inside the vault (see
core::indexer::IndexManager::directory_contents), this stays a live disk
walk rather than an index lookup.
 */
pub fn get_dir_entries(dir: &Path) -> Vec<(PathBuf, String, bool)> {
    let walker = WalkBuilder::new(dir).max_depth(Some(1)).hidden(true).git_ignore(true).build();

    let mut entries: Vec<(PathBuf, String, bool)> = walker
        .into_iter()
        .filter_map(|res| res.ok())
        .par_bridge()
        .filter(|entry| entry.path() != dir)
        .filter(|entry| !is_temp_file(&entry.file_name().to_string_lossy()))
        .map(|entry| {
            let path = entry.path();
            let is_dir = entry.file_type().map_or(false, |ft| ft.is_dir());
            let name = entry.file_name().to_string_lossy().into_owned();

            (path.to_path_buf(), name, is_dir)
        })
        .collect();

    entries.par_sort_unstable_by(|a, b| b.2.cmp(&a.2).then(a.1.cmp(&b.1)));

    entries
}

/// A `FileEntry` for one path, read from the filesystem -- for the
/// watcher's incremental updates, where only one path is known at a time.
/// `None` when the path is gone or its metadata is unreadable.
///
/// `root` computes `is_in_hidden_dir` relative to the vault rather than
/// the absolute path; `None` leaves it false.
pub fn build_file_entry(path: &Path, root: Option<&Path>) -> Option<FileEntry> {
    let name = path.file_name()?.to_string_lossy().into_owned();
    let metadata = fs::metadata(path).ok()?;
    let is_in_hidden_dir = root.and_then(|r| path.strip_prefix(r).ok()).is_some_and(has_hidden_ancestor);

    Some(FileEntry {
        path: path.to_string_lossy().into_owned(),
        is_hidden: is_hidden(&name),
        is_in_hidden_dir,
        name,
        is_dir: metadata.is_dir(),
        created_at: metadata.created().ok().map(system_time_to_iso),
        modified_at: metadata.modified().ok().map(system_time_to_iso),
    })
}

/// Every file and directory under DIR, recursively, as a FileEntry each --
/// the one full walk core::indexer::IndexManager::scan_directory builds the
/// whole index from (files, notes and headings alike come out of this same
/// pass, rather than each doing their own separate walk). Includes hidden
/// files/dirs (e.g. `.attachments`) -- the index is meant to be a complete
/// record of the vault; callers that need to hide these from a
/// folder-browsing UI filter on `is_hidden`/`is_in_hidden_dir` themselves
/// (see IndexManager::directory_contents*).
pub fn get_file_entries_recursive(dir: &Path) -> Vec<FileEntry> {
    WalkBuilder::new(dir)
        .hidden(false)
        .git_ignore(true)
        .build()
        .into_iter()
        .filter_map(|res| res.ok())
        .par_bridge()
        .filter(|entry| entry.path() != dir)
        .filter(|entry| !is_temp_file(&entry.file_name().to_string_lossy()))
        .map(|entry| {
            let metadata = entry.metadata().ok();
            let is_dir = entry.file_type().map_or(false, |ft| ft.is_dir());
            let name = entry.file_name().to_string_lossy().into_owned();
            let relative = entry.path().strip_prefix(dir).unwrap_or_else(|_| entry.path());

            FileEntry {
                path: entry.path().to_string_lossy().into_owned(),
                is_hidden: is_hidden(&name),
                is_in_hidden_dir: has_hidden_ancestor(relative),
                name,
                is_dir,
                created_at: metadata.as_ref().and_then(|m| m.created().ok()).map(system_time_to_iso),
                modified_at: metadata.as_ref().and_then(|m| m.modified().ok()).map(system_time_to_iso),
            }
        })
        .collect()
}

/*
Get all files in DIR with extension EXT. Searches in ALL subdirectories of DIR.
 */
pub fn get_files_by_extension(dir: &Path, ext: &str) -> Vec<PathBuf> {
    WalkBuilder::new(dir)
        .hidden(true)
        .git_ignore(true)
        .build()
        .into_iter()
        .filter_map(|res| res.ok())
        .par_bridge()
        .filter(|entry| {
            let path = entry.path();
            path.is_file() && path.extension().and_then(|e| e.to_str()) == Some(ext)
        })
        .map(|entry| entry.path().to_path_buf())
        .collect()
}

/*
Checks path INPUT for latest valid path and returns a list of possible
continuations to the path.
e.g.
INPUT: /home/user
checks all folders in /home for files starting with de
 */
pub fn get_path_suggestions(input: &str) -> Vec<String> {
    if input.is_empty() {
        return vec![];
    }

    let input_path = Path::new(input);

    let (dir_to_search, prefix) = if input.ends_with('/') || input.ends_with('\\') {
        (input_path, "")
    } else {
        (input_path.parent().unwrap_or_else(|| Path::new("")), input_path.file_name().and_then(|n| n.to_str()).unwrap_or(""))
    };

    let search_dir = if dir_to_search.as_os_str().is_empty() { Path::new(".") } else { dir_to_search };
    let prefix_lower = prefix.to_lowercase();

    let entries = get_dir_entries(search_dir);

    let mut suggestions: Vec<String> = entries
        .into_iter()
        .filter(|(_, name, is_dir)| *is_dir && name.to_lowercase().starts_with(&prefix_lower))
        .map(|(path, _, _)| {
            let mut full_path = path.to_string_lossy().to_string();
            if !full_path.ends_with(std::path::MAIN_SEPARATOR) {
                full_path.push(std::path::MAIN_SEPARATOR);
            }
            full_path
        })
        .take(5)
        .collect();

    suggestions.sort();
    suggestions
}

pub fn read_lines(path: &Path) -> io::Result<Vec<String>> {
    let content = fs::read_to_string(path)?;
    Ok(content.lines().map(String::from).collect())
}

/// Lines back to file content, with the trailing newline every write
/// puts there.
///
/// Public because a caller reconstructing an undo snapshot from the lines
/// it read must produce exactly what `write_lines` will produce --
/// `read_lines` drops the final newline, so a plain `join("\n")` makes
/// every undo churn the file by one byte.
pub fn lines_to_content(lines: &[String]) -> String {
    if lines.is_empty() {
        return String::new();
    }
    let capacity = lines.iter().map(|l| l.len() + 1).sum();
    let mut content = String::with_capacity(capacity);
    for line in lines {
        content.push_str(line);
        content.push('\n');
    }
    content
}

/// Where a file's own declared kind is enforced on writes.
///
/// Line-based editing funnels into `write_lines` (directly or via
/// splice/insert/append below) and whole-content writes into
/// `write_atomic_checked`; between them those two cover every editing path
/// in the app. What they do NOT cover is deliberate: `write_atomic` itself
/// stays unguarded, because restore puts back what it snapshotted and the
/// generators write the very mirrors this refuses.
///
/// Asked of the FILE, not of its path: a generated mirror says what it is
/// in its own first lines (see core::file_kind), so the rule keeps holding
/// when the file is moved, renamed, or synced to another device. The
/// generators write with `fs::write` and are deliberately not subject to
/// it.
///
/// And asked of the file as it is NOW, not of the lines about to be
/// written -- those are the same thing right up until somebody writes a
/// mirror with its `#+NOBO_TYPE:` line removed, which is precisely the
/// write this exists to refuse. It costs one `open` and at most 16 lines
/// (see file_kind::SCAN_LINES), so the guard is not what makes a write
/// slow.
fn refuse_unwritable_kind(path: &Path) -> io::Result<()> {
    refuse_unwritable(crate::core::file_kind::of_path(path))
}

/// The same for a caller that has just read the file and therefore knows
/// its first lines already -- no second `open` for the same answer.
pub(crate) fn refuse_unwritable_kind_of(lines: &[String]) -> io::Result<()> {
    refuse_unwritable(crate::core::file_kind::from_lines(lines))
}

fn refuse_unwritable(kind: crate::core::file_kind::FileKind) -> io::Result<()> {
    if !crate::core::file_kind::capabilities(kind).editable {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            crate::t!(
                "Diese Datei wird von nobo erzeugt und bei jedem Aktualisieren neu geschrieben -- Änderungen daran gingen verloren.",
                "nobo generates this file and rewrites it on every refresh -- changes to it would be lost."
            ),
        ));
    }
    Ok(())
}

pub fn write_lines(path: &Path, lines: &[String]) -> io::Result<()> {
    refuse_unwritable_kind(path)?;
    write_checked(path, lines)
}

/// Replaces a file's whole content, asking the guard first.
///
/// The line-based entry points above cover editing a file that is already
/// parsed into lines; this covers the callers that hold the finished text
/// instead -- saving an editor buffer, appending to an archive, rewriting
/// links across the vault. Before this existed each of those reached for
/// `fs::write` and got neither the guard nor atomicity.
///
/// Deliberately NOT folded into `write_atomic` itself: `splice_lines` and
/// friends have already read the file and asked
/// `refuse_unwritable_kind_of` about the lines they read, and a guard
/// further down would make them open the same file a second time for an
/// answer they have -- which is the whole reason that variant exists.
/// `write_atomic` therefore stays the raw, unguarded write, for restore
/// and for the generators that write mirrors by design.
pub fn write_atomic_checked(path: &Path, content: &str) -> io::Result<()> {
    refuse_unwritable_kind(path)?;
    write_atomic(path, content)
}

/// The write itself, for the callers that have already asked the guard --
/// because they hold the lines they read and asked
/// `refuse_unwritable_kind_of` about those (see `splice_lines`, and
/// `heading_utils::HeadingEdit::commit`, which re-reads once to compare
/// and would otherwise open the file a third time for an answer it has).
pub(crate) fn write_checked(path: &Path, lines: &[String]) -> io::Result<()> {
    write_atomic(path, &lines_to_content(lines))
}

/// Writes CONTENT to PATH so a reader sees either the whole new file or
/// the whole old one -- never half a note.
///
/// `fs::write` truncates first and fills afterwards. Whatever happens in
/// between -- a crash, a power loss, or on Android the perfectly routine
/// killing of a backgrounded app -- leaves the file empty or cut off, and
/// the note is gone. A sibling temp file renamed over the target cannot do
/// that: within one directory the rename either happened or it did not.
///
/// Three details a naive temp-and-rename gets wrong, and this does not:
///
/// - **A symlinked note.** `fs::write` follows the link and writes the
///   file behind it; a rename would REPLACE the link with a regular file
///   and orphan whatever it pointed at. The target is resolved first.
/// - **Permissions.** A freshly created file gets the process umask, so
///   the replaced file's own mode is carried over instead -- a note the
///   user made readable to something else stays readable.
/// - **The temp file itself.** Named in nobo's own `#...#` convention, so
///   `temp_file_utils::is_temp_file` keeps it out of the index and the
///   file browser for the instant it exists (see `IndexManager::update`
///   and `get_file_entries_recursive`), and it is removed again on every
///   failure path -- a write that goes wrong leaves nothing behind and
///   the old file exactly as it was.
///
/// The `sync_all` before the rename is what makes the rest true on a
/// journalling filesystem: the rename orders the directory entry, not the
/// data behind it, and without it a crash just after can leave the new
/// name pointing at a block of zeroes. It costs one fsync per write.
pub fn write_atomic(path: &Path, content: &str) -> io::Result<()> {
    write_atomic_bytes(path, content.as_bytes())
}

/// The same for content that is not text -- an attachment arriving from
/// sync, say. Everything above applies unchanged; only the payload type
/// differs, and a vault holds both kinds of file.
pub fn write_atomic_bytes(path: &Path, content: &[u8]) -> io::Result<()> {
    use std::io::Write;

    // A path that does not resolve is a file that does not exist yet --
    // then there is no link to preserve and the path IS the target.
    let target = canonicalize_or_self(path);
    if let Some(parent) = target.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }

    let (temp_path, mut file) = create_temp_beside(&target)?;

    let written = file.write_all(content).and_then(|()| file.sync_all());
    // Closed before the rename: Windows will not move an open file, and
    // there is nothing left to write to it in any case.
    drop(file);

    let result = written.and_then(|()| {
        if let Ok(metadata) = fs::metadata(&target) {
            // Best effort: a filesystem that cannot carry a mode (FAT on a
            // stick, some Android storage) is no reason to fail the write.
            let _ = fs::set_permissions(&temp_path, metadata.permissions());
        }
        fs::rename(&temp_path, &target)
    });

    if let Err(e) = result {
        let _ = fs::remove_file(&temp_path);
        return Err(e);
    }
    Ok(())
}

/// How many names to try before giving up -- a directory that hands back
/// `AlreadyExists` this often is not going to start cooperating.
const MAX_TEMP_ATTEMPTS: u32 = 32;

/// A freshly created, exclusively held temp file next to TARGET.
///
/// `create_new` is the whole exclusion mechanism: the name that did not
/// exist a syscall ago is ours, and no second process can have taken it in
/// between. The pid keeps two nobo processes on one vault apart and the
/// counter keeps two writes inside one process apart, so the first attempt
/// is essentially always the one that succeeds.
fn create_temp_beside(target: &Path) -> io::Result<(PathBuf, fs::File)> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);

    let parent = match target.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let name = target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "datei".to_string());
    let pid = std::process::id();

    for _ in 0..MAX_TEMP_ATTEMPTS {
        let candidate = parent.join(format!("#{name}.{pid}.{}#", NEXT.fetch_add(1, Ordering::Relaxed)));
        match fs::OpenOptions::new().write(true).create_new(true).open(&candidate) {
            Ok(file) => return Ok((candidate, file)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }

    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        crate::t!("kein freier temporärer Dateiname neben {}", "no free temporary filename next to {}", target.display()),
    ))
}

/// Replaces the half-open line range `start_idx..end_idx` (0-based) of the
/// file at PATH with NEW_LINES.
///
/// A range reaching past the end of the file is an error, not something to
/// trim to fit. The two are not close: a caller here is replacing a
/// heading's block, and its numbers come from a parse of this same file --
/// if they no longer fit, the file changed underneath and the block
/// belongs somewhere else now. Clamping turns that into a write that
/// lands on whatever occupies those lines instead, or truncates the file
/// to the start of the range, and says nothing. It hid two real corruption
/// bugs before it was taken out.
///
/// (`insert_lines_after` below still clamps on purpose -- "after a line
/// past the end" has an obvious, harmless meaning there: append.)
pub fn splice_lines(path: &Path, start_idx: usize, end_idx: usize, new_lines: Vec<String>) -> io::Result<()> {
    let mut lines = read_lines(path)?;
    // What was read IS what is on disk, so the guard needs no second look
    // (see refuse_unwritable_kind_of). Asked before the splice, because
    // after it the declaration may be one of the lines that went.
    refuse_unwritable_kind_of(&lines)?;

    if end_idx > lines.len() || start_idx > end_idx {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            crate::t!(
                "{}: Zeilen {}..{} gibt es nicht mehr (die Datei hat {}) -- sie wurde inzwischen geändert, bitte neu laden.",
                "{}: lines {}..{} are gone (the file has {}) -- it has changed since, please reload.",
                path.display(),
                start_idx,
                end_idx,
                lines.len()
            ),
        ));
    }

    lines.splice(start_idx..end_idx, new_lines);
    write_checked(path, &lines)
}

/// Creates a new file if the path does not exist
pub fn insert_lines_after(path: &Path, after_line: usize, new_lines: Vec<String>) -> io::Result<()> {
    let existed = path.exists();
    let mut lines = if existed { read_lines(path)? } else { Vec::new() };
    if existed {
        refuse_unwritable_kind_of(&lines)?;
    }
    let idx = after_line.min(lines.len());
    lines.splice(idx..idx, new_lines);
    write_checked(path, &lines)
}

/// Creates a new file if the path does not exist
pub fn append_lines(path: &Path, new_lines: Vec<String>) -> io::Result<()> {
    let existed = path.exists();
    let mut lines = if existed { read_lines(path)? } else { Vec::new() };
    if existed {
        refuse_unwritable_kind_of(&lines)?;
    }
    lines.extend(new_lines);
    write_checked(path, &lines)
}

/// Creates a new file if the path does not exist
pub fn insert_line_after(path: &Path, after_line: usize, new_line: String) -> io::Result<()> {
    insert_lines_after(path, after_line, vec![new_line])
}

/// Creates a new file if the path does not exist
pub fn append_line(path: &Path, new_line: String) -> io::Result<()> {
    append_lines(path, vec![new_line])
}

pub fn remove_lines(path: &Path, start_line: usize, end_line: usize) -> io::Result<()> {
    let lines = read_lines(path)?;
    refuse_unwritable_kind_of(&lines)?;
    let remaining: Vec<String> = lines
        .into_iter()
        .enumerate()
        .filter(|(i, _)| {
            let line_no = i + 1;
            !(line_no >= start_line && line_no <= end_line)
        })
        .map(|(_, l)| l)
        .collect();
    write_checked(path, &remaining)
}

/// Splits LINES into what stays and what is archived, by 1-based
/// inclusive RANGES.
///
/// Takes the lines rather than a path on purpose: the ranges were worked
/// out from a parse, and re-reading the file here would let them index
/// into a different version of it -- the same drift `read_and_parse`
/// exists to rule out for heading edits.
pub fn extract_line_ranges(lines: &[String], ranges: &[(usize, usize)]) -> (String, String) {
    let mut keep = Vec::new();
    let mut archived = Vec::new();

    for (i, line) in lines.iter().enumerate() {
        let line_no = i + 1;
        if ranges.iter().any(|(s, e)| line_no >= *s && line_no <= *e) {
            archived.push(line.clone());
        } else {
            keep.push(line.clone());
        }
    }

    (lines_to_content(&keep), lines_to_content(&archived))
}

/// Renames FROM to TO, refusing rather than replacing when TO exists.
///
/// `fs::rename` silently replaces an existing target on Unix, so the
/// callers' `to.exists()` check was a check-then-act with a full read of
/// the source file inside the window -- and sync writes into the same
/// vault from the async runtime. A note that had just arrived from another
/// device under the same name was destroyed, with no error and no history
/// entry (the recorded snapshots describe only the file being moved).
///
/// Done with a hard link rather than a platform call: `link(2)` fails with
/// `EEXIST` if the target exists, which is exactly the atomic reservation
/// wanted, and it needs no new dependency -- `renameat2` /
/// `renamex_np` / `MoveFileEx` would mean `libc` in `nobo-core`, against
/// which `docs/architecture.md` argues. The two names briefly point at one
/// inode; if the second step fails the file exists twice, which is visible
/// and fixable, unlike the loss this replaces.
///
/// A filesystem without hard links (FAT on a stick, some Android storage)
/// falls back to the old check-then-rename. Narrower than nothing, and the
/// only portable option left.
///
/// Still not a lock against the OTHER direction: a sync download can
/// truncate the source while this runs. Closing that needs the vault
/// mutations serialised against the sync run.
pub fn rename_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    match fs::hard_link(from, to) {
        Ok(()) => fs::remove_file(from),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Err(e),
        Err(_) => {
            if to.exists() {
                return Err(io::Error::new(io::ErrorKind::AlreadyExists, "target exists"));
            }
            fs::rename(from, to)
        }
    }
}

pub fn remove_file(path: &Path) -> io::Result<()> {
    fs::remove_file(path)
}

/// Adds CONTENT to the end of the file at PATH, creating it if needed.
///
/// The read is allowed to fail with "not found" and nothing else. It used
/// to fall back to an empty string on ANY error -- so a locked file, a
/// permission problem or a bad sector turned "append one task" into
/// "replace the whole archive with this one task", and the archive is
/// precisely the file nobody looks at often enough to notice.
pub fn append_to_file(path: &Path, content: &str) -> io::Result<()> {
    let mut existing = match fs::read_to_string(path) {
        Ok(existing) => existing,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    if !existing.is_empty() && !existing.ends_with('\n') {
        existing.push('\n');
    }
    existing.push_str(content);
    write_atomic_checked(path, &existing)
}

/* --------- Test -------- */

#[cfg(test)]
mod file_kind_write_guard_tests {
    use super::*;
    use crate::core::file_kind::{FileKind, FileKindSpec, StateSupport};
    use tempfile::TempDir;

    /// Kinds of this module's own, registered the way a feature registers
    /// one (see core::file_kind). The guard is a rule about capabilities,
    /// not about anybody's calendar -- and this layer must not know that
    /// features exist (tools/check-layers-rust.mjs).
    const GENERATED: FileKind = FileKind::new("test-generated");
    const DECLARED_BUT_EDITABLE: FileKind = FileKind::new("test-editable");

    inventory::submit! {
        FileKindSpec {
            kind: GENERATED,
            served_by: None,
            editable: false,
            states: StateSupport::None,
            reorderable: false,
            can_add_headings: false,
        }
    }

    inventory::submit! {
        FileKindSpec {
            kind: DECLARED_BUT_EDITABLE,
            served_by: None,
            editable: true,
            states: StateSupport::DoneOnly,
            reorderable: true,
            can_add_headings: true,
        }
    }

    /// The half `write_string` never had. Whole-content writers -- saving
    /// an editor buffer, rewriting links across the vault -- used to reach
    /// past the guard entirely.
    #[test]
    fn write_atomic_checked_refuses_a_generated_file() {
        let tmp = TempDir::new().unwrap();
        let file = write_mirror(&tmp, "kalender.org");

        let err = write_atomic_checked(&file, "* Von Hand\n").unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert!(fs::read_to_string(&file).unwrap().contains("* Termin"));
    }

    /// Restore and the mirror generators write past the guard on purpose,
    /// so the raw entry point has to stay raw.
    #[test]
    fn write_atomic_itself_stays_unguarded() {
        let tmp = TempDir::new().unwrap();
        let file = write_mirror(&tmp, "kalender.org");

        write_atomic(&file, "regenerated\n").unwrap();

        assert_eq!(fs::read_to_string(&file).unwrap(), "regenerated\n");
    }

    /// The declaration travels with the file, so this holds wherever the
    /// file sits -- a path convention would not survive a move.
    fn write_mirror(dir: &TempDir, name: &str) -> std::path::PathBuf {
        let file = dir.path().join(name);
        std::fs::write(&file, format!("#+TITLE: X\n{}\n\n* Termin\n", GENERATED.declaration())).unwrap();
        file
    }

    /// The rule, at the only place it is actually enforced: every editing
    /// path in the app ends up in write_lines.
    #[test]
    fn writing_to_a_file_declared_uneditable_is_refused() {
        let dir = TempDir::new().unwrap();
        let file = write_mirror(&dir, "anywhere.org");
        let before = std::fs::read_to_string(&file).unwrap();

        let err = write_lines(&file, &["* Geändert".to_string()]).unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), before, "content must be untouched");
    }

    #[test]
    fn splice_insert_and_append_are_refused_too_not_just_write_lines() {
        let dir = TempDir::new().unwrap();
        let file = write_mirror(&dir, "work.org");
        let before = std::fs::read_to_string(&file).unwrap();

        assert!(splice_lines(&file, 0, 1, vec!["x".to_string()]).is_err());
        assert!(insert_lines_after(&file, 0, vec!["x".to_string()]).is_err());
        assert!(append_lines(&file, vec!["x".to_string()]).is_err());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), before);
    }

    /// `remove_lines` too, which is the one that could have gone
    /// unnoticed: it writes through the same door but takes a different
    /// route to it.
    #[test]
    fn removing_lines_from_an_uneditable_file_is_refused() {
        let dir = TempDir::new().unwrap();
        let file = write_mirror(&dir, "termine.org");
        let before = std::fs::read_to_string(&file).unwrap();

        assert!(remove_lines(&file, 1, 1).is_err());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), before);
    }

    /// The loophole the guard has to keep closed: asking the LINES BEING
    /// WRITTEN instead of the file would let a write that drops the
    /// declaration through -- and that write is exactly the one that
    /// destroys a mirror. The helpers ask before they change anything, so
    /// a splice that removes the first line is still refused.
    #[test]
    fn a_write_that_strips_the_declaration_is_still_refused() {
        let dir = TempDir::new().unwrap();
        let file = write_mirror(&dir, "spiegel.org");
        let before = std::fs::read_to_string(&file).unwrap();

        assert!(splice_lines(&file, 0, 1, vec!["* Jetzt gehört sie mir".to_string()]).is_err());
        assert!(write_lines(&file, &["* Jetzt gehört sie mir".to_string()]).is_err());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), before);
    }

    /// A file may declare a kind and still be editable -- the guard must
    /// not lump every declared kind together.
    #[test]
    fn a_declared_but_editable_kind_still_writes() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("einkauf.org");
        std::fs::write(&file, format!("{}\n", DECLARED_BUT_EDITABLE.declaration())).unwrap();

        write_lines(&file, &["* TODO Milch".to_string()]).unwrap();

        assert!(std::fs::read_to_string(&file).unwrap().contains("Milch"));
    }

    #[test]
    fn an_ordinary_vault_file_still_writes() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join(".nobo").join("actions.org");

        write_lines(&file, &["* TODO A".to_string()]).unwrap();

        assert!(std::fs::read_to_string(&file).unwrap().contains("* TODO A"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_file(dir: &Path, name: &str, content: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, content).unwrap();
        path
    }

    /* --- get_dir_entries --- */

    #[test]
    fn lists_files_and_dirs_in_top_level_only() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "a.txt", "x");
        fs::create_dir(tmp.path().join("subdir")).unwrap();
        write_file(&tmp.path().join("subdir"), "nested.txt", "y");

        let entries = get_dir_entries(tmp.path());
        let names: Vec<&str> = entries.iter().map(|(_, n, _)| n.as_str()).collect();

        assert!(names.contains(&"a.txt"));
        assert!(names.contains(&"subdir"));
        assert!(!names.contains(&"nested.txt")); // max_depth(1) - keine Rekursion
    }

    #[test]
    fn dirs_are_sorted_before_files_alphabetically() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "b.txt", "x");
        write_file(tmp.path(), "a.txt", "x");
        fs::create_dir(tmp.path().join("z_dir")).unwrap();

        let entries = get_dir_entries(tmp.path());
        // Dirs (is_dir=true) zuerst, dann Files, jeweils alphabetisch.
        assert_eq!(entries[0].1, "z_dir");
        assert!(entries[0].2);
        assert_eq!(entries[1].1, "a.txt");
        assert_eq!(entries[2].1, "b.txt");
    }

    #[test]
    fn empty_directory_returns_empty_list() {
        let tmp = TempDir::new().unwrap();
        assert!(get_dir_entries(tmp.path()).is_empty());
    }

    #[test]
    fn temp_files_are_excluded() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "#note.org#", "draft");
        write_file(tmp.path(), "note.org", "final");

        let entries = get_dir_entries(tmp.path());
        let names: Vec<&str> = entries.iter().map(|(_, n, _)| n.as_str()).collect();

        assert!(!names.contains(&"#note.org#"));
        assert!(names.contains(&"note.org"));
    }

    #[test]
    fn hidden_files_are_excluded() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), ".hidden", "x");
        write_file(tmp.path(), "visible.txt", "x");

        let entries = get_dir_entries(tmp.path());
        let names: Vec<&str> = entries.iter().map(|(_, n, _)| n.as_str()).collect();

        assert!(!names.contains(&".hidden"));
        assert!(names.contains(&"visible.txt"));
    }

    /* --- get_file_entries_recursive --- */

    #[test]
    fn recursive_listing_includes_nested_files_and_dirs() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "top.txt", "x");
        fs::create_dir(tmp.path().join("sub")).unwrap();
        write_file(&tmp.path().join("sub"), "nested.txt", "y");
        fs::create_dir(tmp.path().join("sub/deeper")).unwrap();
        write_file(&tmp.path().join("sub/deeper"), "deep.txt", "z");

        let entries = get_file_entries_recursive(tmp.path());
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();

        assert!(names.contains(&"top.txt"));
        assert!(names.contains(&"sub"));
        assert!(names.contains(&"nested.txt"));
        assert!(names.contains(&"deeper"));
        assert!(names.contains(&"deep.txt"));
    }

    #[test]
    fn recursive_listing_reads_modified_at_metadata() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "a.txt", "x");

        let entries = get_file_entries_recursive(tmp.path());
        let entry = entries.iter().find(|e| e.name == "a.txt").unwrap();

        assert!(entry.modified_at.is_some());
    }

    #[test]
    fn recursive_listing_excludes_temp_files() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "#draft.org#", "x");
        write_file(tmp.path(), "note.org", "y");

        let entries = get_file_entries_recursive(tmp.path());
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();

        assert!(!names.contains(&"#draft.org#"));
        assert!(names.contains(&"note.org"));
    }

    /* --- build_file_entry --- */

    #[test]
    fn build_file_entry_reads_path_name_and_kind() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), "a.org", "x");

        let entry = build_file_entry(&path, Some(tmp.path())).unwrap();

        assert_eq!(entry.name, "a.org");
        assert!(!entry.is_dir);
        assert!(!entry.is_hidden);
        assert!(!entry.is_in_hidden_dir);
        assert!(entry.modified_at.is_some());
    }

    #[test]
    fn build_file_entry_none_for_missing_path() {
        let tmp = TempDir::new().unwrap();
        assert!(build_file_entry(&tmp.path().join("missing.org"), Some(tmp.path())).is_none());
    }

    #[test]
    fn build_file_entry_flags_own_hidden_name() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), ".hidden", "x");

        let entry = build_file_entry(&path, Some(tmp.path())).unwrap();

        assert!(entry.is_hidden);
        assert!(!entry.is_in_hidden_dir);
    }

    #[test]
    fn build_file_entry_flags_hidden_ancestor_relative_to_root() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join(".attachments")).unwrap();
        let path = write_file(&tmp.path().join(".attachments"), "photo.png", "pixels");

        // TempDir's own path is itself named `/tmp/.tmpXXXXXX` (dot-prefixed) --
        // that must NOT by itself count; only `.attachments`, which is hidden
        // *relative to the given root*, should.
        let entry = build_file_entry(&path, Some(tmp.path())).unwrap();

        assert!(!entry.is_hidden);
        assert!(entry.is_in_hidden_dir);
    }

    #[test]
    fn build_file_entry_treats_unknown_root_as_not_in_hidden_dir() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join(".attachments")).unwrap();
        let path = write_file(&tmp.path().join(".attachments"), "photo.png", "pixels");

        let entry = build_file_entry(&path, None).unwrap();

        assert!(!entry.is_in_hidden_dir);
    }

    /* --- is_hidden --- */

    #[test]
    fn is_hidden_matches_dotfiles_only() {
        assert!(is_hidden(".hidden"));
        assert!(!is_hidden("visible.txt"));
    }

    /* --- has_hidden_ancestor --- */

    #[test]
    fn has_hidden_ancestor_catches_a_hidden_ancestor_dir() {
        assert!(has_hidden_ancestor(Path::new("notes/.attachments/photo.png")));
        assert!(!has_hidden_ancestor(Path::new("notes/sub/photo.png")));
    }

    #[test]
    fn has_hidden_ancestor_ignores_the_entrys_own_hidden_name() {
        // ".hidden" is the entry itself, not an ancestor -- see `is_hidden`
        // for that case.
        assert!(!has_hidden_ancestor(Path::new("notes/.hidden")));
    }

    #[test]
    fn has_hidden_ancestor_ignores_dot_and_dotdot() {
        assert!(!has_hidden_ancestor(Path::new("./notes/../notes/photo.png")));
    }

    /* --- place_attachment / unique_path_in --- */

    #[test]
    fn place_attachment_creates_missing_dir_and_writes_file() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".attachments");

        let result = place_attachment(&dir, "photo.png", |dest| fs::write(dest, b"pixels")).unwrap();

        assert_eq!(Path::new(&result), dir.join("photo.png"));
        assert_eq!(fs::read(&result).unwrap(), b"pixels");
    }

    #[test]
    fn place_attachment_avoids_overwriting_on_name_collision() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "photo.png", "original");

        let result = place_attachment(tmp.path(), "photo.png", |dest| fs::write(dest, b"new")).unwrap();

        assert_eq!(Path::new(&result).file_name().unwrap(), "photo-2.png");
        assert_eq!(fs::read_to_string(tmp.path().join("photo.png")).unwrap(), "original");
        assert_eq!(fs::read(&result).unwrap(), b"new");
    }

    /* --- get_files_by_extension --- */

    #[test]
    fn finds_files_with_matching_extension_recursively() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "a.org", "x");
        fs::create_dir(tmp.path().join("sub")).unwrap();
        write_file(&tmp.path().join("sub"), "b.org", "y");
        write_file(tmp.path(), "c.txt", "z");

        let results = get_files_by_extension(tmp.path(), "org");

        assert_eq!(results.len(), 2);
        assert!(results.iter().any(|p| p.file_name().unwrap() == "a.org"));
        assert!(results.iter().any(|p| p.file_name().unwrap() == "b.org"));
    }

    #[test]
    fn no_matching_extension_returns_empty() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "a.txt", "x");
        assert!(get_files_by_extension(tmp.path(), "org").is_empty());
    }

    /* --- get_path_suggestions --- */

    #[test]
    fn empty_input_returns_no_suggestions() {
        assert!(get_path_suggestions("").is_empty());
    }

    #[test]
    fn suggests_matching_subdirectories_by_prefix() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("desktop")).unwrap();
        fs::create_dir(tmp.path().join("documents")).unwrap();
        fs::create_dir(tmp.path().join("music")).unwrap();

        let input = tmp.path().join("de").to_string_lossy().into_owned();
        let suggestions = get_path_suggestions(&input);

        assert_eq!(suggestions.len(), 1);
        assert!(suggestions[0].contains("desktop"));
    }

    #[test]
    fn trailing_separator_lists_all_subdirs_without_prefix_filter() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("alpha")).unwrap();
        fs::create_dir(tmp.path().join("beta")).unwrap();

        let mut input = tmp.path().to_string_lossy().into_owned();
        input.push(std::path::MAIN_SEPARATOR);

        let suggestions = get_path_suggestions(&input);
        assert_eq!(suggestions.len(), 2);
    }

    #[test]
    fn files_are_not_suggested_only_directories() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("devdir")).unwrap();
        write_file(tmp.path(), "devfile.txt", "x");

        let input = tmp.path().join("dev").to_string_lossy().into_owned();
        let suggestions = get_path_suggestions(&input);

        assert_eq!(suggestions.len(), 1);
        assert!(suggestions[0].contains("devdir"));
    }

    #[test]
    fn suggestions_are_capped_at_five() {
        let tmp = TempDir::new().unwrap();
        for i in 0..8 {
            fs::create_dir(tmp.path().join(format!("dir{}", i))).unwrap();
        }

        let input = tmp.path().join("dir").to_string_lossy().into_owned();
        let suggestions = get_path_suggestions(&input);

        assert_eq!(suggestions.len(), 5);
    }

    /* --- read_lines / write_lines --- */

    #[test]
    fn read_lines_splits_content_by_newline() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), "f.txt", "a\nb\nc");

        let lines = read_lines(&path).unwrap();
        assert_eq!(lines, vec!["a", "b", "c"]);
    }

    #[test]
    fn read_lines_missing_file_returns_err() {
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("does_not_exist.txt");
        assert!(read_lines(&missing).is_err());
    }

    #[test]
    fn write_lines_joins_with_newline_and_trailing_newline() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("out.txt");

        write_lines(&path, &["a".to_string(), "b".to_string()]).unwrap();
        let content = fs::read_to_string(&path).unwrap();

        assert_eq!(content, "a\nb\n");
    }

    #[test]
    fn write_lines_empty_vec_writes_empty_file() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("empty.txt");

        write_lines(&path, &[]).unwrap();
        let content = fs::read_to_string(&path).unwrap();

        assert_eq!(content, "");
    }

    #[test]
    fn read_write_roundtrip_preserves_lines() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("roundtrip.txt");
        let original = vec!["one".to_string(), "two".to_string(), "three".to_string()];

        write_lines(&path, &original.clone()).unwrap();
        let read_back = read_lines(&path).unwrap();

        assert_eq!(read_back, original);
    }

    /* --- splice_lines --- */

    #[test]
    fn splice_replaces_line_range() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), "f.txt", "a\nb\nc\nd");

        splice_lines(&path, 1, 3, vec!["X".to_string()]).unwrap();
        let lines = read_lines(&path).unwrap();

        assert_eq!(lines, vec!["a", "X", "d"]);
    }

    /// The empty range AT the end is a legitimate way to append, and stays
    /// one -- it names lines that exist (none of them).
    #[test]
    fn splice_of_the_empty_range_at_the_end_appends() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), "f.txt", "a\nb");

        splice_lines(&path, 2, 2, vec!["c".to_string()]).unwrap();
        let lines = read_lines(&path).unwrap();

        assert_eq!(lines, vec!["a", "b", "c"]);
    }

    /// A range past the end is refused rather than trimmed to fit.
    ///
    /// Every caller here is replacing a heading's block against numbers
    /// from a parse of this same file. A range that no longer fits means
    /// the file changed underneath, and the block belongs somewhere else
    /// now -- clamping would write it over whatever moved into those lines
    /// instead, or cut the file off at the start of the range, and say
    /// nothing. Two real corruption bugs hid behind that.
    #[test]
    fn splice_past_the_end_is_refused_and_leaves_the_file_untouched() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), "f.txt", "a\nb");

        let err = splice_lines(&path, 2, 10, vec!["c".to_string()]).unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(read_lines(&path).unwrap(), vec!["a", "b"], "nothing was written");
    }

    #[test]
    fn splice_of_an_inverted_range_is_refused() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), "f.txt", "a\nb\nc");

        let err = splice_lines(&path, 2, 1, vec!["X".to_string()]).unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(read_lines(&path).unwrap(), vec!["a", "b", "c"], "nothing was written");
    }

    /* --- write_atomic --- */

    /// The point of the whole exercise: a reader never sees half a note.
    /// What is observable from a test is the other half of that bargain --
    /// the temp file it goes through does not survive the write.
    #[test]
    fn write_leaves_no_temp_file_behind() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("note.org");

        write_lines(&path, &["* Eins".to_string()]).unwrap();
        write_lines(&path, &["* Zwei".to_string()]).unwrap();

        let leftovers: Vec<String> = fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name != "note.org")
            .collect();

        assert!(leftovers.is_empty(), "temp files left in the vault: {leftovers:?}");
        assert_eq!(read_lines(&path).unwrap(), vec!["* Zwei"]);
    }

    /// And whatever it does leave behind for the instant it exists is
    /// named so the index and the file browser ignore it -- otherwise a
    /// watcher event mid-write would put a phantom note in the sidebar.
    #[test]
    fn the_temp_file_a_write_goes_through_is_recognized_as_one() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("note.org");

        let (temp_path, _file) = create_temp_beside(&target).unwrap();
        let name = temp_path.file_name().unwrap().to_string_lossy().into_owned();

        assert!(crate::utils::temp_file_utils::is_temp_file(&name), "not filtered anywhere: {name}");
        fs::remove_file(&temp_path).unwrap();
    }

    /// A rename replaces the name it is given. Handed a symlink, that
    /// would swap the link itself for a regular file and orphan whatever
    /// it pointed at -- so the link is resolved before anything is moved.
    #[cfg(unix)]
    #[test]
    fn writing_through_a_symlink_writes_the_file_behind_it() {
        let tmp = TempDir::new().unwrap();
        let real = write_file(tmp.path(), "real.org", "* Alt\n");
        let link = tmp.path().join("link.org");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        write_lines(&link, &["* Neu".to_string()]).unwrap();

        assert!(fs::symlink_metadata(&link).unwrap().file_type().is_symlink(), "the link must survive as a link");
        assert_eq!(fs::read_to_string(&real).unwrap(), "* Neu\n", "and point at the new content");
    }

    /// A fresh file gets the umask; the file being REPLACED already has a
    /// mode the user may have chosen. It is carried over.
    #[cfg(unix)]
    #[test]
    fn writing_preserves_the_mode_of_the_file_it_replaces() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), "note.org", "* Alt\n");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();

        write_lines(&path, &["* Neu".to_string()]).unwrap();

        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o640, "the replaced file's own mode has to survive");
    }

    #[test]
    fn splice_with_empty_replacement_deletes_range() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), "f.txt", "a\nb\nc");

        splice_lines(&path, 1, 2, vec![]).unwrap();
        let lines = read_lines(&path).unwrap();

        assert_eq!(lines, vec!["a", "c"]);
    }

    /* --- insert_line_after --- */

    #[test]
    fn insert_line_after_middle() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), "f.txt", "a\nb\nc");

        insert_line_after(&path, 1, "X".to_string()).unwrap();
        let lines = read_lines(&path).unwrap();

        assert_eq!(lines, vec!["a", "X", "b", "c"]);
    }

    #[test]
    fn insert_line_after_zero_prepends() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), "f.txt", "a\nb");

        insert_line_after(&path, 0, "X".to_string()).unwrap();
        let lines = read_lines(&path).unwrap();

        assert_eq!(lines, vec!["X", "a", "b"]);
    }

    #[test]
    fn insert_line_after_beyond_end_appends() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), "f.txt", "a\nb");

        insert_line_after(&path, 99, "X".to_string()).unwrap();
        let lines = read_lines(&path).unwrap();

        assert_eq!(lines, vec!["a", "b", "X"]);
    }

    /* --- append_line --- */

    #[test]
    fn append_line_adds_to_end() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), "f.txt", "a\nb");

        append_line(&path, "c".to_string()).unwrap();
        let lines = read_lines(&path).unwrap();

        assert_eq!(lines, vec!["a", "b", "c"]);
    }

    #[test]
    fn append_line_to_empty_file() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), "f.txt", "");

        append_line(&path, "first".to_string()).unwrap();
        let lines = read_lines(&path).unwrap();

        assert_eq!(lines, vec!["first"]);
    }

    /* --- insert_lines_after --- */

    #[test]
    fn insert_lines_after_middle_keeps_order() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), "f.txt", "a\nb\nc");

        insert_lines_after(&path, 1, vec!["X".to_string(), "Y".to_string()]).unwrap();
        let lines = read_lines(&path).unwrap();

        assert_eq!(lines, vec!["a", "X", "Y", "b", "c"]);
    }

    #[test]
    fn insert_lines_after_zero_prepends_block() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), "f.txt", "a\nb");

        insert_lines_after(&path, 0, vec!["X".to_string(), "Y".to_string()]).unwrap();
        let lines = read_lines(&path).unwrap();

        assert_eq!(lines, vec!["X", "Y", "a", "b"]);
    }

    #[test]
    fn insert_lines_after_beyond_end_appends_block() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), "f.txt", "a\nb");

        insert_lines_after(&path, 99, vec!["X".to_string(), "Y".to_string()]).unwrap();
        let lines = read_lines(&path).unwrap();

        assert_eq!(lines, vec!["a", "b", "X", "Y"]);
    }

    #[test]
    fn insert_lines_after_empty_block_changes_nothing() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), "f.txt", "a\nb");

        insert_lines_after(&path, 1, vec![]).unwrap();
        let lines = read_lines(&path).unwrap();

        assert_eq!(lines, vec!["a", "b"]);
    }

    #[test]
    fn insert_lines_after_creates_file_if_missing() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("new.org");

        insert_lines_after(&path, 0, vec!["* TODO A".to_string(), "SCHEDULED: <2026-08-05 Wed>".to_string()]).unwrap();
        let lines = read_lines(&path).unwrap();

        assert_eq!(lines, vec!["* TODO A", "SCHEDULED: <2026-08-05 Wed>"]);
    }

    /* --- append_lines --- */

    #[test]
    fn append_lines_adds_block_to_end() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), "f.txt", "a\nb");

        append_lines(&path, vec!["c".to_string(), "d".to_string()]).unwrap();
        let lines = read_lines(&path).unwrap();

        assert_eq!(lines, vec!["a", "b", "c", "d"]);
    }

    #[test]
    fn append_lines_to_empty_file() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), "f.txt", "");

        append_lines(&path, vec!["first".to_string(), "second".to_string()]).unwrap();
        let lines = read_lines(&path).unwrap();

        assert_eq!(lines, vec!["first", "second"]);
    }

    #[test]
    fn append_lines_creates_missing_parent_dirs() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("gtd").join("inbox.org");

        append_lines(&path, vec!["* Capture".to_string()]).unwrap();

        assert!(path.exists());
        assert_eq!(read_lines(&path).unwrap(), vec!["* Capture"]);
    }

    #[test]
    fn append_lines_empty_block_leaves_file_untouched() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), "f.txt", "a\nb");

        append_lines(&path, vec![]).unwrap();
        let lines = read_lines(&path).unwrap();

        assert_eq!(lines, vec!["a", "b"]);
    }

    /* --- remove_lines --- */

    #[test]
    fn remove_lines_deletes_inclusive_range() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), "f.txt", "a\nb\nc\nd");

        remove_lines(&path, 2, 3).unwrap();
        let lines = read_lines(&path).unwrap();

        assert_eq!(lines, vec!["a", "d"]);
    }

    #[test]
    fn remove_lines_single_line() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), "f.txt", "a\nb\nc");

        remove_lines(&path, 2, 2).unwrap();
        let lines = read_lines(&path).unwrap();

        assert_eq!(lines, vec!["a", "c"]);
    }

    #[test]
    fn remove_lines_entire_file_leaves_empty() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), "f.txt", "a\nb");

        remove_lines(&path, 1, 2).unwrap();
        let lines = read_lines(&path).unwrap();

        assert!(lines.is_empty());
    }

    /* --- extract_line_ranges --- */

    #[test]
    fn extract_line_ranges_splits_keep_and_archived() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), "f.txt", "a\nb\nc\nd\ne");

        let (keep, archived) = extract_line_ranges(&read_lines(&path).unwrap(), &[(2, 3)]);

        assert_eq!(keep, "a\nd\ne\n");
        assert_eq!(archived, "b\nc\n");
    }

    #[test]
    fn extract_line_ranges_multiple_ranges() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), "f.txt", "a\nb\nc\nd\ne");

        let (keep, archived) = extract_line_ranges(&read_lines(&path).unwrap(), &[(1, 1), (4, 5)]);

        assert_eq!(keep, "b\nc\n");
        assert_eq!(archived, "a\nd\ne\n");
    }

    #[test]
    fn extract_line_ranges_no_matches_keeps_everything() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), "f.txt", "a\nb");

        let (keep, archived) = extract_line_ranges(&read_lines(&path).unwrap(), &[(99, 100)]);

        assert_eq!(keep, "a\nb\n");
        assert_eq!(archived, "");
    }

    /* --- rename_no_replace --- */

    #[test]
    fn rename_no_replace_moves_the_file() {
        let tmp = TempDir::new().unwrap();
        let from = tmp.path().join("alt.org");
        let to = tmp.path().join("neu.org");
        fs::write(&from, "Inhalt").unwrap();

        rename_no_replace(&from, &to).unwrap();

        assert!(!from.exists());
        assert_eq!(fs::read_to_string(&to).unwrap(), "Inhalt");
    }

    /// The whole point: an existing target is never replaced. `fs::rename`
    /// would have overwritten it without a word.
    #[test]
    fn rename_no_replace_refuses_an_existing_target() {
        let tmp = TempDir::new().unwrap();
        let from = tmp.path().join("alt.org");
        let to = tmp.path().join("neu.org");
        fs::write(&from, "verschoben").unwrap();
        fs::write(&to, "gerade vom anderen Gerät angekommen").unwrap();

        let err = rename_no_replace(&from, &to).unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read_to_string(&to).unwrap(), "gerade vom anderen Gerät angekommen");
        assert!(from.exists(), "and the source is still there");
    }

    /* --- write_atomic_checked --- */

    #[test]
    fn write_atomic_checked_writes_raw_content() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("raw.txt");

        write_atomic_checked(&path, "hello\nworld").unwrap();
        let content = fs::read_to_string(&path).unwrap();

        assert_eq!(content, "hello\nworld");
    }

    /// A vault holds attachments, and sync writes them through the same
    /// path as notes.
    #[test]
    fn write_atomic_bytes_round_trips_non_utf8_content() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("bild.png");
        let bytes: Vec<u8> = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0xff, 0xfe];

        write_atomic_bytes(&path, &bytes).unwrap();

        assert_eq!(fs::read(&path).unwrap(), bytes);
    }

    /* --- append_to_file --- */

    #[test]
    fn append_to_file_creates_new_file_if_missing() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("new_archive.txt");

        append_to_file(&path, "first entry").unwrap();
        let content = fs::read_to_string(&path).unwrap();

        assert_eq!(content, "first entry");
    }

    #[test]
    fn append_to_file_adds_newline_before_appending_to_existing() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("archive.txt");
        fs::write(&path, "existing content").unwrap();

        append_to_file(&path, "new entry").unwrap();
        let content = fs::read_to_string(&path).unwrap();

        assert_eq!(content, "existing content\nnew entry");
    }

    #[test]
    fn append_to_file_does_not_double_newline_if_already_present() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("archive.txt");
        fs::write(&path, "existing content\n").unwrap();

        append_to_file(&path, "new entry").unwrap();
        let content = fs::read_to_string(&path).unwrap();

        assert_eq!(content, "existing content\nnew entry");
    }

    /// The archive-eating bug. A read that fails for any reason other than
    /// "not there" used to fall back to an empty string, so appending one
    /// task REPLACED the archive with that task.
    #[test]
    fn append_to_file_refuses_when_the_existing_content_cannot_be_read() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("archive.txt");
        fs::write(&path, "zwei Jahre erledigter Aufgaben\n").unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();

            let result = append_to_file(&path, "neue Aufgabe");

            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
            assert!(result.is_err(), "an unreadable archive must not be replaced");
            assert_eq!(fs::read_to_string(&path).unwrap(), "zwei Jahre erledigter Aufgaben\n");
        }
    }
}
