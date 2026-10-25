//! The History page: graphs of anything the history service recorded over the last hour, day or
//! week, arranged however the user likes (dragged, resized, or from a layout); the programs that
//! were busiest; and installing or removing the service.

use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime};

use ferrotop_core::archive::{self, ProgramSummary};
use ferrotop_core::format;
use ferrotop_core::series::{self, GraphSpec, Placement, Reading, Unit};
use ferrotop_core::series_file;
use ferrotop_core::settings::Settings;
use ferrotop_core::table::{Cell, Column, Sort, Table, Tone};
use ferrotop_sys::service_host::{self, ServiceState};
use slint::{ModelRc, SharedString, VecModel};

use crate::tables::TableView;
use crate::{App, HISTORY_PAGE, HistoryGraph, MainWindow, background};

/// Points across each graph.
const POINTS: usize = 360;
/// Room between rows of graphs.
const ROW_GAP: f32 = 12.0;
/// A graph added from a menu: half the page wide.
const NEW_SPAN: u8 = 6;
const NEW_HEIGHT: u16 = 130;
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

    /// A day or a week reads the every-minute series files: plenty for a graph's points.
    fn minutes(self) -> bool {
        self != Range::Hour
    }
}

/// A series' graph points: each slice's average and peak, `None` where nothing was recorded.
type Slices = Vec<Option<(f64, f64)>>;

/// The installed service's program file, by size and time written.
type BuildKey = (u64, SystemTime);

/// What one read of the archive found.
#[derive(Debug)]
struct Loaded {
    range: Range,
    series: HashMap<String, Slices>,
    /// Units the series files gave, for series this machine no longer has.
    units: HashMap<String, Unit>,
    busiest: Vec<ProgramSummary>,
    /// Seconds recorded in the range, and the oldest of them.
    seconds: usize,
    first: Option<u32>,
    /// Whether the service runs this very program file, if that could be checked.
    same_build: Option<(BuildKey, bool)>,
}

/// Why the installed service should be updated.
#[derive(Debug, Clone, PartialEq)]
enum Outdated {
    /// It's another version of Ferrotop.
    Version(String),
    /// It's the same version but another build, which may not record everything this one can.
    Build,
}

pub struct HistoryPage {
    range: Range,
    pub(crate) view: TableView,
    graphs: Rc<VecModel<HistoryGraph>>,
    loading: bool,
    loaded_at: Option<Instant>,
    /// The last read, kept to draw graphs that move or change size without reading again.
    loaded: Option<Loaded>,
    service: ServiceState,
    outdated: Option<Outdated>,
    same_build: Option<(BuildKey, bool)>,
    busy: bool,
    /// The Layouts menu's layouts, as last listed.
    layouts: Vec<(String, Vec<GraphSpec>)>,
}

impl Default for HistoryPage {
    fn default() -> Self {
        Self {
            range: Range::Hour,
            view: TableView::new(Some(Sort { column: 1, ascending: false })),
            graphs: Rc::new(VecModel::default()),
            loading: false,
            loaded_at: None,
            loaded: None,
            service: ServiceState::NotInstalled,
            outdated: None,
            same_build: None,
            busy: false,
            layouts: Vec::new(),
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

    fn seconds(&self) -> usize {
        self.loaded.as_ref().map_or(0, |l| l.seconds)
    }
}

impl App {
    /// At startup: the saved arrangement.
    pub(crate) fn init_history(&mut self, ui: &MainWindow) {
        ui.set_history_programs_shown(self.settings.history_programs);
        ui.set_history_programs_width(self.settings.history_programs_width as f32);
        self.draw_history(ui);
    }

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
        let version = (history.service != ServiceState::NotInstalled)
            .then(|| {
                let exe = service_host::service_exe_path().ok()?;
                ferrotop_sys::version::version_info(&exe.to_string_lossy()).ok()?.product_version
            })
            .flatten()
            .filter(|version| version != env!("CARGO_PKG_VERSION"));
        history.outdated = match (version, &history.same_build) {
            _ if history.service == ServiceState::NotInstalled => None,
            (Some(version), _) => Some(Outdated::Version(version)),
            (None, Some((_, false))) => Some(Outdated::Build),
            (None, _) => None,
        };
        ui.set_history_outdated(history.outdated.is_some());
        self.update_history_status(ui);
    }

    fn update_history_status(&self, ui: &MainWindow) {
        let history = &self.pages.history;
        let running = matches!(history.service, ServiceState::Running | ServiceState::Stopped | ServiceState::Changing);
        let status = match (history.service, history.seconds()) {
            _ if running && let Some(outdated) = &history.outdated => match outdated {
                Outdated::Version(version) => format!(
                    "The history service is Ferrotop {version}; update it to this version ({}).",
                    env!("CARGO_PKG_VERSION")
                ),
                Outdated::Build => {
                    "The history service is another build of this version; update it so it records everything this one can."
                        .to_owned()
                }
            },
            (ServiceState::Running, 0) => "Recording. The first figures appear within a few seconds.".to_owned(),
            (ServiceState::Running, _) => {
                let first = history.loaded.as_ref().and_then(|l| l.first);
                let span = first.map_or(0, |first| archive::now_unix().saturating_sub(u64::from(first)));
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
                "Record history keeps every Performance figure, even with Ferrotop closed (asks for administrator approval).".to_owned()
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
        if history.loaded.is_none() {
            history.view.clear("Reading the history…");
            self.pages.push_history(ui);
        }
        let mut ids: Vec<String> = Vec::new();
        for id in self.settings.history_graphs.iter().flat_map(|g| &g.series) {
            if !ids.contains(id) {
                ids.push(id.clone());
            }
        }
        let checked = self.pages.history.same_build;
        background(move || read_history(range, &ids, checked), |app, ui, loaded| app.history_loaded(ui, loaded));
    }

    fn history_loaded(&mut self, ui: &MainWindow, loaded: Loaded) {
        self.pages.history.loading = false;
        // A range chosen since this load started gets its own load.
        if loaded.range != self.pages.history.range {
            self.load_history(ui);
            return;
        }
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
        let build_changed = loaded.same_build.map(|(_, same)| same) != history.same_build.map(|(_, same)| same);
        history.same_build = loaded.same_build;
        history.loaded = Some(loaded);
        self.pages.push_history(ui);
        self.draw_history(ui);
        if build_changed {
            self.refresh_history_state(ui);
        } else {
            self.update_history_status(ui);
        }
    }

    /// Lays out the graphs and draws them from the last read.
    pub(crate) fn draw_history(&mut self, ui: &MainWindow) {
        let readings = series::readings(&self.perf);
        let specs = &self.settings.history_graphs;
        let (places, height) = series::flow(specs, ROW_GAP);
        let loaded = self.pages.history.loaded.as_ref();
        let graphs: Vec<HistoryGraph> =
            specs.iter().zip(&places).map(|(spec, place)| graph(spec, place, &readings, loaded)).collect();
        crate::sync_model(&self.pages.history.graphs, graphs);
        ui.set_history_content_height(height);
    }

    /// Saves and draws the graphs after a change; reads the files again for added series.
    fn history_graphs_changed(&mut self, ui: &MainWindow, added: bool) {
        self.save_settings(ui);
        self.draw_history(ui);
        if added {
            self.pages.history.loaded_at = None;
            if self.page == HISTORY_PAGE {
                self.load_history(ui);
            }
        }
    }

    pub(crate) fn history_graph_dropped(&mut self, ui: &MainWindow, (index, column, y): (i32, f32, f32)) {
        let Ok(from) = usize::try_from(index) else { return };
        let (places, _) = series::flow(&self.settings.history_graphs, ROW_GAP);
        let to = series::drop_index(&places, column, y);
        series::move_graph(&mut self.settings.history_graphs, from, to);
        self.history_graphs_changed(ui, false);
    }

    pub(crate) fn history_graph_resized(&mut self, ui: &MainWindow, (index, span, height): (i32, i32, f32)) {
        let Some(graph) = usize::try_from(index).ok().and_then(|i| self.settings.history_graphs.get_mut(i)) else {
            return;
        };
        *graph = GraphSpec::new(&graph.key(), span.clamp(0, 255) as u8, height.round().clamp(0.0, 65_535.0) as u16);
        self.history_graphs_changed(ui, false);
    }

    pub(crate) fn history_graph_removed(&mut self, ui: &MainWindow, index: i32) {
        if let Ok(index) = usize::try_from(index)
            && index < self.settings.history_graphs.len()
        {
            self.settings.history_graphs.remove(index);
            self.history_graphs_changed(ui, false);
        }
    }

    pub(crate) fn is_in_history(&self, key: &str) -> bool {
        self.settings.history_graphs.iter().any(|g| g.key() == key)
    }

    /// Adds a graph of `key` (one series id, or two joined by `+`) to History, or removes it.
    pub(crate) fn set_history_graph(&mut self, ui: &MainWindow, key: &str, show: bool) {
        let graphs = &mut self.settings.history_graphs;
        match graphs.iter().position(|g| g.key() == key) {
            Some(index) if !show => {
                graphs.remove(index);
                self.history_graphs_changed(ui, false);
            }
            None if show && graphs.len() < series::MAX_GRAPHS => {
                graphs.push(GraphSpec::new(key, NEW_SPAN, NEW_HEIGHT));
                self.history_graphs_changed(ui, true);
            }
            None if show => self.show_message(ui, format!("History holds up to {} graphs", series::MAX_GRAPHS)),
            _ => {}
        }
    }

    pub(crate) fn toggle_history_programs(&mut self, ui: &MainWindow) {
        self.settings.history_programs = !self.settings.history_programs;
        ui.set_history_programs_shown(self.settings.history_programs);
        self.save_settings(ui);
    }

    pub(crate) fn history_programs_resized(&mut self, ui: &MainWindow, width: f32) {
        self.settings.history_programs_width = width.round().clamp(200.0, 4000.0) as u32;
        self.save_settings(ui);
    }

    /// Lists the Layouts menu: built-in layouts for this machine's sensors, then layout files.
    pub(crate) fn list_history_layouts(&mut self, ui: &MainWindow) {
        let readings = series::readings(&self.perf);
        let mut layouts: Vec<(String, Vec<GraphSpec>)> =
            series::templates(&readings).into_iter().map(|(name, graphs)| (name.to_owned(), graphs)).collect();
        layouts.extend(layout_files());
        let names: Vec<SharedString> = layouts.iter().map(|(name, _)| name.as_str().into()).collect();
        ui.set_history_layouts(ModelRc::new(VecModel::from(names)));
        self.pages.history.layouts = layouts;
    }

    /// A layout was chosen from the menu: -1 saves this one to a file, -2 opens the folder.
    pub(crate) fn choose_history_layout(&mut self, ui: &MainWindow, index: i32) {
        match index {
            -1 => self.save_history_layout(ui),
            -2 => self.open_layouts_folder(ui),
            _ => {
                let Some((name, graphs)) =
                    usize::try_from(index).ok().and_then(|i| self.pages.history.layouts.get(i)).cloned()
                else {
                    return;
                };
                self.settings.history_graphs = graphs;
                self.history_graphs_changed(ui, true);
                self.show_message(ui, format!("Showing the {name} layout"));
            }
        }
    }

    fn save_history_layout(&mut self, ui: &MainWindow) {
        let graphs = &self.settings.history_graphs;
        let saved =
            Settings::layouts_dir().ok_or_else(|| std::io::Error::other("APPDATA is not set")).and_then(|dir| {
                std::fs::create_dir_all(&dir)?;
                write_layouts_readme(&dir);
                let (name, path) = (1..1000)
                    .map(|n| format!("My layout {n}"))
                    .map(|name| (name.clone(), dir.join(format!("{name}.json"))))
                    .find(|(_, path)| !path.exists())
                    .ok_or_else(|| std::io::Error::other("there are too many saved layouts"))?;
                std::fs::write(&path, series::layout_to_json(&name, graphs))?;
                Ok(name)
            });
        let message = match saved {
            Ok(name) => format!("Saved as \"{name}.json\" in the layouts folder; rename it to name it in the menu"),
            Err(error) => format!("Couldn't save the layout: {error}"),
        };
        self.show_message(ui, message);
    }

    fn open_layouts_folder(&mut self, ui: &MainWindow) {
        let Some(dir) = Settings::layouts_dir() else { return };
        let _ = std::fs::create_dir_all(&dir);
        write_layouts_readme(&dir);
        if let Err(error) = ferrotop_sys::control::open_folder(&dir.to_string_lossy()) {
            self.show_message(ui, format!("Couldn't open the layouts folder: {error}"));
        }
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
                app.pages.history.same_build = None;
                app.refresh_history_state(ui);
                app.load_history(ui);
            },
        );
    }
}

/// Reads what's needed for the graphs of `ids` and the programs table (on a background thread).
fn read_history(range: Range, ids: &[String], checked: Option<(BuildKey, bool)>) -> Loaded {
    let to = archive::now_unix();
    let from = to.saturating_sub(range.seconds());
    let mut loaded = Loaded {
        range,
        series: HashMap::new(),
        units: HashMap::new(),
        busiest: Vec::new(),
        seconds: 0,
        first: None,
        same_build: check_build(checked),
    };
    let Ok(dir) = service_host::data_dir() else { return loaded };
    let records = archive::read_seconds(&dir, from, to);
    loaded.seconds = records.len();
    loaded.first = records.first().map(|r| r.time);
    let (builtin, other): (Vec<&String>, Vec<&String>) = ids.iter().partition(|id| series::is_builtin(id));
    for id in builtin {
        let points = records.iter().filter_map(|r| series::builtin_value(r, id).map(|value| (r.time, value, value)));
        loaded.series.insert(id.clone(), slices(points, from, to));
    }
    drop(records);
    let other: Vec<String> = other.into_iter().cloned().collect();
    let (points, units) = series_file::read(&dir, &other, from, to, range.minutes());
    for ((id, points), unit) in other.iter().zip(points).zip(units) {
        loaded.series.insert(id.clone(), slices(points, from, to));
        if let Some(unit) = unit {
            loaded.units.insert(id.clone(), unit);
        }
    }
    loaded.busiest = archive::busiest(&archive::read_minutes(&dir, from, to));
    loaded
}

/// Averages `(time, value, peak)` points into [`POINTS`] equal slices of `from..to`, keeping
/// each slice's peak.
fn slices(points: impl IntoIterator<Item = (u32, f64, f64)>, from: u64, to: u64) -> Slices {
    let span = to.saturating_sub(from).max(1);
    let mut sums = vec![(0.0, 0u32, f64::MIN); POINTS];
    for (time, value, peak) in points {
        let Some(offset) = u64::from(time).checked_sub(from).filter(|o| *o < span) else { continue };
        let slot = &mut sums[(u128::from(offset) * POINTS as u128 / u128::from(span)) as usize];
        slot.0 += value;
        slot.1 += 1;
        slot.2 = slot.2.max(peak);
    }
    sums.into_iter().map(|(sum, count, peak)| (count > 0).then(|| (sum / f64::from(count), peak))).collect()
}

/// Whether the installed service is this very program file: after an update to a build with the
/// same version number, it would otherwise go on recording only what the old one could. Checked
/// again only when the service's file changes.
fn check_build(previous: Option<(BuildKey, bool)>) -> Option<(BuildKey, bool)> {
    let service = service_host::service_exe_path().ok()?;
    let metadata = std::fs::metadata(&service).ok()?;
    let key = (metadata.len(), metadata.modified().ok()?);
    if let Some((checked, same)) = previous
        && checked == key
    {
        return Some((key, same));
    }
    let own = std::env::current_exe().ok()?;
    let same =
        std::fs::metadata(&own).ok()?.len() == key.0 && std::fs::read(&own).ok()? == std::fs::read(&service).ok()?;
    Some((key, same))
}

/// SVG paths for one series' averages, with breaks where nothing was recorded.
fn paths(slices: &[Option<(f64, f64)>], max: f64) -> (String, String) {
    use std::fmt::Write;
    let last = slices.len().saturating_sub(1).max(1) as f64;
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
    for (i, slice) in slices.iter().enumerate() {
        match slice.map(|(average, _)| average).filter(|v| v.is_finite()) {
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

fn peak(slices: &[Option<(f64, f64)>]) -> Option<f64> {
    slices.iter().flatten().map(|(_, peak)| *peak).filter(|v| v.is_finite()).reduce(f64::max)
}

fn average(slices: &[Option<(f64, f64)>]) -> Option<f64> {
    let values: Vec<f64> = slices.iter().flatten().map(|(average, _)| *average).filter(|v| v.is_finite()).collect();
    (!values.is_empty()).then(|| values.iter().sum::<f64>() / values.len() as f64)
}

/// One graph, from what was read (if anything yet).
fn graph(spec: &GraphSpec, place: &Placement, readings: &[Reading], loaded: Option<&Loaded>) -> HistoryGraph {
    let reading = |id: &str| readings.iter().find(|r| r.id == id);
    let unit_of = |id: &str| {
        reading(id)
            .map(|r| r.unit)
            .or_else(|| loaded.and_then(|l| l.units.get(id).copied()))
            .or_else(|| series::builtin_unit(id))
            .unwrap_or(Unit::Fraction)
    };
    let none = Vec::new();
    let slices_of = |id: &str| loaded.and_then(|l| l.series.get(id)).unwrap_or(&none);
    let first = spec.series[0].as_str();
    let (unit, a) = (unit_of(first), slices_of(first));
    let second = spec.series.get(1).map(|id| (unit_of(id), slices_of(id), reading(id).and_then(|r| r.capacity)));
    let capacity = reading(first).and_then(|r| r.capacity);
    // Two series in the same unit share a scale; otherwise each fills the graph.
    let shared_peak = match second {
        Some((unit_b, b, _)) if unit_b == unit => peak(a).into_iter().chain(peak(b)).reduce(f64::max),
        _ => peak(a),
    };
    let ceiling = unit.ceiling(shared_peak.unwrap_or(0.0), capacity);
    let (line, fill) = paths(a, ceiling);
    let line_b = second.map_or_else(String::new, |(unit_b, b, capacity_b)| {
        let ceiling_b = if unit_b == unit { ceiling } else { unit_b.ceiling(peak(b).unwrap_or(0.0), capacity_b) };
        paths(b, ceiling_b).0
    });
    let corner = match (second, average(a), peak(a)) {
        (None, Some(average), Some(top)) => format!("average {} · peak {}", unit.format(average), unit.format(top)),
        (Some((unit_b, b, _)), _, Some(top)) => match peak(b) {
            Some(top_b) => format!("peak {} · {}", unit.format(top), unit_b.format(top_b)),
            None => format!("peak {}", unit.format(top)),
        },
        _ => String::new(),
    };
    HistoryGraph {
        kind: reading(first).map_or_else(|| series::kind_of(first), |r| r.kind),
        title: series::graph_title(readings, &spec.series).into(),
        corner: corner.into(),
        line: line.into(),
        fill: fill.into(),
        line_b: line_b.into(),
        column: i32::from(place.column),
        span: i32::from(place.span),
        top: place.top,
        height: place.height,
    }
}

/// Layouts dropped in the layouts folder, named by their files.
fn layout_files() -> Vec<(String, Vec<GraphSpec>)> {
    let Some(entries) = Settings::layouts_dir().and_then(|dir| std::fs::read_dir(dir).ok()) else { return Vec::new() };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|e| e.eq_ignore_ascii_case("json")))
        .collect();
    files.sort();
    files
        .into_iter()
        .take(32)
        .filter_map(|path| {
            if std::fs::metadata(&path).ok()?.len() > series::MAX_LAYOUT_FILE {
                return None;
            }
            let (_, graphs) = series::layout_from_json(&std::fs::read_to_string(&path).ok()?)?;
            let name: String = path.file_stem()?.to_string_lossy().chars().take(60).collect();
            Some((name, graphs))
        })
        .collect()
}

/// Explains the layout files, once, in the folder people drop them into.
fn write_layouts_readme(dir: &std::path::Path) {
    let path = dir.join("README.txt");
    if path.exists() {
        return;
    }
    let example = series::layout_to_json(
        "Example",
        &[GraphSpec::new("cpu.temp", 12, 200), GraphSpec::new("cpu", 6, 120), GraphSpec::new("memory", 6, 120)],
    );
    let text = format!(
        "Ferrotop History layouts\r\n\
         ========================\r\n\r\n\
         Each .json file here is a History layout: it shows in the Layouts menu under its file name.\r\n\
         \"Save this layout to a file\" writes the current one here, to copy or edit.\r\n\r\n\
         A layout lists graphs in order. Each graph shows one series, or two (a line over the first);\r\n\
         \"width\" is in twelfths of the page (2 to 12) and \"height\" in pixels (60 to 900). Graphs fill\r\n\
         rows left to right. Series ids are shown by saving a layout that has the graphs you want.\r\n\r\n\
         Example:\r\n\r\n{}",
        example.replace('\n', "\r\n")
    );
    let _ = std::fs::write(path, text);
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

    #[test]
    fn paths_break_at_gaps() {
        let slices = [Some((0.5, 0.5)), Some((1.0, 1.0)), None, Some((0.0, 0.0))];
        let (line, fill) = paths(&slices, 1.0);
        assert_eq!(line.matches('M').count(), 2, "{line}");
        assert!(line.starts_with("M 0.00 50.00 L 33.33 0.00"));
        assert_eq!(fill.matches('Z').count(), 2);
        let (empty, _) = paths(&[None, None], 1.0);
        assert_eq!(empty, "M 0 100 L 100 100");
    }

    #[test]
    fn slices_average_and_keep_peaks() {
        let points = [(100, 1.0, 1.0), (101, 3.0, 5.0), (460, 2.0, 2.0)];
        let slices = slices(points, 100, 3700);
        assert_eq!(slices.len(), POINTS);
        assert_eq!(slices[0], Some((2.0, 5.0)));
        assert_eq!(slices[36], Some((2.0, 2.0)));
        assert_eq!(slices[1], None);
    }

    #[test]
    fn graphs_show_what_was_read() {
        let mut loaded = Loaded {
            range: Range::Hour,
            series: HashMap::new(),
            units: HashMap::new(),
            busiest: Vec::new(),
            seconds: 0,
            first: None,
            same_build: None,
        };
        loaded.series.insert("cpu".into(), vec![Some((0.2, 0.4)), None]);
        loaded.series.insert("gone.0".into(), vec![Some((60.0, 70.0))]);
        loaded.units.insert("gone.0".into(), Unit::Celsius);
        let place = Placement { column: 6, span: 6, top: 142.0, height: 130.0 };
        let cpu = graph(&GraphSpec::new("cpu", 6, 130), &place, &[], Some(&loaded));
        assert_eq!(cpu.corner, "average 20% · peak 40%");
        assert_eq!((cpu.column, cpu.span, cpu.top), (6, 6, 142.0));
        let gone = graph(&GraphSpec::new("gone.0", 6, 130), &place, &[], Some(&loaded));
        assert_eq!(gone.corner, "average 60 °C · peak 70 °C");
        assert_eq!(gone.title, "gone.0");
        let empty = graph(&GraphSpec::new("cpu+memory", 6, 130), &place, &[], None);
        assert_eq!(empty.corner, "");
    }
}
