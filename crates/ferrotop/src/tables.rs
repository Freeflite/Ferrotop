//! Glue between core [`Table`]s and the UI's generic table component: sorting, filtering and
//! selection live here, and the Slint models are updated in place so lists keep their scroll
//! position.

use std::rc::Rc;

use ferrotop_core::table::{Row, Sort, Table, Tone};
use slint::{Model, ModelRc, SharedString, VecModel};

use crate::{TableColumn, TableMeta, TableRowData};

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
        }
    }

    pub fn columns_model(&self) -> ModelRc<TableColumn> {
        ModelRc::from(Rc::clone(&self.columns))
    }

    pub fn rows_model(&self) -> ModelRc<TableRowData> {
        ModelRc::from(Rc::clone(&self.rows))
    }

    /// Replaces the data, keeping sort, filter and selection.
    pub fn set_table(&mut self, table: Table) {
        // A different set of columns (another tab) invalidates the sort.
        let columns: Vec<(&'static str, u32)> = table.columns.iter().map(|c| (c.title, c.width)).collect();
        if columns != self.shown_columns {
            if self.sort.is_some_and(|s| s.column >= table.columns.len()) || !self.shown_columns.is_empty() {
                self.sort = None;
            }
            self.shown_columns = columns;
            let mut x = 0.0;
            let models: Vec<TableColumn> = table
                .columns
                .iter()
                .map(|column| {
                    let width = column.width as f32;
                    let model = TableColumn { title: column.title.into(), x, width, numeric: column.numeric };
                    x += width;
                    model
                })
                .collect();
            crate::sync_model(&self.columns, models);
        }
        self.table = table;
        self.refresh();
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
        let new: Vec<(Vec<SharedString>, i32, bool)> = rows
            .iter()
            .map(|row| {
                let tone = match row.tone {
                    Tone::Plain => 0,
                    Tone::Good => 1,
                    Tone::Muted => 2,
                    Tone::Warning => 3,
                };
                (
                    row.cells.iter().map(|c| SharedString::from(c.text.as_str())).collect(),
                    tone,
                    Some(row.id) == selected,
                )
            })
            .collect();
        sync_rows(&self.rows, new);
    }

    pub fn content_width(&self) -> f32 {
        self.table.columns.iter().map(|c| c.width as f32).sum()
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

    /// Header click: the column's natural order, then reversed, then unsorted.
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
