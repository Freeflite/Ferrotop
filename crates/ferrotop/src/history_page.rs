//! The History page: what the history service recorded over the last hour, day or week, the
//! programs that were busiest, and installing or removing the service.

use std::rc::Rc;
use std::time::{Duration, Instant};

use ferrotop_core::archive::{self, ProgramSummary, SecondRecord};
use ferrotop_core::format;
use ferrotop_core::history::nice_ceiling;
use ferrotop_core::table::{Cell, Column, Sort, Table, Tone};
use ferrotop_sys::service_host::{self, ServiceState};
use slint::{ModelRc, VecModel};

use crate::tables::TableView;
use crate::{App, HistoryGraph, MainWindow, background};

/// Points across each graph.
const POINTS: usize = 360;
const ERROR_CANCELLED: u32 = 1223;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Range {
    Hour,
    Day,
    Week,
}

impl Range {
    const ALL: [Range; 3] = [Range::Hour, Range::Day, Range::Week];

    fn seconds(self) -> u64 {
        match self {
            Range::Hour => 3600,
            Range::Day => 86_400,
            Range::Week => 7 * 86_400,
        }
    }

    fn axis_start(self) -> &'static str {
        match self {
            Range::Hour => "1 hour ago",
            Range::Day => "24 hours ago",
            Range::Week => "7 days ago",
        }
    }

    /// How often to read the files again while the page is showing: a week is a lot to read.
    fn refresh(self) -> Duration {
        match self {
            Range::Hour => Duration::from_secs(5),
            Range::Day => Duration::from_secs(30),
            Range::Week => Duration::from_secs(120),
        }
    }
}

/// What one read of the archive found.
#[derive(Debug)]
struct Loaded {
    range: Range,
    points: Vec<Option<SecondRecord>>,
    busiest: Vec<ProgramSummary>,
    /// Seconds recorded in the range.
    seconds: usize,
    /// The oldest of them.
    first: Option<u32>,
}

pub struct HistoryPage {
    range: Range,
    pub(crate) view: TableView,
    graphs: Rc<VecModel<HistoryGraph>>,
    loading: bool,
    loaded_at: Option<Instant>,
    seconds: usize,
    /// When the oldest record in the range was taken.
    first: Option<u32>,
    service: ServiceState,
    /// The installed service's version, when it isn't this one.
    outdated: Option<String>,
    busy: bool,
}

impl Default for HistoryPage {
    fn default() -> Self {
        Self {
            range: Range::Hour,
            view: TableView::new(Some(Sort { column: 1, ascending: false })),
            graphs: Rc::new(VecModel::default()),
            loading: false,
            loaded_at: None,
            seconds: 0,
            first: None,
            service: ServiceState::NotInstalled,
            outdated: None,
            busy: false,
        }
    }
}

impl HistoryPage {
    pub fn bind(&mut self, ui: &MainWindow) {
        ui.set_history_columns(self.view.columns_model());
        ui.set_history_rows(self.view.rows_model());
        ui.set_history_graphs(ModelRc::from(Rc::clone(&self.graphs)));
        ui.set_history_axis_start(self.range.axis_start().into());
        self.view.empty_text = "No programs recorded in this range yet.".into();
    }
}

impl App {
    /// The History page was opened.
    pub(crate) fn show_history(&mut self, ui: &MainWindow) {
        self.refresh_history_state(ui);
        self.load_history(ui);
    }

    /// Each refresh: read the files again now and then while the page is showing.
    pub(crate) fn history_tick(&mut self, ui: &MainWindow) {
        let history = &self.pages.history;
        if history.loaded_at.is_none_or(|at| at.elapsed() >= history.range.refresh()) {
            self.refresh_history_state(ui);
            self.load_history(ui);
        }
    }

    pub(crate) fn choose_history_range(&mut self, ui: &MainWindow, index: i32) {
        let Some(&range) = usize::try_from(index).ok().and_then(|i| Range::ALL.get(i)) else { return };
        let history = &mut self.pages.history;
        history.range = range;
        history.loaded_at = None;
        ui.set_history_range(index);
        ui.set_history_axis_start(range.axis_start().into());
        self.load_history(ui);
    }

    fn refresh_history_state(&mut self, ui: &MainWindow) {
        let history = &mut self.pages.history;
        history.service = service_host::state();
        ui.set_history_installed(history.service != ServiceState::NotInstalled);
        // A newer (or older) Ferrotop than the one the service runs offers to update it.
        history.outdated = (history.service != ServiceState::NotInstalled)
            .then(|| {
                let exe = service_host::service_exe_path().ok()?;
                ferrotop_sys::version::version_info(&exe.to_string_lossy()).ok()?.product_version
            })
            .flatten()
            .filter(|version| version != env!("CARGO_PKG_VERSION"));
        ui.set_history_outdated(history.outdated.is_some());
        self.update_history_status(ui);
    }

    fn update_history_status(&self, ui: &MainWindow) {
        let history = &self.pages.history;
        let status = match (history.service, history.seconds) {
            (ServiceState::Running | ServiceState::Stopped | ServiceState::Changing, _)
                if let Some(version) = &history.outdated =>
            {
                format!("The history service is Ferrotop {version}; update it to this version ({}).", env!("CARGO_PKG_VERSION"))
            }
            (ServiceState::Running, 0) => "Recording. The first figures appear within a few seconds.".to_owned(),
            (ServiceState::Running, _) => {
                let span = history.first.map_or(0, |first| archive::now_unix().saturating_sub(u64::from(first)));
                if span + 120 < history.range.seconds() {
                    // Not a full range yet: say so, as the graph only fills its right-hand end.
                    format!("Recording; {} recorded so far.", format::duration(span))
                } else {
                    format!("Recording around the clock; {} days are kept.", archive::KEEP_DAYS)
                }
            }
            (ServiceState::Stopped | ServiceState::Changing, _) => {
                "The history service is installed but not running.".to_owned()
            }
            (ServiceState::NotInstalled, 0) => {
                "Record history keeps CPU, memory, disk, network and GPU use, even with Ferrotop closed (asks for administrator approval).".to_owned()
            }
            (ServiceState::NotInstalled, _) => "Not recording; showing what was recorded before.".to_owned(),
        };
        ui.set_history_status(status.into());
    }

    fn load_history(&mut self, ui: &MainWindow) {
        let history = &mut self.pages.history;
        if history.loading {
            return;
        }
        history.loading = true;
        history.loaded_at = Some(Instant::now());
        let range = history.range;
        if history.seconds == 0 {
            history.view.clear("Reading the history…");
            self.pages.push_history(ui);
        }
        background(
            move || {
                let to = archive::now_unix();
                let from = to.saturating_sub(range.seconds());
                let Ok(dir) = service_host::data_dir() else {
                    return Loaded { range, points: Vec::new(), busiest: Vec::new(), seconds: 0, first: None };
                };
                let seconds = archive::read_seconds(&dir, from, to);
                Loaded {
                    range,
                    points: archive::buckets(&seconds, from, to, POINTS),
                    busiest: archive::busiest(&archive::read_minutes(&dir, from, to)),
                    seconds: seconds.len(),
                    first: seconds.first().map(|s| s.time),
                }
            },
            |app, ui, loaded| app.history_loaded(ui, loaded),
        );
    }

    fn history_loaded(&mut self, ui: &MainWindow, loaded: Loaded) {
        self.pages.history.loading = false;
        // A range chosen since this load started gets its own load.
        if loaded.range != self.pages.history.range {
            self.load_history(ui);
            return;
        }
        let total_memory = self.provider.stats().memory.total_physical;
        crate::sync_model(&self.pages.history.graphs, graphs(&loaded.points, total_memory));
        self.pages.history.seconds = loaded.seconds;
        self.pages.history.first = loaded.first;
        let history = &mut self.pages.history;
        history.view.set_table(busiest_table(&loaded.busiest));
        history.view.status = match loaded.busiest.len() {
            0 => String::new(),
            1 => "1 program".into(),
            n => format!("{n} programs"),
        };
        if loaded.busiest.is_empty() {
            history.view.empty_text = "No programs recorded in this range yet.".into();
        }
        self.pages.push_history(ui);
        self.update_history_status(ui);
    }

    /// Installs the service, or removes it if it's installed, through an elevated copy of
    /// Ferrotop (the UAC prompt is the confirmation).
    pub(crate) fn history_service_clicked(&mut self, ui: &MainWindow) {
        let history = &mut self.pages.history;
        if history.busy {
            return;
        }
        // An outdated service is updated in place: installing again replaces it.
        let installed = history.service != ServiceState::NotInstalled && history.outdated.is_none();
        let updating = history.outdated.is_some();
        history.busy = true;
        ui.set_history_busy(true);
        let arguments = if installed { "--uninstall-service" } else { "--install-service" };
        background(
            move || service_host::run_self_elevated(arguments),
            move |app, ui, result| {
                app.pages.history.busy = false;
                ui.set_history_busy(false);
                let what = match (installed, updating) {
                    (true, _) => "Removing the history service",
                    (false, true) => "Updating the history service",
                    (false, false) => "Installing the history service",
                };
                let message = match result {
                    Ok(0) if installed => "Stopped recording. What was recorded is kept.".to_owned(),
                    Ok(0) if updating => "Updated the history service.".to_owned(),
                    Ok(0) => "Recording history from now on.".to_owned(),
                    Ok(1) => format!("{what} failed"),
                    Ok(code) => format!("{what} failed: {}", std::io::Error::from_raw_os_error(code as i32)),
                    Err(ferrotop_sys::Error::Win32 { code: ERROR_CANCELLED, .. }) => "Cancelled".to_owned(),
                    Err(error) => format!("{what} failed: {error}"),
                };
                app.show_message(ui, message);
                app.pages.history.loaded_at = None;
                app.refresh_history_state(ui);
                app.load_history(ui);
            },
        );
    }
}

/// SVG paths for one series, with breaks where nothing was recorded.
fn series(points: &[Option<SecondRecord>], max: f64, value: impl Fn(&SecondRecord) -> f64) -> (String, String) {
    use std::fmt::Write;
    let last = points.len().saturating_sub(1).max(1) as f64;
    let (mut line, mut fill) = (String::new(), String::new());
    let mut run: Vec<(f64, f64)> = Vec::new();
    let mut flush = |run: &mut Vec<(f64, f64)>| {
        if let [only] = run[..] {
            // A lone point still shows, as a short dash.
            run.push((only.0 + 0.3, only.1));
        }
        if let (Some(first), Some(end)) = (run.first().copied(), run.last().copied()) {
            for (i, (x, y)) in run.iter().enumerate() {
                let _ = write!(line, "{}{x:.2} {y:.2} ", if i == 0 { "M " } else { "L " });
            }
            let _ = write!(fill, "M {:.2} 100 ", first.0);
            for (x, y) in run.iter() {
                let _ = write!(fill, "L {x:.2} {y:.2} ");
            }
            let _ = write!(fill, "L {:.2} 100 Z ", end.0);
        }
        run.clear();
    };
    for (i, point) in points.iter().enumerate() {
        match point.map(|r| value(&r)).filter(|v| v.is_finite()) {
            Some(v) => {
                let fraction = if max > 0.0 { (v / max).clamp(0.0, 1.0) } else { 0.0 };
                run.push((i as f64 * 100.0 / last, 100.0 - fraction * 100.0));
            }
            None => flush(&mut run),
        }
    }
    flush(&mut run);
    if line.is_empty() {
        // Nothing recorded: a flat baseline.
        return ("M 0 100 L 100 100".into(), "M 0 100 L 100 100 Z".into());
    }
    (line.trim_end().to_owned(), fill.trim_end().to_owned())
}

fn peak(points: &[Option<SecondRecord>], value: impl Fn(&SecondRecord) -> f64) -> Option<f64> {
    points.iter().flatten().map(value).filter(|v| v.is_finite()).reduce(f64::max)
}

fn average(points: &[Option<SecondRecord>], value: impl Fn(&SecondRecord) -> f64) -> Option<f64> {
    let values: Vec<f64> = points.iter().flatten().map(value).filter(|v| v.is_finite()).collect();
    (!values.is_empty()).then(|| values.iter().sum::<f64>() / values.len() as f64)
}

fn percent(fraction: f64) -> String {
    format!("{:.0}%", fraction * 100.0)
}

fn graph(kind: i32, title: &str, corner: String, (line, fill): (String, String)) -> HistoryGraph {
    HistoryGraph { kind, title: title.into(), corner: corner.into(), line: line.into(), fill: fill.into() }
}

fn graphs(points: &[Option<SecondRecord>], total_memory: u64) -> Vec<HistoryGraph> {
    let cpu = |r: &SecondRecord| f64::from(r.cpu);
    let memory = |r: &SecondRecord| r.memory_used as f64;
    let disk = |r: &SecondRecord| f64::from(r.disk_read) + f64::from(r.disk_write);
    let network = |r: &SecondRecord| f64::from(r.sent) + f64::from(r.received);
    let gpu = |r: &SecondRecord| f64::from(r.gpu);
    let temperature = |r: &SecondRecord| f64::from(r.cpu_temp);
    let usage = |value: &dyn Fn(&SecondRecord) -> f64| match (average(points, value), peak(points, value)) {
        (Some(avg), Some(top)) => format!("average {} · peak {}", percent(avg), percent(top)),
        _ => String::new(),
    };
    let memory_peak = peak(points, memory);
    let disk_peak = peak(points, disk);
    let network_peak = peak(points, network);
    let mut graphs = vec![
        graph(0, "CPU", usage(&cpu), series(points, 1.0, cpu)),
        graph(
            1,
            "Memory in use",
            memory_peak.map_or_else(String::new, |p| {
                format!("peak {} of {}", format::bytes(p as u64), format::bytes(total_memory))
            }),
            series(points, total_memory.max(1) as f64, memory),
        ),
        graph(
            2,
            "Disk (all drives, read and write)",
            disk_peak.map_or_else(String::new, |p| format!("peak {}", format::rate(p))),
            series(points, nice_ceiling(disk_peak.unwrap_or(1.0)), disk),
        ),
        graph(
            3,
            "Network (all adapters, sent and received)",
            network_peak.map_or_else(String::new, |p| format!("peak {}", format::rate(p))),
            series(points, nice_ceiling(network_peak.unwrap_or(1.0)), network),
        ),
        graph(4, "GPU", usage(&gpu), series(points, 1.0, gpu)),
    ];
    if let Some(hottest) = peak(points, temperature) {
        graphs.push(graph(
            0,
            "CPU temperature",
            format!("peak {hottest:.0} °C"),
            series(points, nice_ceiling(hottest.max(50.0)), temperature),
        ));
    }
    graphs
}

fn busiest_table(busiest: &[ProgramSummary]) -> Table {
    let mut table = Table::new(&[
        Column::text("Program", 190),
        Column::number("Avg CPU", 70),
        Column::number("Peak CPU", 75),
        Column::number("Peak memory", 95),
        Column::number("Disk", 85),
        Column::number("Network", 85),
        Column::number("Minutes busy", 95),
    ]);
    for program in busiest {
        let cpu = |fraction: f32| Cell::number(f64::from(fraction), format!("{:.1}%", fraction * 100.0));
        let bytes =
            |total: f64| Cell::number(total, if total > 0.0 { format::bytes(total as u64) } else { String::new() });
        table.push(
            ferrotop_core::properties::stable_id(&program.name),
            vec![
                Cell::text(&program.name),
                cpu(program.cpu),
                cpu(program.peak_cpu),
                Cell::number(program.peak_private_bytes as f64, format::bytes(program.peak_private_bytes)),
                bytes(program.disk_bytes),
                bytes(program.network_bytes),
                Cell::count(u64::from(program.minutes)),
            ],
            Tone::Plain,
        );
    }
    table
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(cpu: f32) -> SecondRecord {
        SecondRecord { cpu, cpu_temp: f32::NAN, ..SecondRecord::default() }
    }

    #[test]
    fn series_break_at_gaps() {
        let points = [Some(record(0.5)), Some(record(1.0)), None, Some(record(0.0))];
        let (line, fill) = series(&points, 1.0, |r| f64::from(r.cpu));
        assert_eq!(line.matches('M').count(), 2, "{line}");
        assert!(line.starts_with("M 0.00 50.00 L 33.33 0.00"));
        assert_eq!(fill.matches('Z').count(), 2);
        let (empty, _) = series(&[None, None], 1.0, |r| f64::from(r.cpu));
        assert_eq!(empty, "M 0 100 L 100 100");
    }

    #[test]
    fn temperature_graph_only_when_recorded() {
        let points = [Some(record(0.2)), None];
        assert_eq!(graphs(&points, 1 << 30).len(), 5);
        let hot = [Some(SecondRecord { cpu_temp: 70.0, ..record(0.2) })];
        let with = graphs(&hot, 1 << 30);
        assert_eq!(with.len(), 6);
        assert_eq!(with[5].corner, "peak 70 °C");
        assert_eq!(with[0].corner, "average 20% · peak 20%");
    }
}
