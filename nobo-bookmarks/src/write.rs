//! Writing bookmarks: a new entry, an edit, a move, a removal.
//!
//! Each one changes the lines of the one bookmark it is about and leaves
//! every other line of the file as it was -- prose between collections,
//! code blocks, properties the app does not know, the file's line endings.

use nobo_core::core::org_insert::insert_under;
use nobo_core::core::vault::{closed_block_lines, format_heading_line};
use nobo_core::t;

use crate::model::{Draft, Patch, Target};
use crate::parse::{self, Entry, READ_LATER};

/// TAG as org can read it: letters, digits, `_`, `@`, `#`, `%` -- the rest
/// (a `-`, a space) becomes `_`. `web-dev` is `web_dev`; `:web-dev:` is no
/// tag at all to org. `None` for what is left empty.
pub fn org_tag(tag: &str) -> Option<String> {
    let tag: String = tag
        .trim()
        .trim_matches(':')
        .chars()
        .map(|c| if c.is_alphanumeric() || "_@#%".contains(c) { c } else { '_' })
        .collect();
    let tag = tag.trim_matches('_').to_string();
    (!tag.is_empty()).then_some(tag)
}

/// The tags a heading line gets: TAGS made valid, without duplicates,
/// `readlater` last when READ_LATER.
fn heading_tags(tags: &[String], read_later: bool) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for tag in tags.iter().filter_map(|tag| org_tag(tag)) {
        if tag != READ_LATER && !out.contains(&tag) {
            out.push(tag);
        }
    }
    if read_later {
        out.push(READ_LATER.to_string());
    }
    out
}

/// One line, as a title must be: a line break would end the heading.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn property(key: &str, value: &str) -> String {
    format!("{:<10} {}", format!(":{key}:"), one_line(value))
}

/// The note as body lines that cannot be read as structure: a line
/// starting with `*` and a space would be a heading, and the quote markers
/// are what separates the note from the description.
fn note_lines(note: &str) -> Vec<String> {
    note.trim()
        .lines()
        .map(|line| {
            let line = line.trim_end();
            let looks_like_structure = (line.starts_with('*') && line.trim_start_matches('*').starts_with(' '))
                || line.trim().to_ascii_lowercase().starts_with("#+begin_quote")
                || line.trim().to_ascii_lowercase().starts_with("#+end_quote");
            if looks_like_structure { format!(" {line}") } else { line.to_string() }
        })
        .collect()
}

/// DRAFT as an org entry at LEVEL, saved at CREATED.
pub fn entry(draft: &Draft, level: usize, created: &str) -> String {
    entry_with(draft, level, created, None)
}

/// The same, with when it was last changed -- what an import carries over
/// from where the bookmark lived before.
pub fn entry_with(draft: &Draft, level: usize, created: &str, updated: Option<&str>) -> String {
    let mut out = vec![
        format_heading_line(level.max(1), &one_line(&draft.title), &heading_tags(&draft.tags, draft.read_later)),
        ":PROPERTIES:".to_string(),
        property("URL", &draft.url),
        property("CREATED", created),
    ];
    if let Some(updated) = updated {
        out.push(property("UPDATED", updated));
    }
    out.push(":END:".to_string());
    out.extend(note_lines(&draft.note));
    if let Some(description) = draft.description.as_deref().map(one_line).filter(|text| !text.is_empty()) {
        if !draft.note.trim().is_empty() {
            out.push(String::new());
        }
        out.push("#+begin_quote".to_string());
        out.push(description);
        out.push("#+end_quote".to_string());
    }
    let mut text = out.join("\n");
    text.push('\n');
    text
}

/// A file's text as lines, with what it takes to put it back together
/// exactly: its line ending and whether it ended in one.
struct Lines {
    lines: Vec<String>,
    eol: &'static str,
    trailing: bool,
}

impl Lines {
    fn of(text: &str) -> Lines {
        Lines {
            lines: text.lines().map(str::to_string).collect(),
            eol: if text.contains("\r\n") { "\r\n" } else { "\n" },
            trailing: text.ends_with('\n'),
        }
    }

    fn text(&self) -> String {
        let mut out = self.lines.join(self.eol);
        if self.trailing && !out.is_empty() {
            out.push_str(self.eol);
        }
        out
    }
}

/// The bookmark TARGET names in TEXT.
///
/// At its line, if the bookmark there still points where it did; else the
/// one bookmark in the file that does -- lines move when the file is
/// edited elsewhere. Refused when there is none, or several: an edit must
/// never land on a different bookmark than the one it was meant for.
pub fn locate(text: &str, target: &Target) -> Result<Entry, String> {
    let entries = parse::entries(&target.file_path, "", text);
    if let Some(entry) = entries.iter().find(|entry| entry.bookmark.line_no == target.line_no && entry.bookmark.url == target.url) {
        return Ok(entry.clone());
    }
    let mut same_url = entries.into_iter().filter(|entry| entry.bookmark.url == target.url);
    match (same_url.next(), same_url.next()) {
        (Some(entry), None) => Ok(entry),
        (Some(_), Some(_)) => Err(t!(
            "Dieses Lesezeichen steht mehrfach in der Datei und hat seinen Platz verlassen – bitte die Ansicht neu laden.",
            "This bookmark is in the file more than once and has moved -- please reload the view."
        )),
        _ => Err(t!(
            "Das Lesezeichen ist nicht mehr da – die Datei wurde inzwischen geändert.",
            "The bookmark is gone -- the file has changed in the meantime."
        )),
    }
}

/// TEXT with PATCH applied to the bookmark TARGET names. Returns the new
/// text and the bookmark's line (1-based).
pub fn apply(text: &str, target: &Target, patch: &Patch) -> Result<(String, usize), String> {
    let entry = locate(text, target)?;
    let mut file = Lines::of(text);
    let old = &entry.bookmark;

    if patch.title.is_some() || patch.tags.is_some() || patch.read_later.is_some() {
        let title = patch.title.as_deref().map(one_line).filter(|title| !title.is_empty()).unwrap_or_else(|| old.title.clone());
        let tags = patch.tags.clone().unwrap_or_else(|| old.tags.clone());
        let read_later = patch.read_later.unwrap_or(old.read_later);
        file.lines[entry.heading] = format_heading_line(entry.level, &title, &heading_tags(&tags, read_later));
    }
    if let Some(url) = patch.url.as_deref().map(str::trim).filter(|url| !url.is_empty()) {
        file.lines[entry.url_line] = property("URL", url);
    }
    if let Some(note) = &patch.note {
        let mut replacement = note_lines(note);
        // A blank line between the note and the quote under it, as a new
        // entry has; none when there is no quote to keep apart.
        let quote_follows = entry.note_end < file.lines.len()
            && file.lines[entry.note_end].trim().eq_ignore_ascii_case("#+begin_quote");
        if quote_follows && !replacement.is_empty() {
            replacement.push(String::new());
        }
        file.lines.splice(entry.body_start..entry.note_end, replacement);
    }
    Ok((file.text(), entry.heading + 1))
}

/// TEXT without the bookmark TARGET names (with whatever is under it and
/// the blank lines after it), and those lines with its level -- for a
/// move.
pub fn remove(text: &str, target: &Target) -> Result<(String, Vec<String>, usize), String> {
    let entry = locate(text, target)?;
    let mut file = Lines::of(text);
    // Up to the next heading at its level or above -- blank lines before
    // that heading are its own and go with it.
    let block: Vec<String> = file.lines[entry.heading..entry.subtree_end].to_vec();
    file.lines.drain(entry.heading..entry.subtree_end);
    Ok((file.text(), block, entry.level))
}

/// BLOCK (a bookmark with what is under it, at FROM) moved to level TO:
/// every heading in it shifted by the same amount, code blocks left alone.
pub fn relevel(block: &[String], from: usize, to: usize) -> String {
    let in_block = closed_block_lines(block);
    let mut out: Vec<String> = block
        .iter()
        .enumerate()
        .map(|(index, line)| {
            let stars = line.chars().take_while(|c| *c == '*').count();
            let is_heading = !in_block[index] && stars > 0 && line[stars..].starts_with(' ');
            if !is_heading {
                return line.clone();
            }
            let level = (stars + to).saturating_sub(from).max(1);
            format!("{}{}", "*".repeat(level), &line[stars..])
        })
        .collect();
    while out.last().is_some_and(|line| line.trim().is_empty()) {
        out.pop();
    }
    let mut text = out.join("\n");
    text.push('\n');
    text
}

/// Moving within one file: TEXT with the bookmark TARGET names taken out
/// and put under COLLECTION. Returns the new text and its new line.
pub fn move_within(text: &str, target: &Target, collection: &[String]) -> Result<(String, usize), String> {
    let (without, block, level) = remove(text, target)?;
    Ok(insert_under(&without, collection, |to| relevel(&block, level, to)))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: &str = "#+TITLE: Arbeit\n#+NOBO_TYPE: bookmarks\n\n* Rust\n** reqwest :http:\n:PROPERTIES:\n:URL:      https://docs.rs/reqwest\n:OWN:      bleibt\n:END:\nAlte Notiz.\n\n#+begin_quote\nAn HTTP client.\n#+end_quote\n** serde\n:PROPERTIES:\n:URL: https://serde.rs\n:END:\n* Reisen\nText über Reisen.\n";

    fn target(line_no: usize, url: &str) -> Target {
        Target { file_path: "/v/a.org".into(), line_no, url: url.into() }
    }

    #[test]
    fn a_new_entry_is_org_chef_shaped_and_safe() {
        let draft = Draft {
            title: "reqwest\n– Rust".into(),
            url: "https://docs.rs/reqwest".into(),
            tags: vec!["web-dev".into(), "http".into(), "http".into()],
            read_later: true,
            note: "Warum:\n* nicht vergessen".into(),
            description: Some("An HTTP\nclient.".into()),
        };
        let text = entry(&draft, 2, "[2026-10-12 Mo 14:30]");
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].starts_with("** reqwest – Rust") && lines[0].ends_with(":web_dev:http:readlater:"), "{text}");
        assert_eq!(lines[2], ":URL:      https://docs.rs/reqwest");
        assert_eq!(lines[3], ":CREATED:  [2026-10-12 Mo 14:30]");
        assert_eq!(lines[6], " * nicht vergessen", "a starred note line is no heading");
        assert_eq!(&lines[8..], ["#+begin_quote", "An HTTP client.", "#+end_quote"]);
    }

    #[test]
    fn an_edit_changes_its_own_lines_and_nothing_else() {
        let (out, line) = apply(FILE, &target(5, "https://docs.rs/reqwest"), &Patch {
            title: Some("reqwest – Rust".into()),
            read_later: Some(true),
            note: Some("Neue Notiz.".into()),
            ..Patch::default()
        }).unwrap();
        assert_eq!(line, 5);
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[4].starts_with("** reqwest – Rust") && lines[4].ends_with(":http:readlater:"), "{out}");
        assert_eq!(lines[7], ":OWN:      bleibt", "a property the app does not know stays");
        assert_eq!(&lines[9..14], ["Neue Notiz.", "", "#+begin_quote", "An HTTP client.", "#+end_quote"]);
        assert!(out.ends_with("* Reisen\nText über Reisen.\n"));
    }

    #[test]
    fn an_edit_finds_its_bookmark_after_the_file_moved_on() {
        let shifted = format!("Neue Zeile oben.\n{FILE}");
        let (out, line) = apply(&shifted, &target(5, "https://docs.rs/reqwest"), &Patch { url: Some("https://docs.rs/reqwest/0.13".into()), ..Patch::default() }).unwrap();
        assert_eq!(line, 6);
        assert!(out.contains(":URL:      https://docs.rs/reqwest/0.13"));
    }

    #[test]
    fn an_edit_is_refused_when_the_bookmark_is_gone() {
        let err = apply(FILE, &target(5, "https://weg.example"), &Patch::default()).unwrap_err();
        assert!(!err.is_empty());
    }

    #[test]
    fn removing_takes_the_bookmark_and_its_gap() {
        let (out, block, level) = remove(FILE, &target(15, "https://serde.rs")).unwrap();
        assert_eq!(level, 2);
        assert_eq!(block[0], "** serde");
        assert!(out.contains("#+end_quote\n* Reisen"), "{out}");
    }

    #[test]
    fn moving_files_it_under_another_collection_at_the_right_level() {
        let (out, line) = move_within(FILE, &target(15, "https://serde.rs"), &["Reisen".to_string()]).unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[line - 1], "** serde");
        assert!(out.ends_with("Text über Reisen.\n\n** serde\n:PROPERTIES:\n:URL: https://serde.rs\n:END:\n"), "{out}");
        let (top, _) = move_within(FILE, &target(15, "https://serde.rs"), &[]).unwrap();
        assert!(top.ends_with("\n* serde\n:PROPERTIES:\n:URL: https://serde.rs\n:END:\n"), "{top}");
    }

    #[test]
    fn crlf_stays_crlf() {
        let crlf = FILE.replace('\n', "\r\n");
        let (out, _) = apply(&crlf, &target(5, "https://docs.rs/reqwest"), &Patch { read_later: Some(true), ..Patch::default() }).unwrap();
        assert!(!out.replace("\r\n", "").contains('\n'));
    }

    /// What is saved is what is read back -- under a collection, between
    /// other entries, with nothing around it disturbed.
    #[test]
    fn a_saved_bookmark_reads_back_as_it_was_written() {
        let draft = Draft {
            title: "tokio".into(),
            url: "https://tokio.rs".into(),
            tags: vec!["async".into()],
            read_later: true,
            note: "Runtime.".into(),
            description: Some("An asynchronous runtime.".into()),
        };
        let (out, line) = insert_under(FILE, &["Rust".to_string()], |level| entry(&draft, level, "[2026-10-12 Mo 14:30]"));
        let all = parse::bookmarks_of("/v/a.org", "Arbeit", &out);
        let tokio = all.iter().find(|bookmark| bookmark.url == "https://tokio.rs").expect("read back");
        assert_eq!(tokio.line_no, line);
        assert_eq!(tokio.title, "tokio");
        assert_eq!(tokio.tags, ["async"]);
        assert!(tokio.read_later);
        assert_eq!(tokio.note, "Runtime.");
        assert_eq!(tokio.description.as_deref(), Some("An asynchronous runtime."));
        assert_eq!(tokio.collection, ["Rust"]);
        assert_eq!(tokio.created.as_deref(), Some("[2026-10-12 Mo 14:30]"));
        assert_eq!(all.len(), 3, "the other two are still there");
        assert!(out.contains("* Reisen\nText über Reisen.\n"));
    }

    #[test]
    fn tags_are_made_valid_for_org() {
        assert_eq!(org_tag(" web-dev "), Some("web_dev".into()));
        assert_eq!(org_tag(":rust:"), Some("rust".into()));
        assert_eq!(org_tag(" - "), None);
    }
}
