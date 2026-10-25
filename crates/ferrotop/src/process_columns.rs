//! The process list's columns besides Name: what each shows and how, which are shown in what
//! order, and how wide. The user can show, hide, resize and drag them; all of it is saved.

use ferrotop_core::format;
use ferrotop_core::provider::{ProcessItem, is_pseudo_process};
use ferrotop_core::tree::SortColumn;
use slint::{ModelRc, SharedString, VecModel};

use crate::{App, ColumnChoice, MainWindow, ProcessColumn, UI_COLUMNS, tables};

// Text colors; see `ProcessColumn.kind` in widgets.slint.
const PLAIN: i32 = 0;
const MUTED: i32 = 1;
const CPU: i32 = 2;
const GPU: i32 = 3;
const DISK: i32 = 4;
const NETWORK: i32 = 5;
const MEMORY: i32 = 6;

const MIN_WIDTH: u32 = 36;
const MAX_WIDTH: u32 = 2000;

pub struct ColumnDef {
    pub column: SortColumn,
    /// The heading.
    pub title: &'static str,
    /// The View menu's name for it.
    pub menu: &'static str,
    pub width: u32,
    pub numeric: bool,
    pub kind: i32,
}

const fn def(
    column: SortColumn,
    title: &'static str,
    menu: &'static str,
    width: u32,
    numeric: bool,
    kind: i32,
) -> ColumnDef {
    ColumnDef { column, title, menu, width, numeric, kind }
}

/// Every column besides Name, in the View menu's order, which is also where a column goes when
/// it's shown again.
pub const COLUMNS: [ColumnDef; 16] = [
    def(SortColumn::Pid, "PID", "PID", 60, true, MUTED),
    def(SortColumn::Cpu, "CPU", "CPU", 56, true, CPU),
    def(SortColumn::Gpu, "GPU", "GPU", 52, true, GPU),
    def(SortColumn::Memory, "RAM", "RAM (private working set)", 84, true, MEMORY),
    def(SortColumn::IoRate, "I/O total", "I/O total", 78, true, DISK),
    def(SortColumn::Disk, "Disk", "Disk", 78, true, DISK),
    def(SortColumn::Network, "Network", "Network", 78, true, NETWORK),
    def(SortColumn::PrivateBytes, "Private bytes", "Private bytes", 84, true, PLAIN),
    def(SortColumn::WorkingSet, "Working set", "Working set", 84, true, PLAIN),
    def(SortColumn::GpuDedicated, "GPU memory", "GPU memory", 86, true, PLAIN),
    def(SortColumn::GpuShared, "Shared GPU", "Shared GPU", 86, true, PLAIN),
    def(SortColumn::Threads, "Threads", "Threads", 62, true, PLAIN),
    def(SortColumn::Handles, "Handles", "Handles", 70, true, PLAIN),
    def(SortColumn::User, "User name", "User name", 130, false, PLAIN),
    def(SortColumn::Description, "Description", "Description", 170, false, PLAIN),
    def(SortColumn::Signer, "Verified signer", "Verified signer", 170, false, PLAIN),
];

fn definition(column: SortColumn) -> Option<&'static ColumnDef> {
    COLUMNS.iter().find(|d| d.column == column)
}

/// A process's text for `column`.
pub fn cell(column: SortColumn, item: &ProcessItem) -> SharedString {
    let text = match column {
        SortColumn::Pid if is_pseudo_process(item.key.pid) => String::new(),
        SortColumn::Pid => item.key.pid.to_string(),
        SortColumn::Cpu => format::cpu(item.cpu_usage),
        SortColumn::Gpu => format::cpu((item.gpu.usage / 100.0) as f32),
        SortColumn::Memory => format::bytes(item.private_working_set),
        SortColumn::IoRate => format::rate(item.io_total_rate()),
        SortColumn::Disk => format::rate(item.disk_rate()),
        SortColumn::Network => format::rate(item.network_rate()),
        SortColumn::PrivateBytes => format::bytes(item.private_bytes),
        SortColumn::WorkingSet => format::bytes(item.working_set),
        SortColumn::GpuDedicated => format::bytes(item.gpu.dedicated),
        SortColumn::GpuShared => format::bytes(item.gpu.shared),
        SortColumn::Threads => format::count(u64::from(item.threads)),
        SortColumn::Handles => format::count(u64::from(item.handles)),
        SortColumn::User => item.user().to_owned(),
        SortColumn::Description => item.description().to_owned(),
        SortColumn::Signer => item.signer(),
        SortColumn::Name | SortColumn::Natural => String::new(),
    };
    text.into()
}

impl App {
    /// The shown columns, in display order.
    pub(crate) fn shown_columns(&self) -> Vec<&'static ColumnDef> {
        self.settings.columns.iter().filter_map(|&column| definition(column)).collect()
    }

    fn column_width(&self, def: &ColumnDef) -> u32 {
        self.settings.column_widths.get(&def.column).copied().unwrap_or(def.width)
    }

    /// Sends the columns, their widths and the View menu's checkboxes to the window.
    pub(crate) fn push_columns(&self, ui: &MainWindow) {
        let mut x = 0.0;
        let columns: Vec<ProcessColumn> = self
            .shown_columns()
            .into_iter()
            .map(|def| {
                let width = self.column_width(def) as f32;
                let column = ProcessColumn {
                    title: def.title.into(),
                    x,
                    width,
                    numeric: def.numeric,
                    kind: def.kind,
                    sort: UI_COLUMNS.iter().position(|c| *c == def.column).unwrap_or(0) as i32,
                };
                x += width;
                column
            })
            .collect();
        ui.set_process_columns(ModelRc::new(VecModel::from(columns)));
        ui.set_process_columns_width(x);
        ui.set_name_column_width(self.settings.name_width.map_or(0.0, |w| w as f32));
        let choices: Vec<ColumnChoice> = COLUMNS
            .iter()
            .enumerate()
            .map(|(index, def)| ColumnChoice {
                title: def.menu.into(),
                shown: self.settings.columns.contains(&def.column),
                index: index as i32,
            })
            .collect();
        let half = choices.len().div_ceil(2);
        ui.set_column_choices_left(ModelRc::new(VecModel::from(choices[..half].to_vec())));
        ui.set_column_choices_right(ModelRc::new(VecModel::from(choices[half..].to_vec())));
    }

    /// The View menu's checkbox for column `index` (in [`COLUMNS`]).
    pub(crate) fn toggle_column(&mut self, ui: &MainWindow, (index, shown): (i32, bool)) {
        let Some(def) = usize::try_from(index).ok().and_then(|i| COLUMNS.get(i)) else { return };
        let columns = &mut self.settings.columns;
        columns.retain(|c| *c != def.column);
        if shown {
            // Back where it belongs: before the first shown column that comes after it in the
            // menu's order.
            let rank = |column: SortColumn| COLUMNS.iter().position(|d| d.column == column).unwrap_or(usize::MAX);
            let mine = rank(def.column);
            let at = columns.iter().position(|&c| rank(c) > mine).unwrap_or(columns.len());
            columns.insert(at, def.column);
        }
        self.push_columns(ui);
        self.rebuild_rows(ui);
        self.save_settings(ui);
    }

    /// A column's grip was dragged; `position` -1 is the Name column.
    pub(crate) fn resize_process_column(&mut self, ui: &MainWindow, (position, width): (i32, f32)) {
        let width = (width.round() as u32).clamp(MIN_WIDTH, MAX_WIDTH);
        if position < 0 {
            self.settings.name_width = Some(width.max(120));
        } else if let Some(def) = usize::try_from(position).ok().and_then(|i| self.shown_columns().get(i).copied()) {
            self.settings.column_widths.insert(def.column, width);
        } else {
            return;
        }
        self.push_columns(ui);
        // Saved with the next refresh, not on every movement of the mouse.
        self.settings_dirty = true;
    }

    /// A column was dragged and dropped with its middle at `x` (from the end of Name).
    pub(crate) fn drop_process_column(&mut self, ui: &MainWindow, (position, x): (i32, f32)) {
        let Ok(position) = usize::try_from(position) else { return };
        if position >= self.settings.columns.len() {
            return;
        }
        let widths: Vec<f32> = self.shown_columns().iter().map(|def| self.column_width(def) as f32).collect();
        let target = tables::drop_position(&widths, x);
        let column = self.settings.columns.remove(position);
        self.settings.columns.insert(target.min(self.settings.columns.len()), column);
        self.push_columns(ui);
        self.rebuild_rows(ui);
        self.save_settings(ui);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_column_but_name_has_a_definition() {
        for column in SortColumn::ALL {
            let expected = !matches!(column, SortColumn::Name | SortColumn::Natural);
            assert_eq!(definition(column).is_some(), expected, "{column:?}");
        }
    }
}
