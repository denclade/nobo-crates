// SPDX-FileCopyrightText: 2026 Dennis Burgermeister
// SPDX-License-Identifier: GPL-3.0-or-later

//! Reading and writing files: atomic writes, the guard against writing a
//! generated file, line-based edits. Feature writes go through
//! core::vault_write, which builds on these.

use super::paths::canonicalize_or_self;
use std::path::{Path, PathBuf};
use std::{fs, io};

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
pub(super) fn refuse_unwritable_kind(path: &Path) -> io::Result<()> {
    refuse_unwritable(crate::core::file_kind::of_path(path))
}

/// The same for a caller that has just read the file and therefore knows
/// its first lines already -- no second `open` for the same answer.
pub(crate) fn refuse_unwritable_kind_of(lines: &[String]) -> io::Result<()> {
    refuse_unwritable(crate::core::file_kind::from_lines(lines))
}

pub(super) fn refuse_unwritable(kind: crate::core::file_kind::FileKind) -> io::Result<()> {
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
pub(super) const MAX_TEMP_ATTEMPTS: u32 = 32;

/// A freshly created, exclusively held temp file next to TARGET.
///
/// `create_new` is the whole exclusion mechanism: the name that did not
/// exist a syscall ago is ours, and no second process can have taken it in
/// between. The pid keeps two nobo processes on one vault apart and the
/// counter keeps two writes inside one process apart, so the first attempt
/// is essentially always the one that succeeds.
pub(super) fn create_temp_beside(target: &Path) -> io::Result<(PathBuf, fs::File)> {
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
