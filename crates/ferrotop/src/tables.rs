//! Glue between core [`Table`]s and the UI's generic table component: sorting, filtering,
//! selection and the column layout (order and widths, which the user can change and which are
//! saved) live here, and the Slint models are updated in place so lists keep their scroll
//! position.

use std::collections::HashMap;
use std::rc::Rc;

use ferrotop_core::table::{Row, Sort, Table, Tone};
use slint::{Model, ModelRc, SharedString, VecModel};

use crate::{TableColumn, TableMeta, TableRowData};

/// A column layout: titles in display order, with their widths.
pub type ColumnLayout = Vec<(String, u32)>;

const MIN_WIDTH: u32 = 36;
const MAX_WIDTH: u32 = 2000;

pub struct TableView {
    pub table: Table,
    pub sort: Option<Sort>,
    filter: String,
    /// The selected row's id, so the selection survives refreshes and re-sorting.
    pub selected: Option<u64>,
    /// Row ids in display order.
    order: Vec<u64>,
    pub columns: Rc<VecModel<TableColumn>>,
    pub rows: Rc<VecModel<TableRowData>>,
    pub status: String,
    pub empty_text: String,
    shown_columns: Vec<(&'static str, u32)>,
    /// Columns in display order, as indexes into `table.columns`.
    display: Vec<usize>,
    /// Widths chosen by the user, by column title.
    widths: HashMap<String, u32>,
    /// The saved layout, applied whenever this table's columns appear.
    saved: ColumnLayout,
}

impl TableView {
    pub fn new(sort: Option<Sort>) -> Self {
        Self {
            table: Table::default(),
            sort,
            filter: String::new(),
            selected: None,
            order: Vec::new(),
            columns: Rc::new(VecModel::default()),
            rows: Rc::new(VecModel::default()),
            status: String::new(),
            empty_text: String::new(),
            shown_columns: Vec::new(),
            display: Vec::new(),
            widths: HashMap::new(),
            saved: Vec::new(),
        }
    }

    pub fn columns_model(&self) -> ModelRc<TableColumn> {
        ModelRc::from(Rc::clone(&self.columns))
    }

    pub fn rows_model(&self) -> ModelRc<TableRowData> {
        ModelRc::from(Rc::clone(&self.rows))
    }

    /// Sets the saved column layout, applying it now if the table is showing.
    pub fn set_layout(&mut self, layout: ColumnLayout) {
        self.saved = layout;
        if !self.table.columns.is_empty() {
            self.apply_saved();
            self.rebuild_columns();
            self.refresh();
        }
    }

    /// The current layout, for saving.
    pub fn layout(&self) -> ColumnLayout {
        self.display
            .iter()
            .filter_map(|&i| self.table.columns.get(i))
            .map(|column| (column.title.to_owned(), self.width_of(column.title, column.width)))
            .collect()
    }

    fn width_of(&self, title: &str, default: u32) -> u32 {
        self.widths.get(title).copied().unwrap_or(default)
    }

    /// Orders and sizes the current columns as saved: saved titles first, in their order, then
    /// any columns the layout doesn't know (new in a later version) in their natural order.
    fn apply_saved(&mut self) {
        let titles: Vec<&str> = self.table.columns.iter().map(|c| c.title).collect();
        let mut display: Vec<usize> =
            self.saved.iter().filter_map(|(title, _)| titles.iter().position(|t| t == title)).collect();
        display.dedup();
        for i in 0..titles.len() {
            if !display.contains(&i) {
                display.push(i);
            }
        }
        self.display = display;
        self.widths = self
            .saved
            .iter()
            .filter(|(title, _)| titles.contains(&title.as_str()))
            .map(|(title, width)| (title.clone(), (*width).clamp(MIN_WIDTH, MAX_WIDTH)))
            .collect();
    }

    fn rebuild_columns(&mut self) {
        let mut x = 0.0;
        let models: Vec<TableColumn> = self
            .display
            .iter()
            .filter_map(|&i| Some((i, self.table.columns.get(i)?)))
            .map(|(index, column)| {
                let width = self.width_of(column.title, column.width) as f32;
                let model =
                    TableColumn { title: column.title.into(), x, width, numeric: column.numeric, index: index as i32 };
                x += width;
                model
            })
            .collect();
        crate::sync_model(&self.columns, models);
    }

    /// Replaces the data, keeping sort, filter, selection and layout.
    pub fn set_table(&mut self, table: Table) {
        // A different set of columns (another tab) invalidates the sort.
        let columns: Vec<(&'static str, u32)> = table.columns.iter().map(|c| (c.title, c.width)).collect();
        let changed = columns != self.shown_columns;
        if changed {
            if self.sort.is_some_and(|s| s.column >= table.columns.len()) || !self.shown_columns.is_empty() {
                self.sort = None;
            }
            self.shown_columns = columns;
        }
        self.table = table;
        if changed {
            self.apply_saved();
            self.rebuild_columns();
        }
        self.refresh();
    }

    /// The user dragged column `position` (in display order) to `width` pixels.
    pub fn resize(&mut self, position: usize, width: f32) {
        let Some(column) = self.display.get(position).and_then(|&i| self.table.columns.get(i)) else { return };
        self.widths.insert(column.title.to_owned(), (width.round() as u32).clamp(MIN_WIDTH, MAX_WIDTH));
        self.rebuild_columns();
    }

    /// The user dropped column `position` (in display order) with its middle at `x`.
    pub fn move_column(&mut self, position: usize, x: f32) {
        if position >= self.display.len() {
            return;
        }
        let target = drop_position(&self.display_widths(), x);
        let column = self.display.remove(position);
        self.display.insert(target.min(self.display.len()), column);
        self.rebuild_columns();
        self.refresh();
    }

    fn display_widths(&self) -> Vec<f32> {
        self.display
            .iter()
            .filter_map(|&i| self.table.columns.get(i))
            .map(|c| self.width_of(c.title, c.width) as f32)
            .collect()
    }

    pub fn clear(&mut self, empty_text: &str) {
        self.selected = None;
        self.empty_text = empty_text.to_owned();
        self.set_table(Table { columns: self.table.columns.clone(), rows: Vec::new() });
    }

    pub fn set_filter(&mut self, filter: &str) {
        self.filter = filter.to_owned();
        self.refresh();
    }

    /// Rebuilds the visible rows from the table, filter, sort and selection.
    pub fn refresh(&mut self) {
        let rows = self.table.view(&self.filter, self.sort);
        self.order = rows.iter().map(|row| row.id).collect();
        let selected = self.selected;
        let display = &self.display;
        let new: Vec<(Vec<SharedString>, i32, bool)> = rows
            .iter()
            .map(|row| {
                let tone = match row.tone {
                    Tone::Plain => 0,
                    Tone::Good => 1,
                    Tone::Muted => 2,
                    Tone::Warning => 3,
                };
                let cells = display
                    .iter()
                    .map(|&i| row.cells.get(i).map_or_else(SharedString::new, |c| SharedString::from(c.text.as_str())))
                    .collect();
                (cells, tone, Some(row.id) == selected)
            })
            .collect();
        sync_rows(&self.rows, new);
    }

    pub fn content_width(&self) -> f32 {
        self.display_widths().iter().sum()
    }

    pub fn selected_index(&self) -> i32 {
        self.selected.and_then(|id| self.order.iter().position(|row| *row == id)).map_or(-1, |i| i as i32)
    }

    pub fn meta(&self) -> TableMeta {
        TableMeta {
            content_width: self.content_width(),
            sort_column: self.sort.map_or(-1, |s| s.column as i32),
            sort_ascending: self.sort.is_some_and(|s| s.ascending),
            selected_index: self.selected_index(),
            status: self.status.as_str().into(),
            empty_text: self.empty_text.as_str().into(),
        }
    }

    /// Header click: the column's natural order, then reversed, then unsorted. `column` is the
    /// column's index in the data, not its display position.
    pub fn sort_by(&mut self, column: usize) {
        let default = self.table.default_ascending(column);
        self.sort = match self.sort {
            Some(Sort { column: c, ascending }) if c == column && ascending == default => {
                Some(Sort { column, ascending: !ascending })
            }
            Some(Sort { column: c, .. }) if c == column => None,
            _ => Some(Sort { column, ascending: default }),
        };
        self.refresh();
    }

    pub fn select_index(&mut self, index: i32) {
        self.selected = usize::try_from(index).ok().and_then(|i| self.order.get(i)).copied();
        self.refresh();
    }

    pub fn move_selection(&mut self, step: i32) {
        if self.order.is_empty() {
            return;
        }
        let last = self.order.len() as i32 - 1;
        let next = match self.selected_index() {
            -1 => 0,
            current => (current + step).clamp(0, last),
        };
        self.select_index(next);
    }

    pub fn selected_row(&self) -> Option<&Row> {
        let id = self.selected?;
        self.table.rows.iter().find(|row| row.id == id)
    }
}

/// Where a column dropped with its middle at `x` lands, given the widths of the columns in
/// display order: the position whose span holds `x` (past the end means last).
pub fn drop_position(widths: &[f32], x: f32) -> usize {
    let mut right = 0.0;
    for (position, width) in widths.iter().enumerate() {
        right += width;
        if x < right {
            return position;
        }
    }
    widths.len().saturating_sub(1)
}

/// Updates rows in place, touching only those whose text, tone or selection changed.
fn sync_rows(model: &VecModel<TableRowData>, rows: Vec<(Vec<SharedString>, i32, bool)>) {
    let len = rows.len();
    for (index, (cells, tone, selected)) in rows.into_iter().enumerate() {
        let unchanged = model.row_data(index).is_some_and(|row| {
            row.tone == tone
                && row.selected == selected
                && row.cells.row_count() == cells.len()
                && cells.iter().enumerate().all(|(i, cell)| row.cells.row_data(i).as_ref() == Some(cell))
        });
        if unchanged {
            continue;
        }
        let row = TableRowData { cells: ModelRc::new(VecModel::from(cells)), tone, selected };
        if index < model.row_count() {
            model.set_row_data(index, row);
        } else {
            model.push(row);
        }
    }
    while model.row_count() > len {
        model.remove(model.row_count() - 1);
    }
}

#[cfg(test)]
mod tests {
    use ferrotop_core::table::{Cell, Column};

    use super::*;

    fn table() -> Table {
        let mut table = Table::new(&[Column::text("A", 100), Column::number("B", 50), Column::text("C", 80)]);
        table.push(1, vec![Cell::text("a1"), Cell::count(2), Cell::text("c1")], Tone::Plain);
        table
    }

    fn cells(view: &TableView) -> Vec<String> {
        let row = view.rows.row_data(0).expect("row");
        (0..row.cells.row_count()).map(|i| row.cells.row_data(i).unwrap_or_default().to_string()).collect()
    }

    #[test]
    fn drops_land_where_the_middle_falls() {
        assert_eq!(drop_position(&[100.0, 50.0, 80.0], 10.0), 0);
        assert_eq!(drop_position(&[100.0, 50.0, 80.0], 120.0), 1);
        assert_eq!(drop_position(&[100.0, 50.0, 80.0], 9999.0), 2);
        assert_eq!(drop_position(&[], 5.0), 0);
    }

    #[test]
    fn columns_move_resize_and_restore() {
        let mut view = TableView::new(None);
        view.set_table(table());
        assert_eq!(cells(&view), ["a1", "2", "c1"]);
        // Drag C (position 2) to the front, widen B.
        view.move_column(2, 5.0);
        view.resize(2, 120.4);
        assert_eq!(cells(&view), ["c1", "a1", "2"]);
        let layout = view.layout();
        assert_eq!(layout, [("C".into(), 80), ("A".into(), 100), ("B".into(), 120)]);
        assert_eq!(view.columns.row_data(2).map(|c| (c.x, c.index)), Some((180.0, 1)), "B sits after C and A");
        assert_eq!(view.content_width(), 300.0);

        // A new view restores it, even with a column it hasn't seen.
        let mut restored = TableView::new(None);
        restored.set_layout(layout);
        let mut wider = Table::new(&[
            Column::text("A", 100),
            Column::number("B", 50),
            Column::text("C", 80),
            Column::text("D", 60),
        ]);
        wider.push(1, vec![Cell::text("a1"), Cell::count(2), Cell::text("c1"), Cell::text("d1")], Tone::Plain);
        restored.set_table(wider);
        assert_eq!(cells(&restored), ["c1", "a1", "2", "d1"]);
        // Sorting still uses the data's column, wherever it's shown.
        restored.sort_by(1);
        assert_eq!(restored.sort.map(|s| s.column), Some(1));
    }
}
