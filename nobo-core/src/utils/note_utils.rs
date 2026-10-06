use std::collections::{HashMap, HashSet};
use std::io;
use crate::core::heading_identity;
use crate::core::models::{EmptyPropertiesDrawer, Heading, HeadingLocation, Note, OrphanedCustomId, UnpromotedOrgFile, ORG_PROPERTIES_END, ORG_PROPERTIES_START};
use crate::core::org_parser::parse_property;
use crate::utils::fs_utils;
use crate::utils::string_utils::count_leading_stars;
use chrono::{Datelike, Local, Timelike, Weekday};
use serde::Serialize;
use std::path::{Path, PathBuf};
use unicode_normalization::UnicodeNormalization;

const FILENAME_SYNC_PROPERTY: &str = "FILENAME_SYNC";
const FILENAME_SYNC_IGNORE_VALUE: &str = "ignore";

// We use the file naming scheme introduced by Prot in denote
// @@ID==SIGNATURE--TITLE__KEYWORDS.EXTENSION
// file-naming scheme docs: https://protesilaos.com/emacs/denote#h:4e9c7512-84dc-4dfb-9fa9-e15d51178e5d
// TODO: Support Signature - Check usefullness in this note app approach
pub fn parse_note_record(path: &Path) -> Option<Note> {
    let filename = path.file_name()?.to_str()?;

    // Check if its a denote-style orgfile
    if !filename.ends_with(".org") || !filename.contains("--") {
        return None;
    }

    let name_without_ext = filename.trim_end_matches(".org");
    let (id_part, rest) = name_without_ext.split_once("--")?;
    let (title_part, tags_part) = rest.split_once("__").unwrap_or((rest, ""));

    let title = title_part.replace('-', " ");

    let tags = if tags_part.is_empty() {
        Vec::new()
    } else {
        tags_part.split('_').map(|s| s.to_string()).collect()
    };

    let date_str = denote_identifier_date(id_part).unwrap_or_default();

    Some(Note {
        path: path.to_string_lossy().into_owned(),
        identifier: id_part.to_string(),
        title,
        date_str,
        tags,
    })
}

/// The `DD.MM.YYYY HH:MM` a Denote identifier names, or `None` when the
/// leading part of the filename is not one.
///
/// A Denote identifier is exactly `YYYYMMDDTHHMMSS`: 15 ASCII characters
/// with a literal `T` at index 8 and digits everywhere else. Checking that
/// -- rather than a bare `len() >= 13` followed by byte slices -- fixes
/// two things at once.
///
/// It cannot panic any more. `len()` counts BYTES, so
/// "Größenübersicht--tabelle.org" passed the length check with a 15-byte
/// leading part, and slicing at 9..11 landed inside the `ü`. That is a
/// panic per org file, from the startup scan's `into_par_iter` and from
/// every watcher update -- and with `panic = "abort"` in the release
/// profile it takes the process with it, so the app dies at launch on a
/// file the user cannot reach to rename. nobo never writes such a name
/// itself (`build_denote_filename` collapses non-alphanumerics); files
/// arriving from Emacs, from sync or from a manual rename are the point of
/// this format.
///
/// And it stops inventing dates. "Projektplan-2024--x.org" used to render
/// as "tp.ek.Proj an:-2".
fn denote_identifier_date(id: &str) -> Option<String> {
    const IDENTIFIER_LEN: usize = 15;
    const T_INDEX: usize = 8;

    let bytes = id.as_bytes();
    if bytes.len() != IDENTIFIER_LEN || bytes[T_INDEX] != b'T' {
        return None;
    }
    if bytes.iter().enumerate().any(|(i, b)| i != T_INDEX && !b.is_ascii_digit()) {
        return None;
    }

    Some(format!("{}.{}.{} {}:{}", &id[6..8], &id[4..6], &id[0..4], &id[9..11], &id[11..13]))
}

/// The Denote-style filename a note with these header values is expected to
/// have: `IDENTIFIER--slug(TITLE)__tag1_tag2.EXTENSION`. Single source of
/// truth for note creation (generate_note_file), drift detection
/// (check_filename_sync), and assigning Denote metadata to an arbitrary
/// attachment (preview_denote_filename, for images/PDFs from the file
/// browser) -- so none of the three can disagree on what "correct" looks
/// like. `extension` has no leading dot (e.g. "org", "png").
pub fn build_denote_filename(identifier: &str, title: &str, tags: &[String], extension: &str) -> String {
    // Format title to fs safe.
    //
    // `is_alphanumeric`, not an ASCII test plus a hand-written list of
    // German letters: this app is used in more than one language, and
    // "Ελλάδα" or "日本語" collapsing to a row of dashes makes the file
    // unfindable by its own name. Transliteration (ä -> a) is the wrong
    // answer too -- the filename is what `check_filename_sync` compares
    // against the title, so it would report every note as out of sync.
    //
    // NFC first, because the same character can arrive decomposed (macOS
    // filesystems hand out NFD) and `ä` as `a` + U+0308 would otherwise
    // slug differently from `ä` as one code point.
    let mut safe_title = String::new();
    let mut last_was_dash = false;
    for c in title.nfc().collect::<String>().to_lowercase().chars() {
        if c.is_alphanumeric() {
            safe_title.push(c);
            last_was_dash = false;
        } else if !last_was_dash {
            safe_title.push('-');
            last_was_dash = true;
        }
    }
    let safe_title = safe_title.trim_matches('-').to_string();

    // Format tags (__tag1_tag2)
    let tag_string = if tags.is_empty() {
        String::new()
    } else {
        format!("__{}", tags.join("_"))
    };

    format!("{}--{}{}.{}", identifier, safe_title, tag_string, extension)
}

/// `YYYYMMDDTHHMMSS` for NOW -- the Denote identifier format, shared by
/// generate_note_file (brand-new notes) and duplicate_file's default
/// suggestion (an existing note's fields, just a fresh timestamp).
fn format_identifier(now: chrono::DateTime<Local>) -> String {
    format!("{}T{:02}{:02}{:02}", now.format("%Y%m%d"), now.hour(), now.minute(), now.second())
}

pub fn generate_note_file(dir: &str, title: &str, tags: &[String], author: &str) -> io::Result<String> {
    let now = Local::now();
    let id = format_identifier(now);
    let filename = build_denote_filename(&id, title, tags, "org");
    let content = note_content(now, &id, title, tags, author, &[], "* \n");
    crate::utils::fs_utils::write_atomic(&Path::new(dir).join(&filename), &content)?;
    Ok(filename)
}

/// A new note with KEYWORDS (whole lines, `#+NOBO_TYPE: cookbook`) after
/// the usual header and BODY after it -- for a note that is something
/// from the start, a cookbook say, rather than a note declared one later.
///
/// Never replaces a file: one already at that name -- the same title in
/// the same second -- is an error, not something to write over.
pub fn create_note_file(
    dir: &str,
    title: &str,
    tags: &[String],
    author: &str,
    keywords: &[String],
    body: &str,
) -> io::Result<String> {
    use std::io::Write;
    let now = Local::now();
    let id = format_identifier(now);
    let filename = build_denote_filename(&id, title, tags, "org");
    let content = note_content(now, &id, title, tags, author, keywords, body);
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(Path::new(dir).join(&filename))?;
    file.write_all(content.as_bytes())?;
    file.sync_all()?;
    Ok(filename)
}

/// The header every new note starts with, then KEYWORDS, a blank line
/// and BODY.
fn note_content(
    now: chrono::DateTime<Local>,
    id: &str,
    title: &str,
    tags: &[String],
    author: &str,
    keywords: &[String],
    body: &str,
) -> String {
    let weekday_de = match now.weekday() {
        Weekday::Mon => "Mo", Weekday::Tue => "Di", Weekday::Wed => "Mi",
        Weekday::Thu => "Do", Weekday::Fri => "Fr", Weekday::Sat => "Sa",
        Weekday::Sun => "So",
    };
    let date_str = format!("[{} {} {:02}:{:02}]", now.format("%Y-%m-%d"), weekday_de, now.hour(), now.minute());

    let mut content = format!("#+TITLE:      {}\n#+DATE:       {}\n", title, date_str);
    if !author.trim().is_empty() {
        content.push_str(&format!("#+AUTHOR:     {}\n", author.trim()));
    }
    if !tags.is_empty() {
        content.push_str(&format!("#+FILETAGS:   :{}:\n", tags.join(":")));
    }
    content.push_str(&format!("#+IDENTIFIER: {}\n", id));
    for keyword in keywords {
        content.push_str(keyword);
        content.push('\n');
    }
    content.push('\n');
    content.push_str(body);
    content
}

/// Denote note fields the "duplicate" flow lets the user confirm/override
/// before it actually happens (see DuplicateNoteModal) -- `None` on any
/// field means "keep the source's own value" (only `identifier` defaults to
/// a fresh timestamp instead, since that's the whole point of duplicating).
#[derive(Default)]
pub struct DuplicateOverrides {
    pub title: Option<String>,
    pub tags: Option<Vec<String>>,
    pub identifier: Option<String>,
}

/// Duplicates `path` into the same directory.
///
/// A Denote-shaped note gets an updated header per `overrides` and is
/// renamed to match. The identifier defaults to a fresh timestamp, so
/// vault-wide uniqueness holds even when the caller changed nothing else
/// -- an id link must resolve to exactly one note.
///
/// Anything else is a byte-for-byte copy under a "(Kopie)" name, ignoring
/// `overrides`.
pub fn duplicate_file(path: &Path, overrides: DuplicateOverrides) -> io::Result<String> {
    if parse_note_record(path).is_some() {
        return duplicate_denote_note(path, overrides);
    }

    let new_path = generic_duplicate_path(path);
    std::fs::copy(path, &new_path)?;
    Ok(new_path.to_string_lossy().into_owned())
}

fn duplicate_denote_note(path: &Path, overrides: DuplicateOverrides) -> io::Result<String> {
    let lines = fs_utils::read_lines(path)?;
    let ParsedHeader { title: source_title, filetags: source_tags, .. } = parse_header(&lines);

    let new_title = overrides.title.unwrap_or_else(|| source_title.unwrap_or_default());
    let new_tags = overrides.tags.unwrap_or(source_tags);
    let new_id = overrides.identifier.unwrap_or_else(|| format_identifier(Local::now()));

    let extension = path.extension().and_then(|e| e.to_str()).unwrap_or("org");
    let new_filename = build_denote_filename(&new_id, &new_title, &new_tags, extension);
    let new_path = path.parent().unwrap_or_else(|| Path::new("")).join(&new_filename);

    let new_lines = rewrite_header(&lines, &new_title, &new_tags, &new_id);
    fs_utils::write_lines(&new_path, &new_lines)?;
    Ok(new_path.to_string_lossy().into_owned())
}

/// Gives a Denote note a new title and tags -- in its header and in its
/// filename, in one step, so the two cannot come apart the way they do
/// when only one is edited (see check_filename_sync). The identifier
/// stays: it is what every link to the note points at, and the note's
/// date with it. Returns the path the note lives at afterwards, which is
/// the old one when the new name slugs to the same filename.
///
/// Renamed before it is rewritten: the name collision is the failure that
/// actually happens, and it should happen before anything is touched.
pub fn retitle_note(path: &Path, title: &str, tags: &[String]) -> io::Result<String> {
    let lines = fs_utils::read_lines(path)?;
    let header = parse_header(&lines);
    let identifier = header.identifier.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "not a Denote note: no #+IDENTIFIER: line"))?;

    let extension = path.extension().and_then(|e| e.to_str()).unwrap_or("org");
    let new_filename = build_denote_filename(&identifier, title, tags, extension);
    let current_name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    // Normalised like check_filename_sync's own comparison: on APFS the
    // same name can be stored decomposed, and renaming a file onto itself
    // in the other form is refused as a collision.
    let new_path = if current_name.nfc().eq(new_filename.nfc()) {
        path.to_path_buf()
    } else {
        let new_path = path.parent().unwrap_or_else(|| Path::new("")).join(&new_filename);
        fs_utils::rename_no_replace(path, &new_path)?;
        new_path
    };

    let new_lines = rewrite_header(&lines, title, tags, &identifier);
    fs_utils::write_lines(&new_path, &new_lines)?;
    Ok(new_path.to_string_lossy().into_owned())
}

/// Every line verbatim, except within the file-level preamble (before the
/// first heading) #+TITLE:/#+IDENTIFIER:/#+FILETAGS: are rewritten to the
/// given values -- everything else (body, headings, other keywords,
/// :PROPERTIES: drawers) is kept exactly as-is.
fn rewrite_header(lines: &[String], new_title: &str, new_tags: &[String], new_id: &str) -> Vec<String> {
    let mut new_lines = Vec::with_capacity(lines.len() + 1);
    let mut past_header = false;
    let mut filetags_written = false;
    let mut identifier_line_idx: Option<usize> = None;

    for line in lines {
        if !past_header && is_heading_line(line) {
            past_header = true;
        }
        if past_header {
            new_lines.push(line.clone());
            continue;
        }

        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("#+") {
            if let Some((key, _)) = rest.split_once(':') {
                match key.trim().to_uppercase().as_str() {
                    "TITLE" => { new_lines.push(format!("#+TITLE:      {}", new_title)); continue; }
                    "IDENTIFIER" => {
                        new_lines.push(format!("#+IDENTIFIER: {}", new_id));
                        identifier_line_idx = Some(new_lines.len() - 1);
                        continue;
                    }
                    "FILETAGS" => {
                        filetags_written = true;
                        if !new_tags.is_empty() {
                            new_lines.push(format!("#+FILETAGS:   :{}:", new_tags.join(":")));
                        }
                        continue;
                    }
                    _ => {}
                }
            }
        }
        new_lines.push(line.clone());
    }

    // The source had no #+FILETAGS: line to rewrite in place (no tags, or a
    // handwritten file missing it) but the result should have one --
    // insert it right after #+IDENTIFIER: instead of just dropping the tags.
    if !filetags_written && !new_tags.is_empty() {
        let insert_at = identifier_line_idx.map(|i| i + 1).unwrap_or(new_lines.len());
        new_lines.insert(insert_at, format!("#+FILETAGS:   :{}:", new_tags.join(":")));
    }

    new_lines
}

/// "name.ext" -> "name (Kopie).ext", or "name (Kopie 2).ext" etc. if that's
/// already taken -- the generic (non-denote) duplicate-naming fallback.
fn generic_duplicate_path(path: &Path) -> PathBuf {
    let parent = path.parent().unwrap_or_else(|| Path::new(""));
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("file");
    let ext = path.extension().and_then(|e| e.to_str());

    let name_for = |n: u32| match ext {
        Some(e) if n == 1 => format!("{stem} (Kopie).{e}"),
        Some(e) => format!("{stem} (Kopie {n}).{e}"),
        None if n == 1 => format!("{stem} (Kopie)"),
        None => format!("{stem} (Kopie {n})"),
    };

    let mut n = 1;
    loop {
        let candidate = parent.join(name_for(n));
        if !candidate.exists() {
            return candidate;
        }
        n += 1;
    }
}

#[derive(Debug, Clone, Serialize, specta::Type)]
pub struct FilenameSyncStatus {
    pub in_sync: bool,
    pub ignored: bool,
    pub current_name: String,
    pub expected_name: String,
}

struct ParsedHeader {
    title: Option<String>,
    identifier: Option<String>,
    filetags: Vec<String>,
    ignored: bool,
}

/// Column-0 only, matching real org-mode and org_parser::parse_heading_line
/// exactly -- an indented "  * foo" line is never a heading there either,
/// so it must not end the preamble scan below.
fn is_heading_line(line: &str) -> bool {
    let stars = count_leading_stars(line);
    stars > 0 && line[stars..].starts_with(' ')
}

/// Scans the file-level preamble (everything before the first heading) for
/// the `#+TITLE:`/`#+IDENTIFIER:`/`#+FILETAGS:` keywords `generate_note_file`
/// writes, plus a `:FILENAME_SYNC: ignore` property in a file-level
/// `:PROPERTIES:` drawer (see set_filename_sync_ignored) if the user has
/// opted out of the rename prompt for this file.
fn parse_header(lines: &[String]) -> ParsedHeader {
    let mut header = ParsedHeader { title: None, identifier: None, filetags: Vec::new(), ignored: false };
    let mut in_properties = false;

    for line in lines {
        if is_heading_line(line) {
            break;
        }

        let trimmed = line.trim();

        if trimmed.eq_ignore_ascii_case(ORG_PROPERTIES_START) {
            in_properties = true;
            continue;
        }
        if trimmed.eq_ignore_ascii_case(ORG_PROPERTIES_END) {
            in_properties = false;
            continue;
        }
        if in_properties {
            if let Some((k, v)) = parse_property(trimmed) {
                if k.eq_ignore_ascii_case(FILENAME_SYNC_PROPERTY) && v.eq_ignore_ascii_case(FILENAME_SYNC_IGNORE_VALUE) {
                    header.ignored = true;
                }
            }
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix("#+") {
            if let Some((key, value)) = rest.split_once(':') {
                let value = value.trim().to_string();
                match key.trim().to_uppercase().as_str() {
                    "TITLE" => header.title = Some(value),
                    "IDENTIFIER" => header.identifier = Some(value),
                    "FILETAGS" => {
                        header.filetags = value.split(':').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect();
                    }
                    _ => {}
                }
            }
        }
    }

    header
}

/// Compares a note's on-disk filename against what its own header (`#+TITLE`/
/// `#+IDENTIFIER`/`#+FILETAGS`) says it should be. `None` when there's
/// nothing Denote-shaped to compare against (not an `.org` file, or missing
/// `#+TITLE`/`#+IDENTIFIER` -- e.g. `actions.org`).
pub fn check_filename_sync(path: &Path) -> Option<FilenameSyncStatus> {
    if path.extension().and_then(|e| e.to_str()) != Some("org") {
        return None;
    }

    let lines = fs_utils::read_lines(path).ok()?;
    let header = parse_header(&lines);
    let title = header.title?;
    let identifier = header.identifier?;

    let expected_name = build_denote_filename(&identifier, &title, &header.filetags, "org");
    let current_name = path.file_name()?.to_string_lossy().into_owned();

    // Compare Unicode-normalized: on macOS/APFS a filename containing e.g.
    // "ä" is stored decomposed (a + combining diaeresis) even when written
    // as the precomposed form we build here from #+TITLE -- same rendered
    // name, different bytes. Without this, umlauts/accents would falsely
    // trigger the rename prompt on every open/save.
    let normalized_match = current_name.nfc().eq(expected_name.nfc());

    Some(FilenameSyncStatus {
        in_sync: normalized_match,
        ignored: header.ignored,
        current_name,
        expected_name,
    })
}

/// Marks a note as "don't ask to rename again" by writing `:FILENAME_SYNC:
/// ignore` into its file-level `:PROPERTIES:` drawer (before the first
/// heading), creating the drawer if none exists yet.
pub fn set_filename_sync_ignored(path: &Path) -> io::Result<()> {
    let mut lines = fs_utils::read_lines(path)?;

    let heading_idx = lines.iter().position(|l| is_heading_line(l)).unwrap_or(lines.len());

    let drawer_start = lines[..heading_idx].iter().position(|l| l.trim().eq_ignore_ascii_case(ORG_PROPERTIES_START));

    if let Some(start) = drawer_start {
        let end = lines[start..heading_idx].iter().position(|l| l.trim().eq_ignore_ascii_case(ORG_PROPERTIES_END)).map(|i| start + i);
        let Some(end) = end else { return Ok(()) };

        let already_set = lines[start + 1..end].iter().any(|l| {
            parse_property(l.trim()).map_or(false, |(k, _)| k.eq_ignore_ascii_case(FILENAME_SYNC_PROPERTY))
        });

        if !already_set {
            lines.insert(end, format!(":{}: {}", FILENAME_SYNC_PROPERTY, FILENAME_SYNC_IGNORE_VALUE));
        }
    } else {
        let drawer = vec![
            ORG_PROPERTIES_START.to_string(),
            format!(":{}: {}", FILENAME_SYNC_PROPERTY, FILENAME_SYNC_IGNORE_VALUE),
            ORG_PROPERTIES_END.to_string(),
        ];
        lines.splice(heading_idx..heading_idx, drawer);
    }

    fs_utils::write_lines(path, &lines)
}

#[derive(Debug, Clone, Serialize, specta::Type)]
pub struct IdentifierChange {
    pub old_identifier: String,
    pub new_identifier: String,
}

/// Detects an edited `#+IDENTIFIER:` between two versions of a note's raw
/// content (typically the last-saved buffer vs. the about-to-be-saved one --
/// see useNoteEditor.ts's guarded saveActiveNote). `None` when either side
/// has no identifier (e.g. a brand new note) or both match.
pub fn identifier_changed(old_content: &str, new_content: &str) -> Option<IdentifierChange> {
    let to_lines = |s: &str| s.lines().map(String::from).collect::<Vec<_>>();
    let old_identifier = parse_header(&to_lines(old_content)).identifier?;
    let new_identifier = parse_header(&to_lines(new_content)).identifier?;

    if old_identifier == new_identifier {
        return None;
    }

    Some(IdentifierChange { old_identifier, new_identifier })
}

/// Rewrites every `id:OLD_IDENTIFIER` occurrence to `id:NEW_IDENTIFIER`
/// across every .org file under `dir` -- a plain literal substring replace is
/// safe here since Denote identifiers are a fixed `YYYYMMDDTHHMMSS` shape, so
/// one identifier can never be a prefix of another. Returns how many files
/// were actually changed.
pub fn replace_link_references(dir: &Path, old_identifier: &str, new_identifier: &str) -> io::Result<u32> {
    let from = format!("id:{}", old_identifier);
    let to = format!("id:{}", new_identifier);

    let mut changed: u32 = 0;
    for path in fs_utils::get_files_by_extension(dir, "org") {
        let content = std::fs::read_to_string(&path)?;
        if !content.contains(&from) {
            continue;
        }
        fs_utils::write_atomic_checked(&path, &content.replace(&from, &to))?;
        changed += 1;
    }

    Ok(changed)
}

/// Every heading-link anchor referenced in `body` -- for
/// `[[denote:20240118T060608::#h:eed0fb8e][...]]` that is `h:eed0fb8e`,
/// the CUSTOM_ID verbatim, prefix included.
///
/// Substring scanning rather than a regex: a CUSTOM_ID runs until the next
/// `]`, org link syntax's own separator, and never contains one.
fn referenced_heading_ids(body: &str) -> impl Iterator<Item = &str> {
    const MARKER: &str = "::#";
    body.match_indices(MARKER).map(move |(idx, _)| {
        let rest = &body[idx + MARKER.len()..];
        let end = rest.find(']').unwrap_or(rest.len());
        &rest[..end]
    })
}

/// Every heading anywhere under DIR whose own CUSTOM_ID is never referenced
/// by a heading-link from any other heading -- see Settings' Maintenance
/// section, which surfaces these for review before commands_headings::
/// remove_custom_ids deletes them. Scans every file's raw content directly
/// from disk (not the in-memory index's `note_bodies` cache, which only
/// covers files with a Denote header -- a link could just as well sit in a
/// GTD file, which has none), same reasoning replace_link_references's own
/// `dir: &Path` disk scan above already applies to identifier rewrites.
pub fn find_orphaned_custom_ids(
    dir: &Path,
    headings_by_file: &HashMap<String, Vec<Heading>>,
) -> io::Result<Vec<OrphanedCustomId>> {
    let mut referenced: HashSet<String> = HashSet::new();
    for path in fs_utils::get_files_by_extension(dir, "org") {
        let content = std::fs::read_to_string(&path)?;
        referenced.extend(referenced_heading_ids(&content).map(String::from));
    }

    let mut orphaned: Vec<OrphanedCustomId> = headings_by_file
        .values()
        .flatten()
        .filter_map(|heading| {
            let id = heading_identity::custom_id_of(heading)?;
            if referenced.contains(id) {
                return None;
            }
            Some(OrphanedCustomId {
                location: HeadingLocation { file_path: heading.file_path.clone(), target: heading_identity::target_of(heading) },
                title: heading.title.clone(),
                custom_id: id.to_string(),
            })
        })
        .collect();

    orphaned.sort_by(|a, b| a.location.file_path.cmp(&b.location.file_path).then(a.location.target.line_no.cmp(&b.location.target.line_no)));
    Ok(orphaned)
}

/// PATH's own `#+TITLE:` if it has one, else its filename without the
/// extension -- the display title for a plain `.org` file that isn't a
/// recognized Denote note yet (see find_unpromoted_org_files/
/// commands_search::search_files, both of which need SOME title to
/// search/rank/show it by, without it needing a real Note record).
fn title_or_filename(path: &Path) -> String {
    let lines = fs_utils::read_lines(path).unwrap_or_default();
    parse_header(&lines)
        .title
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| path.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string())
}

/// The `#+TITLE:` in already-loaded text, if it has a non-empty one.
///
/// Same header parse as `title_or_filename`, for callers holding the
/// content rather than a path -- see `IndexManager::unpromoted_org_files`,
/// which reads it out of the index's body cache instead of off disk.
pub fn title_from_text(text: &str) -> Option<String> {
    let lines: Vec<String> = text.lines().map(str::to_string).collect();
    parse_header(&lines).title.filter(|t| !t.is_empty())
}

/// Every `.org` file under `dir` that is not a recognized Denote note, and
/// so has no stable identifier -- "plain .org files" in the UI.
///
/// Does not exclude the GTD files, which match this check too -- promoting
/// one would be wrong, but the caller has the config in scope and filters
/// them back out.
///
/// Walks the disk and opens each candidate for its `#+TITLE:`, which is
/// why the SEARCH does not use this any more (it asked on every
/// keystroke); see `IndexManager::unpromoted_org_files` for the
/// index-backed answer and a test holding the two together.
///
/// The maintenance screen keeps this one deliberately: it is asked once,
/// on purpose, by someone about to rename files -- and unlike the index
/// it is also right during the very first scan after an install, where
/// the index would answer "none found" for a vault full of them.
pub fn find_unpromoted_org_files(dir: &Path) -> Vec<UnpromotedOrgFile> {
    let mut files: Vec<UnpromotedOrgFile> = fs_utils::get_files_by_extension(dir, "org")
        .into_iter()
        .filter(|path| parse_note_record(path).is_none())
        .map(|path| UnpromotedOrgFile { title: title_or_filename(&path), path: path.to_string_lossy().into_owned() })
        .collect();
    files.sort_by(|a, b| a.path.cmp(&b.path));
    files
}

/// Promotes a plain `.org` file into a Denote note: stamps a fresh
/// `#+IDENTIFIER:` and, if missing, a `#+TITLE:` from the filename, then
/// renames the file to match.
///
/// The rename is not optional: the "is this a note" check is entirely
/// filename-driven, so a header alone stays invisible to the rest of the
/// app. Everything past the preamble is carried over untouched.
pub fn promote_org_file_to_note(path: &Path) -> io::Result<Note> {
    let lines = fs_utils::read_lines(path)?;
    let ParsedHeader { title: existing_title, filetags, .. } = parse_header(&lines);

    let title = existing_title
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| path.file_stem().and_then(|s| s.to_str()).unwrap_or("Untitled").to_string());
    let id = format_identifier(Local::now());

    let mut new_lines = Vec::with_capacity(lines.len() + 2);
    let mut past_header = false;
    let mut title_written = false;
    let mut identifier_written = false;

    for line in &lines {
        if !past_header && is_heading_line(line) {
            if !title_written {
                new_lines.push(format!("#+TITLE:      {}", title));
            }
            if !identifier_written {
                new_lines.push(format!("#+IDENTIFIER: {}", id));
            }
            past_header = true;
        }
        if past_header {
            new_lines.push(line.clone());
            continue;
        }

        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("#+") {
            if let Some((key, _)) = rest.split_once(':') {
                match key.trim().to_uppercase().as_str() {
                    "TITLE" => { new_lines.push(format!("#+TITLE:      {}", title)); title_written = true; continue; }
                    "IDENTIFIER" => { new_lines.push(format!("#+IDENTIFIER: {}", id)); identifier_written = true; continue; }
                    _ => {}
                }
            }
        }
        new_lines.push(line.clone());
    }

    // No heading anywhere in the file (a preamble-only file) -- the loop
    // above never reached its "about to leave the preamble" insertion
    // point, so anything still missing needs appending here instead.
    if !past_header {
        if !title_written {
            new_lines.push(format!("#+TITLE:      {}", title));
        }
        if !identifier_written {
            new_lines.push(format!("#+IDENTIFIER: {}", id));
        }
    }

    fs_utils::write_lines(path, &new_lines)?;

    let extension = path.extension().and_then(|e| e.to_str()).unwrap_or("org");
    let new_filename = build_denote_filename(&id, &title, &filetags, extension);
    let new_path = path.parent().unwrap_or_else(|| Path::new("")).join(&new_filename);
    std::fs::rename(path, &new_path)?;

    parse_note_record(&new_path)
        .ok_or_else(|| io::Error::other("promoted file still not recognized as a note"))
}

/// Every heading anywhere in HEADINGS_BY_FILE whose own `:PROPERTIES:...
/// :END:` drawer is completely empty -- see EmptyPropertiesDrawer's own
/// doc comment.
pub fn find_empty_properties_drawers(headings_by_file: &HashMap<String, Vec<Heading>>) -> io::Result<Vec<EmptyPropertiesDrawer>> {
    let mut found = Vec::new();

    for headings in headings_by_file.values() {
        let Some(first) = headings.first() else { continue };
        let lines = fs_utils::read_lines(Path::new(&first.file_path))?;

        for heading in headings {
            // 0-indexed line right after the (1-indexed) heading line.
            let start = heading.line_no;
            let (Some(properties_line), Some(end_line)) = (lines.get(start), lines.get(start + 1)) else { continue };
            if !properties_line.trim().eq_ignore_ascii_case(ORG_PROPERTIES_START) { continue; }
            if !end_line.trim().eq_ignore_ascii_case(ORG_PROPERTIES_END) { continue; }

            found.push(EmptyPropertiesDrawer {
                location: HeadingLocation { file_path: heading.file_path.clone(), target: heading_identity::target_of(heading) },
                title: heading.title.clone(),
            });
        }
    }

    found.sort_by(|a, b| a.location.file_path.cmp(&b.location.file_path).then(a.location.target.line_no.cmp(&b.location.target.line_no)));
    Ok(found)
}

/* --------- Test -------- */

#[cfg(test)]
mod tests {

    /* --- a new note --- */

    #[test]
    fn a_new_note_with_keywords_carries_them_after_its_header() {
        let dir = tempfile::tempdir().unwrap();
        let name = super::create_note_file(
            dir.path().to_str().unwrap(), "Kochbuch", &[], "Dennis",
            &["#+NOBO_TYPE: cookbook".to_string()], "",
        ).unwrap();

        assert!(name.ends_with("--kochbuch.org"), "{name}");
        let text = std::fs::read_to_string(dir.path().join(&name)).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "#+TITLE:      Kochbuch");
        assert_eq!(lines[2], "#+AUTHOR:     Dennis");
        assert!(lines[3].starts_with("#+IDENTIFIER: "));
        assert_eq!(lines[4], "#+NOBO_TYPE: cookbook");
        assert!(text.ends_with("cookbook\n\n"), "no empty heading in it: {text:?}");
    }

    /// The same title in the same second is the same file name: the
    /// second one must fail, not write over the first.
    #[test]
    fn a_new_note_never_replaces_one_that_is_there() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_str().unwrap();
        let first = super::create_note_file(path, "Kochbuch", &[], "", &[], "* Lasagne\n").unwrap();
        std::fs::write(dir.path().join(&first), "meins\n").unwrap();

        // Retried until it lands in the same second as `first` was named
        // for, or the second is over and there is nothing to collide with.
        for _ in 0..3 {
            match super::create_note_file(path, "Kochbuch", &[], "", &[], "") {
                Err(e) => {
                    assert_eq!(e.kind(), std::io::ErrorKind::AlreadyExists);
                    break;
                }
                Ok(other) => assert_ne!(other, first),
            }
        }
        assert_eq!(std::fs::read_to_string(dir.path().join(&first)).unwrap(), "meins\n");
    }

    /* --- the filename slug --- */

    /// A title in a language whose letters are not ASCII must still
    /// produce a findable filename. The old test was an ASCII check plus
    /// a hand-written list of four German letters, so Greek, Cyrillic,
    /// Japanese and French accents all collapsed to dashes.
    #[test]
    fn a_non_ascii_title_keeps_its_letters_in_the_filename() {
        for (title, expected) in [
            ("Größenübersicht", "größenübersicht"),
            ("Ελλάδα", "ελλάδα"),
            ("Заметка", "заметка"),
            ("日本語のノート", "日本語のノート"),
            ("Café résumé", "café-résumé"),
        ] {
            let name = super::build_denote_filename("20260101T000000", title, &[], "org");
            assert!(name.contains(expected), "{title}: got {name}");
        }
    }

    /// macOS hands out decomposed filenames, so the same title can arrive
    /// as `a` + combining diaeresis. Both spellings have to slug the same
    /// way or `check_filename_sync` reports the note as out of sync
    /// forever.
    #[test]
    fn a_decomposed_title_slugs_like_its_composed_form() {
        let composed = super::build_denote_filename("20260101T000000", "Bär", &[], "org");
        let decomposed = super::build_denote_filename("20260101T000000", "Ba\u{308}r", &[], "org");

        assert_eq!(composed, decomposed);
    }

    /* --- the Denote identifier (see denote_identifier_date) --- */

    /// The panic. `len()` counts bytes, so a German title passed the old
    /// `>= 13` check and the byte slice landed inside a multi-byte
    /// character -- once per org file, from the startup scan and from
    /// every watcher update, and `panic = "abort"` makes that the whole
    /// process.
    #[test]
    fn a_filename_with_umlauts_where_the_identifier_should_be_does_not_panic() {
        let note = super::parse_note_record(std::path::Path::new("/vault/Größenübersicht--tabelle.org")).unwrap();

        assert_eq!(note.date_str, "", "not an identifier, so no date is invented");
    }

    #[test]
    fn a_non_identifier_ascii_prefix_no_longer_invents_a_date() {
        let note = super::parse_note_record(std::path::Path::new("/vault/Projektplan-2024--x.org")).unwrap();

        assert_eq!(note.date_str, "");
    }

    #[test]
    fn a_real_identifier_still_reads_as_a_date() {
        let note = super::parse_note_record(std::path::Path::new("/vault/20260827T091500--notiz.org")).unwrap();

        assert_eq!(note.date_str, "27.08.2026 09:15");
    }
    use super::*;
    use tempfile::TempDir;

    fn write_file(dir: &Path, name: &str, content: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn in_sync_when_filename_matches_header() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), "20250718T140125--cookbook__essen.org",
            "#+TITLE:      Cookbook\n#+FILETAGS:   :essen:\n#+IDENTIFIER: 20250718T140125\n\n* \n");

        let status = check_filename_sync(&path).unwrap();
        assert!(status.in_sync);
        assert!(!status.ignored);
    }

    #[test]
    fn build_denote_filename_preserves_arbitrary_extension() {
        let name = build_denote_filename("20260707T224500", "Vacation Photo", &["urlaub".to_string()], "png");
        assert_eq!(name, "20260707T224500--vacation-photo__urlaub.png");
    }

    #[test]
    fn out_of_sync_after_title_change() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), "20250718T140125--cookbook__essen.org",
            "#+TITLE:      Kochbuch\n#+FILETAGS:   :essen:\n#+IDENTIFIER: 20250718T140125\n\n* \n");

        let status = check_filename_sync(&path).unwrap();
        assert!(!status.in_sync);
        assert_eq!(status.expected_name, "20250718T140125--kochbuch__essen.org");
    }

    #[test]
    fn out_of_sync_after_tag_added() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), "20250718T140125--cookbook__essen.org",
            "#+TITLE:      Cookbook\n#+FILETAGS:   :essen:kochen:\n#+IDENTIFIER: 20250718T140125\n\n* \n");

        let status = check_filename_sync(&path).unwrap();
        assert!(!status.in_sync);
        assert_eq!(status.expected_name, "20250718T140125--cookbook__essen_kochen.org");
    }

    #[test]
    fn in_sync_despite_differing_unicode_normalization() {
        let tmp = TempDir::new().unwrap();
        // Decomposed "a" + combining diaeresis (U+0061 U+0308) -- what e.g.
        // macOS/APFS may store on disk even when the app requested the
        // precomposed "ä" (U+00E4) used in the #+TITLE below. Same rendered
        // name, different bytes; must still compare as in sync.
        let decomposed_name = "20260707T224500--gera\u{0308}t__essen.org";
        let path = write_file(tmp.path(), decomposed_name,
            "#+TITLE:      Ger\u{00e4}t\n#+FILETAGS:   :essen:\n#+IDENTIFIER: 20260707T224500\n\n* \n");

        let status = check_filename_sync(&path).unwrap();
        assert!(status.in_sync);
    }

    #[test]
    fn none_when_missing_identifier() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), "actions.org", "#+TITLE: Actions\n\n* TODO Something\n");

        assert!(check_filename_sync(&path).is_none());
    }

    #[test]
    fn none_for_non_org_files() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), "image.png", "not really a png");

        assert!(check_filename_sync(&path).is_none());
    }

    /* --- duplicate_file --- */

    #[test]
    fn duplicate_denote_note_without_overrides_keeps_title_gets_fresh_identifier() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), "20250718T140125--cookbook__essen.org",
            "#+TITLE:      Cookbook\n#+FILETAGS:   :essen:\n#+IDENTIFIER: 20250718T140125\n\n* Erste Aufgabe\nInhalt\n");

        let new_path_str = duplicate_file(&path, DuplicateOverrides::default()).unwrap();
        let new_path = Path::new(&new_path_str);

        assert!(new_path.exists());
        assert_ne!(new_path, path);

        let content = std::fs::read_to_string(new_path).unwrap();
        // Default suggestion is "just bump the identifier" -- title/tags
        // stay exactly as the source had them unless overridden.
        assert!(content.contains("#+TITLE:      Cookbook\n"));
        assert!(content.contains("#+FILETAGS:   :essen:"));
        assert!(!content.contains("20250718T140125")); // old identifier must not survive
        assert!(content.contains("* Erste Aufgabe\nInhalt")); // body untouched
    }

    #[test]
    fn duplicate_denote_note_applies_title_tags_and_identifier_overrides() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), "20250718T140125--cookbook__essen.org",
            "#+TITLE:      Cookbook\n#+FILETAGS:   :essen:\n#+IDENTIFIER: 20250718T140125\n\n* \n");

        let overrides = DuplicateOverrides {
            title: Some("Kochbuch 2".to_string()),
            tags: Some(vec!["kochen".to_string()]),
            identifier: Some("20260101T120000".to_string()),
        };
        let new_path_str = duplicate_file(&path, overrides).unwrap();
        let content = std::fs::read_to_string(&new_path_str).unwrap();

        assert!(new_path_str.contains("20260101T120000--kochbuch-2__kochen.org"));
        assert!(content.contains("#+TITLE:      Kochbuch 2"));
        assert!(content.contains("#+FILETAGS:   :kochen:"));
        assert!(content.contains("#+IDENTIFIER: 20260101T120000"));
        assert!(!content.contains("essen"));
    }

    #[test]
    fn duplicate_denote_note_adds_filetags_line_when_source_had_none() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), "20250718T140125--cookbook.org",
            "#+TITLE:      Cookbook\n#+IDENTIFIER: 20250718T140125\n\n* \n");

        let overrides = DuplicateOverrides { tags: Some(vec!["neu".to_string()]), ..Default::default() };
        let new_path_str = duplicate_file(&path, overrides).unwrap();
        let content = std::fs::read_to_string(&new_path_str).unwrap();

        assert!(content.contains("#+FILETAGS:   :neu:"));
    }

    #[test]
    fn duplicate_denote_note_twice_gets_two_distinct_files() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), "20250718T140125--cookbook.org",
            "#+TITLE:      Cookbook\n#+IDENTIFIER: 20250718T140125\n\n* \n");

        let first = duplicate_file(&path, DuplicateOverrides::default()).unwrap();
        // Denote identifiers are second-granularity -- force a distinct one
        // so the second duplicate can't collide with the first by chance.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let second = duplicate_file(&path, DuplicateOverrides::default()).unwrap();

        assert_ne!(first, second);
        assert!(Path::new(&first).exists());
        assert!(Path::new(&second).exists());
    }

    #[test]
    fn retitle_rewrites_header_and_renames_keeping_the_identifier() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), "20250718T140125--cookbook__essen.org",
            "#+TITLE:      Cookbook\n#+FILETAGS:   :essen:\n#+IDENTIFIER: 20250718T140125\n\n* Erste Aufgabe\nInhalt\n");

        let new_path = retitle_note(&path, "Kochbuch", &["kochen".to_string(), "essen".to_string()]).unwrap();

        assert!(new_path.ends_with("20250718T140125--kochbuch__kochen_essen.org"));
        assert!(!path.exists(), "the old name is gone");
        let content = std::fs::read_to_string(&new_path).unwrap();
        assert!(content.contains("#+TITLE:      Kochbuch\n"));
        assert!(content.contains("#+FILETAGS:   :kochen:essen:"));
        assert!(content.contains("#+IDENTIFIER: 20250718T140125"));
        assert!(content.contains("* Erste Aufgabe\nInhalt\n"), "the body is untouched");
        assert!(check_filename_sync(Path::new(&new_path)).unwrap().in_sync);
    }

    #[test]
    fn retitle_to_the_same_filename_only_rewrites_the_header() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), "20250718T140125--cookbook.org",
            "#+TITLE:      cookbook\n#+IDENTIFIER: 20250718T140125\n\n* \n");

        // Slugs to the name the file already has -- a case change only.
        let new_path = retitle_note(&path, "Cookbook", &[]).unwrap();

        assert_eq!(Path::new(&new_path), path);
        assert!(std::fs::read_to_string(&path).unwrap().contains("#+TITLE:      Cookbook\n"));
    }

    #[test]
    fn retitle_refuses_to_replace_an_existing_note() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), "20250718T140125--cookbook.org",
            "#+TITLE:      Cookbook\n#+IDENTIFIER: 20250718T140125\n\n* \n");
        write_file(tmp.path(), "20250718T140125--kochbuch.org", "#+TITLE: Kochbuch\n");

        let err = retitle_note(&path, "Kochbuch", &[]).unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        // Nothing touched: the header still says what the name says.
        assert!(std::fs::read_to_string(&path).unwrap().contains("#+TITLE:      Cookbook\n"));
    }

    #[test]
    fn retitle_needs_an_identifier() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), "actions.org", "#+TITLE: Actions\n\n* TODO Something\n");

        assert!(retitle_note(&path, "Aktionen", &[]).is_err());
        assert!(path.exists());
    }

    #[test]
    fn duplicate_non_denote_file_is_a_plain_copy_with_kopie_suffix() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), "image.png", "raw bytes");

        let new_path_str = duplicate_file(&path, DuplicateOverrides::default()).unwrap();
        let new_path = Path::new(&new_path_str);

        assert_eq!(new_path.file_name().unwrap(), "image (Kopie).png");
        assert_eq!(std::fs::read_to_string(new_path).unwrap(), "raw bytes");
    }

    #[test]
    fn duplicate_non_denote_file_increments_kopie_number_on_collision() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), "image.png", "raw bytes");
        write_file(tmp.path(), "image (Kopie).png", "already taken");

        let new_path_str = duplicate_file(&path, DuplicateOverrides::default()).unwrap();

        assert_eq!(Path::new(&new_path_str).file_name().unwrap(), "image (Kopie 2).png");
    }

    #[test]
    fn ignoring_creates_drawer_and_suppresses_future_prompts() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), "20250718T140125--cookbook__essen.org",
            "#+TITLE:      Kochbuch\n#+FILETAGS:   :essen:\n#+IDENTIFIER: 20250718T140125\n\n* \n");

        set_filename_sync_ignored(&path).unwrap();

        let status = check_filename_sync(&path).unwrap();
        assert!(!status.in_sync);
        assert!(status.ignored);
    }

    #[test]
    fn ignoring_reuses_existing_drawer_instead_of_duplicating() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), "20250718T140125--cookbook__essen.org",
            "#+TITLE:      Kochbuch\n#+FILETAGS:   :essen:\n#+IDENTIFIER: 20250718T140125\n:PROPERTIES:\n:CUSTOM_ID: abc\n:END:\n\n* \n");

        set_filename_sync_ignored(&path).unwrap();
        set_filename_sync_ignored(&path).unwrap();

        let lines = fs_utils::read_lines(&path).unwrap();
        let drawer_opens = lines.iter().filter(|l| l.trim() == ORG_PROPERTIES_START).count();
        let ignore_lines = lines.iter().filter(|l| l.trim().eq_ignore_ascii_case(":FILENAME_SYNC: ignore")).count();

        assert_eq!(drawer_opens, 1);
        assert_eq!(ignore_lines, 1);
        assert!(lines.iter().any(|l| l.trim() == ":CUSTOM_ID: abc"));
    }

    /* --- identifier_changed --- */

    #[test]
    fn identifier_changed_detects_a_real_change() {
        let old = "#+TITLE: Cookbook\n#+IDENTIFIER: 20250718T140125\n\n* \n";
        let new = "#+TITLE: Cookbook\n#+IDENTIFIER: 20260101T000000\n\n* \n";

        let change = identifier_changed(old, new).unwrap();
        assert_eq!(change.old_identifier, "20250718T140125");
        assert_eq!(change.new_identifier, "20260101T000000");
    }

    #[test]
    fn identifier_changed_ignores_title_and_tag_only_edits() {
        let old = "#+TITLE: Cookbook\n#+IDENTIFIER: 20250718T140125\n\n* \n";
        let new = "#+TITLE: Kochbuch\n#+FILETAGS: :essen:\n#+IDENTIFIER: 20250718T140125\n\n* \n";

        assert!(identifier_changed(old, new).is_none());
    }

    #[test]
    fn identifier_changed_none_when_either_side_missing() {
        let with_id = "#+TITLE: Cookbook\n#+IDENTIFIER: 20250718T140125\n\n* \n";
        let without_id = "#+TITLE: Actions\n\n* TODO Something\n";

        assert!(identifier_changed(with_id, without_id).is_none());
        assert!(identifier_changed(without_id, with_id).is_none());
    }

    /* --- replace_link_references --- */

    #[test]
    fn replace_link_references_rewrites_matching_files_only() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "a.org", "See [[id:OLD123][Cookbook]] for recipes.\nAnd [[id:OLD123]].\n");
        write_file(tmp.path(), "b.org", "Unrelated note, no links here.\n");
        write_file(tmp.path(), "c.org", "Also references [[id:OLD123][it]] once.\n");

        let changed = replace_link_references(tmp.path(), "OLD123", "NEW456").unwrap();
        assert_eq!(changed, 2);

        let a = std::fs::read_to_string(tmp.path().join("a.org")).unwrap();
        assert_eq!(a, "See [[id:NEW456][Cookbook]] for recipes.\nAnd [[id:NEW456]].\n");
        let c = std::fs::read_to_string(tmp.path().join("c.org")).unwrap();
        assert!(c.contains("[[id:NEW456][it]]"));
        let b = std::fs::read_to_string(tmp.path().join("b.org")).unwrap();
        assert_eq!(b, "Unrelated note, no links here.\n");
    }

    #[test]
    fn replace_link_references_no_matches_returns_zero() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "a.org", "No links in here.\n");

        assert_eq!(replace_link_references(tmp.path(), "OLD123", "NEW456").unwrap(), 0);
    }

    /* --- find_orphaned_custom_ids --- */

    fn heading_with_custom_id(file_path: &str, line_no: usize, title: &str, custom_id: &str) -> Heading {
        let mut properties = HashMap::new();
        properties.insert("CUSTOM_ID".to_string(), custom_id.to_string());
        Heading {
            id: format!("{file_path}:{line_no}"),
            file_path: file_path.to_string(),
            line_no,
            last_line: line_no,
            level: 1,
            title: title.to_string(),
            properties,
            ..Default::default()
        }
    }

    #[test]
    fn find_orphaned_custom_ids_ignores_a_referenced_heading() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "a.org",
            "See [[denote:20240118T060608::#h:eed0fb8e-4cc7-478f][Some test::Heading text]].\n");

        let mut by_file = HashMap::new();
        // The `h:` lives on the id's own value (see generate_custom_id) --
        // the link's search-string is compared to it verbatim, prefix
        // included, no separate marker stripped off either side.
        by_file.insert("a.org".to_string(), vec![heading_with_custom_id("a.org", 1, "Heading text", "h:eed0fb8e-4cc7-478f")]);

        let orphaned = find_orphaned_custom_ids(tmp.path(), &by_file).unwrap();
        assert!(orphaned.is_empty());
    }

    #[test]
    fn find_orphaned_custom_ids_still_matches_a_pre_prefix_bare_uuid_id_and_link() {
        // Backward compatibility: an id stamped before the h: prefix
        // convention existed stays a bare UUID forever (nothing migrates
        // it), and a link created against it at the time has no h: prefix
        // either -- both sides just need to agree with EACH OTHER, not
        // with the current generate_custom_id shape.
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "a.org", "See [[denote:20240118T060608::#abc-123][Some test::Heading text]].\n");

        let mut by_file = HashMap::new();
        by_file.insert("a.org".to_string(), vec![heading_with_custom_id("a.org", 1, "Heading text", "abc-123")]);

        assert!(find_orphaned_custom_ids(tmp.path(), &by_file).unwrap().is_empty());
    }

    #[test]
    fn find_orphaned_custom_ids_surfaces_an_unreferenced_heading() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "a.org", "No links to anything here.\n");

        let mut by_file = HashMap::new();
        by_file.insert("a.org".to_string(), vec![heading_with_custom_id("a.org", 3, "Orphan", "abc-123")]);

        let orphaned = find_orphaned_custom_ids(tmp.path(), &by_file).unwrap();
        assert_eq!(orphaned.len(), 1);
        assert_eq!(orphaned[0].custom_id, "abc-123");
        assert_eq!(orphaned[0].title, "Orphan");
        assert_eq!(orphaned[0].location.target.line_no, 3);
    }

    #[test]
    fn find_orphaned_custom_ids_finds_a_reference_from_a_different_file_than_the_target() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "a.org", "* Target\n");
        write_file(tmp.path(), "b.org", "Linked from elsewhere: [[denote:X::#h:shared-id][A::Target]]\n");

        let mut by_file = HashMap::new();
        by_file.insert("a.org".to_string(), vec![heading_with_custom_id("a.org", 1, "Target", "h:shared-id")]);

        assert!(find_orphaned_custom_ids(tmp.path(), &by_file).unwrap().is_empty());
    }

    #[test]
    fn find_orphaned_custom_ids_skips_headings_without_a_custom_id() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "a.org", "No links here.\n");

        let mut plain = heading_with_custom_id("a.org", 1, "Plain", "");
        plain.properties.clear();
        let mut by_file = HashMap::new();
        by_file.insert("a.org".to_string(), vec![plain]);

        assert!(find_orphaned_custom_ids(tmp.path(), &by_file).unwrap().is_empty());
    }

    /* --- find_unpromoted_org_files / promote_org_file_to_note --- */

    #[test]
    fn find_unpromoted_org_files_finds_a_plain_file_and_reads_its_own_title() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "install.org", "#+TITLE: Moodle PMHS Notizen\n\n* Generell\n");
        write_file(tmp.path(), "20240101T000000--real-note.org", "#+TITLE: Real Note\n#+IDENTIFIER: 20240101T000000\n\n* Heading\n");

        let found = find_unpromoted_org_files(tmp.path());
        assert_eq!(found.len(), 1, "the real Denote note is excluded: {found:?}");
        assert!(found[0].path.ends_with("install.org"));
        assert_eq!(found[0].title, "Moodle PMHS Notizen");
    }

    #[test]
    fn find_unpromoted_org_files_falls_back_to_the_filename_when_there_is_no_title() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "random-notes.org", "* Just a heading, no keywords at all\n");

        let found = find_unpromoted_org_files(tmp.path());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].title, "random-notes");
    }

    #[test]
    fn promote_org_file_to_note_stamps_an_identifier_renames_and_keeps_the_body() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), "install.org",
            "#+TITLE: Moodle PMHS Notizen\n#+AUTHOR: Dennis\n\n* Generell\n:PROPERTIES:\n:END:\n** Quellen\nSome content mentioning moodle.\n");

        let note = promote_org_file_to_note(&path).unwrap();

        assert!(!path.exists(), "old (un-Denote-named) path is gone");
        assert!(Path::new(&note.path).exists(), "renamed file exists at the new path");
        // Note.title is always DERIVED FROM THE FILENAME SLUG for every
        // Denote note in this app (see parse_note_record/
        // build_denote_filename) -- lowercase, dashes as spaces, same
        // lossy round-trip an ordinary generate_note_file/duplicate_file
        // note already goes through, not something this promotion path
        // does differently.
        assert_eq!(note.title, "moodle pmhs notizen");
        assert!(!note.identifier.is_empty());
        assert!(parse_note_record(Path::new(&note.path)).is_some(), "now recognized as a real note");

        let content = std::fs::read_to_string(&note.path).unwrap();
        assert!(content.contains("#+IDENTIFIER:"), "{content}");
        assert!(content.contains("#+AUTHOR: Dennis"), "unrelated preamble line preserved: {content}");
        assert!(content.contains("** Quellen\nSome content mentioning moodle."), "body preserved verbatim: {content}");
    }

    #[test]
    fn promote_org_file_to_note_adds_a_title_too_when_the_file_has_none() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), "random-notes.org", "* Just a heading\nSome body.\n");

        let note = promote_org_file_to_note(&path).unwrap();
        assert_eq!(note.title, "random notes", "lossy filename-slug round-trip, see the other promote test's own comment");
        let content = std::fs::read_to_string(&note.path).unwrap();
        assert!(content.contains("#+TITLE:"), "{content}");
        assert!(content.contains("#+IDENTIFIER:"), "{content}");
    }

    /* --- find_empty_properties_drawers --- */

    fn make_heading_for_drawer_test(file_path: &str, line_no: usize, title: &str) -> Heading {
        Heading {
            id: format!("{file_path}:{line_no}"),
            file_path: file_path.to_string(),
            line_no,
            last_line: line_no + 2,
            level: 1,
            title: title.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn find_empty_properties_drawers_finds_an_empty_one_but_not_a_populated_one() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), "a.org",
            "* Empty\n:PROPERTIES:\n:END:\n* Populated\n:PROPERTIES:\n:EFFORT: 0:30\n:END:\n");
        let path_str = path.to_string_lossy().into_owned();

        let mut by_file = HashMap::new();
        by_file.insert(path_str.clone(), vec![
            make_heading_for_drawer_test(&path_str, 1, "Empty"),
            make_heading_for_drawer_test(&path_str, 4, "Populated"),
        ]);

        let found = find_empty_properties_drawers(&by_file).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].title, "Empty");
        assert_eq!(found[0].location.target.line_no, 1);
    }

    #[test]
    fn find_empty_properties_drawers_ignores_a_heading_with_no_drawer_at_all() {
        let tmp = TempDir::new().unwrap();
        let path = write_file(tmp.path(), "a.org", "* Heading\nJust body text, no drawer.\n");
        let path_str = path.to_string_lossy().into_owned();

        let mut by_file = HashMap::new();
        by_file.insert(path_str.clone(), vec![make_heading_for_drawer_test(&path_str, 1, "Heading")]);

        assert!(find_empty_properties_drawers(&by_file).unwrap().is_empty());
    }
}