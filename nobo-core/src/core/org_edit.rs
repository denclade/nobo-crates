//! What a toolbar button does to org text.
//!
//! Every function here is pure: text and a selection in, a replacement
//! out. Nothing reads a file, nothing knows about an editor. That is what
//! makes them testable, and it is also what lets the host apply them
//! through its own editing API -- which matters more than it sounds.
//! nobo's frontend learned this the hard way: an edit the editor makes on
//! the user's behalf has to go through the same path as typing, or the
//! change shows on screen, never reaches the content signal, and is
//! silently lost at the next save. Here the host cannot get that wrong,
//! because all it ever receives is "replace this range with this text".
//!
//! Offsets are UTF-16 code units, like everywhere else this crate talks to
//! a host (see [`crate::core::org_ast`]).

use serde::{Deserialize, Serialize};

use crate::core::org_ast::{self, aggregate_checkboxes, heading_level, indent_of, list_of};
use crate::core::models::TODO_STATES;
use crate::core::org_sort::{self, SortKey};
use crate::core::org_table::{self, Position, Row, Table};

/// One replacement, plus where the caret goes afterwards.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Edit {
    pub from: u32,
    pub to: u32,
    pub insert: String,
    /// Caret position in the document *after* the edit.
    pub cursor: u32,
    /// End of the selection afterwards; equal to `cursor` when there is
    /// none.
    pub cursor_end: u32,
}

impl Edit {
    fn at(from: u32, to: u32, insert: impl Into<String>, cursor: u32) -> Edit {
        Edit { from, to, insert: insert.into(), cursor, cursor_end: cursor }
    }
}

/// A document split into lines, with each line's offset -- the shape
/// almost every edit below needs.
struct Doc {
    lines: Vec<String>,
    starts: Vec<u32>,
}

fn utf16_len(text: &str) -> u32 {
    text.encode_utf16().count() as u32
}

impl Doc {
    fn new(text: &str) -> Doc {
        let lines: Vec<String> = text.split('\n').map(|line| line.to_string()).collect();
        let mut starts = Vec::with_capacity(lines.len());
        let mut offset = 0u32;
        for line in &lines {
            starts.push(offset);
            offset += utf16_len(line) + 1; // the newline
        }
        Doc { lines, starts }
    }

    /// The line an offset sits on.
    fn line_at(&self, offset: u32) -> usize {
        match self.starts.binary_search(&offset) {
            Ok(index) => index,
            Err(0) => 0,
            Err(index) => index - 1,
        }
    }

    fn start(&self, line: usize) -> u32 {
        self.starts[line]
    }

    fn end(&self, line: usize) -> u32 {
        self.starts[line] + utf16_len(&self.lines[line])
    }

    /// An edit that replaces LINE with TEXT, leaving the caret where it
    /// was relative to the line's end -- which is what keeps typing
    /// continuous when a toolbar rewrites the line under the caret.
    fn replace_line(&self, line: usize, text: &str, cursor: u32) -> Edit {
        Edit::at(self.start(line), self.end(line), text, cursor)
    }
}

// ---------------------------------------------------------------------
// Lists
// ---------------------------------------------------------------------

/// The bullet styles a list can be switched to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BulletKind {
    Dash,
    Plus,
    Numbered,
    Lettered,
}

fn bullet_for(kind: BulletKind, index: usize) -> String {
    match kind {
        BulletKind::Dash => "-".to_string(),
        BulletKind::Plus => "+".to_string(),
        BulletKind::Numbered => format!("{}.", index + 1),
        // Letters stop at "z" rather than rolling over: a list that runs
        // out of letters reads better than one that starts again at "a".
        BulletKind::Lettered => format!("{}.", (b'a' + (index.min(25)) as u8) as char),
    }
}

/// The next bullet after BULLET, for a list that is being continued.
fn next_bullet(bullet: &str) -> String {
    if let Some(digits) = bullet.strip_suffix('.') {
        if let Ok(number) = digits.parse::<usize>() {
            return format!("{}.", number + 1);
        }
        if digits.len() == 1 {
            let ch = digits.chars().next().expect("one character");
            if ch.is_ascii_alphabetic() && ch != 'z' && ch != 'Z' {
                return format!("{}.", (ch as u8 + 1) as char);
            }
            return format!("{ch}.");
        }
    }
    bullet.to_string()
}

/// Enter inside a list: the next item, with the same bullet kind and an
/// empty checkbox if this one had any.
///
/// An item that is empty ends the list instead -- the way every editor
/// with list continuation behaves, and the only way out of one without
/// reaching for a delete key.
pub fn continue_list_on_enter(text: &str, cursor: u32) -> Option<Edit> {
    let doc = Doc::new(text);
    let line = doc.line_at(cursor);
    let source = &doc.lines[line];
    let (bullet, checkbox) = list_of(source)?;

    let indent: String = source.chars().take(indent_of(source)).collect();
    let body = source
        .chars()
        .skip(indent.chars().count() + bullet.chars().count())
        .collect::<String>();
    let body = body.trim_start();
    let body = match checkbox {
        Some(_) => body.chars().skip(3).collect::<String>().trim_start().to_string(),
        None => body.to_string(),
    };

    if body.is_empty() {
        // Ends the list: the empty item goes away and the caret lands on a
        // blank line.
        return Some(Edit::at(doc.start(line), doc.end(line), "", doc.start(line)));
    }

    let marker = format!(
        "\n{indent}{} {}",
        next_bullet(&bullet),
        if checkbox.is_some() { "[ ] " } else { "" }
    );
    let at = doc.end(line);
    let cursor = at + utf16_len(&marker);
    Some(Edit::at(at, at, marker, cursor))
}

/// A new list item below the caret's line, with or without a checkbox --
/// the toolbar's list button, for a line that is not a list yet.
pub fn insert_list_item(text: &str, cursor: u32, checkbox: bool) -> Edit {
    let doc = Doc::new(text);
    let line = doc.line_at(cursor);
    let source = &doc.lines[line];
    let marker = if checkbox { "- [ ] " } else { "- " };

    if let Some((bullet, existing)) = list_of(source) {
        // Already a list item: this is the next one.
        let indent: String = source.chars().take(indent_of(source)).collect();
        let inserted = format!(
            "\n{indent}{} {}",
            next_bullet(&bullet),
            if checkbox || existing.is_some() { "[ ] " } else { "" }
        );
        let at = doc.end(line);
        return Edit::at(at, at, inserted.clone(), at + utf16_len(&inserted));
    }

    if source.trim().is_empty() {
        // Turns the empty line itself into the item.
        return Edit::at(doc.start(line), doc.end(line), marker, doc.start(line) + utf16_len(marker));
    }

    let indent: String = source.chars().take(indent_of(source)).collect();
    let inserted = format!("\n{indent}{marker}");
    let at = doc.end(line);
    Edit::at(at, at, inserted.clone(), at + utf16_len(&inserted))
}

/// Ticks, clears or adds a checkbox on the caret's line.
///
/// A line that is a list item without a checkbox gets one; a line that is
/// not a list at all becomes one. A parent whose state is aggregated is
/// left alone -- see [`org_ast::toggle_checkbox`].
pub fn toggle_checkbox(text: &str, cursor: u32) -> Option<Edit> {
    let doc = Doc::new(text);
    let line = doc.line_at(cursor);
    let source = &doc.lines[line];

    match list_of(source) {
        Some((_, Some(_))) => {
            let updated = org_ast::toggle_checkbox(text, line + 1)?;
            // Aggregation may touch parents further up, so the whole
            // document is the edit -- the caret does not move.
            Some(Edit::at(0, utf16_len(text), updated, cursor))
        }
        Some((bullet, None)) => {
            let start = indent_of(source) + bullet.chars().count();
            let gap = source.chars().skip(start).take_while(|c| c.is_whitespace()).count();
            let at = doc.start(line) + start as u32;
            // Replaces the whole gap, so a wide one normalizes instead of
            // leaving a stray space before the text.
            Some(Edit::at(at, at + gap as u32, " [ ] ", cursor + 4))
        }
        None => {
            let at = doc.start(line) + indent_of(source) as u32;
            Some(Edit::at(at, at, "- [ ] ", cursor + 6))
        }
    }
}

/// Rewrites the bullet marker across the whole list the caret is in.
///
/// Always the whole list: mixed bullet styles read as broken, and a
/// numbering that starts again halfway down is worse than none.
pub fn change_list_type(text: &str, cursor: u32, kind: BulletKind) -> Option<Edit> {
    let doc = Doc::new(text);
    let line = doc.line_at(cursor);
    let (_, _) = list_of(&doc.lines[line])?;
    let indent = indent_of(&doc.lines[line]);

    let mut first = line;
    while first > 0 {
        let previous = &doc.lines[first - 1];
        match list_of(previous) {
            Some(_) if indent_of(previous) == indent => first -= 1,
            _ => break,
        }
    }
    let mut last = line;
    while last + 1 < doc.lines.len() {
        let next = &doc.lines[last + 1];
        match list_of(next) {
            Some(_) if indent_of(next) == indent => last += 1,
            _ => break,
        }
    }

    let mut out: Vec<String> = Vec::new();
    for (index, number) in (first..=last).zip(0usize..) {
        let source = &doc.lines[index];
        let (bullet, checkbox) = list_of(source).expect("checked by the walk above");
        let rest: String = source
            .chars()
            .skip(indent_of(source) + bullet.chars().count())
            .collect::<String>()
            .trim_start()
            .to_string();
        let _ = checkbox;
        let pad: String = source.chars().take(indent_of(source)).collect();
        out.push(format!("{pad}{} {rest}", bullet_for(kind, number)));
    }

    let joined = out.join("\n");
    Some(Edit::at(doc.start(first), doc.end(last), joined, cursor))
}

/// Indents or outdents every line the selection touches by two spaces --
/// what Tab does on a keyboard, and what the toolbar's arrows do here.
pub fn indent(text: &str, from: u32, to: u32, direction: i32) -> Edit {
    const STEP: usize = 2;
    let doc = Doc::new(text);
    let first = doc.line_at(from);
    let last = doc.line_at(to.max(from));

    let mut out: Vec<String> = Vec::new();
    for index in first..=last {
        let source = &doc.lines[index];
        if direction > 0 {
            out.push(format!("{}{source}", " ".repeat(STEP)));
        } else {
            let removable = source.chars().take(STEP).take_while(|c| *c == ' ').count();
            out.push(source.chars().skip(removable).collect());
        }
    }

    let joined = out.join("\n");
    let shift = if direction > 0 { STEP as u32 } else { 0 };
    let cursor = if direction > 0 { from + shift } else { from.saturating_sub(STEP as u32) };
    Edit { from: doc.start(first), to: doc.end(last), insert: joined, cursor, cursor_end: cursor }
}

// ---------------------------------------------------------------------
// Headings
// ---------------------------------------------------------------------

/// Makes the caret's line a heading of LEVEL, or plain text at level 0.
pub fn set_heading_level(text: &str, cursor: u32, level: usize) -> Edit {
    let doc = Doc::new(text);
    let line = doc.line_at(cursor);
    let source = &doc.lines[line];

    let body: String = match heading_level(source) {
        Some(stars) => source.chars().skip(stars + 1).collect::<String>().trim_start().to_string(),
        None => source.trim_start().to_string(),
    };
    let updated = if level == 0 { body } else { format!("{} {body}", "*".repeat(level.min(6))) };
    let cursor = doc.start(line) + utf16_len(&updated);
    doc.replace_line(line, &updated, cursor)
}

/// Sets or clears the TODO keyword of the heading the caret is on.
///
/// Only a heading carries one, so a plain line is made into a first-level
/// heading first -- which is what somebody reaching for a state button on
/// a line of text means.
pub fn set_todo_state(text: &str, cursor: u32, state: Option<&str>) -> Edit {
    let doc = Doc::new(text);
    let line = doc.line_at(cursor);
    let source = &doc.lines[line];

    let (stars, rest) = match heading_level(source) {
        Some(stars) => (stars, source.chars().skip(stars + 1).collect::<String>()),
        None => (1usize, source.trim_start().to_string()),
    };
    let rest = rest.trim_start();
    let without_state = match rest.split_once(' ') {
        Some((first, tail)) if TODO_STATES.contains(&first) => tail.to_string(),
        _ if TODO_STATES.contains(&rest) => String::new(),
        _ => rest.to_string(),
    };

    let updated = match state {
        Some(state) if !state.is_empty() => format!("{} {state} {without_state}", "*".repeat(stars)),
        _ => format!("{} {without_state}", "*".repeat(stars)),
    };
    let updated = updated.trim_end().to_string();
    let cursor = doc.start(line) + utf16_len(&updated);
    doc.replace_line(line, &updated, cursor)
}

// ---------------------------------------------------------------------
// Inline markup
// ---------------------------------------------------------------------

fn marker_for(kind: org_ast::InlineKind) -> &'static str {
    match kind {
        org_ast::InlineKind::Bold => "*",
        org_ast::InlineKind::Italic => "/",
        org_ast::InlineKind::Strikethrough => "+",
        org_ast::InlineKind::Monospace => "~",
        org_ast::InlineKind::Underline => "_",
        org_ast::InlineKind::Link => "",
    }
}

/// Wraps the selection (or the word under the caret) in a markup pair, or
/// unwraps it when it is already wrapped.
pub fn toggle_inline_markup(text: &str, from: u32, to: u32, kind: org_ast::InlineKind) -> Edit {
    let marker = marker_for(kind);
    if marker.is_empty() {
        return Edit::at(from, to, "", from);
    }
    let units: Vec<u16> = text.encode_utf16().collect();
    let (from, to) = if from == to { word_at(&units, from) } else { (from, to) };
    let selected = String::from_utf16_lossy(&units[from as usize..to as usize]);

    // Already marked up, either inside the selection or just outside it.
    if selected.starts_with(marker) && selected.ends_with(marker) && selected.chars().count() > 1 {
        let inner: String = selected.chars().skip(1).take(selected.chars().count() - 2).collect();
        let cursor = from + utf16_len(&inner);
        return Edit { from, to, insert: inner, cursor, cursor_end: from };
    }
    let outside_before = from >= 1 && String::from_utf16_lossy(&units[from as usize - 1..from as usize]) == marker;
    let outside_after = (to as usize) < units.len()
        && String::from_utf16_lossy(&units[to as usize..to as usize + 1]) == marker;
    if outside_before && outside_after {
        let cursor = from - 1 + utf16_len(&selected);
        return Edit { from: from - 1, to: to + 1, insert: selected, cursor, cursor_end: from - 1 };
    }

    let wrapped = format!("{marker}{selected}{marker}");
    let cursor = from + utf16_len(&wrapped) - 1;
    Edit { from, to, insert: wrapped, cursor, cursor_end: cursor }
}

/// The word around OFFSET, for the markup buttons when nothing is
/// selected. An empty range when the caret is not in a word, which the
/// caller turns into an empty pair to type inside.
fn word_at(units: &[u16], offset: u32) -> (u32, u32) {
    let is_word = |unit: u16| {
        let ch = char::from_u32(unit as u32).unwrap_or(' ');
        !ch.is_whitespace() && ch != '*' && ch != '/' && ch != '~' && ch != '+'
    };
    let mut start = offset as usize;
    let mut end = offset as usize;
    while start > 0 && units.get(start - 1).copied().is_some_and(is_word) {
        start -= 1;
    }
    while end < units.len() && units.get(end).copied().is_some_and(is_word) {
        end += 1;
    }
    (start as u32, end as u32)
}

/// Inserts a link, using the selection as its description when there is
/// one.
pub fn insert_link(text: &str, from: u32, to: u32, target: &str, description: &str) -> Edit {
    let units: Vec<u16> = text.encode_utf16().collect();
    let selected = if to > from {
        String::from_utf16_lossy(&units[from as usize..to as usize])
    } else {
        String::new()
    };
    let label = if !description.is_empty() {
        description.to_string()
    } else {
        selected
    };
    let inserted = if label.is_empty() {
        format!("[[{target}]]")
    } else {
        format!("[[{target}][{label}]]")
    };
    let cursor = from + utf16_len(&inserted);
    Edit::at(from, to, inserted, cursor)
}

/// Inserts plain text over the selection -- a date, a rule, a whole block
/// template. `cursor_marker` is where the caret should land inside it
/// (org-tempo's `%?`), or the end when there is none.
pub fn insert_text(text: &str, from: u32, to: u32, insert: &str) -> Edit {
    let _ = text;
    match insert.find("%?") {
        Some(index) => {
            let before = utf16_len(&insert[..index]);
            let cleaned = insert.replacen("%?", "", 1);
            Edit::at(from, to, cleaned, from + before)
        }
        None => Edit::at(from, to, insert, from + utf16_len(insert)),
    }
}

// ---------------------------------------------------------------------
// Tables
// ---------------------------------------------------------------------

/// A fresh ROWS x COLUMNS table at the caret, on a line of its own.
pub fn insert_table(text: &str, from: u32, to: u32, rows: usize, columns: usize) -> Edit {
    let doc = Doc::new(text);
    let line = doc.line_at(from);
    let table = org_table::new_table(rows, columns);
    // On its own line: a table that starts halfway through a sentence is
    // not a table any more.
    let leading = if doc.lines[line].trim().is_empty() { "" } else { "\n" };
    let insert = format!("{leading}{table}\n");
    let at = if leading.is_empty() { doc.start(line) } else { doc.end(line) };
    Edit::at(at, to.max(at), insert.clone(), at + utf16_len(&insert))
}

/// One table row as a form: the cells, and the header labels to put over
/// them.
///
/// The row is edited as fields rather than as text because space-aligned
/// pipe text is unusable on a phone keyboard -- which is nobo's own
/// reasoning for its row sheet, and it holds here twice over.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TableRow {
    /// Index among the data rows, rules skipped.
    pub row: usize,
    pub cells: Vec<String>,
    /// One label per column, empty when the table has no header row.
    pub headers: Vec<String>,
    /// The column the caret is in, so a form can focus it.
    pub column: usize,
}

/// The row the caret is in, or `None` when it is not in a table.
pub fn table_row_at(text: &str, cursor: u32) -> Option<TableRow> {
    let doc = Doc::new(text);
    let line = doc.line_at(cursor);
    let lines: Vec<&str> = text.split('\n').collect();
    let table = org_table::find_at(&lines, line)?;
    let index = line - table.start;
    let cells = match table.rows.get(index)? {
        Row::Cells(cells) => cells.clone(),
        Row::Separator => return None,
    };
    let row = table.rows[..index].iter().filter(|row| matches!(row, Row::Cells(_))).count();
    Some(TableRow {
        row,
        column: org_table::column_at(&doc.lines[line], cursor - doc.start(line)),
        headers: table.header().cloned().unwrap_or_default(),
        cells,
    })
}

/// Replaces the caret's row with CELLS and reformats the whole table --
/// every column's width depends on every row, so there is no smaller edit
/// to make.
pub fn set_table_row(text: &str, cursor: u32, cells: Vec<String>) -> Option<Edit> {
    with_table(text, cursor, |table, index| {
        table.rows[index] = Row::Cells(cells);
        true
    })
}

pub fn table_insert_row(text: &str, cursor: u32, position: Position) -> Option<Edit> {
    with_table(text, cursor, |table, index| {
        org_table::insert_row(table, index, position);
        true
    })
}

pub fn table_delete_row(text: &str, cursor: u32) -> Option<Edit> {
    with_table(text, cursor, |table, index| {
        org_table::delete_row(table, index);
        true
    })
}

pub fn table_insert_column(text: &str, cursor: u32, position: Position) -> Option<Edit> {
    let column = column_of(text, cursor)?;
    with_table(text, cursor, move |table, _| {
        org_table::insert_column(table, column, position);
        true
    })
}

pub fn table_delete_column(text: &str, cursor: u32) -> Option<Edit> {
    let column = column_of(text, cursor)?;
    with_table(text, cursor, move |table, _| {
        org_table::delete_column(table, column);
        true
    })
}

/// The table under the caret as a grid, for an editor that shows it as
/// one (see `replace_table`), with where the caret is in it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TableAt {
    pub table: Table,
    /// Index into `table.rows`.
    pub row: usize,
    pub column: usize,
}

pub fn table_at(text: &str, cursor: u32) -> Option<TableAt> {
    let doc = Doc::new(text);
    let line = doc.line_at(cursor);
    let lines: Vec<&str> = text.split('\n').collect();
    let table = org_table::find_at(&lines, line)?;
    Some(TableAt {
        row: line - table.start,
        column: org_table::column_at(&doc.lines[line], cursor - doc.start(line)),
        table,
    })
}

/// The table under the caret replaced by ROWS, as a table editor hands
/// them back -- one edit for everything done there, so one undo.
pub fn replace_table(text: &str, cursor: u32, rows: Vec<Row>) -> Option<Edit> {
    if rows.is_empty() {
        return None;
    }
    with_table(text, cursor, move |table, _| {
        table.rows = rows;
        true
    })
}

/// Sorts what the caret is in -- a table column, a list, a heading's
/// children -- by KEY (see `org_sort`). The caret goes to the start of
/// what was sorted.
pub fn sort(text: &str, cursor: u32, key: SortKey, descending: bool) -> Option<Edit> {
    let sorted = org_sort::sort(text, cursor, key, descending)?;
    let doc = Doc::new(text);
    let from = doc.start(sorted.first);
    // Also when nothing moved: "already in that order" is an answer, and
    // `None` means "nothing to sort here".
    Some(Edit::at(from, doc.end(sorted.last), sorted.lines.join("\n"), from))
}

/// Re-aligns the table under the caret without changing a cell.
pub fn format_table(text: &str, cursor: u32) -> Option<Edit> {
    with_table(text, cursor, |_, _| true)
}

fn column_of(text: &str, cursor: u32) -> Option<usize> {
    let doc = Doc::new(text);
    let line = doc.line_at(cursor);
    doc.lines.get(line).map(|text| org_table::column_at(text, cursor - doc.start(line)))
}

/// The shape every table edit shares: read the whole table, change it,
/// write the whole table back.
fn with_table(
    text: &str,
    cursor: u32,
    change: impl FnOnce(&mut org_table::Table, usize) -> bool,
) -> Option<Edit> {
    let doc = Doc::new(text);
    let line = doc.line_at(cursor);
    let lines: Vec<&str> = text.split('\n').collect();
    let mut table = org_table::find_at(&lines, line)?;
    let index = line - table.start;
    if !change(&mut table, index) {
        return None;
    }
    let rendered = org_table::render(&table).join("\n");
    let from = doc.start(table.start);
    let to = doc.end(table.end);
    // The caret stays on its own line, at the start of it: the column
    // widths have just moved, so any finer position would be a guess.
    Some(Edit::at(from, to, rendered, from))
}

// ---------------------------------------------------------------------
// Source blocks
// ---------------------------------------------------------------------

/// Sets the language of the `#+BEGIN_SRC` the caret is in.
pub fn set_src_language(text: &str, cursor: u32, language: &str) -> Option<Edit> {
    let doc = Doc::new(text);
    let mut line = doc.line_at(cursor);
    // Walk back to the BEGIN line the caret is under.
    loop {
        let trimmed = doc.lines[line].trim();
        if trimmed.len() > 2 && trimmed[2..].to_uppercase().starts_with("BEGIN_SRC") {
            break;
        }
        if line == 0 {
            return None;
        }
        line -= 1;
    }
    let source = &doc.lines[line];
    let indent: String = source.chars().take(indent_of(source)).collect();
    let rest = source.trim();
    let after = &rest["#+BEGIN_SRC".len().min(rest.len())..];
    // Keep the header arguments, replace only the language token.
    let arguments = match after.trim().split_once(" :") {
        Some((_, tail)) => format!(" :{tail}"),
        None if after.trim().starts_with(':') => format!(" {}", after.trim()),
        None => String::new(),
    };
    let updated = format!("{indent}#+BEGIN_SRC {language}{arguments}");
    Some(doc.replace_line(line, updated.trim_end(), doc.start(line) + utf16_len(updated.trim_end())))
}

/// Re-runs the checkbox aggregation over the whole document -- what the
/// editor calls after an edit that may have changed a child's state.
pub fn refresh_checkboxes(text: &str) -> Option<Edit> {
    let updated = aggregate_checkboxes(text);
    (updated != text).then(|| Edit::at(0, utf16_len(text), updated, 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Applies an edit the way a host would, so a test reads as the text
    /// the user would see.
    fn apply(text: &str, edit: &Edit) -> String {
        let units: Vec<u16> = text.encode_utf16().collect();
        let mut out = String::from_utf16_lossy(&units[..edit.from as usize]);
        out.push_str(&edit.insert);
        out.push_str(&String::from_utf16_lossy(&units[edit.to as usize..]));
        out
    }

    #[test]
    fn enter_continues_a_list_with_its_own_bullet() {
        let text = "1. first";
        let edit = continue_list_on_enter(text, 8).expect("a list line");
        assert_eq!(apply(text, &edit), "1. first\n2. ");
        assert_eq!(edit.cursor, 12);
    }

    #[test]
    fn enter_carries_the_checkbox_but_not_its_state() {
        let text = "- [X] done";
        let edit = continue_list_on_enter(text, 10).expect("a list line");
        assert_eq!(apply(text, &edit), "- [X] done\n- [ ] ");
    }

    #[test]
    fn enter_on_an_empty_item_ends_the_list() {
        let text = "- first\n- ";
        let edit = continue_list_on_enter(text, 10).expect("a list line");
        assert_eq!(apply(text, &edit), "- first\n");
    }

    #[test]
    fn indentation_keeps_the_nesting() {
        let text = "- one\n  - two";
        let edit = continue_list_on_enter(text, 13).expect("a list line");
        assert_eq!(apply(text, &edit), "- one\n  - two\n  - ");
    }

    #[test]
    fn the_checkbox_button_adds_then_ticks() {
        let added = toggle_checkbox("- plain", 7).expect("a list line");
        assert_eq!(apply("- plain", &added), "- [ ] plain");
        let ticked = toggle_checkbox("- [ ] plain", 11).expect("a list line");
        assert_eq!(apply("- [ ] plain", &ticked), "- [X] plain");
        let made = toggle_checkbox("plain text", 0).expect("any line");
        assert_eq!(apply("plain text", &made), "- [ ] plain text");
    }

    #[test]
    fn changing_the_bullet_renumbers_the_whole_list() {
        let text = "- one\n- two\n- three\n\nafter";
        let edit = change_list_type(text, 0, BulletKind::Numbered).expect("a list line");
        assert_eq!(apply(text, &edit), "1. one\n2. two\n3. three\n\nafter");
    }

    #[test]
    fn indent_and_outdent_move_every_touched_line() {
        let text = "one\ntwo\nthree";
        let edit = indent(text, 0, 5, 1);
        assert_eq!(apply(text, &edit), "  one\n  two\nthree");
        let back = indent("  one\n  two", 0, 8, -1);
        assert_eq!(apply("  one\n  two", &back), "one\ntwo");
    }

    #[test]
    fn a_heading_level_can_be_set_and_taken_away() {
        let edit = set_heading_level("some text", 0, 2);
        assert_eq!(apply("some text", &edit), "** some text");
        let back = set_heading_level("** some text", 0, 0);
        assert_eq!(apply("** some text", &back), "some text");
        let deeper = set_heading_level("* one", 0, 3);
        assert_eq!(apply("* one", &deeper), "*** one");
    }

    #[test]
    fn a_todo_state_replaces_the_one_that_is_there() {
        let edit = set_todo_state("* TODO Buy milk", 0, Some("DONE"));
        assert_eq!(apply("* TODO Buy milk", &edit), "* DONE Buy milk");
        let cleared = set_todo_state("* DONE Buy milk", 0, None);
        assert_eq!(apply("* DONE Buy milk", &cleared), "* Buy milk");
        let added = set_todo_state("* Buy milk", 0, Some("NEXT"));
        assert_eq!(apply("* Buy milk", &added), "* NEXT Buy milk");
    }

    #[test]
    fn a_state_button_on_plain_text_makes_a_heading() {
        let edit = set_todo_state("buy milk", 0, Some("TODO"));
        assert_eq!(apply("buy milk", &edit), "* TODO buy milk");
    }

    #[test]
    fn markup_wraps_a_selection_and_unwraps_it_again() {
        let text = "one two three";
        let edit = toggle_inline_markup(text, 4, 7, org_ast::InlineKind::Bold);
        assert_eq!(apply(text, &edit), "one *two* three");
        let back = toggle_inline_markup("one *two* three", 4, 9, org_ast::InlineKind::Bold);
        assert_eq!(apply("one *two* three", &back), "one two three");
    }

    #[test]
    fn markup_without_a_selection_takes_the_word_under_the_caret() {
        let text = "one two three";
        let edit = toggle_inline_markup(text, 5, 5, org_ast::InlineKind::Monospace);
        assert_eq!(apply(text, &edit), "one ~two~ three");
    }

    #[test]
    fn markup_unwraps_when_the_markers_are_just_outside_the_selection() {
        let text = "one *two* three";
        let edit = toggle_inline_markup(text, 5, 8, org_ast::InlineKind::Bold);
        assert_eq!(apply(text, &edit), "one two three");
    }

    #[test]
    fn a_link_uses_the_selection_as_its_description() {
        let text = "see the manual here";
        let edit = insert_link(text, 8, 14, "https://example.org", "");
        assert_eq!(apply(text, &edit), "see the [[https://example.org][manual]] here");
    }

    #[test]
    fn inserted_text_puts_the_caret_where_the_marker_was() {
        let edit = insert_text("", 0, 0, "#+BEGIN_SRC %?\n#+END_SRC");
        assert_eq!(apply("", &edit), "#+BEGIN_SRC \n#+END_SRC");
        assert_eq!(edit.cursor, 12);
    }

    #[test]
    fn a_new_table_lands_on_its_own_line() {
        let text = "before";
        let edit = insert_table(text, 6, 6, 1, 2);
        assert_eq!(apply(text, &edit), "before\n|   |   |\n|---+---|\n|   |   |\n");
    }

    #[test]
    fn a_row_is_read_as_fields_and_written_back_formatted() {
        let text = "| Name | Value |\n|------+-------|\n| a | 1 |";
        let row = table_row_at(text, 36).expect("a data row");
        assert_eq!(row.headers, vec!["Name".to_string(), "Value".to_string()]);
        assert_eq!(row.cells, vec!["a".to_string(), "1".to_string()]);
        assert_eq!(row.row, 1);

        let edit = set_table_row(text, 36, vec!["longer".into(), "2".into()]).expect("a table");
        assert_eq!(
            apply(text, &edit),
            "| Name   | Value |\n|--------+-------|\n| longer | 2     |"
        );
    }

    #[test]
    fn a_separator_row_is_not_a_form() {
        let text = "| a |\n|---|\n| 1 |";
        assert!(table_row_at(text, 8).is_none());
    }

    #[test]
    fn a_column_is_added_where_the_caret_is() {
        let text = "| a | b |\n|---+---|\n| 1 | 2 |";
        let edit = table_insert_column(text, 2, org_table::Position::After).expect("a table");
        assert_eq!(apply(text, &edit), "| a |   | b |\n|---+---+---|\n| 1 |   | 2 |");
    }

    #[test]
    fn a_source_blocks_language_can_be_changed_from_inside_it() {
        let text = "#+BEGIN_SRC python :results output\nprint(1)\n#+END_SRC";
        let edit = set_src_language(text, 40, "bash").expect("a source block");
        assert_eq!(apply(text, &edit), "#+BEGIN_SRC bash :results output\nprint(1)\n#+END_SRC");
    }

    #[test]
    fn a_language_can_be_set_on_a_block_that_had_none() {
        let text = "#+BEGIN_SRC\ncode\n#+END_SRC";
        let edit = set_src_language(text, 0, "python").expect("a source block");
        assert_eq!(apply(text, &edit), "#+BEGIN_SRC python\ncode\n#+END_SRC");
    }

    #[test]
    fn offsets_hold_up_with_umlauts() {
        // Every offset here is a UTF-16 unit; a byte index would land
        // inside "ü" and split the word in the middle.
        let text = "Grüße *fett* mehr";
        let edit = toggle_inline_markup(text, 13, 13, org_ast::InlineKind::Italic);
        assert_eq!(apply(text, &edit), "Grüße *fett* /mehr/");
    }
}
