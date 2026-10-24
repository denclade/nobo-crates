// SPDX-FileCopyrightText: 2026 Dennis Burgermeister
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::core::heading_identity;
use crate::core::heading_utils::format_new_heading_line;
use crate::core::models::{
    is_real_structure, property_value, Heading, OrgTimestamp, ORG_CLOSED, ORG_DEADLINE, ORG_PROPERTIES_END, ORG_PROPERTIES_START,
    ORG_SCHEDULED, ORG_STATE_LOG, TODO_STATES,
};
use crate::core::task_utils;
use crate::utils::clock_utils;
use crate::utils::effort_utils::ORG_EFFORT_PROPERTY;
use crate::utils::string_utils::{count_leading_stars, extract_bracket_value};
use crate::utils::{effort_utils, timestamp_utils};
use std::fs;
use std::path::Path;

/// One node property line, read as org reads it (`org-property-re`):
/// `:KEY: value`, `:KEY:` or `:KEY+: value`, indented or not. KEY holds
/// no whitespace and ends at the first colon that is followed by a blank
/// or the end of the line -- so `:URL: https://a:b` is the key URL, and
/// `:URL:https://x` (no blank after the colon) is no property at all.
/// A `KEY+` comes back as such: org appends its value to KEY, and the
/// properties here are written back as they were read.
pub fn parse_property(line: &str) -> Option<(String, String)> {
    let rest = line.trim_start().strip_prefix(':')?;
    for (at, c) in rest.char_indices() {
        if c.is_whitespace() {
            return None;
        }
        if c == ':' && at > 0 {
            let after = &rest[at + 1..];
            if after.is_empty() || after.starts_with([' ', '\t']) {
                return Some((rest[..at].to_string(), after.trim().to_string()));
            }
        }
    }
    None
}

/*
If the first heading row a TODO state if it does not have one yet.
 */
pub fn ensure_heading_state(lines: &mut [String], state: &str) {
    let Some(first) = lines.first_mut() else {
        return;
    };
    let Some(ParsedHeadingLine { level, state: existing_state, priority, title, tags, raw_cookie }) = parse_heading_line(first) else {
        return;
    };

    if existing_state.is_some() {
        return;
    }

    // Include statistics cookie (progress information, e.g. [XX%] or [1/2])
    let full_title = match raw_cookie {
        Some(cookie) => format!("{} {}", title, cookie),
        None => title,
    };

    *first = format_new_heading_line(level, &Some(state.to_string()), priority.as_deref(), &full_title, &tags, None);
}

/*
LINE e.g. * TODO Dokuwiki - Alte Nutzer löschen Workflow
Checks if line starts with *, if yes, counts them for LEVEL.
Then splits line after *s (trims start) to get the headings text.
Splits first word and check if its a TODO_STATE. if yes, return TODO
state and rest of the string.
 */
pub fn parse_heading_line(line: &str) -> Option<ParsedHeadingLine> {
    if !line.starts_with('*') {
        return None;
    }

    let level = count_leading_stars(line);
    // Org's rule (`org-outline-regexp-bol`, `^\*+ `): the stars, then a
    // space. `*bold* text` at the start of a body line is emphasis -- read
    // as a heading, it ended the entry above it and invented one out of
    // the user's note. A tab or nothing after the stars is no heading to
    // org either.
    if !line[level..].starts_with(' ') {
        return None;
    }
    let remainder = line[level..].trim();

    let mut tags = Vec::new();
    let mut title_part = remainder.to_string();

    // Check for tags seperated through tab / whitespaces
    if let Some(last_ws_pos) = title_part.rfind(|c: char| c.is_whitespace()) {
        let tail = title_part[last_ws_pos..].trim();
        // `org-tag-group-re`: colons around letters, digits and `_@#%`
        // only -- `Treffen um 10:30:` or `:-):` is title text.
        let is_tag_group = tail.len() > 2
            && tail.starts_with(':')
            && tail.ends_with(':')
            && tail.chars().all(|c| c == ':' || c.is_alphanumeric() || "_@#%".contains(c))
            && tail[1..tail.len() - 1].split(':').any(|tag| !tag.is_empty());
        if is_tag_group {
            tags = tail[1..tail.len() - 1].split(':').filter(|s| !s.is_empty()).map(|s| s.to_string()).collect();
            title_part = title_part[..last_ws_pos].trim_end().to_string();
        }
    }

    let mut raw_cookie = None;

    // Statistiken extrahieren, ABER speichern!
    if title_part.ends_with(']') {
        if let Some(idx) = title_part.rfind(" [") {
            let cookie_inner = &title_part[idx + 2..title_part.len() - 1];
            if is_statistics_cookie(cookie_inner) {
                // Den ganzen Block inkl. Klammern speichern (z.B. "[33%]")
                raw_cookie = Some(format!("[{}]", cookie_inner));
                title_part = title_part[..idx].trim().to_string();
            }
        }
    }

    let words: Vec<&str> = title_part.splitn(2, ' ').collect();

    let (state, title) = if words.len() >= 2 && TODO_STATES.contains(&words[0]) {
        (Some(words[0].to_string()), words[1..].join(" "))
    } else if words.len() == 1 && TODO_STATES.contains(&words[0]) {
        (Some(words[0].to_string()), String::new())
    } else {
        (None, title_part)
    };

    // The priority cookie sits between the keyword and the title, so it
    // can only be read once the keyword has been split off.
    let (priority, title) = split_priority(&title);

    Some(ParsedHeadingLine { level, state, priority, title, tags, raw_cookie })
}

/// What a heading line is made of.
///
/// A struct rather than the six-tuple this used to return: three of the
/// six are `Option<String>`, and at a call site `parsed.3` says nothing
/// about which.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedHeadingLine {
    pub level: usize,
    pub state: Option<String>,
    pub priority: Option<String>,
    pub title: String,
    pub tags: Vec<String>,
    pub raw_cookie: Option<String>,
}

/// Splits a leading `[#A]` off a title.
///
/// Org's priority range is configurable, so anything short and
/// non-whitespace between `[#` and `]` counts -- the point is to stop
/// carrying it inside the title string, not to police somebody else's
/// scale.
fn split_priority(title: &str) -> (Option<String>, String) {
    let Some(rest) = title.strip_prefix("[#") else { return (None, title.to_string()) };
    let Some(end) = rest.find(']') else { return (None, title.to_string()) };
    let cookie = &rest[..end];
    // `org-priority-regexp`: capital letters or digits (A-Z, 0-64).
    if cookie.is_empty() || !cookie.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()) {
        return (None, title.to_string());
    }
    (Some(cookie.to_string()), rest[end + 1..].trim_start().to_string())
}

const BLOCK_BEGIN_PREFIX: &str = "#+begin_";
const BLOCK_END_PREFIX: &str = "#+end_";

/// `#+BEGIN_EXAMPLE` -> `Some("EXAMPLE")`, for any case and any block
/// name. Anything after the name is ignored; the name alone is what
/// matches the closing `#+END_` line.
///
/// Uses `str::get` rather than index slicing: a fixed byte offset can land
/// mid-character on non-ASCII input and panic. If the case-insensitive
/// match against an ASCII prefix succeeds, that region is provably ASCII,
/// so the later fixed-offset slice is safe too.
pub(crate) fn block_begin_name(line: &str) -> Option<&str> {
    let prefix = line.get(..BLOCK_BEGIN_PREFIX.len())?;
    if !prefix.eq_ignore_ascii_case(BLOCK_BEGIN_PREFIX) {
        return None;
    }
    let name = line[BLOCK_BEGIN_PREFIX.len()..].split_whitespace().next().unwrap_or("");
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

/// "#+END_EXAMPLE" -> Some("EXAMPLE"); see block_begin_name's own doc
/// comment (identical safety reasoning).
pub(crate) fn block_end_name(line: &str) -> Option<&str> {
    let prefix = line.get(..BLOCK_END_PREFIX.len())?;
    if !prefix.eq_ignore_ascii_case(BLOCK_END_PREFIX) {
        return None;
    }
    let name = line[BLOCK_END_PREFIX.len()..].trim();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

/// The blocks of a FILE's lines, as org reads them: each `#+BEGIN_<name>`
/// with the first `#+END_<name>` after it, as (begin, end) line indexes.
///
/// Org's rules, exactly (org-element's block parsers, and the manual on
/// literal examples):
///
/// - **The first matching end closes the block.** Blocks of one name do
///   not nest; org asks for the inner markers to be escaped with a comma
///   (`,#+END_SRC`), and the writers here do that (see
///   org_syntax::escape_code_line).
/// - **A heading ends the search.** A `* ` line inside a block is a
///   heading to org -- sections are cut at headings before blocks are
///   looked for -- which is why org escapes it as `,* `. A block whose
///   end lies past a heading is therefore not closed.
/// - **A block that is never closed is no block.** Its `#+BEGIN_` line is
///   ordinary text and reading carries on with the very next line.
///
/// The one place that decides it for a file: the reader, the writers and
/// the refile that shifts levels all ask here, because any two of them
/// disagreeing about a line is a file written wrong.
pub fn block_regions<S: AsRef<str>>(lines: &[S]) -> Vec<(usize, usize)> {
    regions(lines, true)
}

/// The blocks of TYPED content -- a description as its editor shows it,
/// unescaped, the way org's own `org-edit-src` buffer shows a block: a
/// `*`-line in there is code, and an inner block of the same name nests.
/// What the editor gives is escaped on its way into the file (see
/// heading_utils::serialize), after which `block_regions` reads it back
/// to the same blocks.
pub fn content_block_regions<S: AsRef<str>>(lines: &[S]) -> Vec<(usize, usize)> {
    regions(lines, false)
}

fn regions<S: AsRef<str>>(lines: &[S], file: bool) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let Some(name) = block_begin_name(lines[i].as_ref().trim_start()) else {
            i += 1;
            continue;
        };
        let mut depth = 1usize;
        let mut end = None;
        for (j, line) in lines.iter().enumerate().skip(i + 1) {
            let line = line.as_ref();
            if file && parse_heading_line(line).is_some() {
                break;
            }
            let trimmed = line.trim_start();
            if !file && block_begin_name(trimmed).is_some_and(|n| n.eq_ignore_ascii_case(name)) {
                depth += 1;
            } else if block_end_name(trimmed).is_some_and(|n| n.eq_ignore_ascii_case(name)) {
                depth -= 1;
                if depth == 0 {
                    end = Some(j);
                    break;
                }
            }
        }
        match end {
            Some(end) => {
                out.push((i, end));
                i = end + 1;
            }
            None => i += 1,
        }
    }
    out
}

fn mark(len: usize, regions: &[(usize, usize)]) -> Vec<bool> {
    let mut inside = vec![false; len];
    for &(begin, end) in regions {
        inside[begin..=end].iter_mut().for_each(|x| *x = true);
    }
    inside
}

/// Which of a file's LINES belong to a block, its two marker lines
/// included (see block_regions). What is inside is text: no line there is
/// a heading, and nothing that rewrites headings may touch it.
pub fn closed_block_lines<S: AsRef<str>>(lines: &[S]) -> Vec<bool> {
    mark(lines.len(), &block_regions(lines))
}

/// The same for typed content (see content_block_regions).
pub fn content_block_lines<S: AsRef<str>>(lines: &[S]) -> Vec<bool> {
    mark(lines.len(), &content_block_regions(lines))
}

/// Inverts `apply_content_heading_offset`: a `*`-line absorbed as content
/// was demoted on write by `offset` extra stars, so it lands deep enough
/// to be absorbed rather than escaping as a sibling. Stripping them back
/// off means `content` always shows the short form the user typed.
///
/// A line with `<= offset` stars was not produced by that demotion -- a
/// hand-authored shallower note -- and is left untouched.
fn strip_content_heading_offset(line: &str, offset: usize) -> String {
    let star_count = count_leading_stars(line);
    // A run of stars is only a demoted heading when a heading could follow
    // it -- org needs whitespace (or nothing) after the stars. `**fett**`
    // in column 0 is inline emphasis, and stripping by star count alone
    // handed the description editor `*fett**`; edit the description once
    // and the star is gone for good.
    //
    // The write side already asks exactly this (`is_heading_attempt` in
    // heading_utils::serialize) and indents such a line instead of adding
    // stars -- so text this app wrote can never trigger it, and text from
    // Emacs or a hand-edited file always could.
    let after_stars = &line[star_count..];
    let is_heading_attempt = after_stars.is_empty() || after_stars.starts_with(char::is_whitespace);

    if star_count > offset && is_heading_attempt {
        line[offset..].to_string()
    } else {
        line.to_string()
    }
}

/// Schließt ein Heading ab: leitet abgeleitete Felder aus den Properties ab
/// und normalisiert den Content.
fn finalize_heading(heading: &mut Heading, last_line: usize) {
    heading.last_line = last_line;
    heading.occurrence = first_bare_active_timestamp(&heading.content);

    heading.effort_minutes = property_value(&heading.properties, ORG_EFFORT_PROPERTY).and_then(effort_utils::parse_effort);

    if heading.content.ends_with('\n') {
        heading.content.pop();
    }
    if heading.raw_content.ends_with('\n') {
        heading.raw_content.pop();
    }
}

/// The first body line that is nothing but an active timestamp.
///
/// Org treats any active timestamp in an entry as an agenda occurrence;
/// this deliberately only recognizes one standing alone on its line,
/// which is what generated calendar entries look like. Prose merely
/// mentioning a date mid-sentence does not turn a note into an
/// appointment.
fn first_bare_active_timestamp(content: &str) -> Option<OrgTimestamp> {
    content.lines().find_map(|line| {
        let trimmed = line.trim();
        if !trimmed.starts_with('<') || !trimmed.ends_with('>') {
            return None;
        }
        timestamp_utils::parse_timestamp(trimmed)
    })
}

/// Peeks past an optional planning line for a `:PROPERTIES:` drawer
/// carrying a `:CUSTOM_ID:`. Every heading this app creates has one, so
/// this is what distinguishes a bare state-less app-created heading -- an
/// untriaged Inbox item -- from prose that happens to start with `*`. The
/// two are otherwise syntactically identical, and a second bare item would
/// vanish into the first's content on reparse.
///
/// Not folded into `is_real_structure`, which is shared with the write
/// side and has no raw-lines lookahead available.
///
/// Follows Org's drawer placement rule -- a drawer directly follows its
/// heading -- so this stops the moment that stops holding rather than
/// scanning the whole entry.
fn has_upcoming_custom_id<S: AsRef<str>>(lines: &[S], heading_index: usize) -> bool {
    let mut i = heading_index + 1;
    while i < lines.len() {
        let trimmed = lines[i].as_ref().trim();
        if trimmed.is_empty() || trimmed.starts_with(ORG_SCHEDULED) || trimmed.starts_with(ORG_DEADLINE) || trimmed.starts_with(ORG_CLOSED)
        {
            i += 1;
            continue;
        }
        break;
    }

    if i >= lines.len() || !lines[i].as_ref().trim().eq_ignore_ascii_case(ORG_PROPERTIES_START) {
        return false;
    }

    for line in &lines[i + 1..] {
        let trimmed = line.as_ref().trim();
        if trimmed.eq_ignore_ascii_case(ORG_PROPERTIES_END) {
            return false;
        }
        if let Some((key, _)) = parse_property(trimmed) {
            if key.eq_ignore_ascii_case(heading_identity::CUSTOM_ID_PROPERTY) {
                return true;
            }
        }
    }
    false
}

/// From this many stars on, org stops treating a `*`-line as structure
/// and treats it as an inlinetask -- a block that lives INSIDE a section,
/// optionally closed by a line of the same depth reading `END`.
///
/// Matches `org-inlinetask-min-level`'s default. Without this an
/// inlinetask parsed as a real heading: it split off as its own level-15
/// entry, and the heading it belonged to gained a `[0%]` cookie for a
/// child it never had.
const INLINETASK_MIN_LEVEL: usize = 15;

/// Org's own statistics-cookie grammar: `[N%]` or `[N/M]`, digits only,
/// either half allowed to be empty (`[%]`, `[/]`, `[3/]`).
///
/// Checked rather than sniffed for `%` or `/`, because a heading is a
/// perfectly ordinary place for a link -- and `* Siehe [[http://x][Doku]]`
/// ends in `]` and contains `/`, so the loose test cut the link out of the
/// title and wrote the truncated version back on the next edit.
fn is_statistics_cookie(inner: &str) -> bool {
    if let Some(done) = inner.strip_suffix('%') {
        return done.chars().all(|c| c.is_ascii_digit());
    }
    match inner.split_once('/') {
        Some((done, total)) => done.chars().all(|c| c.is_ascii_digit()) && total.chars().all(|c| c.is_ascii_digit()),
        None => false,
    }
}

/// The name in a `:NAME:` line, if it is shaped like a drawer marker at
/// all -- org allows letters, digits, `_` and `-`.
fn drawer_name(trimmed: &str) -> Option<&str> {
    let inner = trimmed.trim_end().strip_prefix(':')?.strip_suffix(':')?;
    let legal = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
    (!inner.is_empty() && inner.chars().all(legal)).then_some(inner)
}

const DRAWER_END_NAME: &str = "END";
const LOGBOOK_DRAWER_NAME: &str = "LOGBOOK";

/// Whether the marker at START is closed before the next heading, i.e.
/// whether it opens a drawer at all.
///
/// Stops at anything that could be a heading line, which is deliberately a
/// superset of what this parser accepts as one: stopping early can only
/// make a marker count as body text, which is the safe direction.
fn drawer_closes_before_next_heading<S: AsRef<str>>(lines: &[S], start: usize) -> bool {
    lines[start + 1..]
        .iter()
        .map(|line| line.as_ref())
        .take_while(|line| !looks_like_heading_line(line))
        .any(|line| drawer_name(line.trim_start()).is_some_and(|name| name.eq_ignore_ascii_case(DRAWER_END_NAME)))
}

fn looks_like_heading_line(line: &str) -> bool {
    let stars = line.len() - line.trim_start_matches('*').len();
    stars > 0 && line[stars..].starts_with([' ', '\t'])
}

pub fn parse_headings_from_lines<I, S>(file_path: &Path, lines: I) -> Vec<Heading>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut headings = Vec::new();
    let mut current_heading: Option<Heading> = None;
    let mut in_properties = false;
    // The name of the non-PROPERTIES drawer currently open, if any -- see
    // the marker handling in the loop below. `:LOGBOOK:` is modelled
    // (clocks/history/logbook_extra); anything else is kept verbatim in
    // `raw_drawers`, which is what `raw_drawer` collects into.
    let mut in_drawer: Option<String> = None;
    let mut raw_drawer: Vec<String> = Vec::new();
    // Whether nothing but heading metadata has been seen since the last
    // heading line. Org allows a planning line only directly after the
    // heading, and puts CLOCK and state-log entries in the metadata block
    // above the body -- and prose is perfectly free to start a line with
    // `SCHEDULED:` or `CLOCK:` further down. See where this is read.
    let mut in_metadata_prefix = false;
    let mut total_lines = 0usize;

    let file_path_str = file_path.to_string_lossy().into_owned();

    // Collected up front (not consumed lazily) so a state-less/tag-less
    // `*`-line can look ITSELF up in has_upcoming_custom_id below --
    // deciding whether to absorb it as a sub-note needs to see past its
    // own line, into whatever PROPERTIES drawer follows.
    let lines: Vec<S> = lines.into_iter().collect();
    // Which lines are the inside of a closed block -- decided once, up
    // front, by the same rule every writer uses (see closed_block_lines).
    let regions = block_regions(&lines);
    let in_block = mark(lines.len(), &regions);
    let mut block_inside = vec![false; lines.len()];
    for &(begin, end) in &regions {
        block_inside[begin + 1..end].iter_mut().for_each(|x| *x = true);
    }

    for (i, raw_line) in lines.iter().enumerate() {
        let line = raw_line.as_ref();
        let line_no = i + 1;
        total_lines = line_no;
        let trimmed = line.trim_start();

        // Everything inside a closed literal block is opaque content,
        // checked before even the heading check below -- a `*`-prefixed
        // line in here must NOT be reinterpreted as a new heading. Kept
        // verbatim, stars and all: the writer leaves these lines alone
        // too, so there is no offset to take off.
        if in_block[i] {
            if let Some(ref mut heading) = current_heading {
                // The inside of a block as its editor shows it: org's comma
                // escape taken off again (org-unescape-code-in-string), the
                // marker lines as they are.
                if block_inside[i] {
                    heading.content.push_str(&crate::core::org_syntax::unescape_code_line(line));
                } else {
                    heading.content.push_str(line);
                }
                heading.content.push('\n');
                heading.raw_content.push_str(line);
                heading.raw_content.push('\n');
            }
            continue;
        }

        // Headings start with `*` at column 0, matching org exactly -- an
        // indented `  * foo` is not a heading there either, and pasted
        // prose with indented star lines must not spawn tasks. Checked
        // against the raw line, unlike properties, planning and drawer
        // lines below, which are commonly indented.
        if let Some(ParsedHeadingLine { level, state, priority, title, tags, .. }) = parse_heading_line(line) {
            // A `*`-line only counts as real structure (a task or a
            // project) if it carries a recognized TODO keyword or a
            // :project: tag -- the user's own explicit rule (see
            // models::is_real_structure, shared with
            // heading_utils::apply_content_heading_offset -- the write
            // side MUST use the exact same predicate for the round trip
            // below to hold). Everything else -- a bare "* Notiz" a user
            // typed into a description, pasted prose that happens to
            // start a line with '*', a hand-authored plain outline
            // heading -- is absorbed as a sub-note of whichever real
            // heading precedes it instead of becoming its own (possibly
            // wrongly-nested, or outright phantom-sibling-swallowing)
            // heading. This is the read-side half of the
            // heading_utils::apply_content_heading_offset /
            // strip_content_heading_offset round trip: a heading typed as
            // "* Notiz" in the UI is always written with extra leading
            // stars so it lands deep enough to be absorbed HERE rather
            // than escape as a sibling, no matter how many stars the user
            // typed -- and strip_content_heading_offset below undoes
            // exactly that offset again before it reaches heading.content.
            if level >= INLINETASK_MIN_LEVEL || (!is_real_structure(&state, &tags) && !has_upcoming_custom_id(&lines, i)) {
                if let Some(ref mut heading) = current_heading {
                    in_metadata_prefix = false;
                    let stripped = strip_content_heading_offset(line, heading.level);
                    heading.content.push_str(&stripped);
                    heading.content.push('\n');
                    heading.raw_content.push_str(line);
                    heading.raw_content.push('\n');
                    continue;
                }
                // No current heading to fold into -- this is the very
                // first heading in the file, so it has nothing to be a
                // sub-note OF. Falls through and becomes its own
                // state-less, tag-less heading below; there's nothing
                // else it could become.
            }

            // Finish previous tasks description if we had a previous task.
            if let Some(mut heading) = current_heading.take() {
                finalize_heading(&mut heading, line_no - 1);
                headings.push(heading);
            }

            in_properties = false;
            in_drawer = None;
            raw_drawer.clear();
            in_metadata_prefix = true;

            current_heading = Some(Heading {
                priority,
                id: format!("{}:{}", file_path_str, line_no),
                file_path: file_path_str.clone(),
                line_no,
                level,
                state,
                title,
                tags,
                // Everything else is filled in from the lines that follow
                // this one, if they are there at all -- so it starts empty.
                ..Default::default()
            });
            continue;
        }

        if let Some(ref mut heading) = current_heading {
            if in_metadata_prefix && trimmed.eq_ignore_ascii_case(ORG_PROPERTIES_START) && drawer_closes_before_next_heading(&lines, i) {
                in_properties = true;
                continue;
            } else if trimmed.eq_ignore_ascii_case(ORG_PROPERTIES_END) && in_properties {
                in_properties = false;
                continue;
            } else if in_properties {
                if let Some((k, v)) = parse_property(trimmed) {
                    // First occurrence into the map (which is what org
                    // reads for a repeated key), any further one kept
                    // verbatim -- see Heading::repeated_properties.
                    if heading.properties.contains_key(&k) {
                        heading.repeated_properties.push((k, v));
                    } else {
                        heading.property_order.push(k.clone());
                        heading.properties.insert(k, v);
                    }
                }
                continue;
            }

            // Planning line (CLOSED, SCHEDULED, DEADLINE).
            //
            // Only in the metadata prefix: org allows one only directly
            // after the heading, and a body line may perfectly well begin
            // with "SCHEDULED: " -- prose, a quote, a pasted log. Such a
            // line was consumed here and then written back by nothing,
            // because the writer emits planning from the typed fields.
            if in_metadata_prefix
                && (trimmed.starts_with(ORG_CLOSED) || trimmed.starts_with(ORG_SCHEDULED) || trimmed.starts_with(ORG_DEADLINE))
            {
                let mut recognised = false;
                if let Some(closed) = extract_bracket_value(trimmed, ORG_CLOSED, '[', ']') {
                    heading.closed = Some(closed);
                    recognised = true;
                }
                if let Some(sched) = extract_bracket_value(trimmed, ORG_SCHEDULED, '<', '>') {
                    heading.scheduled = timestamp_utils::parse_timestamp(&sched).or_else(|| Some(OrgTimestamp::unparsed(&sched)));
                    recognised = true;
                }
                if let Some(dl) = extract_bracket_value(trimmed, ORG_DEADLINE, '<', '>') {
                    // Kept verbatim when it cannot be read, exactly like
                    // SCHEDULED above. Dropping it deleted the user's
                    // deadline on the next rewrite -- a timestamp this
                    // build does not understand is still the user's.
                    heading.deadline = timestamp_utils::parse_timestamp(&dl).or_else(|| Some(OrgTimestamp::unparsed(&dl)));
                    recognised = true;
                }
                // Nothing usable in it, so it is not a planning line at
                // all -- fall through and keep it as the text it is,
                // rather than consuming a line nothing will write back.
                if recognised {
                    continue;
                }
            }

            // History (Logbook)
            let in_log_position = in_metadata_prefix || in_drawer.as_deref().is_some_and(|n| n.eq_ignore_ascii_case(LOGBOOK_DRAWER_NAME));

            if in_log_position && trimmed.starts_with(ORG_STATE_LOG) {
                heading.history.push(line.trim_end().to_string());
                if in_drawer.as_deref().is_some_and(|n| n.eq_ignore_ascii_case(LOGBOOK_DRAWER_NAME)) {
                    heading.raw_logbook.push(line.trim_end().to_string());
                }
                continue;
            }

            // CLOCK entries -- recognized unconditionally, same as
            // ORG_STATE_LOG above, not gated on being inside a :LOGBOOK:
            // drawer (robust regardless of whether the raw file's drawer
            // nesting is intact). Placed BEFORE the generic drawer
            // catch-all below: :LOGBOOK:/:END: markers themselves still
            // fall into that and are discarded, since
            // heading_utils::build_org_heading_block_inner regenerates the
            // whole drawer wrapper fresh on every rewrite.
            if in_log_position && trimmed.starts_with(clock_utils::CLOCK_PREFIX) {
                heading.clocks.push(line.trim_end().to_string());
                if in_drawer.as_deref().is_some_and(|n| n.eq_ignore_ascii_case(LOGBOOK_DRAWER_NAME)) {
                    heading.raw_logbook.push(line.trim_end().to_string());
                }
                continue;
            }

            // A drawer marker, or body text that merely looks like one?
            //
            // This used to swallow every line of the shape `:...:` --
            // `:shrug:`, `:das ist eine Notiz:`, an emoji shortcode, a
            // line of dialogue. Silently, and permanently: nothing else in
            // the app writes a line that shape, so nobody would think to
            // look for it.
            //
            // A drawer is only a drawer once something closes it. So a
            // well-formed `:NAME:` counts as one only when a matching
            // `:END:` follows before the next heading, and a bare `:END:`
            // counts only when a drawer is actually open.
            //
            // The markers are still dropped rather than kept, because
            // build_org_heading_block regenerates the LOGBOOK wrapper on
            // every rewrite; preserving foreign drawers verbatim needs a
            // place on `Heading` to put them.
            if let Some(name) = drawer_name(trimmed) {
                if name.eq_ignore_ascii_case(DRAWER_END_NAME) {
                    if let Some(open_name) = in_drawer.take() {
                        // A foreign drawer is handed on whole, markers
                        // included, so it can be written back exactly as
                        // it came in.
                        if !open_name.eq_ignore_ascii_case(LOGBOOK_DRAWER_NAME) {
                            raw_drawer.push(line.trim_end().to_string());
                            heading.raw_drawers.push(std::mem::take(&mut raw_drawer));
                        }
                        continue;
                    }
                } else if in_metadata_prefix && drawer_closes_before_next_heading(&lines, i) {
                    if !name.eq_ignore_ascii_case(LOGBOOK_DRAWER_NAME) {
                        raw_drawer.push(line.trim_end().to_string());
                    }
                    in_drawer = Some(name.to_string());
                    continue;
                }
            }

            // Inside a drawer, everything that is not one of the modelled
            // shapes above belongs to that drawer -- not to the body.
            if let Some(open_name) = &in_drawer {
                if open_name.eq_ignore_ascii_case(LOGBOOK_DRAWER_NAME) {
                    heading.logbook_extra.push(line.trim_end().to_string());
                    heading.raw_logbook.push(line.trim_end().to_string());
                } else {
                    raw_drawer.push(line.trim_end().to_string());
                }
                continue;
            }

            // Description / Content
            if trimmed.is_empty() && heading.content.is_empty() {
                continue;
            }

            in_metadata_prefix = false;
            heading.content.push_str(line);
            heading.content.push('\n');
            heading.raw_content.push_str(line);
            heading.raw_content.push('\n');
        }
    }

    if let Some(mut heading) = current_heading {
        finalize_heading(&mut heading, total_lines);
        headings.push(heading);
    }

    task_utils::annotate_progress(&mut headings);
    task_utils::annotate_clocked_time(&mut headings);
    headings
}

pub fn extract_headings_from_file(file_path: &Path) -> Vec<Heading> {
    let content = match fs::read_to_string(file_path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };

    parse_headings_from_lines(file_path, content.lines())
}

/* --------- TESTS ------------ */

#[cfg(test)]
mod tests {
    /// Emphasis at the start of a line is text; only stars and a space
    /// (or the stars alone) make a heading.
    #[test]
    fn stars_need_a_space_to_be_a_heading() {
        assert!(super::parse_heading_line("*wichtig* lesen").is_none());
        assert!(super::parse_heading_line("**fett**").is_none());
        assert_eq!(super::parse_heading_line("** Titel").map(|h| h.level), Some(2));
        assert!(super::parse_heading_line("*\tTab").is_none(), "org wants a space");
        assert!(super::parse_heading_line("**").is_none());
        assert_eq!(super::parse_heading_line("* ").map(|h| h.title), Some(String::new()), "an empty heading");
    }

    /// `org-tag-group-re`: only letters, digits and `_@#%` between colons.
    #[test]
    fn only_valid_tag_groups_are_tags() {
        let tagged = super::parse_heading_line("* Treffen :arbeit:büro_2@x:").unwrap();
        assert_eq!(tagged.tags, ["arbeit", "büro_2@x"]);
        for title in ["* Treffen um 10:30:", "* Smiley :-):", "* Pfad :a/b:", "* Leer ::"] {
            let heading = super::parse_heading_line(title).unwrap();
            assert!(heading.tags.is_empty(), "{title}");
            assert_eq!(heading.title, title[2..], "{title}");
        }
    }

    #[test]
    fn a_priority_is_a_capital_letter_or_a_number() {
        assert_eq!(super::parse_heading_line("* TODO [#A] x").unwrap().priority.as_deref(), Some("A"));
        assert_eq!(super::parse_heading_line("* [#10] x").unwrap().priority.as_deref(), Some("10"));
        assert_eq!(super::parse_heading_line("* [#a] x").unwrap().priority, None);
        assert_eq!(super::parse_heading_line("* [#x-y] x").unwrap().title, "[#x-y] x");
    }

    /// `org-property-re`.
    #[test]
    fn a_property_line_is_read_as_org_reads_it() {
        use super::parse_property;
        assert_eq!(parse_property(":URL: https://a:b"), Some(("URL".into(), "https://a:b".into())));
        assert_eq!(parse_property("   :Created:  [2026-10-12]"), Some(("Created".into(), "[2026-10-12]".into())));
        assert_eq!(parse_property(":EMPTY:"), Some(("EMPTY".into(), String::new())));
        assert_eq!(parse_property(":TAGS+: b"), Some(("TAGS+".into(), "b".into())));
        assert_eq!(parse_property(":a:b: c"), Some(("a:b".into(), "c".into())));
        assert_eq!(parse_property(":URL:https://x"), None, "no blank after the key");
        assert_eq!(parse_property(":zwei worte: x"), None);
        assert_eq!(parse_property("::"), None);
    }

    use super::*;
    use crate::core::heading_utils;
    use crate::core::models::{HeadingUpdatePayload, ORG_TAGS_COLUMN};
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn write_temp_org(content: &str) -> NamedTempFile {
        let mut file = NamedTempFile::with_suffix(".org").unwrap();
        write!(file, "{}", content).unwrap();
        file
    }

    /* --- code blocks: what is inside is text --- */

    fn parsed(text: &str) -> Vec<Heading> {
        let lines: Vec<&str> = text.lines().collect();
        parse_headings_from_lines(Path::new("t.org"), lines)
    }

    fn titles(text: &str) -> Vec<String> {
        parsed(text).into_iter().map(|h| format!("{} {}", h.level, h.title)).collect()
    }

    /// The reported case: org code quoted in a task's description, with a
    /// src block of its own inside -- as org writes it, every inner `*`
    /// and `#+` line escaped with a comma. It stays one block, and the
    /// description shows it unescaped, as org's src editor does.
    #[test]
    fn a_src_block_inside_a_src_block_stays_inside() {
        let text = "\
* TODO Projekt :project:
** TODO Bug melden
#+BEGIN_SRC org
,**** TODO zitiert
,#+BEGIN_SRC org
,***** TODO noch tiefer zitiert
,#+END_SRC
,***** TODO nach dem inneren Block
#+END_SRC
** TODO Nächste Aufgabe
";
        assert_eq!(titles(text), vec!["1 Projekt", "2 Bug melden", "2 Nächste Aufgabe"]);
        let bug = &parsed(text)[1];
        assert!(bug.content.contains("***** TODO nach dem inneren Block"), "kept verbatim:\n{}", bug.content);
    }

    /// The dangerous one. A block that is never closed -- an `#+END_SRC`
    /// deleted by hand, or a paste cut short -- used to stay open until the
    /// end of the file: every project below vanished into one task's
    /// description, and deleting that task deleted all of them. Org itself
    /// treats an unclosed begin line as text, and so does this now.
    #[test]
    fn a_block_that_is_never_closed_hides_nothing() {
        let text = "\
* TODO Projekt A :project:
** TODO Aufgabe
#+BEGIN_SRC org
* TODO nicht geschlossen
* TODO Projekt B :project:
* DONE Projekt C :project:
";
        let found = titles(text);
        assert!(found.contains(&"1 Projekt B".to_string()), "{found:?}");
        assert!(found.contains(&"1 Projekt C".to_string()), "{found:?}");
    }

    /// Only markers of the block's own name count. A quote inside a src
    /// block is text; it neither opens nor closes anything.
    #[test]
    fn a_block_of_another_name_inside_is_just_text() {
        let text = "\
* TODO A
#+BEGIN_SRC org
#+begin_quote
,** TODO zitiert
#+END_SRC
* TODO B
";
        assert_eq!(titles(text), vec!["1 A", "1 B"]);
    }

    /// Org's rules: blocks do not nest -- the first matching end closes --
    /// and a block that is never closed is none.
    #[test]
    fn the_block_rule_marks_exactly_the_closed_blocks() {
        let lines = ["a", "#+BEGIN_SRC org", "#+BEGIN_SRC org", "x", "#+END_SRC", "y", "#+END_SRC", "b", "#+begin_example", "z"];
        let inside = closed_block_lines(&lines);
        assert_eq!(inside, vec![false, true, true, true, true, false, false, false, false, false]);
    }

    /// An unescaped `* ` line inside a block is a heading to org -- the
    /// section ends there, and with it the block's chance to close.
    #[test]
    fn an_unescaped_heading_in_a_block_is_a_heading_as_in_org() {
        let text = "\
* TODO A
#+BEGIN_SRC org
* TODO B
#+END_SRC
";
        assert_eq!(titles(text), vec!["1 A", "1 B"]);
        assert_eq!(closed_block_lines(&text.lines().collect::<Vec<_>>()), vec![false; 4]);
    }

    #[test]
    fn parses_simple_todo_heading() {
        let file = write_temp_org("* TODO Testaufgabe\nEin bisschen Text.\n");
        let headings = extract_headings_from_file(file.path());

        assert_eq!(headings.len(), 1);
        assert_eq!(headings[0].state, Some("TODO".to_string()));
        assert_eq!(headings[0].title, "Testaufgabe");
        assert_eq!(headings[0].content, "Ein bisschen Text.");
    }

    #[test]
    fn a_heading_with_no_todo_keyword_and_nothing_preceding_it_is_still_a_real_heading() {
        // A state-less, :project:-less "* ..." line only counts as real
        // structure when there's NO current heading to fold into instead
        // (see parse_headings_from_lines's own doc comment on the rule) --
        // the very first heading in a file has nothing to be a sub-note
        // OF, so it's still created as its own (state-less) heading here.
        // This app's own Projects are the concrete real-world case: a
        // level-1 heading with no state, just a :project: tag.
        let file = write_temp_org("* Plain heading, no keyword\n");
        let headings = extract_headings_from_file(file.path());
        assert_eq!(headings.len(), 1);
        assert_eq!(headings[0].state, None);
        assert_eq!(headings[0].title, "Plain heading, no keyword");
    }

    #[test]
    fn a_state_less_tag_less_heading_after_a_real_one_is_absorbed_as_its_sub_note() {
        // The actual reported bug this rule exists to fix: without it, a
        // bare "* ..." line typed into a description (or pasted prose
        // that happens to start with one) becomes a NEW sibling heading,
        // silently swallowing/re-parenting everything that follows it.
        let file = write_temp_org(concat!(
            "* TODO Real task\n",
            "Some notes.\n",
            "* Not a keyword or a project, just prose\n",
            "More notes after it.\n",
        ));
        let headings = extract_headings_from_file(file.path());
        assert_eq!(headings.len(), 1, "the bare heading-looking line must not spawn a sibling");
        assert!(headings[0].content.contains("Some notes."));
        assert!(headings[0].content.contains("Not a keyword or a project, just prose"));
        assert!(headings[0].content.contains("More notes after it."), "content AFTER the bare line must not be dropped");
    }

    #[test]
    fn a_todo_keyword_makes_a_star_line_real_structure_even_deep_in_a_description() {
        let file = write_temp_org(concat!("* TODO Real task\n", "Some notes.\n", "*** NEXT A genuine nested subtask\n",));
        let headings = extract_headings_from_file(file.path());
        assert_eq!(headings.len(), 2);
        assert_eq!(headings[1].title, "A genuine nested subtask");
        assert_eq!(headings[1].state, Some("NEXT".to_string()));
        assert_eq!(headings[1].level, 3);
    }

    #[test]
    fn a_project_tag_makes_a_star_line_real_structure_even_without_a_state() {
        let file = write_temp_org(concat!("* TODO Real task\n", "* A state-less heading tagged :project:\n",));
        let headings = extract_headings_from_file(file.path());
        assert_eq!(headings.len(), 2);
        assert_eq!(headings[1].title, "A state-less heading tagged");
        assert!(headings[1].tags.contains(&"project".to_string()));
    }

    #[test]
    fn a_bare_heading_with_its_own_custom_id_property_is_real_structure() {
        // Two genuine, untriaged Inbox items -- neither carries a TODO
        // state or a :project: tag, but each has its own :CUSTOM_ID:
        // property (every heading this app creates gets one, see
        // heading_utils::build_new_heading_block). Without the
        // has_upcoming_custom_id lookahead, the second one would silently
        // vanish into the first one's own content on every reparse.
        let file = write_temp_org(concat!(
            "* First inbox item\n",
            ":PROPERTIES:\n",
            ":CUSTOM_ID: aaaa-1111\n",
            ":END:\n",
            "* Second inbox item\n",
            ":PROPERTIES:\n",
            ":CUSTOM_ID: bbbb-2222\n",
            ":END:\n",
        ));
        let headings = extract_headings_from_file(file.path());
        assert_eq!(headings.len(), 2);
        assert_eq!(headings[0].title, "First inbox item");
        assert_eq!(headings[1].title, "Second inbox item");
    }

    #[test]
    fn a_bare_heading_with_a_scheduled_line_before_its_custom_id_is_still_real_structure() {
        // SCHEDULED/DEADLINE/CLOSED planning lines sit between the heading
        // line and its PROPERTIES drawer -- the lookahead must skip past
        // them, not stop at the first non-blank line.
        let file = write_temp_org(concat!(
            "* First inbox item\n",
            ":PROPERTIES:\n",
            ":CUSTOM_ID: aaaa-1111\n",
            ":END:\n",
            "* Second inbox item\n",
            "SCHEDULED: <2026-01-01 Thu>\n",
            ":PROPERTIES:\n",
            ":CUSTOM_ID: bbbb-2222\n",
            ":END:\n",
        ));
        let headings = extract_headings_from_file(file.path());
        assert_eq!(headings.len(), 2);
        assert_eq!(headings[1].title, "Second inbox item");
    }

    #[test]
    fn a_bare_heading_with_no_properties_drawer_at_all_is_still_absorbed() {
        // Guards the ORIGINAL absorption behavior this lookahead must not
        // regress: genuinely accidental structure (pasted prose, a stray
        // '*' in a description) has no PROPERTIES drawer of its own at
        // all, so it must still be swallowed as a sub-note.
        let file = write_temp_org(concat!(
            "* TODO Real task\n",
            ":PROPERTIES:\n",
            ":CUSTOM_ID: aaaa-1111\n",
            ":END:\n",
            "* Not a keyword or a project, just prose\n",
            "More notes after it.\n",
        ));
        let headings = extract_headings_from_file(file.path());
        assert_eq!(headings.len(), 1);
        assert!(headings[0].content.contains("Not a keyword or a project, just prose"));
    }

    #[test]
    fn strip_content_heading_offset_strips_exactly_the_offset_when_the_line_has_more_stars() {
        // Star count strictly greater than offset -- exactly what our own
        // demotion produces (offset + at least 1 typed star).
        assert_eq!(strip_content_heading_offset("*** Notiz", 2), "* Notiz");
        assert_eq!(strip_content_heading_offset("**** Notiz", 2), "** Notiz");
    }

    #[test]
    fn strip_content_heading_offset_leaves_a_line_with_too_few_or_exactly_offset_stars_untouched() {
        // Not producible by our own demotion, which always adds STRICTLY
        // more than `offset` stars (offset + at least 1 typed star) --
        // exactly `offset` stars, or fewer, means a hand-authored file
        // with a shallower note than the current heading's own level.
        // Guessing here would corrupt real content.
        assert_eq!(strip_content_heading_offset("** Notiz", 2), "** Notiz");
        assert_eq!(strip_content_heading_offset("* Notiz", 2), "* Notiz");
        assert_eq!(strip_content_heading_offset("Plain text", 2), "Plain text");
    }

    /// Stars with no space after them are emphasis, not a heading -- the
    /// same question the write side asks before adding any.
    #[test]
    fn strip_content_heading_offset_leaves_inline_emphasis_alone() {
        assert_eq!(strip_content_heading_offset("**fett**", 1), "**fett**");
        assert_eq!(strip_content_heading_offset("***sehr fett***", 1), "***sehr fett***");
        // And a real demoted heading still loses exactly the offset.
        assert_eq!(strip_content_heading_offset("** Notiz", 1), "* Notiz");
    }

    #[test]
    fn an_indented_asterisk_line_is_not_a_heading_matching_real_org_mode() {
        // Org requires `*` at column 0 for a heading, so trimming leading
        // whitespace before the check would be more lenient than org --
        // and pasted prose with indented star lines would spawn tasks.
        let file = write_temp_org(concat!(
            "* TODO Real task\n",
            "Some description text.\n",
            "  * Not a heading -- indented\n",
            "  ** Also not a heading\n",
            "\tAlso indented (tab), still not a heading\n",
            "More description text.\n",
        ));
        let headings = extract_headings_from_file(file.path());

        assert_eq!(headings.len(), 1, "the indented '*' lines must not spawn new headings");
        assert!(headings[0].content.contains("Not a heading -- indented"));
        assert!(headings[0].content.contains("Also not a heading"));
        assert!(headings[0].content.contains("More description text."));
    }

    #[test]
    fn content_inside_an_example_block_is_never_parsed_as_a_heading_even_at_column_zero() {
        // The standard, real org-mode mechanism for protecting text that
        // WOULD otherwise be misread as structure (even a column-0 '*',
        // which the previous test's fix alone can't help with) --
        // #+BEGIN_EXAMPLE/#+END_EXAMPLE content is opaque to structural
        // parsing.
        let file = write_temp_org(concat!(
            "* TODO Real task\n",
            "#+BEGIN_EXAMPLE\n",
            ",* Unterschrift\n",
            ",** Nested-looking line\n",
            "#+END_EXAMPLE\n",
            "Real description continues.\n",
        ));
        let headings = extract_headings_from_file(file.path());

        assert_eq!(headings.len(), 1, "column-0 '*' lines inside the block must not spawn new headings");
        assert!(headings[0].content.contains("#+BEGIN_EXAMPLE"));
        assert!(headings[0].content.contains("* Unterschrift"));
        assert!(headings[0].content.contains("** Nested-looking line"));
        assert!(headings[0].content.contains("#+END_EXAMPLE"));
        assert!(headings[0].content.contains("Real description continues."));
    }

    #[test]
    fn example_block_matching_is_case_insensitive_and_requires_a_matching_name() {
        let file = write_temp_org(concat!("* TODO A\n", "#+begin_quote\n", "* Not a heading\n", "#+END_QUOTE\n", "* TODO B\n",));
        let headings = extract_headings_from_file(file.path());
        assert_eq!(headings.len(), 2, "lowercase #+begin_/#+END_ still pair up correctly");
        assert_eq!(headings[1].title, "B");
    }

    #[test]
    fn a_mismatched_end_name_does_not_close_the_block() {
        // #+END_QUOTE doesn't close a #+BEGIN_EXAMPLE -- real org-mode
        // requires the names to match, so a heading-looking line between
        // them (even another #+END_ of the wrong kind) stays protected.
        let file =
            write_temp_org(concat!("* TODO A\n", "#+BEGIN_EXAMPLE\n", "#+END_QUOTE\n", "* Still not a heading\n", "#+END_EXAMPLE\n",));
        let headings = extract_headings_from_file(file.path());
        assert_eq!(headings.len(), 1);
        assert!(headings[0].content.contains("#+END_QUOTE"));
        assert!(headings[0].content.contains("* Still not a heading"));
    }

    #[test]
    fn parses_tags_correctly() {
        let file = write_temp_org("* Projekt X :work:urgent:\n");
        let headings = extract_headings_from_file(file.path());

        assert_eq!(headings[0].tags, vec!["work".to_string(), "urgent".to_string()]);
        assert_eq!(headings[0].title, "Projekt X");
    }

    #[test]
    fn last_line_stops_at_next_heading_of_any_level() {
        let file = write_temp_org("* TODO Parent\nContent A\n** NEXT Child\nContent B\n* DONE Sibling\n");
        let headings = extract_headings_from_file(file.path());

        assert_eq!(headings[0].last_line, 2);
        assert_eq!(headings[1].last_line, 4);
    }

    #[test]
    fn ignores_statistics_cookie_in_title() {
        let file = write_temp_org("* TODO Aufgaben [50%]\n");
        let headings = extract_headings_from_file(file.path());

        assert_eq!(headings[0].title, "Aufgaben");
    }

    #[test]
    fn ensure_heading_state_adds_state_when_missing() {
        let mut lines = vec!["** Etwas erledigen".to_string()];
        ensure_heading_state(&mut lines, "TODO");
        assert_eq!(lines[0], "** TODO Etwas erledigen");
    }

    #[test]
    fn ensure_heading_state_keeps_existing_state() {
        let mut lines = vec!["** NEXT Etwas erledigen".to_string()];
        ensure_heading_state(&mut lines, "TODO");
        assert_eq!(lines[0], "** NEXT Etwas erledigen");
    }

    #[test]
    fn ensure_heading_state_preserves_tags_and_realigns() {
        let mut lines = vec![format_new_heading_line(2, &None, None, "Etwas", &["work".to_string()], None)];
        ensure_heading_state(&mut lines, "TODO");
        assert!(lines[0].starts_with("** TODO Etwas"));
        assert!(lines[0].ends_with(":work:"));
        assert_eq!(lines[0].chars().count(), ORG_TAGS_COLUMN);
    }

    #[test]
    fn ensure_heading_state_ignores_non_heading_lines() {
        let mut lines = vec!["kein Heading".to_string()];
        ensure_heading_state(&mut lines, "TODO");
        assert_eq!(lines[0], "kein Heading");
    }

    #[test]
    fn parses_effort_property_for_all_headings() {
        let file = write_temp_org("* TODO Erste\n:PROPERTIES:\n:Effort: 1:30\n:END:\n* TODO Zweite\n:PROPERTIES:\n:Effort: 0:45\n:END:\n");
        let headings = extract_headings_from_file(file.path());

        assert_eq!(headings[0].effort_minutes, Some(90));
        assert_eq!(headings[1].effort_minutes, Some(45));
    }

    #[test]
    fn effort_property_is_case_insensitive() {
        let file = write_temp_org("* TODO A\n:PROPERTIES:\n:EFFORT: 2:00\n:END:\n");
        let headings = extract_headings_from_file(file.path());
        assert_eq!(headings[0].effort_minutes, Some(120));
    }

    #[test]
    fn heading_without_effort_has_none() {
        let file = write_temp_org("* TODO A\n");
        assert_eq!(extract_headings_from_file(file.path())[0].effort_minutes, None);
    }

    #[test]
    fn parses_scheduled_with_time_range() {
        let file = write_temp_org("* TODO Termin\nSCHEDULED: <2026-08-05 Wed 14:00-15:30>\n");
        let h = &extract_headings_from_file(file.path())[0];
        let ts = h.scheduled.as_ref().unwrap();

        assert_eq!(ts.start_date, "2026-08-05");
        assert_eq!(ts.start_time.as_deref(), Some("14:00"));
        assert_eq!(ts.end_time.as_deref(), Some("15:30"));
        assert_eq!(ts.duration_minutes(), Some(90));
    }

    #[test]
    fn parses_scheduled_date_only() {
        let file = write_temp_org("* TODO A\nSCHEDULED: <2026-08-05 Wed>\n");
        let ts = extract_headings_from_file(file.path())[0].scheduled.clone().unwrap();

        assert_eq!(ts.start_date, "2026-08-05");
        assert!(ts.start_time.is_none());
    }

    /// A calendar entry's date lives in a bare active timestamp line, not
    /// behind SCHEDULED: -- see nobo_calendar::org for why.
    #[test]
    fn a_bare_active_timestamp_line_becomes_the_headings_occurrence() {
        let file = write_temp_org("* Team-Meeting\n:PROPERTIES:\n:CUSTOM_ID: h:1\n:END:\n<2026-08-21 Fri 14:00-15:00>\nZoom-Link\n");
        let h = &extract_headings_from_file(file.path())[0];

        let occurrence = h.occurrence.as_ref().unwrap();
        assert_eq!(occurrence.start_date, "2026-08-21");
        assert_eq!(occurrence.start_time.as_deref(), Some("14:00"));
        assert_eq!(occurrence.end_time.as_deref(), Some("15:00"));
        // The line stays in the body, so a rewrite preserves it verbatim.
        assert!(h.content.contains("<2026-08-21 Fri 14:00-15:00>"), "{}", h.content);
        assert!(h.scheduled.is_none(), "an occurrence is not a plan to work on it");
    }

    /// A note that merely mentions a date is not an appointment.
    #[test]
    fn a_timestamp_inside_prose_is_not_an_occurrence() {
        let file = write_temp_org("* TODO Notiz\n:PROPERTIES:\n:CUSTOM_ID: h:1\n:END:\nTermin war <2026-08-21 Fri> laut Mail.\n");
        let h = &extract_headings_from_file(file.path())[0];

        assert!(h.occurrence.is_none(), "{:?}", h.occurrence);
    }

    #[test]
    fn an_inactive_timestamp_line_is_not_an_occurrence() {
        let file = write_temp_org("* TODO Notiz\n:PROPERTIES:\n:CUSTOM_ID: h:1\n:END:\n[2026-08-21 Fri 14:00]\n");
        let h = &extract_headings_from_file(file.path())[0];

        assert!(h.occurrence.is_none());
    }

    #[test]
    fn parses_scheduled_with_repeater() {
        let file = write_temp_org("* TODO Wiederkehrend\nSCHEDULED: <2026-08-05 Wed +1w>\n");
        let ts = extract_headings_from_file(file.path())[0].scheduled.clone().unwrap();

        assert!(ts.repeater.is_some());
        assert_eq!(ts.repeater.as_ref().unwrap().unit, 'w');
    }

    #[test]
    fn heading_without_scheduled_has_none() {
        let file = write_temp_org("* TODO A\n");
        assert!(extract_headings_from_file(file.path())[0].scheduled.is_none());
    }

    #[test]
    fn scheduled_survives_parse_write_roundtrip() {
        let file = write_temp_org("* TODO A\nSCHEDULED: <2026-08-05 Wed 14:00-15:30>\n");
        let h = &extract_headings_from_file(file.path())[0];

        let block = heading_utils::build_org_heading_block(h, &HeadingUpdatePayload::default());
        assert!(block.iter().any(|l| l == "SCHEDULED: <2026-08-05 Wed 14:00-15:30>"));
    }

    /// A hand-written outline inside a heading's body must survive a
    /// rewrite that never touched the body.
    ///
    /// The ghost-prefix offset is deliberately asymmetric on the way in
    /// (a line with `<= level` stars is left alone, being a hand-authored
    /// shallower section) and was unconditional on the way out, so
    /// `* Kapitel 2` came back as `** Kapitel 2`. No edit was needed:
    /// clocking in, undoing, a bulk action, a merge or an ancestor touch
    /// rewrites the block too, and the shift compounds relative depth --
    /// `* Kapitel 2` and `** Unter 2.1` both become `**`, turning a child
    /// into a sibling.
    #[test]
    fn a_hand_written_outline_in_the_body_is_not_demoted_by_an_unrelated_rewrite() {
        let file = write_temp_org(concat!(
            "* TODO Aufgabe\n",
            ":PROPERTIES:\n",
            ":CUSTOM_ID: h:x\n",
            ":END:\n",
            "* Kapitel 2\n",
            "Text B\n",
            "** Unter 2.1\n",
        ));
        let h = &extract_headings_from_file(file.path())[0];

        // A state change: nothing about the body.
        let payload = HeadingUpdatePayload { state: Some("DONE".into()), ..Default::default() };
        let block = heading_utils::build_org_heading_block(h, &payload).join("\n");

        assert!(block.contains("\n* Kapitel 2"), "not demoted: {block}");
        assert!(block.contains("\n** Unter 2.1"), "and its child keeps its depth: {block}");
    }

    /// The other half: text typed in the app's description editor still
    /// gets the offset, so a `* Notiz` stays part of this heading's body.
    #[test]
    fn an_edited_body_still_goes_through_the_ghost_prefix_offset() {
        let file = write_temp_org("* TODO Aufgabe\n:PROPERTIES:\n:CUSTOM_ID: h:x\n:END:\nAlt\n");
        let h = &extract_headings_from_file(file.path())[0];

        let payload = HeadingUpdatePayload { content: Some("* Notiz".into()), ..Default::default() };
        let block = heading_utils::build_org_heading_block(h, &payload).join("\n");

        assert!(block.contains("\n** Notiz"), "typed text is demoted so it stays body: {block}");
    }

    /* --- statistics cookies vs. everything else in square brackets --- */

    /// A link in a heading ends in `]` and contains `/`, which the old
    /// "contains % or /" test read as a cookie and cut out of the title.
    #[test]
    fn a_link_at_the_end_of_a_heading_is_not_a_statistics_cookie() {
        let file = write_temp_org("* TODO Siehe [[http://example.com/doku][Doku]]\n");
        let h = &extract_headings_from_file(file.path())[0];

        assert_eq!(h.title, "Siehe [[http://example.com/doku][Doku]]");
        assert!(h.progress_stats.is_none());
    }

    #[test]
    fn a_real_cookie_is_still_recognised() {
        for (heading, title) in [("* Projekt [33%]", "Projekt"), ("* Projekt [1/3]", "Projekt"), ("* Projekt [/]", "Projekt")] {
            let file = write_temp_org(&format!("{heading}\n:PROPERTIES:\n:CUSTOM_ID: h:x\n:END:\n"));
            assert_eq!(extract_headings_from_file(file.path())[0].title, title, "{heading}");
        }
    }

    /// `[2026-01-01]` and `[fn:1]` end in `]` too.
    #[test]
    fn a_bracketed_date_or_footnote_stays_in_the_title() {
        let file = write_temp_org("* TODO Notiz [fn:1]\n");
        assert_eq!(extract_headings_from_file(file.path())[0].title, "Notiz [fn:1]");
    }

    /* --- drawer markers --- */

    /// Org drawer names are case-insensitive; only the uppercase spelling
    /// was recognised, so a lowercase drawer hid the CUSTOM_ID and the
    /// heading silently became a different heading vault-wide.
    #[test]
    fn a_lowercase_properties_drawer_is_still_a_properties_drawer() {
        let file = write_temp_org("* TODO Aufgabe\n:properties:\n:custom_id: h:7\n:end:\n");
        let h = &extract_headings_from_file(file.path())[0];

        // Looked up case-insensitively, because the file's own spelling is
        // deliberately preserved (see models::set_property).
        assert_eq!(crate::core::models::property_value(&h.properties, "CUSTOM_ID"), Some("h:7"));
        assert!(!h.content.contains("custom_id"), "the id must not end up as body text: {:?}", h.content);
    }

    /// Body text of the shape `:word:` used to be deleted outright -- the
    /// only silent text loss in the parser.
    #[test]
    fn colon_wrapped_body_text_is_not_mistaken_for_a_drawer() {
        let file = write_temp_org("* TODO Aufgabe\n:shrug:\nund weiter\n");
        let h = &extract_headings_from_file(file.path())[0];

        assert!(h.content.contains(":shrug:"), "{:?}", h.content);
        assert!(h.content.contains("und weiter"), "{:?}", h.content);
    }

    /// A drawer nobody here understands comes back exactly as it went in.
    ///
    /// It used to be flattened: the markers were dropped and the contents
    /// were written back as ordinary body text. The words survived, the
    /// structure did not -- folding, non-export, and whatever the drawer
    /// meant.
    #[test]
    fn a_foreign_drawer_round_trips_verbatim() {
        let file = write_temp_org("* TODO Aufgabe\n:NOTES:\ngeheim\n:END:\nText\n");
        let h = &extract_headings_from_file(file.path())[0];

        assert_eq!(h.raw_drawers, vec![vec![":NOTES:".to_string(), "geheim".to_string(), ":END:".to_string()]]);
        assert!(!h.content.contains("geheim"), "not also loose in the body: {:?}", h.content);
        assert!(h.content.contains("Text"), "the real body still is: {:?}", h.content);

        let block = heading_utils::build_org_heading_block(h, &HeadingUpdatePayload::default());
        let rewritten = block.join("\n");
        assert!(rewritten.contains(":NOTES:\ngeheim\n:END:"), "{rewritten}");
    }

    /// `org-log-done note`, `org-log-reschedule` and `C-c C-z` all write
    /// notes into the LOGBOOK. They used to be thrown out of the drawer
    /// and written back below it as body text -- and an indented
    /// continuation line landed under a different log entry, destroying
    /// the link between a note and the state change it explains.
    #[test]
    fn a_logbook_note_stays_inside_the_logbook() {
        let file = write_temp_org(concat!(
            "* DONE Aufgabe\n",
            ":LOGBOOK:\n",
            "- Note taken on [2026-08-05 Wed 09:00] \\\\\n",
            "  weil der Kunde abgesagt hat\n",
            "CLOCK: [2026-08-05 Wed 08:00]--[2026-08-05 Wed 09:00] =>  1:00\n",
            ":END:\n",
            "Body\n",
        ));
        let h = &extract_headings_from_file(file.path())[0];

        let block = heading_utils::build_org_heading_block(h, &HeadingUpdatePayload::default()).join("\n");
        let logbook = block.split(":LOGBOOK:").nth(1).and_then(|rest| rest.split(":END:").next()).unwrap_or("");

        assert!(logbook.contains("Note taken on"), "the note is inside the drawer: {block}");
        assert!(logbook.contains("weil der Kunde abgesagt hat"), "and so is its continuation: {block}");
    }

    /// The lookahead must not run past the heading it belongs to.
    #[test]
    fn a_marker_closed_only_under_the_next_heading_is_body_text() {
        let file = write_temp_org("* TODO Erste\n:offen:\n* TODO Zweite\n:END:\n");
        let headings = extract_headings_from_file(file.path());

        assert!(headings[0].content.contains(":offen:"), "{:?}", headings[0].content);
    }

    /* --- an unreadable DEADLINE is kept, not deleted --- */

    #[test]
    fn a_deadline_this_build_cannot_read_survives_a_rewrite() {
        let file = write_temp_org("* TODO A\nDEADLINE: <irgendwas seltsames>\n");
        let h = &extract_headings_from_file(file.path())[0];

        let block = heading_utils::build_org_heading_block(h, &HeadingUpdatePayload::default());
        assert!(block.iter().any(|l| l.contains("irgendwas seltsames")), "the deadline must not be dropped: {block:?}");
    }

    /// A `:PROPERTIES:` drawer belonging to an absorbed sub-heading (e.g. a
    /// bookmark entry `*** Title` with `:URL:` under a `* Collection`
    /// heading, or a recipe in a multi-recipe file) must stay in place
    /// under its own sub-heading rather than being swallowed into the
    /// first heading's `properties` / `repeated_properties` and hoisted
    /// to the top of the file on `stamp_custom_id_only`.
    #[test]
    fn properties_drawers_under_absorbed_sub_headings_stay_in_place() {
        let original = concat!(
            "* Rust\n",
            "** Async\n",
            "*** Beitrag 1 :rust:\n",
            ":PROPERTIES:\n",
            ":URL: https://example.org/1\n",
            ":END:\n",
            "Notiz 1\n",
            "*** Beitrag 2 :rust:\n",
            ":PROPERTIES:\n",
            ":URL: https://example.org/2\n",
            ":END:\n",
        );
        let file = write_temp_org(original);
        let headings = extract_headings_from_file(file.path());
        assert_eq!(headings.len(), 1);
        assert!(!headings[0].properties.contains_key("URL"), "sub-heading URLs do not belong to the top heading");
        assert!(headings[0].repeated_properties.is_empty());

        let stamped = heading_utils::stamp_custom_id_only(&headings[0]).join("\n");
        assert!(stamped.contains("*** Beitrag 1 :rust:\n:PROPERTIES:\n:URL: https://example.org/1\n:END:"), "{stamped}");
        assert!(stamped.contains("*** Beitrag 2 :rust:\n:PROPERTIES:\n:URL: https://example.org/2\n:END:"), "{stamped}");
    }
}
