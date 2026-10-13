//! Writing bookmarks: a new entry, an edit, a move, a removal.
//!
//! Each one changes the lines of the one bookmark it is about and leaves
//! every other line of the file as it was -- prose between collections,
//! code blocks, properties the app does not know, a TODO keyword someone
//! put on a bookmark in Emacs, each line's own line ending.

use nobo_core::core::org_insert::insert_under_heading;
use nobo_core::core::org_syntax::{escape_body_line, one_line, property_line};
use nobo_core::core::vault::{closed_block_lines, format_heading, format_heading_line, parse_heading_line};
use nobo_core::t;

use crate::model::{Draft, Patch, Target};
use crate::parse::{self, display_title, Entry, READ_LATER};

/// TAG as org can read it: letters, digits, `_`, `@`, `#`, `%` -- the rest
/// (a `-`, a space) becomes `_`. `web-dev` is `web_dev`; `:web-dev:` is no
/// tag at all to org. `None` for what is left empty.
pub fn org_tag(tag: &str) -> Option<String> {
    let tag: String =
        tag.trim().trim_matches(':').chars().map(|c| if c.is_alphanumeric() || "_@#%".contains(c) { c } else { '_' }).collect();
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

/// What org reads back as the title of a heading called NAME: a name
/// from elsewhere (a Linkwarden collection, a typed "Neue Sammlung")
/// can end in what looks like tags (`Work :x:`) or start with a TODO
/// keyword, and org takes those off the title. Collections are compared
/// this way, so a name finds the heading it was written as.
pub fn as_read(name: &str) -> String {
    parse_heading_line(&format!("* {}", one_line(name))).map(|heading| heading.title).unwrap_or_default()
}

/// The note as body lines that cannot be read as structure: no line of it
/// can start a heading or a block (see org_syntax::escape_body_line, which
/// the reader undoes).
fn note_lines(note: &str) -> Vec<String> {
    note.trim().lines().map(|line| escape_body_line(line.trim_end())).collect()
}

/// The quote block a page's DESCRIPTION is kept in -- none for an empty one.
fn quote_lines(description: Option<&str>) -> Vec<String> {
    match description.map(one_line).filter(|text| !text.is_empty()) {
        Some(text) => vec!["#+begin_quote".to_string(), escape_body_line(&text), "#+end_quote".to_string()],
        None => Vec::new(),
    }
}

/// Note and description as an entry's body: a blank line between them.
fn body(mut note: Vec<String>, quote: Vec<String>) -> Vec<String> {
    while note.last().is_some_and(|line| line.trim().is_empty()) {
        note.pop();
    }
    if !note.is_empty() && !quote.is_empty() {
        note.push(String::new());
    }
    note.extend(quote);
    note
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
        property_line("URL", &draft.url),
        property_line("CREATED", created),
    ];
    if let Some(updated) = updated {
        out.push(property_line("UPDATED", updated));
    }
    out.push(":END:".to_string());
    out.extend(body(note_lines(&draft.note), quote_lines(draft.description.as_deref())));
    let mut text = out.join("\n");
    text.push('\n');
    text
}

/// A file's text as lines, each with its own line ending -- a file that
/// mixes them (a sync client that wrote some lines with CRLF) keeps every
/// line it does not change exactly as it was. New lines get the ending
/// most of the file has.
struct Lines {
    lines: Vec<(String, &'static str)>,
    eol: &'static str,
}

impl Lines {
    fn of(text: &str) -> Lines {
        let lines: Vec<(String, &'static str)> = text
            .split_inclusive('\n')
            .map(|piece| match piece.strip_suffix("\r\n") {
                Some(line) => (line.to_string(), "\r\n"),
                None => match piece.strip_suffix('\n') {
                    Some(line) => (line.to_string(), "\n"),
                    None => (piece.to_string(), ""),
                },
            })
            .collect();
        let crlf = lines.iter().filter(|(_, eol)| *eol == "\r\n").count();
        let lf = lines.iter().filter(|(_, eol)| *eol == "\n").count();
        Lines { lines, eol: if crlf > lf { "\r\n" } else { "\n" } }
    }

    fn get(&self, at: usize) -> &str {
        &self.lines[at].0
    }

    /// Line AT says CONTENT now; its ending stays.
    fn set(&mut self, at: usize, content: String) {
        self.lines[at].0 = content;
    }

    fn splice(&mut self, range: std::ops::Range<usize>, new: Vec<String>) {
        let eol = self.eol;
        // A file without a final line break keeps having none.
        let last_ending = (range.end == self.lines.len()).then(|| self.lines.last().map(|(_, eol)| *eol)).flatten();
        let mut new: Vec<(String, &'static str)> = new.into_iter().map(|line| (line, eol)).collect();
        if let (Some(ending), Some(last)) = (last_ending, new.last_mut()) {
            last.1 = ending;
        }
        self.lines.splice(range, new);
    }

    fn text(&self) -> String {
        self.lines.iter().map(|(line, eol)| format!("{line}{eol}")).collect()
    }
}

/// The leading whitespace of LINE -- an indented drawer stays indented.
fn indent_of(line: &str) -> &str {
    &line[..line.len() - line.trim_start().len()]
}

/// The bookmark TARGET names in TEXT.
///
/// At its line, if the bookmark there still points where it did; else the
/// one bookmark in the file that does -- lines move when the file is
/// edited elsewhere. Refused when there is none, or several, or when the
/// entry has changed since it was read (its revision): an edit must never
/// land on a different bookmark than the one it was meant for, nor write
/// over what someone else just wrote.
pub fn locate(text: &str, target: &Target) -> Result<Entry, String> {
    let entries = parse::entries(&target.file_path, "", text);
    let at_line = entries.iter().find(|entry| entry.bookmark.line_no == target.line_no && entry.bookmark.url == target.url).cloned();
    let entry = match at_line {
        Some(entry) => entry,
        None => {
            let mut same_url = entries.into_iter().filter(|entry| entry.bookmark.url == target.url);
            match (same_url.next(), same_url.next()) {
                (Some(entry), None) => entry,
                (Some(_), Some(_)) => {
                    return Err(t!(
                        "Dieses Lesezeichen steht mehrfach in der Datei und hat seinen Platz verlassen – bitte die Ansicht neu laden.",
                        "This bookmark is in the file more than once and has moved -- please reload the view."
                    ))
                }
                _ => {
                    return Err(t!(
                        "Das Lesezeichen ist nicht mehr da – die Datei wurde inzwischen geändert.",
                        "The bookmark is gone -- the file has changed in the meantime."
                    ))
                }
            }
        }
    };
    if !target.revision.is_empty() && entry.bookmark.revision != target.revision {
        return Err(t!(
            "Das Lesezeichen wurde inzwischen anderswo geändert – die Ansicht ist neu geladen, bitte noch einmal.",
            "The bookmark was changed elsewhere in the meantime -- the view has been reloaded, please try again."
        ));
    }
    Ok(entry)
}

/// TEXT with PATCH applied to the bookmark TARGET names, `:UPDATED:` set
/// to NOW. Returns the new text and the bookmark's line (1-based).
pub fn apply(text: &str, target: &Target, patch: &Patch, now: &str) -> Result<(String, usize), String> {
    let entry = locate(text, target)?;
    let mut file = Lines::of(text);
    let old = &entry.bookmark;

    // Bottom up, so the line numbers above stay valid: body, drawer, heading.
    if patch.note.is_some() || patch.description.is_some() {
        let note = match &patch.note {
            Some(note) => note_lines(note),
            None => (entry.body_start..entry.note_end).map(|at| file.get(at).to_string()).collect(),
        };
        let (quote, body_end) = match entry.quote {
            Some((start, end)) => ((start..=end).map(|at| file.get(at).to_string()).collect(), end + 1),
            None => (Vec::new(), entry.note_end),
        };
        let quote = match &patch.description {
            Some(description) => quote_lines(Some(description)),
            None => quote,
        };
        file.splice(entry.body_start.min(body_end)..body_end, body(note, quote));
    }

    let indent = indent_of(file.get(entry.url_line)).to_string();
    let changes_anything = patch != &Patch::default();
    if changes_anything {
        let updated = format!("{indent}{}", property_line("UPDATED", now));
        let existing = (entry.url_line..entry.drawer_end)
            .find(|&at| file.get(at).trim_start().get(..9).is_some_and(|key| key.eq_ignore_ascii_case(":UPDATED:")));
        match existing {
            Some(at) => file.set(at, updated),
            None => file.splice(entry.drawer_end..entry.drawer_end, vec![updated]),
        }
    }
    if let Some(url) = patch.url.as_deref().map(str::trim).filter(|url| !url.is_empty()) {
        file.set(entry.url_line, format!("{indent}{}", property_line("URL", url)));
    }

    if patch.title.is_some() || patch.tags.is_some() || patch.read_later.is_some() {
        let mut heading = entry.heading_line.clone();
        // The title is only rewritten when it changed from what was shown:
        // a link in it (`[[url][Name]]`, shown as "Name") stays a link.
        if let Some(title) =
            patch.title.as_deref().map(one_line).filter(|title| !title.is_empty() && *title != display_title(&heading.title))
        {
            heading.title = title;
        }
        let tags = patch.tags.clone().unwrap_or_else(|| old.tags.clone());
        heading.tags = heading_tags(&tags, patch.read_later.unwrap_or(old.read_later));
        file.set(entry.heading, format_heading(&heading));
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
    let block: Vec<String> = (entry.heading..entry.subtree_end).map(|at| file.get(at).to_string()).collect();
    file.splice(entry.heading..entry.subtree_end, Vec::new());
    Ok((file.text(), block, entry.heading_line.level))
}

/// BLOCK (a bookmark with what is under it, at FROM) moved to level TO:
/// every heading in it shifted by the same amount, code blocks left alone.
pub fn relevel(block: &[String], from: usize, to: usize) -> String {
    let in_block = closed_block_lines(block);
    let mut out: Vec<String> = block
        .iter()
        .enumerate()
        .map(|(index, line)| match parse_heading_line(line).filter(|_| !in_block[index]) {
            Some(heading) => {
                let level = (heading.level + to).saturating_sub(from).max(1);
                format!("{}{}", "*".repeat(level), &line[heading.level..])
            }
            None => line.clone(),
        })
        .collect();
    while out.last().is_some_and(|line| line.trim().is_empty()) {
        out.pop();
    }
    let mut text = out.join("\n");
    text.push('\n');
    text
}

/// TEXT with the collection PATH there: every missing level added at the
/// end of its parent, with DESCRIPTION under the last one when it is new.
/// Returns the text and the collection's heading line (1-based; `None` for
/// the top level, the empty path).
pub fn ensure_collection(text: String, path: &[String], description: Option<&str>) -> (String, Option<usize>) {
    if path.is_empty() {
        return (text, None);
    }
    let wanted: Vec<String> = path.iter().map(|name| as_read(name)).collect();
    if let Some(found) = parse::outline("", "", &text).collection(&wanted) {
        return (text.clone(), Some(found.heading + 1));
    }
    let (parent, name) = path.split_at(path.len() - 1);
    let (text, parent_line) = ensure_collection(text, parent, None);
    let name = one_line(&name[0]);
    let (text, line) = insert_under_heading(&text, parent_line, |level| {
        let mut lines = vec![format!("{} {name}", "*".repeat(level.max(1)))];
        lines.extend(note_lines(description.unwrap_or("")));
        lines.join("\n") + "\n"
    });
    (text, Some(line))
}

/// TEXT with what BLOCK builds (for the level it lands on) at the end of
/// COLLECTION, which is made if it is not there. Returns the text and the
/// block's first line (1-based).
pub fn file_under(text: &str, collection: &[String], block: impl Fn(usize) -> String) -> (String, usize) {
    let (text, heading) = ensure_collection(text.to_string(), collection, None);
    insert_under_heading(&text, heading, block)
}

/// Moving within one file: TEXT with the bookmark TARGET names taken out
/// and put under COLLECTION. Returns the new text and its new line.
pub fn move_within(text: &str, target: &Target, collection: &[String]) -> Result<(String, usize), String> {
    let (without, block, level) = remove(text, target)?;
    Ok(file_under(&without, collection, |to| relevel(&block, level, to)))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: &str = "#+TITLE: Arbeit\n#+NOBO_TYPE: bookmarks\n\n* Rust\n** reqwest :http:\n:PROPERTIES:\n:URL:      https://docs.rs/reqwest\n:OWN:      bleibt\n:END:\nAlte Notiz.\n\n#+begin_quote\nAn HTTP client.\n#+end_quote\n** serde\n:PROPERTIES:\n:URL: https://serde.rs\n:END:\n* Reisen\nText über Reisen.\n";
    const NOW: &str = "[2026-10-13 Di 09:00]";

    fn target(line_no: usize, url: &str) -> Target {
        Target { file_path: "/v/a.org".into(), line_no, url: url.into(), revision: String::new() }
    }

    fn read(text: &str, url: &str) -> crate::model::Bookmark {
        parse::bookmarks_of("/v/a.org", "A", text).into_iter().find(|b| b.url == url).expect("read back")
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
        let (out, line) = apply(
            FILE,
            &target(5, "https://docs.rs/reqwest"),
            &Patch { title: Some("reqwest – Rust".into()), read_later: Some(true), note: Some("Neue Notiz.".into()), ..Patch::default() },
            NOW,
        )
        .unwrap();
        assert_eq!(line, 5);
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[4].starts_with("** reqwest – Rust") && lines[4].ends_with(":http:readlater:"), "{out}");
        assert_eq!(lines[7], ":OWN:      bleibt", "a property the app does not know stays");
        assert_eq!(lines[8], ":UPDATED:  [2026-10-13 Di 09:00]");
        assert_eq!(&lines[10..15], ["Neue Notiz.", "", "#+begin_quote", "An HTTP client.", "#+end_quote"]);
        assert!(out.ends_with("* Reisen\nText über Reisen.\n"));
    }

    #[test]
    fn an_edit_finds_its_bookmark_after_the_file_moved_on() {
        let shifted = format!("Neue Zeile oben.\n{FILE}");
        let (out, line) = apply(
            &shifted,
            &target(5, "https://docs.rs/reqwest"),
            &Patch { url: Some("https://docs.rs/reqwest/0.13".into()), ..Patch::default() },
            NOW,
        )
        .unwrap();
        assert_eq!(line, 6);
        assert!(out.contains(":URL:      https://docs.rs/reqwest/0.13"));
    }

    #[test]
    fn an_edit_is_refused_when_the_bookmark_is_gone() {
        let err = apply(FILE, &target(5, "https://weg.example"), &Patch::default(), NOW).unwrap_err();
        assert!(!err.is_empty());
    }

    /// Someone changed the note in Emacs after the view loaded: writing
    /// the view's note over it would lose that.
    #[test]
    fn an_edit_is_refused_when_the_entry_changed_meanwhile() {
        let seen = read(FILE, "https://docs.rs/reqwest").revision;
        let changed = FILE.replace("Alte Notiz.", "In Emacs geändert.");
        let stale = Target { revision: seen.clone(), ..target(5, "https://docs.rs/reqwest") };
        assert!(apply(&changed, &stale, &Patch { note: Some("x".into()), ..Patch::default() }, NOW).is_err());
        assert!(apply(FILE, &stale, &Patch { note: Some("x".into()), ..Patch::default() }, NOW).is_ok());
    }

    #[test]
    fn an_edit_keeps_todo_priority_and_cookie() {
        let text = FILE.replace("** reqwest :http:", "** TODO [#A] reqwest [1/2] :http:");
        let (out, _) =
            apply(&text, &target(5, "https://docs.rs/reqwest"), &Patch { read_later: Some(true), ..Patch::default() }, NOW).unwrap();
        let heading = out.lines().nth(4).unwrap();
        assert!(heading.starts_with("** TODO [#A] reqwest [1/2]") && heading.ends_with(":http:readlater:"), "{heading}");
    }

    #[test]
    fn a_link_in_the_title_survives_an_edit_that_keeps_the_title() {
        let text = FILE.replace("** reqwest :http:", "** [[https://docs.rs][reqwest]] :http:");
        let (out, _) = apply(
            &text,
            &target(5, "https://docs.rs/reqwest"),
            &Patch { title: Some("reqwest".into()), tags: Some(vec![]), ..Patch::default() },
            NOW,
        )
        .unwrap();
        assert!(out.lines().nth(4).unwrap().starts_with("** [[https://docs.rs][reqwest]]"), "{out}");
    }

    /// A note that starts with emphasis or holds a quote block of its own
    /// reads back as it was written -- and takes nothing after it along.
    #[test]
    fn a_note_with_org_syntax_round_trips() {
        let note = "*wichtig* lesen\n#+begin_quote\nZitat\n#+end_quote\n* kein Kapitel";
        let (out, _) =
            apply(FILE, &target(5, "https://docs.rs/reqwest"), &Patch { note: Some(note.into()), ..Patch::default() }, NOW).unwrap();
        let reqwest = read(&out, "https://docs.rs/reqwest");
        assert_eq!(reqwest.note, note);
        assert_eq!(reqwest.description.as_deref(), Some("An HTTP client."));
        assert_eq!(read(&out, "https://serde.rs").collection, ["Rust"], "the next one is still where it was");
        assert_eq!(parse::collections_of(&out).len(), 2, "no collection made up from the note");
    }

    #[test]
    fn the_description_can_be_changed_and_removed() {
        let (out, _) =
            apply(FILE, &target(5, "https://docs.rs/reqwest"), &Patch { description: Some("Neu.".into()), ..Patch::default() }, NOW)
                .unwrap();
        let reqwest = read(&out, "https://docs.rs/reqwest");
        assert_eq!((reqwest.note.as_str(), reqwest.description.as_deref()), ("Alte Notiz.", Some("Neu.")));
        let (out, _) =
            apply(&out, &target(5, "https://docs.rs/reqwest"), &Patch { description: Some(String::new()), ..Patch::default() }, NOW)
                .unwrap();
        assert_eq!(read(&out, "https://docs.rs/reqwest").description, None);
        assert!(!out.contains("#+begin_quote"), "{out}");
    }

    #[test]
    fn an_indented_drawer_stays_indented() {
        let text = "* B\n   :PROPERTIES:\n   :URL: https://x\n   :END:\n";
        let (out, _) = apply(text, &target(1, "https://x"), &Patch { url: Some("https://y".into()), ..Patch::default() }, NOW).unwrap();
        assert_eq!(out, "* B\n   :PROPERTIES:\n   :URL:      https://y\n   :UPDATED:  [2026-10-13 Di 09:00]\n   :END:\n");
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
        let (out, _) =
            apply(&crlf, &target(5, "https://docs.rs/reqwest"), &Patch { read_later: Some(true), ..Patch::default() }, NOW).unwrap();
        assert!(!out.replace("\r\n", "").contains('\n'));
    }

    /// One CRLF line in an LF file stays the one; the rest stays LF.
    #[test]
    fn mixed_line_endings_stay_as_they_were() {
        let mixed = FILE.replacen("* Reisen\n", "* Reisen\r\n", 1);
        let (out, _) =
            apply(&mixed, &target(5, "https://docs.rs/reqwest"), &Patch { read_later: Some(true), ..Patch::default() }, NOW).unwrap();
        assert_eq!(out.matches("\r\n").count(), 1, "{out:?}");
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
        let (out, line) = file_under(FILE, &["Rust".to_string()], |level| entry(&draft, level, "[2026-10-12 Mo 14:30]"));
        let all = parse::bookmarks_of("/v/a.org", "Arbeit", &out);
        let tokio = read(&out, "https://tokio.rs");
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

    /// A collection that is not there yet is made, level by level --
    /// rather than the entry landing at the top level unannounced.
    #[test]
    fn a_new_collection_is_made_where_it_is_asked_for() {
        let path = ["Rust".to_string(), "Async".to_string()];
        let (out, line) = file_under(FILE, &path, |level| {
            entry(&Draft { title: "tokio".into(), url: "https://tokio.rs".into(), ..Draft::default() }, level, "[2026-10-12 Mo 14:30]")
        });
        let tokio = read(&out, "https://tokio.rs");
        assert_eq!(tokio.collection, ["Rust", "Async"]);
        assert_eq!(tokio.line_no, line);
        assert!(out.contains("** Async\n"), "{out}");
        assert_eq!(ensure_collection(out.clone(), &path, None).0, out, "made once");
    }

    /// A bookmark called "Rust" before the collection "Rust" is no place
    /// to file under.
    #[test]
    fn a_bookmark_is_never_taken_for_a_collection() {
        let text = "* Rust\n:PROPERTIES:\n:URL: https://rust-lang.org\n:END:\n* Rust\n";
        let (out, _) = file_under(text, &["Rust".to_string()], |level| {
            entry(&Draft { title: "x".into(), url: "https://x".into(), ..Draft::default() }, level, NOW)
        });
        assert_eq!(read(&out, "https://x").collection, ["Rust"]);
        assert_eq!(parse::collections_of(&out).len(), 1, "{out}");
    }

    #[test]
    fn a_skipped_level_is_found_not_made_again() {
        let text = "* A\n*** B\n";
        let path = ["A".to_string(), "B".to_string()];
        let (out, _) =
            file_under(text, &path, |level| entry(&Draft { title: "x".into(), url: "https://x".into(), ..Draft::default() }, level, NOW));
        assert_eq!(out.matches("B\n").count(), 1, "{out}");
        assert_eq!(read(&out, "https://x").collection, ["A", "B"]);
    }

    /// A name that org would read differently (tags at its end, a line
    /// break, a heading-like description) is found again as it was written.
    #[test]
    fn odd_collection_names_are_made_once() {
        let path = ["Work :x:".to_string(), "Zwei\nZeilen".to_string()];
        let (once, _) = ensure_collection(String::new(), &path, Some("* keine Überschrift"));
        let (twice, _) = ensure_collection(once.clone(), &path, Some("* keine Überschrift"));
        assert_eq!(once, twice);
        assert_eq!(parse::collections_of(&once).len(), 2, "{once}");
    }

    #[test]
    fn tags_are_made_valid_for_org() {
        assert_eq!(org_tag(" web-dev "), Some("web_dev".into()));
        assert_eq!(org_tag(":rust:"), Some("rust".into()));
        assert_eq!(org_tag(" - "), None);
    }
}
