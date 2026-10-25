//! A generic sortable, filterable table, shared by the property tabs, services, network and find
//! views. Each cell keeps its display text and a sort key, so numbers sort as numbers.

use std::cmp::Ordering;

#[derive(Debug, Clone, PartialEq)]
pub enum SortKey {
    /// Lower-cased for case-insensitive ordering.
    Text(String),
    Number(f64),
    Empty,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Cell {
    pub text: String,
    pub key: SortKey,
}

impl Cell {
    pub fn text(text: impl Into<String>) -> Self {
        let text = text.into();
        let key = if text.is_empty() { SortKey::Empty } else { SortKey::Text(text.to_lowercase()) };
        Self { text, key }
    }

    pub fn number(value: f64, text: impl Into<String>) -> Self {
        Self { text: text.into(), key: SortKey::Number(value) }
    }

    pub fn count(value: u64) -> Self {
        Self::number(value as f64, crate::format::count(value))
    }

    pub fn bytes(value: u64) -> Self {
        Self::number(value as f64, crate::format::bytes(value))
    }

    /// `0x7ff6_1234` style, sorted numerically.
    pub fn hex(value: u64) -> Self {
        Self::number(value as f64, format!("0x{value:x}"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Column {
    pub title: &'static str,
    /// Logical pixels; the last column stretches to fill.
    pub width: u32,
    pub numeric: bool,
}

impl Column {
    pub const fn text(title: &'static str, width: u32) -> Self {
        Self { title, width, numeric: false }
    }

    pub const fn number(title: &'static str, width: u32) -> Self {
        Self { title, width, numeric: true }
    }
}

/// Row tint, matching the process list's highlight colors where it makes sense.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Tone {
    #[default]
    Plain,
    /// e.g. a running service, an enabled privilege.
    Good,
    /// e.g. a stopped service, a disabled privilege, a deny-only group.
    Muted,
    /// e.g. writable+executable memory, a suspended thread.
    Warning,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    /// Stable identity (thread ID, handle value, address...) so a selection survives refreshes.
    pub id: u64,
    pub cells: Vec<Cell>,
    pub tone: Tone,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Table {
    pub columns: Vec<Column>,
    pub rows: Vec<Row>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sort {
    pub column: usize,
    pub ascending: bool,
}

impl Table {
    pub fn new(columns: &[Column]) -> Self {
        Self { columns: columns.to_vec(), rows: Vec::new() }
    }

    pub fn push(&mut self, id: u64, cells: Vec<Cell>, tone: Tone) {
        debug_assert_eq!(cells.len(), self.columns.len());
        self.rows.push(Row { id, cells, tone });
    }

    /// Rows matching `filter` (case-insensitive, any cell), in `sort` order. Empty cells sort last
    /// either way; ties keep the table's own order.
    pub fn view(&self, filter: &str, sort: Option<Sort>) -> Vec<&Row> {
        let needle = filter.trim().to_lowercase();
        let mut rows: Vec<&Row> = self
            .rows
            .iter()
            .filter(|row| needle.is_empty() || row.cells.iter().any(|c| c.text.to_lowercase().contains(&needle)))
            .collect();
        if let Some(Sort { column, ascending }) = sort.filter(|s| s.column < self.columns.len()) {
            rows.sort_by(|a, b| {
                let (a, b) = (&a.cells[column].key, &b.cells[column].key);
                let order = match (a, b) {
                    (SortKey::Empty, SortKey::Empty) => return Ordering::Equal,
                    (SortKey::Empty, _) => return Ordering::Greater,
                    (_, SortKey::Empty) => return Ordering::Less,
                    (SortKey::Number(x), SortKey::Number(y)) => x.total_cmp(y),
                    (SortKey::Text(x), SortKey::Text(y)) => x.cmp(y),
                    (SortKey::Number(_), SortKey::Text(_)) => Ordering::Less,
                    (SortKey::Text(_), SortKey::Number(_)) => Ordering::Greater,
                };
                if ascending { order } else { order.reverse() }
            });
        }
        rows
    }

    /// The default direction for a first click on a column: numbers biggest-first, text A–Z.
    pub fn default_ascending(&self, column: usize) -> bool {
        !self.columns.get(column).is_some_and(|c| c.numeric)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Table {
        let mut table = Table::new(&[Column::text("Name", 100), Column::number("Size", 60)]);
        table.push(1, vec![Cell::text("beta"), Cell::bytes(2048)], Tone::Plain);
        table.push(2, vec![Cell::text("Alpha"), Cell::bytes(10)], Tone::Plain);
        table.push(3, vec![Cell::text(""), Cell::bytes(4096)], Tone::Plain);
        table
    }

    #[test]
    fn sorts_numbers_numerically_and_empties_last() {
        let table = sample();
        let ids = |rows: Vec<&Row>| rows.iter().map(|r| r.id).collect::<Vec<_>>();
        assert_eq!(ids(table.view("", Some(Sort { column: 1, ascending: false }))), [3, 1, 2]);
        assert_eq!(ids(table.view("", Some(Sort { column: 0, ascending: true }))), [2, 1, 3]);
        assert_eq!(ids(table.view("", Some(Sort { column: 0, ascending: false }))), [1, 2, 3], "empty still last");
        assert_eq!(ids(table.view("", None)), [1, 2, 3]);
        assert_eq!(ids(table.view("", Some(Sort { column: 9, ascending: true }))), [1, 2, 3], "bad column ignored");
    }

    #[test]
    fn filters_any_cell_case_insensitively() {
        let table = sample();
        assert_eq!(table.view("ALP", None).len(), 1);
        assert_eq!(table.view("kB", None).len(), 2);
        assert!(table.default_ascending(0) && !table.default_ascending(1));
    }
}
