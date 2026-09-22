//! Org tables as a grid, and back again.
//!
//! A table on disk is space-aligned pipe text, which is unreadable to edit
//! by hand on a phone and unforgiving to edit by hand anywhere: every
//! column's width depends on every row. So nothing edits the text -- an
//! operation reads the whole table into cells, changes the cells, and the
//! whole table is written back formatted. That is also why there is no
//! "insert a column here" that leaves the rest alone: there is no such
//! thing in a format where the rest has to move.

use serde::{Deserialize, Serialize};

/// One row: either data, or one of org's `|---+---|` rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Row {
    Separator,
    Cells(Vec<String>),
}

/// A table and where it sits, in 0-based line indices.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Table {
    pub start: usize,
    pub end: usize,
    pub rows: Vec<Row>,
    /// The indent the table is written at, kept so a table inside a list
    /// stays inside it.
    pub indent: String,
}

impl Table {
    /// The widest data row, which is how many columns the table has.
    pub fn columns(&self) -> usize {
        self.rows
            .iter()
            .filter_map(|row| match row {
                Row::Cells(cells) => Some(cells.len()),
                Row::Separator => None,
            })
            .max()
            .unwrap_or(0)
    }

    /// The first data row, which org treats as the header when a rule
    /// follows it.
    pub fn header(&self) -> Option<&Vec<String>> {
        match (self.rows.first(), self.rows.get(1)) {
            (Some(Row::Cells(cells)), Some(Row::Separator)) => Some(cells),
            _ => None,
        }
    }

    /// The index in `rows` of the Nth data row, skipping rules.
    pub fn data_row(&self, index: usize) -> Option<usize> {
        self.rows
            .iter()
            .enumerate()
            .filter(|(_, row)| matches!(row, Row::Cells(_)))
            .nth(index)
            .map(|(at, _)| at)
    }
}

fn parse_row(line: &str) -> Row {
    let inner = line.trim().trim_matches('|');
    if !inner.is_empty() && inner.chars().all(|ch| ch == '-' || ch == '+' || ch == '|') {
        return Row::Separator;
    }
    Row::Cells(inner.split('|').map(|cell| cell.trim().to_string()).collect())
}

/// The table the line at LINE (0-based) belongs to, if any.
pub fn find_at(lines: &[&str], line: usize) -> Option<Table> {
    if !lines.get(line).is_some_and(|text| text.trim().starts_with('|')) {
        return None;
    }
    let mut start = line;
    while start > 0 && lines[start - 1].trim().starts_with('|') {
        start -= 1;
    }
    let mut end = line;
    while end + 1 < lines.len() && lines[end + 1].trim().starts_with('|') {
        end += 1;
    }
    let indent: String = lines[start].chars().take_while(|c| c.is_whitespace()).collect();
    Some(Table {
        start,
        end,
        rows: lines[start..=end].iter().map(|line| parse_row(line)).collect(),
        indent,
    })
}

/// The table as lines, every column as wide as its widest cell.
///
/// Measured in characters, which is what the file's own alignment means
/// and what every other org editor will read it back as.
pub fn render(table: &Table) -> Vec<String> {
    let columns = table.columns();
    // At least one character per column, the way org's own formatter
    // leaves an empty one -- a zero-width column is a `||` nobody can aim
    // a caret into.
    let mut widths = vec![1usize; columns];
    for row in &table.rows {
        if let Row::Cells(cells) = row {
            for (index, cell) in cells.iter().enumerate() {
                widths[index] = widths[index].max(cell.chars().count());
            }
        }
    }

    table
        .rows
        .iter()
        .map(|row| match row {
            Row::Separator => {
                let middle: Vec<String> = widths.iter().map(|width| "-".repeat(width + 2)).collect();
                format!("{}|{}|", table.indent, middle.join("+"))
            }
            Row::Cells(cells) => {
                let middle: Vec<String> = (0..columns)
                    .map(|index| {
                        let cell = cells.get(index).map(String::as_str).unwrap_or("");
                        format!(" {cell}{} ", " ".repeat(widths[index] - cell.chars().count()))
                    })
                    .collect();
                format!("{}|{}|", table.indent, middle.join("|"))
            }
        })
        .collect()
}

/// Where a new row or column goes, relative to the one the caret is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Position {
    Before,
    After,
}

pub fn insert_row(table: &mut Table, at: usize, position: Position) {
    let columns = table.columns();
    let index = match position {
        Position::Before => at,
        Position::After => at + 1,
    };
    table.rows.insert(index.min(table.rows.len()), Row::Cells(vec![String::new(); columns]));
}

pub fn delete_row(table: &mut Table, at: usize) {
    if table.rows.len() > 1 && at < table.rows.len() {
        table.rows.remove(at);
    }
}

pub fn insert_column(table: &mut Table, at: usize, position: Position) {
    let index = match position {
        Position::Before => at,
        Position::After => at + 1,
    };
    for row in &mut table.rows {
        if let Row::Cells(cells) = row {
            cells.insert(index.min(cells.len()), String::new());
        }
    }
}

pub fn delete_column(table: &mut Table, at: usize) {
    if table.columns() <= 1 {
        return;
    }
    for row in &mut table.rows {
        if let Row::Cells(cells) = row {
            if at < cells.len() {
                cells.remove(at);
            }
        }
    }
}

/// A fresh table: a header row, a rule, and `rows` empty rows.
pub fn new_table(rows: usize, columns: usize) -> String {
    let columns = columns.max(1);
    let table = Table {
        start: 0,
        end: 0,
        indent: String::new(),
        rows: std::iter::once(Row::Cells(vec![String::new(); columns]))
            .chain(std::iter::once(Row::Separator))
            .chain(std::iter::repeat(Row::Cells(vec![String::new(); columns])).take(rows.max(1)))
            .collect(),
    };
    render(&table).join("\n")
}

/// Which column a character offset on a table line is in.
pub fn column_at(line: &str, column: u32) -> usize {
    let before: String = line.chars().take(column as usize).collect();
    // The leading pipe opens the first column, so it does not count.
    before.matches('|').count().saturating_sub(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(text: &str) -> Table {
        let lines: Vec<&str> = text.split('\n').collect();
        find_at(&lines, 0).expect("a table")
    }

    #[test]
    fn a_table_is_read_and_written_back_aligned() {
        let out = render(&table("| a | bb |\n|---+---|\n| ccc | d |"));
        assert_eq!(out, vec!["| a   | bb |", "|-----+----|", "| ccc | d  |"]);
    }

    #[test]
    fn a_ragged_table_gets_its_missing_cells() {
        let out = render(&table("| a | b | c |\n| d |"));
        assert_eq!(out, vec!["| a | b | c |", "| d |   |   |"]);
    }

    #[test]
    fn rows_and_columns_come_and_go() {
        let mut grid = table("| a | b |\n|---+---|\n| 1 | 2 |");
        insert_row(&mut grid, 2, Position::After);
        assert_eq!(render(&grid).len(), 4);
        insert_column(&mut grid, 0, Position::After);
        assert_eq!(render(&grid)[0], "| a |   | b |");
        delete_column(&mut grid, 1);
        delete_row(&mut grid, 3);
        assert_eq!(render(&grid), vec!["| a | b |", "|---+---|", "| 1 | 2 |"]);
    }

    #[test]
    fn the_last_column_cannot_be_deleted_away() {
        let mut grid = table("| only |");
        delete_column(&mut grid, 0);
        assert_eq!(render(&grid), vec!["| only |"]);
    }

    #[test]
    fn a_header_needs_the_rule_under_it() {
        assert_eq!(table("| a | b |\n|---+---|\n| 1 | 2 |").header(), Some(&vec!["a".to_string(), "b".to_string()]));
        assert!(table("| a | b |\n| 1 | 2 |").header().is_none());
    }

    #[test]
    fn a_new_table_has_a_header_and_its_rows() {
        assert_eq!(new_table(2, 3), "|   |   |   |\n|---+---+---|\n|   |   |   |\n|   |   |   |");
    }

    #[test]
    fn an_offset_lands_in_a_column() {
        assert_eq!(column_at("| a | b | c |", 2), 0);
        assert_eq!(column_at("| a | b | c |", 6), 1);
        assert_eq!(column_at("| a | b | c |", 10), 2);
    }

    #[test]
    fn a_table_inside_a_list_keeps_its_indent() {
        let lines = vec!["- item", "  | a | b |", "  | 1 | 2 |"];
        let grid = find_at(&lines, 1).expect("a table");
        assert_eq!(grid.start, 1);
        assert_eq!(render(&grid)[0], "  | a | b |");
    }
}
