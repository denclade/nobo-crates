//! Sorting what the caret is in, the way `org-sort` does: a table's rows
//! by a column, a list's items, a heading's children.
//!
//! One module for all three because the question is the same -- "what are
//! the things here, and in which order do they go" -- and because the
//! answers have to agree: a date sorts as a date whether it sits in a
//! table cell, a list item or a heading's SCHEDULED line.
//!
//! Two halves. `target` says what would be sorted at the caret and which
//! orders make sense for it, so a host can offer exactly those (a
//! column of prices gets "1 → 9", a list of checkboxes gets "open first").
//! `sort` does it and hands back the lines that replace the old ones. Both
//! are pure: text and caret in, nothing read or written.
//!
//! What moves together:
//!
//! - **Table:** the rows between two rules (`|---+---|`) -- org's own
//!   grouping -- never the header above the first rule. A table with no
//!   rules is one group.
//! - **List:** an item with everything under it -- continuation lines,
//!   sub-items -- among the items at its own indent. Numbered bullets are
//!   renumbered afterwards, so "1. 2. 3." stays "1. 2. 3." (and "1)" stays
//!   "1)").
//! - **Headings:** a heading's direct children with their whole subtrees.
//!   On a line above the first heading, the top-level headings.
//!
//! Empty values go last in either direction: "Z to A" means the Z first,
//! not the blanks.

use std::cmp::Ordering;

use serde::{Deserialize, Serialize};

use crate::core::models::TODO_STATES;
use crate::core::org_ast::{self, heading_level, indent_of, list_of};
use crate::core::org_parser::{closed_block_lines, parse_heading_line};
use crate::core::org_table::{self, Row, Table};

/// By what.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "lowercase")]
pub enum SortKey {
    /// Letters, case and accents ignored, numbers inside compared as
    /// numbers ("Raum 2" before "Raum 10").
    Alpha,
    /// The number the value starts with -- "12,50 €", "3 h".
    Numeric,
    /// The date (and time) in the value: an org timestamp, 2026-10-07 or
    /// 07.10.2026. For headings, their SCHEDULED/DEADLINE.
    Time,
    /// Open checkboxes first, then partly done, then done (lists).
    Checkbox,
    /// In the order of the TODO keywords (headings).
    Todo,
    /// `[#A]` before `[#B]` (headings).
    Priority,
}

/// What kind of thing is being sorted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "lowercase")]
pub enum SortScope {
    Table,
    List,
    Headings,
}

/// What would be sorted at the caret, for a host to offer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct SortTarget {
    pub scope: SortScope,
    /// The column's header for a table, the parent's title for headings;
    /// `None` when there is nothing to name it by.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// 0-based, tables only.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[specta(type = Option<specta_typescript::Number>)]
    pub column: Option<usize>,
    /// How many things would change places.
    #[specta(type = specta_typescript::Number)]
    pub count: usize,
    /// The orders that make sense here, `Alpha` always first.
    pub keys: Vec<SortKey>,
    /// The one to offer first: a date column by date, a number column by
    /// number, everything else alphabetically.
    pub suggested: SortKey,
}

/// The result of a sort: lines FIRST..=LAST (0-based) become LINES.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sorted {
    pub first: usize,
    pub last: usize,
    pub lines: Vec<String>,
}

// ---------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------

/// What would be sorted at CURSOR (a UTF-16 offset), if anything.
pub fn target(text: &str, cursor: u32) -> Option<SortTarget> {
    let found = locate(text, cursor)?;
    Some(found.target())
}

/// Sorts what is at CURSOR by KEY. `None` when there is nothing to sort
/// there, or KEY is not one of the orders `target` offers.
pub fn sort(text: &str, cursor: u32, key: SortKey, descending: bool) -> Option<Sorted> {
    let found = locate(text, cursor)?;
    if !found.target().keys.contains(&key) {
        return None;
    }
    Some(found.apply(key, descending))
}

/// The same for a table on its own, as a table editor holds it: the rows
/// of the group INDEX (into `table.rows`) is in, by COLUMN.
pub fn table_target(table: &Table, index: usize, column: usize) -> Option<SortTarget> {
    TableGroup::new(table, index, column).map(|group| group.target(table))
}

/// Sorts that group in place. False when there is nothing to sort.
pub fn sort_table(table: &mut Table, index: usize, column: usize, key: SortKey, descending: bool) -> bool {
    let Some(group) = TableGroup::new(table, index, column) else { return false };
    if !group.target(table).keys.contains(&key) {
        return false;
    }
    group.apply(table, key, descending);
    true
}

// ---------------------------------------------------------------------
// Finding what to sort
// ---------------------------------------------------------------------

enum Found {
    Table { table: Table, group: TableGroup },
    Blocks(Blocks),
}

impl Found {
    fn target(&self) -> SortTarget {
        match self {
            Found::Table { table, group } => group.target(table),
            Found::Blocks(blocks) => blocks.target(),
        }
    }

    fn apply(self, key: SortKey, descending: bool) -> Sorted {
        match self {
            Found::Table { mut table, group } => {
                group.apply(&mut table, key, descending);
                Sorted { first: table.start, last: table.end, lines: org_table::render(&table) }
            }
            Found::Blocks(blocks) => blocks.apply(key, descending),
        }
    }
}

fn locate(text: &str, cursor: u32) -> Option<Found> {
    let context = org_ast::context(text, cursor);
    let lines: Vec<&str> = text.split('\n').collect();
    let line = context.line.checked_sub(1)?;
    match context.kind.as_str() {
        "table" => {
            let table = org_table::find_at(&lines, line)?;
            let column = org_table::column_at(lines[line], context.column);
            let group = TableGroup::new(&table, line - table.start, column)?;
            Some(Found::Table { table, group })
        }
        // A list line, or a line that continues one (an indented sentence
        // under an item): both are "in the list".
        "list" | "text" if list_item_at(&lines, line).is_some() => {
            list(&lines, list_item_at(&lines, line)?).map(Found::Blocks)
        }
        "heading" => headings(&lines, Some(line)).map(Found::Blocks),
        // Above the first heading -- the title, the keywords, a blank
        // line: the top-level headings.
        _ if first_heading(&lines).is_some_and(|first| line < first) => headings(&lines, None).map(Found::Blocks),
        // In an entry's own text: that entry's children, as org-sort does
        // with point in the body. Not from inside a code block, where the
        // caret is in code, not in the entry.
        _ if !closed_block_lines(&lines)[line] => {
            enclosing_heading(&lines, line).and_then(|parent| headings(&lines, Some(parent))).map(Found::Blocks)
        }
        _ => None,
    }
}

/// The heading whose entry LINE is in -- the nearest heading above it
/// that is not a star line in a code block.
fn enclosing_heading(lines: &[&str], line: usize) -> Option<usize> {
    let in_block = closed_block_lines(lines);
    (0..line).rev().find(|&at| !in_block[at] && heading_level(lines[at]).is_some())
}

/// For each line inside a closed block, the indentation of the line that
/// opens it (`None` outside blocks) -- which item of a list the block,
/// and everything in it, belongs to.
fn block_indents(lines: &[&str]) -> Vec<Option<usize>> {
    let in_block = closed_block_lines(lines);
    let mut indents = vec![None; lines.len()];
    let mut opener = 0;
    for at in 0..lines.len() {
        if !in_block[at] {
            continue;
        }
        if at == 0 || !in_block[at - 1] {
            opener = indent_of(lines[at]);
        }
        indents[at] = Some(opener);
    }
    indents
}

// ---------------------------------------------------------------------
// Tables
// ---------------------------------------------------------------------

/// The rows of one group: `table.rows[first..=last]`, all data rows.
struct TableGroup {
    first: usize,
    last: usize,
    column: usize,
}

impl TableGroup {
    fn new(table: &Table, index: usize, column: usize) -> Option<TableGroup> {
        let header = table.header().is_some();
        // On the header itself: the rows it heads, i.e. the first group
        // under the rule.
        let index = if header && index == 0 { 2 } else { index };
        if !matches!(table.rows.get(index)?, Row::Cells(_)) {
            return None;
        }
        let is_data = |at: usize| matches!(table.rows.get(at), Some(Row::Cells(_))) && !(header && at == 0);
        let mut first = index;
        while first > 0 && is_data(first - 1) {
            first -= 1;
        }
        let mut last = index;
        while is_data(last + 1) {
            last += 1;
        }
        (last > first).then_some(TableGroup { first, last, column })
    }

    fn values<'a>(&self, table: &'a Table) -> Vec<&'a str> {
        table.rows[self.first..=self.last]
            .iter()
            .map(|row| match row {
                Row::Cells(cells) => cells.get(self.column).map(String::as_str).unwrap_or(""),
                Row::Separator => "",
            })
            .collect()
    }

    fn target(&self, table: &Table) -> SortTarget {
        let values = self.values(table);
        let (keys, suggested) = keys_for_values(&values);
        let label = table
            .header()
            .and_then(|header| header.get(self.column))
            .map(|name| name.trim().to_string())
            .filter(|name| !name.is_empty());
        SortTarget {
            scope: SortScope::Table,
            label,
            column: Some(self.column),
            count: values.len(),
            keys,
            suggested,
        }
    }

    fn apply(&self, table: &mut Table, key: SortKey, descending: bool) {
        let values: Vec<Value> = self.values(table).iter().map(|value| Value::of_text(value)).collect();
        let order = order(&values, key, descending);
        let rows: Vec<Row> = order.iter().map(|&at| table.rows[self.first + at].clone()).collect();
        table.rows.splice(self.first..=self.last, rows);
    }
}

// ---------------------------------------------------------------------
// Lists and headings: runs of lines that move as one
// ---------------------------------------------------------------------

/// Units that change places, each a run of lines, in file order.
struct Blocks {
    scope: SortScope,
    label: Option<String>,
    /// The first line of the first unit.
    first: usize,
    units: Vec<Unit>,
    /// Blank lines after each position -- kept by POSITION, not by unit:
    /// the gap between the second and third item stays there whichever
    /// items end up second and third, and the last unit's trailing blank
    /// lines (which separate the list from what follows) stay last.
    gaps: Vec<usize>,
    /// Numbered or lettered bullets to rewrite after sorting.
    renumber: Option<Renumber>,
}

struct Unit {
    lines: Vec<String>,
    value: Value,
}

/// Where the numbering starts, and the terminator it is written with
/// (`1.` or `1)`).
#[derive(Clone, Copy)]
enum Renumber {
    Number(usize, char),
    Letter(u8, char),
}

impl Blocks {
    fn target(&self) -> SortTarget {
        let keys_and_suggested = match self.scope {
            SortScope::Headings => {
                let mut keys = vec![SortKey::Alpha];
                if self.units.iter().any(|unit| unit.value.state.is_some()) {
                    keys.push(SortKey::Todo);
                }
                if self.units.iter().any(|unit| unit.value.priority.is_some()) {
                    keys.push(SortKey::Priority);
                }
                if self.units.iter().any(|unit| unit.value.time.is_some()) {
                    keys.push(SortKey::Time);
                }
                (keys, SortKey::Alpha)
            }
            _ => {
                let texts: Vec<&str> = self.units.iter().map(|unit| unit.value.text.as_str()).collect();
                let (mut keys, suggested) = keys_for_values(&texts);
                if self.units.iter().any(|unit| unit.value.checkbox.is_some()) {
                    keys.push(SortKey::Checkbox);
                }
                (keys, suggested)
            }
        };
        SortTarget {
            scope: self.scope,
            label: self.label.clone(),
            column: None,
            count: self.units.len(),
            keys: keys_and_suggested.0,
            suggested: keys_and_suggested.1,
        }
    }

    fn apply(self, key: SortKey, descending: bool) -> Sorted {
        let values: Vec<Value> = self.units.iter().map(|unit| unit.value.clone()).collect();
        let order = order(&values, key, descending);
        let mut lines = Vec::new();
        let last_original = self.first + self.units.iter().map(|u| u.lines.len()).sum::<usize>()
            + self.gaps.iter().sum::<usize>()
            - 1;
        for (position, &at) in order.iter().enumerate() {
            let mut unit_lines = self.units[at].lines.clone();
            if let Some(renumber) = self.renumber {
                if let Some(first) = unit_lines.first_mut() {
                    *first = with_bullet(first, &bullet_at(renumber, position));
                }
            }
            lines.extend(unit_lines);
            lines.extend(std::iter::repeat_n(String::new(), self.gaps[position]));
        }
        Sorted { first: self.first, last: last_original, lines }
    }
}

/// Splits LINES[START..END] into units at the given unit starts, with the
/// blank lines at the end of each unit taken off into `gaps`.
fn split_units(lines: &[&str], starts: &[usize], end: usize, value_of: impl Fn(&[&str]) -> Value) -> (Vec<Unit>, Vec<usize>) {
    let mut units = Vec::new();
    let mut gaps = Vec::new();
    for (at, &start) in starts.iter().enumerate() {
        let stop = starts.get(at + 1).copied().unwrap_or(end);
        let mut body: Vec<&str> = lines[start..stop].to_vec();
        let mut gap = 0;
        while body.len() > 1 && body.last().is_some_and(|line| line.trim().is_empty()) {
            body.pop();
            gap += 1;
        }
        units.push(Unit { value: value_of(&body), lines: body.iter().map(|line| line.to_string()).collect() });
        gaps.push(gap);
    }
    (units, gaps)
}

// --- lists -------------------------------------------------------------

/// The list item LINE belongs to: the line itself, or the nearest item
/// above that it is indented under.
fn list_item_at(lines: &[&str], line: usize) -> Option<usize> {
    let blocks = block_indents(lines);
    // A `- x` in a code block is code, not an item.
    if blocks[line].is_some() {
        return None;
    }
    if list_of(lines[line]).is_some() {
        return Some(line);
    }
    let indent = indent_of(lines[line]);
    if lines[line].trim().is_empty() || indent == 0 {
        return None;
    }
    (0..line).rev().take_while(|&at| !lines[at].trim().is_empty() && heading_level(lines[at]).is_none()).find(|&at| {
        blocks[at].is_none() && list_of(lines[at]).is_some() && indent_of(lines[at]) < indent
    })
}

fn list(lines: &[&str], item: usize) -> Option<Blocks> {
    let indent = indent_of(lines[item]);
    let blocks = block_indents(lines);
    // A line in a code block is never an item of its own -- a `- x` in
    // there is code, and taking it for a sibling tore the block apart,
    // its #+BEGIN under one item and its #+END under another.
    let is_sibling = |at: usize| blocks[at].is_none() && list_of(lines[at]).is_some() && indent_of(lines[at]) == indent;
    // Belongs to the item above: deeper than the bullet, or blank between
    // two parts of the list. A block belongs as a whole, by where its
    // #+BEGIN sits, whatever its contents are indented to.
    let continues = |at: usize| {
        if let Some(opener) = blocks[at] {
            return opener > indent;
        }
        let line = lines[at];
        if line.trim().is_empty() {
            return false;
        }
        indent_of(line) > indent && heading_level(line).is_none()
    };

    // Up to the first sibling: past continuation lines and single blank
    // lines, stopping at anything shallower.
    let mut first = item;
    let mut at = item;
    while at > 0 {
        at -= 1;
        if is_sibling(at) {
            first = at;
        } else if !(continues(at) || (lines[at].trim().is_empty() && at > 0 && (continues(at - 1) || is_sibling(at - 1)))) {
            break;
        }
    }

    // Down: every sibling, with what follows it.
    let mut starts = vec![first];
    let mut at = first + 1;
    let mut end = lines.len();
    while at < lines.len() {
        if is_sibling(at) {
            starts.push(at);
        } else if lines[at].trim().is_empty() {
            // A blank line inside the list only when the list goes on
            // after it; otherwise it is the list's end.
            let next = (at + 1..lines.len()).find(|&next| !lines[next].trim().is_empty());
            if !next.is_some_and(|next| is_sibling(next) || continues(next)) {
                end = at;
                break;
            }
        } else if !continues(at) {
            end = at;
            break;
        }
        at += 1;
    }
    if starts.len() < 2 {
        return None;
    }

    let renumber = list_of(lines[first]).and_then(|(bullet, _)| {
        let (mark, terminator) = crate::core::org_edit::ordered_bullet(&bullet)?;
        if let Ok(number) = mark.parse::<usize>() {
            Some(Renumber::Number(number, terminator))
        } else {
            mark.bytes().next().filter(u8::is_ascii_alphabetic).map(|letter| Renumber::Letter(letter, terminator))
        }
    });
    let (units, gaps) = split_units(lines, &starts, end, |body| {
        let (_, checkbox) = list_of(body[0]).unwrap_or_default();
        Value { checkbox, ..Value::of_text(&item_text(body[0])) }
    });
    Some(Blocks { scope: SortScope::List, label: None, first, units, gaps, renumber })
}

/// An item's first line without its bullet and checkbox.
fn item_text(line: &str) -> String {
    let rest = line.trim_start();
    let after_bullet = rest.split_once(char::is_whitespace).map(|(_, tail)| tail.trim_start()).unwrap_or("");
    let after_box = if after_bullet.len() >= 3 && after_bullet.starts_with('[') && after_bullet.as_bytes()[2] == b']' {
        after_bullet[3..].trim_start()
    } else {
        after_bullet
    };
    after_box.to_string()
}

fn bullet_at(renumber: Renumber, position: usize) -> String {
    match renumber {
        Renumber::Number(start, terminator) => format!("{}{terminator}", start + position),
        // Past "z" stays "z" rather than rolling over (as org_edit does).
        Renumber::Letter(start, terminator) => {
            let last = if start.is_ascii_uppercase() { b'Z' } else { b'z' };
            format!("{}{terminator}", (start as usize + position).min(last as usize) as u8 as char)
        }
    }
}

fn with_bullet(line: &str, bullet: &str) -> String {
    let indent: String = line.chars().take_while(|c| c.is_whitespace()).collect();
    let rest = &line[indent.len()..];
    let after = rest.find(char::is_whitespace).map(|at| &rest[at..]).unwrap_or("");
    format!("{indent}{bullet}{after}")
}

// --- headings ----------------------------------------------------------

fn first_heading(lines: &[&str]) -> Option<usize> {
    let in_block = closed_block_lines(lines);
    (0..lines.len()).find(|&at| !in_block[at] && heading_level(lines[at]).is_some())
}

/// The children of the heading on PARENT, or the top-level headings for
/// `None`.
fn headings(lines: &[&str], parent: Option<usize>) -> Option<Blocks> {
    let in_block = closed_block_lines(lines);
    let level_at = |at: usize| if in_block[at] { None } else { heading_level(lines[at]) };

    let (from, parent_level) = match parent {
        Some(at) => (at + 1, level_at(at)?),
        None => (first_heading(lines)?, 0),
    };
    // The parent's subtree ends at the next heading at its level or above.
    let end = (from..lines.len()).find(|&at| level_at(at).is_some_and(|level| level <= parent_level)).unwrap_or(lines.len());
    let child_level = (from..end).filter_map(level_at).min()?;
    let starts: Vec<usize> = (from..end).filter(|&at| level_at(at) == Some(child_level)).collect();
    if starts.len() < 2 {
        return None;
    }

    let label = parent.and_then(|at| parse_heading_line(lines[at])).map(|heading| heading.title).filter(|title| !title.is_empty());
    let (units, gaps) = split_units(lines, &starts, end, |body| {
        let heading = parse_heading_line(body[0]);
        // The planning line, which org keeps directly under the heading.
        let planning = body.get(1).filter(|line| {
            let line = line.trim_start();
            line.starts_with("SCHEDULED:") || line.starts_with("DEADLINE:") || line.starts_with("CLOSED:")
        });
        Value {
            state: heading.as_ref().and_then(|h| h.state.clone()),
            priority: heading.as_ref().and_then(|h| h.priority.clone()),
            time: planning.and_then(|line| time_of(line)).or_else(|| time_of(body[0])),
            ..Value::of_text(&heading.map(|h| h.title).unwrap_or_default())
        }
    });
    Some(Blocks { scope: SortScope::Headings, label, first: starts[0], units, gaps, renumber: None })
}

// ---------------------------------------------------------------------
// Values and their order
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
struct Value {
    text: String,
    number: Option<f64>,
    time: Option<String>,
    checkbox: Option<String>,
    state: Option<String>,
    priority: Option<String>,
}

impl Value {
    fn of_text(text: &str) -> Value {
        let text = text.trim();
        Value { text: text.to_string(), number: number_of(text), time: time_of(text), ..Value::default() }
    }
}

/// Which orders a run of plain values supports, and which fits best.
/// Numbers and dates only when EVERY non-empty value is one: a column
/// that is half prices, half "auf Anfrage" sorted "by number" would put
/// the words anywhere.
fn keys_for_values(values: &[&str]) -> (Vec<SortKey>, SortKey) {
    let present: Vec<&str> = values.iter().map(|value| value.trim()).filter(|value| !value.is_empty()).collect();
    let all = |test: fn(&str) -> bool| !present.is_empty() && present.iter().all(|value| test(value));
    let mut keys = vec![SortKey::Alpha];
    let mut suggested = SortKey::Alpha;
    if all(|value| number_of(value).is_some()) {
        keys.push(SortKey::Numeric);
        suggested = SortKey::Numeric;
    }
    if all(|value| time_of(value).is_some()) {
        keys.push(SortKey::Time);
        suggested = SortKey::Time;
    }
    (keys, suggested)
}

/// The positions of VALUES in sorted order. Stable, and a missing value
/// goes last whichever way round.
fn order(values: &[Value], key: SortKey, descending: bool) -> Vec<usize> {
    let mut positions: Vec<usize> = (0..values.len()).collect();
    positions.sort_by(|&a, &b| {
        match (rank(&values[a], key), rank(&values[b], key)) {
            (Some(left), Some(right)) => {
                let ordering = left.cmp(&right);
                if descending { ordering.reverse() } else { ordering }
            }
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        }
    });
    positions
}

/// What a value is compared by under KEY, or `None` when it has nothing
/// to compare (an empty cell, a heading with no state).
fn rank(value: &Value, key: SortKey) -> Option<Rank> {
    match key {
        SortKey::Alpha => (!value.text.is_empty()).then(|| Rank::Parts(natural(&value.text))),
        SortKey::Numeric => value.number.map(Rank::Number),
        SortKey::Time => value.time.clone().map(Rank::Text),
        SortKey::Checkbox => value.checkbox.as_deref().map(|state| {
            Rank::Index(match state {
                " " => 0,
                "-" => 1,
                _ => 2,
            })
        }),
        SortKey::Todo => value.state.as_deref().map(|state| {
            Rank::Index(TODO_STATES.iter().position(|known| *known == state).unwrap_or(TODO_STATES.len()))
        }),
        SortKey::Priority => value.priority.clone().map(Rank::Text),
    }
}

#[derive(Debug, PartialEq, PartialOrd)]
enum Rank {
    Parts(Vec<Part>),
    Number(f64),
    Text(String),
    Index(usize),
}

impl Rank {
    fn cmp(&self, other: &Rank) -> Ordering {
        self.partial_cmp(other).unwrap_or(Ordering::Equal)
    }
}

/// A run of letters (folded) or a run of digits (by value).
#[derive(Debug, PartialEq, PartialOrd)]
enum Part {
    Number(u128),
    Text(String),
}

/// TEXT as a natural sort key: case and the common accents folded away,
/// digit runs compared as numbers.
fn natural(text: &str) -> Vec<Part> {
    let mut parts = Vec::new();
    let mut letters = String::new();
    let mut digits = String::new();
    for ch in text.chars() {
        if ch.is_ascii_digit() {
            if !letters.is_empty() {
                parts.push(Part::Text(std::mem::take(&mut letters)));
            }
            digits.push(ch);
        } else {
            if !digits.is_empty() {
                parts.push(Part::Number(digits.parse().unwrap_or(u128::MAX)));
                digits.clear();
            }
            letters.push_str(&fold(ch));
        }
    }
    if !letters.is_empty() {
        parts.push(Part::Text(letters));
    }
    if !digits.is_empty() {
        parts.push(Part::Number(digits.parse().unwrap_or(u128::MAX)));
    }
    parts
}

fn fold(ch: char) -> String {
    match ch {
        'ä' | 'Ä' | 'à' | 'á' | 'â' | 'À' | 'Á' | 'Â' => "a".into(),
        'ö' | 'Ö' | 'ò' | 'ó' | 'ô' | 'Ò' | 'Ó' | 'Ô' => "o".into(),
        'ü' | 'Ü' | 'ù' | 'ú' | 'û' | 'Ù' | 'Ú' | 'Û' => "u".into(),
        'é' | 'è' | 'ê' | 'É' | 'È' | 'Ê' => "e".into(),
        'ß' => "ss".into(),
        _ => ch.to_lowercase().collect(),
    }
}

/// The number a value starts with: "12,50 €" is 12.5, "1.234,5" is
/// 1234.5, "-3" is -3. A currency sign in front is skipped; a word is
/// not a number.
fn number_of(text: &str) -> Option<f64> {
    let text = text.trim().trim_start_matches(['€', '$', '£']).trim_start();
    let mut raw = String::new();
    for (at, ch) in text.chars().enumerate() {
        if ch.is_ascii_digit() || ch == '.' || ch == ',' || (at == 0 && (ch == '-' || ch == '+')) {
            raw.push(ch);
        } else {
            break;
        }
    }
    let raw = raw.trim_end_matches(['.', ',']);
    if !raw.chars().any(|ch| ch.is_ascii_digit()) {
        return None;
    }
    // The later of "." and "," is the decimal mark when both appear; one
    // of them alone is a decimal mark unless it occurs more than once.
    let dots = raw.matches('.').count();
    let commas = raw.matches(',').count();
    let normal = match (dots, commas) {
        (0, 0) => raw.to_string(),
        (_, 0) if dots > 1 => raw.replace('.', ""),
        (0, _) if commas > 1 => raw.replace(',', ""),
        (_, 0) => raw.to_string(),
        (0, _) => raw.replace(',', "."),
        _ if raw.rfind(',') > raw.rfind('.') => raw.replace('.', "").replace(',', "."),
        _ => raw.replace(',', ""),
    };
    normal.parse().ok()
}

/// The first date in TEXT, as "YYYY-MM-DD" or "YYYY-MM-DD HH:MM" -- which
/// sort correctly as strings. Reads ISO dates (as in org timestamps) and
/// German ones (07.10.2026).
fn time_of(text: &str) -> Option<String> {
    let chars: Vec<char> = text.chars().collect();
    let digits = |from: usize, count: usize| -> Option<String> {
        let run: String = chars.get(from..from + count)?.iter().collect();
        run.chars().all(|ch| ch.is_ascii_digit()).then_some(run)
    };
    let boundary = |at: usize| at == 0 || !chars[at - 1].is_ascii_digit();
    for at in 0..chars.len() {
        if !boundary(at) {
            continue;
        }
        // 2026-10-07
        if let (Some(year), Some('-'), Some(month), Some('-'), Some(day)) =
            (digits(at, 4), chars.get(at + 4), digits(at + 5, 2), chars.get(at + 7), digits(at + 8, 2))
        {
            return Some(with_time(format!("{year}-{month}-{day}"), &chars[at + 10..]));
        }
        // 07.10.2026
        if let (Some(day), Some('.'), Some(month), Some('.'), Some(year)) =
            (digits(at, 2), chars.get(at + 2), digits(at + 3, 2), chars.get(at + 5), digits(at + 6, 4))
        {
            return Some(with_time(format!("{year}-{month}-{day}"), &chars[at + 10..]));
        }
    }
    None
}

/// DATE with the first HH:MM that follows it closely (a weekday may sit
/// between, as in `<2026-10-07 Mi 09:30>`).
fn with_time(date: String, after: &[char]) -> String {
    let window: Vec<char> = after.iter().take(9).copied().collect();
    for slice in window.windows(5) {
        if slice[0].is_ascii_digit() && slice[1].is_ascii_digit() && slice[2] == ':' && slice[3].is_ascii_digit() && slice[4].is_ascii_digit() {
            return format!("{date} {}", slice.iter().collect::<String>());
        }
    }
    date
}

#[cfg(test)]
#[path = "org_sort_tests.rs"]
mod tests;
