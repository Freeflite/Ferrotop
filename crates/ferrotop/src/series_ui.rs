//! Choosing what History and the header show: the header's small graphs, the graph picker, and
//! the Performance page's right-click menu ("Show in History", "Show in the header").

use std::collections::HashMap;

use ferrotop_core::history::History;
use ferrotop_core::perf::PERF_HISTORY;
use ferrotop_core::series::{self, Reading, Unit};
use slint::{ModelRc, SharedString, VecModel};

use crate::{App, HeaderTile, MainWindow, PickerRow, SeriesMenuEntry};

/// Picker targets.
const HISTORY: i32 = 0;
const HEADER: i32 = 1;

/// Graphs of two series the picker offers (as the Performance page draws them): pairs of id
/// endings on the same device.
const PAIRS: [(&str, &str, &str); 3] = [
    (".temp", ".hotspot", "Temperature and hotspot"),
    (".read", ".write", "Read and write"),
    (".receive", ".send", "Received and sent"),
];

/// Histories the app keeps itself for header graphs of series the Performance page doesn't graph
/// (power, fan, clocks, totals).
#[derive(Debug, Default)]
pub struct HeaderHistories(HashMap<String, History>);

fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |first| first.to_uppercase().chain(chars).collect())
}

/// The picker's rows: each device's heading, its pairs (for History), then its series.
fn picker_rows(readings: &[Reading], pairs: bool, shown: impl Fn(&str) -> bool) -> Vec<PickerRow> {
    let row = |title: String, key: String, heading: bool| PickerRow {
        shown: !heading && shown(&key),
        title: title.into(),
        key: key.into(),
        heading,
    };
    let mut rows = Vec::new();
    let mut group: Option<&str> = None;
    for reading in readings {
        if group != Some(reading.group.as_str()) {
            group = Some(&reading.group);
            rows.push(row(reading.group.clone(), String::new(), true));
            if pairs {
                for other in readings.iter().filter(|r| r.group == reading.group) {
                    for (first, second, title) in PAIRS {
                        if let Some(stem) = other.id.strip_suffix(first)
                            && readings.iter().any(|r| r.id == format!("{stem}{second}"))
                        {
                            rows.push(row(title.to_owned(), format!("{stem}{first}+{stem}{second}"), false));
                        }
                    }
                }
            }
        }
        rows.push(row(capitalized(&reading.name), reading.id.clone(), false));
    }
    rows
}

impl App {
    /// Fills the picker for History or the header, before it opens.
    pub(crate) fn open_picker(&mut self, ui: &MainWindow, target: i32) {
        self.picker_target = target;
        let (title, hint) = if target == HEADER {
            (
                "Header graphs",
                "The small graphs beside the tabs, up to 8. Drag one sideways to move it; right-click one to come back here.",
            )
        } else {
            (
                "Add graphs to History",
                "Every Performance figure is recorded while the history service runs. Drag a graph by its title to move it, or by its right or bottom edge to resize it.",
            )
        };
        ui.set_picker_title(title.into());
        ui.set_picker_hint(hint.into());
        let readings = series::readings(&self.perf);
        let rows = if target == HEADER {
            picker_rows(&readings, false, |id| self.settings.header_graphs.iter().any(|g| g == id))
        } else {
            picker_rows(&readings, true, |key| self.is_in_history(key))
        };
        ui.set_picker_rows(ModelRc::new(VecModel::from(rows)));
    }

    pub(crate) fn picker_toggled(&mut self, ui: &MainWindow, (key, shown): (SharedString, bool)) {
        if self.picker_target == HEADER {
            self.set_header_graph(ui, &key, shown);
        } else {
            self.set_history_graph(ui, &key, shown);
        }
    }

    /// Fills the right-click menu for these graph keys (`;` between them): each one can go in
    /// History, and a single series in the header too.
    pub(crate) fn series_menu_requested(&mut self, ui: &MainWindow, keys: SharedString) {
        let readings = series::readings(&self.perf);
        let mut entries = Vec::new();
        for key in keys.split(';').filter_map(series::parse_key) {
            let ids: Vec<String> = key.split('+').map(str::to_owned).collect();
            if !ids.iter().all(|id| readings.iter().any(|r| r.id == *id)) {
                continue;
            }
            let title = series::graph_title(&readings, &ids);
            let text = if self.is_in_history(&key) {
                format!("Remove {title} from History")
            } else {
                format!("Show {title} in History")
            };
            entries.push(SeriesMenuEntry { text: text.into(), key: key.as_str().into(), target: HISTORY });
            if ids.len() == 1 {
                let text = if self.settings.header_graphs.contains(&key) {
                    format!("Remove {title} from the header")
                } else {
                    format!("Show {title} in the header")
                };
                entries.push(SeriesMenuEntry { text: text.into(), key: key.as_str().into(), target: HEADER });
            }
        }
        ui.set_series_menu_entries(ModelRc::new(VecModel::from(entries)));
    }

    pub(crate) fn series_menu_chosen(&mut self, ui: &MainWindow, (key, target): (SharedString, i32)) {
        let ids: Vec<String> = key.split('+').map(str::to_owned).collect();
        let title = series::graph_title(&series::readings(&self.perf), &ids);
        if target == HEADER {
            let show = !self.settings.header_graphs.iter().any(|g| *g == key.as_str());
            self.set_header_graph(ui, &key, show);
        } else {
            let show = !self.is_in_history(&key);
            self.set_history_graph(ui, &key, show);
            let message =
                if show { format!("Added {title} to History") } else { format!("Removed {title} from History") };
            self.show_message(ui, message);
        }
    }

    pub(crate) fn set_header_graph(&mut self, ui: &MainWindow, id: &str, show: bool) {
        let graphs = &mut self.settings.header_graphs;
        let present = graphs.iter().any(|g| g == id);
        if show && !present {
            if graphs.len() >= series::MAX_HEADER {
                return self.show_message(ui, format!("The header holds up to {} graphs", series::MAX_HEADER));
            }
            graphs.push(id.to_owned());
        } else if !show {
            graphs.retain(|g| g != id);
        }
        self.save_settings(ui);
        self.update_header();
    }

    /// A header graph was dragged to another place.
    pub(crate) fn header_graph_moved(&mut self, ui: &MainWindow, (from, to): (i32, i32)) {
        let graphs = &mut self.settings.header_graphs;
        let (Ok(from), Ok(to)) = (usize::try_from(from), usize::try_from(to)) else { return };
        if from >= graphs.len() || to >= graphs.len() || from == to {
            return;
        }
        let graph = graphs.remove(from);
        graphs.insert(to, graph);
        self.save_settings(ui);
        self.update_header();
    }

    /// Keeps a short history of header series the Performance page doesn't graph itself.
    pub(crate) fn record_header(&mut self) {
        let missing: Vec<&String> =
            self.settings.header_graphs.iter().filter(|id| series::live_history(&self.perf, id).is_none()).collect();
        if missing.is_empty() {
            return;
        }
        let readings = series::readings(&self.perf);
        for id in missing {
            if let Some(value) = readings.iter().find(|r| r.id == **id).and_then(|r| r.value) {
                self.header_histories.0.entry(id.clone()).or_insert_with(|| History::new(PERF_HISTORY)).push(value);
            }
        }
        let shown = &self.settings.header_graphs;
        self.header_histories.0.retain(|id, _| shown.contains(id));
    }

    /// The header's small graphs, with their figures now.
    pub(crate) fn update_header(&self) {
        let readings = series::readings(&self.perf);
        let tiles: Vec<HeaderTile> = self
            .settings
            .header_graphs
            .iter()
            .map(|id| {
                let reading = readings.iter().find(|r| r.id == *id);
                let unit = reading.map(|r| r.unit).or_else(|| series::builtin_unit(id)).unwrap_or(Unit::Fraction);
                let capacity = reading.and_then(|r| r.capacity);
                let history = series::live_history(&self.perf, id).or_else(|| self.header_histories.0.get(id));
                let ceiling = unit.ceiling(history.map_or(0.0, History::peak), capacity);
                let (line, fill) = history.map_or_else(
                    || ("M 0 100 L 100 100".to_owned(), "M 0 100 L 100 100 Z".to_owned()),
                    |h| h.svg_paths(100.0, 100.0, ceiling),
                );
                let value = match (reading.and_then(|r| r.value), capacity) {
                    // Memory as a share of what's installed, as Task Manager's header does.
                    (Some(value), Some(capacity)) if unit == Unit::Bytes && capacity > 0.0 => {
                        format!("{:.0}%", value / capacity * 100.0)
                    }
                    (Some(value), _) => unit.format(value),
                    (None, _) => "—".to_owned(),
                };
                HeaderTile {
                    label: series::short_label(&readings, id).into(),
                    value: value.into(),
                    line: line.into(),
                    fill: fill.into(),
                    kind: reading.map_or_else(|| series::kind_of(id), |r| r.kind),
                }
            })
            .collect();
        crate::sync_model(&self.header_tiles, tiles);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reading(id: &str, group: &str, name: &str) -> Reading {
        Reading {
            id: id.into(),
            group: group.into(),
            name: name.into(),
            unit: Unit::Celsius,
            kind: 4,
            capacity: None,
            value: None,
        }
    }

    #[test]
    fn picker_groups_series_and_offers_pairs() {
        let readings = [
            reading("cpu", "CPU", "utilization"),
            reading("gpu.0.load", "GPU 0", "load"),
            reading("gpu.0.temp", "GPU 0", "temperature"),
            reading("gpu.0.hotspot", "GPU 0", "hotspot"),
        ];
        let rows = picker_rows(&readings, true, |key| key == "gpu.0.load");
        let titles: Vec<(&str, &str, bool, bool)> =
            rows.iter().map(|r| (r.title.as_str(), r.key.as_str(), r.heading, r.shown)).collect();
        assert_eq!(
            titles,
            [
                ("CPU", "", true, false),
                ("Utilization", "cpu", false, false),
                ("GPU 0", "", true, false),
                ("Temperature and hotspot", "gpu.0.temp+gpu.0.hotspot", false, false),
                ("Load", "gpu.0.load", false, true),
                ("Temperature", "gpu.0.temp", false, false),
                ("Hotspot", "gpu.0.hotspot", false, false),
            ]
        );
        assert_eq!(picker_rows(&readings, false, |_| false).len(), 6, "no pairs for the header");
    }
}
