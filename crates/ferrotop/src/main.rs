//! Ferrotop: a memory-safe process explorer for Windows.
// `deny` rather than `forbid`: the Slint-generated UI glue opts into `unsafe` internally.
// Handwritten code in this crate can't.
#![deny(unsafe_code)]
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod actions;
mod history_page;
mod pages;
mod performance;
mod process_columns;
mod series_ui;
mod tables;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ferrotop_core::activity::ActivityStatus;
use ferrotop_core::details::{Account, file_key};
use ferrotop_core::format;
use ferrotop_core::perf::{PerfSample, PerformanceProvider};
use ferrotop_core::provider::{ProcessItem, ProcessKey, ProcessProvider, ProcessState, Sample, is_pseudo_process};
use ferrotop_core::sampler::Sampler;
use ferrotop_core::settings::{CpuGraph, Page, Settings, ThemeMode};
use ferrotop_core::tree::{self, Row, SortColumn, SortOrder, ViewOptions};
use ferrotop_sys::service_host;
use ferrotop_sys::tray::{TrayEvent, TrayIcon};
use ferrotop_sys::window::{Rgb, TitleBarStyle};
use performance::Device;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

slint::include_modules!();

const REFRESH_INTERVAL: Duration = Duration::from_secs(1);
/// Refreshes a status-bar message stays up.
const MESSAGE_REFRESHES: u8 = 6;
/// UI page numbers past the four tabs.
const PROPERTIES_PAGE: i32 = 4;
const FIND_PAGE: i32 = 5;
const TOOLS_PAGE: i32 = 6;
const HISTORY_PAGE: i32 = 7;
/// What a declined UAC prompt fails with.
const ERROR_CANCELLED: u32 = 1223;
const WINDOWS_PAGE: i32 = 8;

/// Sort columns in the order the UI numbers them (0 = unsorted).
const UI_COLUMNS: [SortColumn; 18] = [
    SortColumn::Natural,
    SortColumn::Name,
    SortColumn::Pid,
    SortColumn::Cpu,
    SortColumn::IoRate,
    SortColumn::PrivateBytes,
    SortColumn::WorkingSet,
    SortColumn::Threads,
    SortColumn::Handles,
    SortColumn::User,
    SortColumn::Description,
    SortColumn::Gpu,
    SortColumn::GpuDedicated,
    SortColumn::GpuShared,
    SortColumn::Signer,
    SortColumn::Disk,
    SortColumn::Network,
    SortColumn::Memory,
];

/// The Performance page's list models, updated in place each refresh.
#[derive(Default)]
struct PerformanceModels {
    devices: Rc<VecModel<DeviceTile>>,
    big: Rc<VecModel<GraphData>>,
    pairs: Rc<VecModel<GraphPair>>,
    small: Rc<VecModel<GraphData>>,
    top: Rc<VecModel<Stat>>,
    bottom: Rc<VecModel<Stat>>,
    sensors: Rc<VecModel<Stat>>,
    info: Rc<VecModel<Stat>>,
    cells: Rc<VecModel<GraphData>>,
    cell_groups: Rc<VecModel<CellGroup>>,
    toggles: Rc<VecModel<GraphToggle>>,
}

thread_local! {
    /// App state, reachable from samples and background results posted to the UI thread.
    static APP: RefCell<Option<Rc<RefCell<App>>>> = const { RefCell::new(None) };
}

struct App {
    ui: slint::Weak<MainWindow>,
    provider: ProcessProvider,
    sampler: Option<Sampler>,
    model: Rc<VecModel<ProcessRow>>,
    rows: Vec<Row>,
    collapsed: HashSet<ProcessKey>,
    settings: Settings,
    filter: String,
    selected: Option<ProcessKey>,
    /// The process whose path and command line are shown, so they're only queried once.
    details_for: Option<ProcessKey>,
    message_refreshes: u8,
    perf: PerformanceProvider,
    device: Device,
    perf_models: PerformanceModels,
    /// Process icons by image-file key, converted once.
    icons: HashMap<String, slint::Image>,
    /// The theme Windows reported last, to follow live changes in "System" mode.
    system_dark: Option<bool>,
    page: i32,
    pages: pages::Pages,
    actions: actions::Actions,
    tray: Option<TrayIcon>,
    /// False while the window is hidden in the tray.
    visible: bool,
    /// Settings changed by something that happens many times a second (resizing a column), to
    /// save with the next refresh.
    settings_dirty: bool,
    header_tiles: Rc<VecModel<HeaderTile>>,
    header_histories: series_ui::HeaderHistories,
    /// What the graph picker is choosing for: 0 History, 1 the header.
    picker_target: i32,
}

impl App {
    fn new(ui: slint::Weak<MainWindow>, settings: Settings) -> Self {
        let device = Device::parse(&settings.device).unwrap_or(Device::Cpu);
        Self {
            ui,
            provider: ProcessProvider::new(),
            sampler: None,
            model: Rc::new(VecModel::default()),
            rows: Vec::new(),
            collapsed: HashSet::new(),
            settings,
            filter: String::new(),
            selected: None,
            details_for: None,
            message_refreshes: 0,
            perf: PerformanceProvider::new(),
            device,
            perf_models: PerformanceModels::default(),
            icons: HashMap::new(),
            system_dark: None,
            page: 0,
            pages: pages::Pages::default(),
            actions: actions::Actions::default(),
            tray: None,
            visible: false,
            settings_dirty: false,
            header_tiles: Rc::new(VecModel::default()),
            header_histories: series_ui::HeaderHistories::default(),
            picker_target: 0,
        }
    }

    fn apply_sample(&mut self, ui: &MainWindow, sample: ferrotop_sys::Result<Sample>) {
        let sample = match sample {
            Ok(sample) => sample,
            Err(error) => return self.show_message(ui, format!("Refresh failed: {error}")),
        };
        let cpus = sample.cpus.clone();
        self.provider.update(sample);
        self.perf.update_cpu(self.provider.stats().cpu_usage, &cpus);
        self.update_tray_tooltip();
        // Hidden in the tray: keep the data current, but don't build views nobody can see.
        if self.visible {
            self.refresh_view(ui);
        }
    }

    /// Brings every view up to date with the latest data.
    fn refresh_view(&mut self, ui: &MainWindow) {
        if std::mem::take(&mut self.settings_dirty) {
            self.save_settings(ui);
        }
        self.convert_icons();
        if self.selected.is_some_and(|key| self.provider.get(&key).is_none()) {
            self.selected = None;
        }
        if self.message_refreshes > 0 {
            self.message_refreshes -= 1;
            if self.message_refreshes == 0 {
                ui.set_status_message(SharedString::new());
            }
        }
        self.follow_system_theme(ui);
        self.rebuild_rows(ui);
        self.update_system(ui);
        self.update_details(ui);
        if self.page == 1 {
            self.update_performance(ui);
        }
        if self.page == HISTORY_PAGE {
            self.history_tick(ui);
        }
        self.refresh_pages(ui);
    }

    /// Folds in Performance view figures, which arrive from their own sampler thread.
    fn apply_perf(&mut self, ui: &MainWindow, mut perf: PerfSample) {
        self.provider.set_gpu_usage(std::mem::take(&mut perf.gpu_processes));
        self.perf.update_devices(&perf);
        self.record_header();
        if self.visible && self.page == 1 {
            self.update_performance(ui);
        }
    }

    fn update_tray_tooltip(&self) {
        if let Some(tray) = &self.tray {
            let stats = self.provider.stats();
            let memory = stats.memory_used() as f64 * 100.0 / stats.memory.total_physical.max(1) as f64;
            tray.set_tooltip(&format!("Ferrotop\nCPU {:.0}%  ·  memory {memory:.0}%", stats.cpu_usage * 100.0));
        }
    }

    fn tray_event(&mut self, ui: &MainWindow, event: TrayEvent) {
        match event {
            TrayEvent::Show => self.show_window(ui),
            TrayEvent::Exit => {
                self.save_settings(ui);
                let _ = slint::quit_event_loop();
            }
        }
    }

    /// Shows the window (from the tray, or for a second launch) and brings it to the front.
    fn show_window(&mut self, ui: &MainWindow) {
        if !self.visible {
            self.visible = true;
            let _ = ui.show();
            when_native_window(ui.as_weak(), 0);
            self.refresh_view(ui);
        }
        if let Some(hwnd) = window_handle(ui) {
            let _ = ferrotop_sys::desktop::bring_to_front(hwnd);
        }
    }

    /// The window's close button: hide to the tray, or exit.
    fn close_requested(&mut self, ui: &MainWindow) {
        self.save_settings(ui);
        if self.settings.close_to_tray && self.tray.is_some() {
            self.visible = false;
        } else {
            let _ = slint::quit_event_loop();
        }
    }

    fn set_close_to_tray(&mut self, ui: &MainWindow, on: bool) {
        self.settings.close_to_tray = on;
        self.save_settings(ui);
    }

    fn set_start_at_sign_in(&mut self, ui: &MainWindow, on: bool) {
        match ferrotop_sys::system::set_start_at_sign_in(on) {
            Ok(()) if on => self.show_message(ui, "Ferrotop will start in the tray when you sign in".into()),
            Ok(()) => self.show_message(ui, "Ferrotop won't start when you sign in".into()),
            Err(error) => {
                ui.set_start_at_sign_in(!on);
                self.show_message(ui, format!("Couldn't change the sign-in setting: {error}"));
            }
        }
    }

    /// View menu: open Ferrotop instead of Task Manager, or put Task Manager back. It's a setting
    /// for the whole machine, so an elevated copy of Ferrotop makes the change (the UAC prompt is
    /// the confirmation).
    fn set_replace_task_manager(&mut self, ui: &MainWindow, replace: bool) {
        let arguments = if replace { "--replace-task-manager" } else { "--restore-task-manager" };
        self.show_message(ui, "Asking Windows for administrator approval…".into());
        background(
            move || service_host::run_self_elevated(arguments),
            move |app, ui, result| {
                let what = if replace { "Couldn't replace Task Manager" } else { "Couldn't put Task Manager back" };
                let message = match result {
                    Ok(0) if replace => "Task Manager now opens Ferrotop".to_owned(),
                    Ok(0) => "Task Manager is back".to_owned(),
                    Ok(1) => what.to_owned(),
                    Ok(code) => format!("{what}: {}", std::io::Error::from_raw_os_error(code as i32)),
                    Err(ferrotop_sys::Error::Win32 { code: ERROR_CANCELLED, .. }) => "Cancelled".to_owned(),
                    Err(error) => format!("{what}: {error}"),
                };
                app.show_message(ui, message);
                ui.set_replace_task_manager(ferrotop_sys::task_manager::is_replaced());
            },
        );
    }

    /// Turns newly checked image files' icon pixels into UI images, once per file.
    fn convert_icons(&mut self) {
        for item in self.provider.items() {
            let (Some(file), Some(path)) = (&item.file, item.details.as_ref().and_then(|d| d.image_path.as_deref()))
            else {
                continue;
            };
            let key = file_key(path);
            if self.icons.contains_key(&key) {
                continue;
            }
            let image = file.icon.as_ref().map_or_else(slint::Image::default, |icon| {
                let pixels = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                    &icon.rgba,
                    icon.width,
                    icon.height,
                );
                slint::Image::from_rgba8(pixels)
            });
            self.icons.insert(key, image);
        }
        if self.icons.len() > 4096 {
            self.icons.clear();
        }
    }

    fn icon_for(&self, item: &ProcessItem) -> slint::Image {
        item.details
            .as_ref()
            .and_then(|d| d.image_path.as_deref())
            .and_then(|path| self.icons.get(&file_key(path)))
            .cloned()
            .unwrap_or_default()
    }

    fn update_performance(&mut self, ui: &MainWindow) {
        let stats = *self.provider.stats();
        let devices = performance::devices(&self.perf);
        // The chosen device may not be there yet (disks and GPUs show once they can be read) or
        // any more (an unplugged disk): show the CPU meanwhile, but keep the choice.
        let device = if devices.contains(&self.device) { self.device } else { Device::Cpu };
        let tiles = devices.iter().map(|&device| performance::tile(&self.perf, &stats, device)).collect();
        sync_model(&self.perf_models.devices, tiles);
        ui.set_selected_device(devices.iter().position(|d| *d == device).unwrap_or(0) as i32);

        let detail =
            performance::detail(&self.perf, &stats, device, self.settings.cpu_graph, &self.settings.hidden_graphs);
        ui.set_device_title(detail.title.into());
        ui.set_device_model(detail.model.into());
        let utilization_shown = detail.toggles.iter().any(|t| t.id == "cpu.utilization" && t.shown);
        ui.set_show_cpu_graphs(device == Device::Cpu && utilization_shown);
        ui.set_cpu_graph(CpuGraph::ALL.iter().position(|g| *g == self.settings.cpu_graph).unwrap_or(0) as i32);
        ui.set_cells_title(detail.cells_title.into());
        ui.set_cell_columns(detail.cell_columns);
        ui.set_figures_only(detail.figures_only);
        let models = &self.perf_models;
        sync_model(&models.cells, detail.cells);
        sync_model(&models.cell_groups, detail.cell_groups);
        sync_model(&models.big, detail.big);
        sync_model(&models.pairs, detail.pairs);
        sync_model(&models.small, detail.small);
        sync_model(&models.top, detail.top);
        sync_model(&models.bottom, detail.bottom);
        sync_model(&models.sensors, detail.sensors);
        sync_model(&models.info, detail.info);
        sync_model(&models.toggles, detail.toggles);
    }

    /// View menu: show or hide one of the selected device's graphs.
    fn toggle_graph(&mut self, ui: &MainWindow, (index, shown): (i32, bool)) {
        let Some(toggle) = usize::try_from(index).ok().and_then(|i| self.perf_models.toggles.row_data(i)) else {
            return;
        };
        let id = toggle.id.to_string();
        self.settings.hidden_graphs.retain(|hidden| *hidden != id);
        if !shown {
            self.settings.hidden_graphs.push(id);
        }
        self.update_performance(ui);
        self.save_settings(ui);
    }

    fn choose_cpu_graph(&mut self, ui: &MainWindow, index: i32) {
        if let Some(&graph) = usize::try_from(index).ok().and_then(|i| CpuGraph::ALL.get(i)) {
            self.settings.cpu_graph = graph;
            self.update_performance(ui);
            self.save_settings(ui);
        }
    }

    fn select_device(&mut self, ui: &MainWindow, index: i32) {
        if let Some(device) = usize::try_from(index).ok().and_then(|i| performance::devices(&self.perf).get(i).copied())
        {
            self.device = device;
            self.settings.device = device.id();
            self.update_performance(ui);
            self.save_settings(ui);
        }
    }

    fn page_shown(&mut self, ui: &MainWindow, page: i32) {
        self.page = page;
        if let Some(tab) = Page::from_index(page) {
            self.settings.page = tab;
            self.save_settings(ui);
        }
        match page {
            1 => self.update_performance(ui),
            2 => self.load_services(ui),
            3 => self.load_network(ui),
            HISTORY_PAGE => self.show_history(ui),
            WINDOWS_PAGE => self.load_windows(ui),
            _ => {}
        }
    }

    fn show_message(&mut self, ui: &MainWindow, message: String) {
        ui.set_status_message(message.into());
        self.message_refreshes = MESSAGE_REFRESHES;
    }

    fn rebuild_rows(&mut self, ui: &MainWindow) {
        let options = ViewOptions {
            tree: self.settings.tree,
            sort: self.settings.sort,
            order: self.settings.order,
            filter: &self.filter,
            collapsed: &self.collapsed,
        };
        self.rows = tree::build_rows(self.provider.items(), &options);
        let rows: Vec<ProcessRow> = {
            let columns = self.shown_columns();
            self.rows.iter().filter_map(|row| Some(self.ui_row(row, self.provider.get(&row.key)?, &columns))).collect()
        };
        sync_model(&self.model, rows);
        let selected_index = self.selected.and_then(|key| self.rows.iter().position(|row| row.key == key));
        ui.set_selected_index(selected_index.map_or(-1, |i| i as i32));
        ui.set_sort_column(UI_COLUMNS.iter().position(|c| *c == self.settings.sort).unwrap_or(0) as i32);
        ui.set_sort_ascending(self.settings.order == SortOrder::Ascending);
    }

    fn ui_row(&self, row: &Row, item: &ProcessItem, columns: &[&process_columns::ColumnDef]) -> ProcessRow {
        ProcessRow {
            icon: self.icon_for(item),
            name: item.name.as_str().into(),
            cells: ModelRc::new(VecModel::from(
                columns.iter().map(|def| process_columns::cell(def.column, item)).collect::<Vec<_>>(),
            )),
            suspended: item.suspended,
            depth: i32::from(row.depth),
            has_children: row.has_children,
            expanded: row.expanded,
            selected: self.selected == Some(row.key),
            highlight: highlight(item),
        }
    }

    fn update_system(&self, ui: &MainWindow) {
        let stats = self.provider.stats();
        let total = stats.memory.total_physical;
        let used = stats.memory_used();

        self.update_header();

        ui.set_status_cpu(
            format!("CPU {:.1}% (kernel {:.1}%)", stats.cpu_usage * 100.0, stats.kernel_usage * 100.0).into(),
        );
        ui.set_status_memory(
            format!(
                "Memory {} / {}  ·  commit {} / {}",
                format::bytes(used),
                format::bytes(total),
                format::bytes(stats.memory.commit_total),
                format::bytes(stats.memory.commit_limit)
            )
            .into(),
        );
        ui.set_status_counts(
            format!(
                "{} processes · {} threads · {} handles",
                format::count(stats.processes as u64),
                format::count(stats.threads),
                format::count(stats.handles)
            )
            .into(),
        );
    }

    /// Parent as `name (pid)`, using the same creation-time rule as the tree.
    fn parent_label(&self, item: &ProcessItem) -> String {
        self.provider
            .items()
            .filter(|p| p.key.pid == item.parent_pid && p.key.pid != item.key.pid)
            .filter(|p| p.key.pid == 4 || p.key.create_time <= item.key.create_time)
            .max_by_key(|p| p.key.create_time)
            .map_or_else(
                || format!("non-existent process ({})", item.parent_pid),
                |p| format!("{} ({})", p.name, p.key.pid),
            )
    }

    fn update_details(&mut self, ui: &MainWindow) {
        let Some(item) = self.selected.and_then(|key| self.provider.get(&key)) else {
            ui.set_has_selection(false);
            self.details_for = None;
            return;
        };
        ui.set_has_selection(true);
        ui.set_detail_name(item.name.as_str().into());

        let parent = self.parent_label(item);
        let started = match item.key.create_time {
            0 => "at boot".to_owned(),
            created => format!("{} ago", format::duration(seconds_since_filetime(created))),
        };
        let exited = if item.state == ProcessState::Removed { "  ·  exited" } else { "" };
        let suspended = if item.suspended { "  ·  suspended" } else { "" };
        let pid = if is_pseudo_process(item.key.pid) { "—".to_owned() } else { item.key.pid.to_string() };
        ui.set_detail_summary(
            format!(
                "PID {pid}  ·  parent {parent}  ·  session {}  ·  priority {}  ·  started {started}{exited}{suspended}",
                item.session_id, item.base_priority
            )
            .into(),
        );

        let details = item.details.as_deref();
        let user = details.and_then(|d| d.user.as_deref()).map(|user| {
            let elevated = if details.is_some_and(|d| d.elevated) { " (elevated)" } else { "" };
            format!("{user}{elevated}")
        });
        let signer = item.file.as_ref().map(|f| match f.signer() {
            signer if signer.starts_with('(') => signer,
            signer => format!("signed by {signer}"),
        });
        let identity: Vec<String> =
            [details.and_then(|d| d.description.clone()), details.and_then(|d| d.company.clone()), user, signer]
                .into_iter()
                .flatten()
                .collect();
        ui.set_detail_identity(identity.join("  ·  ").into());
        ui.set_detail_services(item.services.join(", ").into());
        let gpu = &item.gpu;
        let gpu = if gpu.engine.is_empty() && gpu.dedicated == 0 && gpu.shared == 0 {
            String::new()
        } else {
            format!(
                "  ·  GPU {}%{}, memory {} dedicated, {} shared",
                fallback(format::cpu((gpu.usage / 100.0) as f32), "0"),
                if gpu.engine.is_empty() { String::new() } else { format!(" ({})", gpu.engine) },
                fallback(format::bytes(gpu.dedicated), "0"),
                fallback(format::bytes(gpu.shared), "0"),
            )
        };
        let activity = match self.provider.stats().activity {
            ActivityStatus::Running | ActivityStatus::Service => format!(
                "  ·  disk read {} write {}  ·  network sent {} received {}",
                fallback(format::rate(item.disk_read_rate), "0"),
                fallback(format::rate(item.disk_write_rate), "0"),
                fallback(format::rate(item.net_send_rate), "0"),
                fallback(format::rate(item.net_receive_rate), "0"),
            ),
            ActivityStatus::NeedsAdmin => "  ·  disk and network: run as administrator".to_owned(),
            ActivityStatus::Failed => String::new(),
        };
        ui.set_detail_usage(
            format!(
                "CPU {}% (kernel {}%, user {}%)  ·  private {}  ·  working set {}  ·  I/O read {} write {} other {}{activity}{gpu}",
                fallback(format::cpu(item.cpu_usage), "0"),
                fallback(format::cpu(item.kernel_usage), "0"),
                fallback(format::cpu(item.user_usage), "0"),
                fallback(format::bytes(item.private_bytes), "0"),
                fallback(format::bytes(item.working_set), "0"),
                fallback(format::rate(item.io_read_rate), "0"),
                fallback(format::rate(item.io_write_rate), "0"),
                fallback(format::rate(item.io_other_rate), "0"),
            )
            .into(),
        );

        if self.details_for != Some(item.key) {
            self.details_for = Some(item.key);
            let pid = item.key.pid;
            let (path, command_line) = if pid == 0 || is_pseudo_process(pid) {
                ("(kernel)".to_owned(), String::new())
            } else if pid == 4 {
                (details.and_then(|d| d.image_path.clone()).unwrap_or_default(), String::new())
            } else {
                let path = match details.and_then(|d| d.image_path.clone()) {
                    Some(path) => path,
                    None => describe(ferrotop_sys::process::image_path(pid)),
                };
                (path, describe(ferrotop_sys::process::command_line(pid)))
            };
            ui.set_detail_path(path.into());
            ui.set_detail_command_line(command_line.into());
        }
    }

    fn select_index(&mut self, ui: &MainWindow, index: i32) {
        self.selected = usize::try_from(index).ok().and_then(|i| self.rows.get(i)).map(|row| row.key);
        self.rebuild_rows(ui);
        self.update_details(ui);
    }

    /// Selects the newest live process with this PID on the Processes page (from the Services,
    /// Network or Find pages).
    fn go_to_process(&mut self, ui: &MainWindow, pid: u32) {
        let Some(key) = self
            .provider
            .items()
            .filter(|item| item.key.pid == pid && item.state != ProcessState::Removed)
            .max_by_key(|item| item.key.create_time)
            .map(|item| item.key)
        else {
            return self.show_message(ui, format!("PID {pid} isn't running"));
        };
        // Expand its ancestors so a tree view shows it.
        let mut parent = self.provider.get(&key).map(|item| (item.parent_pid, item.key.pid));
        for _ in 0..64 {
            let Some((parent_pid, child_pid)) = parent else { break };
            if parent_pid == child_pid {
                break;
            }
            let ancestor =
                self.provider.items().filter(|item| item.key.pid == parent_pid).max_by_key(|i| i.key.create_time);
            parent = ancestor.map(|a| (a.parent_pid, a.key.pid));
            if let Some(ancestor) = ancestor {
                self.collapsed.remove(&ancestor.key);
            }
        }
        self.selected = Some(key);
        self.switch_page(ui, 0);
        self.rebuild_rows(ui);
        self.update_details(ui);
        if self.rows.iter().any(|row| row.key == key) {
            ui.invoke_ensure_visible();
            // The list was hidden a moment ago and has no height yet, so scrolling now can leave
            // the row just out of view: center it once the page is laid out.
            let window = ui.as_weak();
            slint::Timer::single_shot(std::time::Duration::from_millis(60), move || {
                if let Some(ui) = window.upgrade() {
                    ui.invoke_reveal_selected();
                }
            });
        } else {
            self.show_message(ui, format!("PID {pid} is hidden by the filter"));
        }
    }

    fn move_selection(&mut self, ui: &MainWindow, step: i32) {
        if self.rows.is_empty() {
            return;
        }
        let last = self.rows.len() as i32 - 1;
        let current = self.selected.and_then(|key| self.rows.iter().position(|row| row.key == key));
        let next = match current {
            Some(i) => (i as i32 + step).clamp(0, last),
            None => 0,
        };
        self.select_index(ui, next);
    }

    fn toggle_expand(&mut self, ui: &MainWindow, index: i32) {
        if let Some(row) = usize::try_from(index).ok().and_then(|i| self.rows.get(i)) {
            if !self.collapsed.remove(&row.key) {
                self.collapsed.insert(row.key);
            }
            self.rebuild_rows(ui);
        }
    }

    fn set_expanded(&mut self, ui: &MainWindow, expanded: bool) {
        let Some(key) = self.selected else { return };
        let changed = if expanded { self.collapsed.remove(&key) } else { self.collapsed.insert(key) };
        if changed {
            self.rebuild_rows(ui);
        }
    }

    /// Header click: default order → reversed → unsorted, like System Informer.
    fn sort_by(&mut self, ui: &MainWindow, column: i32) {
        let Some(&column) = usize::try_from(column).ok().and_then(|i| UI_COLUMNS.get(i)) else { return };
        let settings = &mut self.settings;
        if column != settings.sort {
            settings.sort = column;
            settings.order = column.default_order();
        } else if settings.order == column.default_order() {
            settings.order = match settings.order {
                SortOrder::Ascending => SortOrder::Descending,
                SortOrder::Descending => SortOrder::Ascending,
            };
        } else {
            settings.sort = SortColumn::Natural;
            settings.order = SortOrder::Ascending;
        }
        self.rebuild_rows(ui);
        self.save_settings(ui);
    }

    fn set_tree(&mut self, ui: &MainWindow, tree: bool) {
        self.settings.tree = tree;
        self.rebuild_rows(ui);
        self.save_settings(ui);
    }

    fn choose_theme(&mut self, ui: &MainWindow, index: i32) {
        self.settings.theme = ThemeMode::ALL.get(index as usize).copied().unwrap_or_default();
        self.apply_theme(ui);
        self.save_settings(ui);
    }

    fn apply_theme(&mut self, ui: &MainWindow) {
        self.system_dark = ferrotop_sys::system::apps_use_light_theme().map(|light| !light);
        let dark = match self.settings.theme {
            ThemeMode::Light => false,
            ThemeMode::Dark => true,
            ThemeMode::System => self.system_dark == Some(true),
        };
        ui.global::<Theme>().set_dark(dark);
        ui.set_dark_mode(dark);
        ui.set_theme_index(ThemeMode::ALL.iter().position(|t| *t == self.settings.theme).unwrap_or(0) as i32);
        style_title_bar(ui);
    }

    /// In "System" mode, follow Windows when the user switches between light and dark.
    fn follow_system_theme(&mut self, ui: &MainWindow) {
        if self.settings.theme == ThemeMode::System {
            let now = ferrotop_sys::system::apps_use_light_theme().map(|light| !light);
            if now != self.system_dark {
                self.apply_theme(ui);
            }
        }
    }

    fn save_settings(&mut self, ui: &MainWindow) {
        let window = ui.window();
        let size = window.size().to_logical(window.scale_factor());
        if size.width >= 1.0 && size.height >= 1.0 {
            self.settings.window = Some((size.width.round() as u32, size.height.round() as u32));
        }
        if let Err(error) = self.settings.save() {
            self.show_message(ui, format!("Couldn't save settings: {error}"));
        }
    }

    fn restart_elevated(&mut self, ui: &MainWindow) {
        self.save_settings(ui);
        match ferrotop_sys::system::relaunch_elevated() {
            Ok(()) => {
                let _ = slint::quit_event_loop();
            }
            Err(error) => self.show_message(ui, format!("Didn't restart as administrator: {error}")),
        }
    }

    /// Process names by PID, for naming handle targets, connection owners and service hosts.
    fn process_names(&self) -> HashMap<u32, String> {
        self.provider
            .items()
            .filter(|item| item.state != ProcessState::Removed)
            .map(|item| (item.key.pid, item.name.clone()))
            .collect()
    }
}

/// Runs `work` on a background thread, then `done` with its result on the UI thread.
fn background<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
    done: impl FnOnce(&mut App, &MainWindow, T) + Send + 'static,
) {
    let spawned = std::thread::Builder::new().name("ferrotop-worker".into()).spawn(move || {
        let result = work();
        let _ = slint::invoke_from_event_loop(move || with_app(|app, ui| done(app, ui, result)));
    });
    if spawned.is_err() {
        // Called from inside an App method, so report through the window directly.
        eprintln!("couldn't start a background task");
    }
}

/// Runs `f` with the app and window, if both still exist. Only call from the UI thread, outside
/// any other borrow of the app.
fn with_app(f: impl FnOnce(&mut App, &MainWindow)) {
    let Some(app) = APP.with(|cell| cell.borrow().clone()) else { return };
    let Ok(mut app) = app.try_borrow_mut() else { return };
    if let Some(ui) = app.ui.upgrade() {
        f(&mut app, &ui);
    }
}

/// Paints the native title bar (caption, text, buttons and border) in the current theme's
/// toolbar colors so it blends into the app. Does nothing until the window exists.
/// Starts the graphs with the last minute or so the history service recorded, if it's running,
/// so they aren't empty when Ferrotop opens.
fn seed_graphs(app: &mut App) {
    let Ok(dir) = service_host::data_dir() else { return };
    let now = ferrotop_core::archive::now_unix();
    let recent = ferrotop_core::archive::read_seconds(&dir, now.saturating_sub(90), now);
    if recent.is_empty() {
        return;
    }
    let total_memory = ferrotop_sys::system::memory_status().map_or(0, |m| m.total_physical);
    app.provider.seed_history(&recent, total_memory);
    app.perf.seed(&recent);
}

/// `--service`: runs as the history service, started by the Service Control Manager.
fn run_history_service() {
    let _ = service_host::run_service(|stop| {
        let Ok(dir) = service_host::data_dir() else { return };
        // Only record into a folder ordinary users can't tamper with (the installer makes it).
        if !service_host::is_trusted_dir(&dir) {
            return;
        }
        let _ = ferrotop_core::recorder::run(&dir, |timeout| stop.wait(timeout));
    });
}

/// The exit code an elevated helper reports: 0 for success, else the Windows error code (or 1).
fn exit_code(result: ferrotop_sys::Result<()>) -> i32 {
    match result {
        Ok(()) => 0,
        Err(ferrotop_sys::Error::Win32 { code, .. }) if code > 1 => code as i32,
        Err(_) => 1,
    }
}

/// The native window's handle value, once it has been shown.
fn window_handle(ui: &MainWindow) -> Option<usize> {
    let handle = ui.window().window_handle();
    match handle.window_handle().map(|h| h.as_raw()) {
        Ok(RawWindowHandle::Win32(win32)) => Some(win32.hwnd.get() as usize),
        _ => None,
    }
}

fn style_title_bar(ui: &MainWindow) {
    let handle = ui.window().window_handle();
    let Ok(RawWindowHandle::Win32(win32)) = handle.window_handle().map(|h| h.as_raw()) else { return };
    let theme = ui.global::<Theme>();
    let rgb = |color: slint::Color| Rgb(color.red(), color.green(), color.blue());
    let style = TitleBarStyle {
        dark: theme.get_dark(),
        caption: rgb(theme.get_panel()),
        text: rgb(theme.get_text()),
        border: rgb(theme.get_border()),
    };
    // Older Windows versions without title-bar theming just keep the default look.
    let _ = ferrotop_sys::window::style_title_bar(win32.hwnd, &style);
}

/// Styles the title bar and watches repaints once the native window exists: `show()` only asks
/// for it, and it's created when the event loop gets to it.
fn when_native_window(ui: slint::Weak<MainWindow>, tries: u32) {
    let Some(window) = ui.upgrade() else { return };
    if window_handle(&window).is_some() {
        style_title_bar(&window);
        watch_repaints(&window);
    } else if tries < 250 {
        slint::Timer::single_shot(Duration::from_millis(20), move || when_native_window(ui, tries + 1));
    }
}

/// Repaints the whole window when Windows asks for part of it: Slint itself repaints only what it
/// changed, which would leave areas Windows lost (after maximizing, say) blank.
fn watch_repaints(ui: &MainWindow) {
    let handle = ui.window().window_handle();
    let Ok(RawWindowHandle::Win32(win32)) = handle.window_handle().map(|h| h.as_raw()) else { return };
    let _ = ferrotop_sys::window::watch_repaints(win32.hwnd, || {
        // Toggling this marks the whole window changed (see `repaint-all` in main.slint).
        let _ = slint::invoke_from_event_loop(|| with_app(|_, ui| ui.set_repaint_all(!ui.get_repaint_all())));
    });
}

/// The row highlight, in System Informer's priority order. Matches `ProcessRow.highlight`.
fn highlight(item: &ProcessItem) -> i32 {
    let details = item.details.as_deref();
    let account = details.map_or(Account::Unknown, |d| d.account);
    match item.state {
        ProcessState::New => 1,
        ProcessState::Removed => 2,
        _ if !item.services.is_empty() => 4,
        _ if matches!(item.key.pid, 0 | 4) || is_pseudo_process(item.key.pid) || account == Account::System => 5,
        _ if account == Account::Own && details.is_some_and(|d| d.elevated) => 6,
        _ if account == Account::Own => 3,
        _ => 0,
    }
}

/// Updates the model in place so the list keeps its scroll position and only changed rows redraw.
fn sync_model<T: Clone + PartialEq + 'static>(model: &VecModel<T>, rows: Vec<T>) {
    let len = rows.len();
    for (index, row) in rows.into_iter().enumerate() {
        if index >= model.row_count() {
            model.push(row);
        } else if model.row_data(index).as_ref() != Some(&row) {
            model.set_row_data(index, row);
        }
    }
    while model.row_count() > len {
        model.remove(model.row_count() - 1);
    }
}

fn describe(result: ferrotop_sys::Result<String>) -> String {
    match result {
        Ok(text) => text,
        Err(error) if error.is_access_denied() => "Access denied (run Ferrotop as administrator)".to_owned(),
        Err(error) => error.to_string(),
    }
}

fn fallback(text: String, empty: &str) -> String {
    if text.is_empty() { empty.to_owned() } else { text }
}

/// Seconds between a FILETIME (100 ns ticks since 1601) and now.
fn seconds_since_filetime(filetime: i64) -> u64 {
    const UNIX_EPOCH_AS_FILETIME: i64 = 116_444_736_000_000_000;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos() / 100) as i64;
    u64::try_from((now + UNIX_EPOCH_AS_FILETIME - filetime) / 10_000_000).unwrap_or(0)
}

/// Runs on the UI thread with a sample posted by the process sampler thread.
fn deliver(sample: ferrotop_sys::Result<Sample>) {
    with_app(|app, ui| app.apply_sample(ui, sample));
}

/// Runs on the UI thread with a sample posted by the Performance sampler thread.
fn deliver_perf(perf: PerfSample) {
    with_app(|app, ui| app.apply_perf(ui, perf));
}

/// Wraps an `App` method as a Slint callback that holds only weak references to the window.
fn handler<A: 'static>(
    ui: &MainWindow,
    app: &Rc<RefCell<App>>,
    action: impl Fn(&mut App, &MainWindow, A) + 'static,
) -> impl Fn(A) + 'static {
    let ui = ui.as_weak();
    let app = Rc::clone(app);
    move |arg| {
        if let Some(ui) = ui.upgrade()
            && let Ok(mut app) = app.try_borrow_mut()
        {
            action(&mut app, &ui, arg);
        }
    }
}

fn main() -> Result<(), slint::PlatformError> {
    // Before anything else loads a DLL: System32-only search, no remote or low-integrity images,
    // no legacy injection points.
    let _ = ferrotop_sys::control::harden_process();
    let mut arguments: Vec<String> = std::env::args().skip(1).collect();
    // Started in Task Manager's place: whatever starts Task Manager picks its arguments, and may
    // be elevated, so none of them count.
    if ferrotop_sys::task_manager::started_as_task_manager(arguments.first().map(String::as_str)) {
        arguments.clear();
    }
    // The history service and its installer are this same program, without a window.
    if arguments.iter().any(|a| a == "--service") {
        run_history_service();
        return Ok(());
    }
    if arguments.iter().any(|a| a == "--install-service") {
        let installed = std::env::current_exe()
            .map_err(|_| ferrotop_sys::Error::Other { message: "couldn't find Ferrotop's own path" })
            .and_then(|exe| service_host::install(&exe));
        std::process::exit(exit_code(installed));
    }
    if arguments.iter().any(|a| a == "--uninstall-service") {
        std::process::exit(exit_code(service_host::uninstall()));
    }
    if arguments.iter().any(|a| a == "--replace-task-manager") {
        let replaced = std::env::current_exe()
            .map_err(|_| ferrotop_sys::Error::Other { message: "couldn't find Ferrotop's own path" })
            .and_then(|exe| ferrotop_sys::task_manager::replace(&exe));
        std::process::exit(exit_code(replaced));
    }
    if arguments.iter().any(|a| a == "--restore-task-manager") {
        std::process::exit(exit_code(ferrotop_sys::task_manager::restore()));
    }
    // Started at sign-in: begin in the tray.
    let start_in_tray = arguments.iter().any(|a| a == "--tray");
    // An elevated restart takes over from the copy that launched it.
    let replace = arguments.iter().any(|a| a == "--replace");
    if !replace {
        // One Ferrotop at a time: a second launch shows the first instead.
        let deferred =
            if start_in_tray { ferrotop_sys::tray::is_running() } else { ferrotop_sys::tray::show_existing() };
        if deferred {
            return Ok(());
        }
    }
    let settings = Settings::load();
    let elevated = ferrotop_sys::system::is_elevated();
    let debug_privilege = elevated && ferrotop_sys::system::enable_debug_privilege().is_ok();

    let ui = MainWindow::new()?;
    if let Some((width, height)) = settings.window {
        ui.window().set_size(slint::LogicalSize::new(width as f32, height as f32));
    }
    ui.set_elevated(elevated);
    ui.set_version(env!("CARGO_PKG_VERSION").into());
    ui.set_tree_mode(settings.tree);
    ui.set_close_to_tray(settings.close_to_tray);
    ui.set_start_at_sign_in(ferrotop_sys::system::sign_in_command().is_some());
    ui.set_replace_task_manager(ferrotop_sys::task_manager::is_replaced());
    let lhm_port = settings.lhm_port;
    let start_page = settings.page.index();
    let app = Rc::new(RefCell::new(App::new(ui.as_weak(), settings)));
    {
        let mut app = app.borrow_mut();
        ui.set_rows(ModelRc::from(Rc::clone(&app.model)));
        let models = &app.perf_models;
        ui.set_devices(ModelRc::from(Rc::clone(&models.devices)));
        ui.set_big_graphs(ModelRc::from(Rc::clone(&models.big)));
        ui.set_graph_pairs(ModelRc::from(Rc::clone(&models.pairs)));
        ui.set_small_graphs(ModelRc::from(Rc::clone(&models.small)));
        ui.set_stats_top(ModelRc::from(Rc::clone(&models.top)));
        ui.set_stats_bottom(ModelRc::from(Rc::clone(&models.bottom)));
        ui.set_stats_sensors(ModelRc::from(Rc::clone(&models.sensors)));
        ui.set_device_info(ModelRc::from(Rc::clone(&models.info)));
        ui.set_cpu_cells(ModelRc::from(Rc::clone(&models.cells)));
        ui.set_cpu_cell_groups(ModelRc::from(Rc::clone(&models.cell_groups)));
        ui.set_graph_toggles(ModelRc::from(Rc::clone(&models.toggles)));
        app.pages.bind(&ui);
        let layouts = app.settings.table_layouts.clone();
        app.pages.apply_layouts(&layouts);
        app.actions.bind(&ui);
        app.apply_theme(&ui);
        app.push_columns(&ui);
        ui.set_header_tiles(ModelRc::from(Rc::clone(&app.header_tiles)));
        app.init_history(&ui);
        if elevated && !debug_privilege {
            app.show_message(&ui, "Running elevated, but SeDebugPrivilege couldn't be enabled".into());
        }
    }
    APP.with(|cell| *cell.borrow_mut() = Some(Rc::clone(&app)));
    seed_graphs(&mut app.borrow_mut());

    let sampler = Sampler::spawn(
        REFRESH_INTERVAL,
        lhm_port,
        // On the sampler threads: hand each sample to the UI thread.
        |sample| {
            let _ = slint::invoke_from_event_loop(move || deliver(sample));
        },
        |perf| {
            let _ = slint::invoke_from_event_loop(move || deliver_perf(perf));
        },
    )
    .map_err(|error| slint::PlatformError::Other(format!("couldn't start the sampler thread: {error}")))?;
    app.borrow_mut().sampler = Some(sampler);

    ui.on_row_clicked(handler(&ui, &app, App::select_index));
    ui.on_row_activated(handler(&ui, &app, App::open_properties_at));
    ui.on_select_device(handler(&ui, &app, App::select_device));
    ui.on_cpu_graph_chosen(handler(&ui, &app, App::choose_cpu_graph));
    ui.on_page_shown(handler(&ui, &app, App::page_shown));
    ui.on_move_selection(handler(&ui, &app, App::move_selection));
    ui.on_toggle_expand(handler(&ui, &app, App::toggle_expand));
    ui.on_set_expanded(handler(&ui, &app, App::set_expanded));
    ui.on_sort_by(handler(&ui, &app, App::sort_by));
    ui.on_tree_toggled(handler(&ui, &app, App::set_tree));
    ui.on_theme_chosen(handler(&ui, &app, App::choose_theme));
    let toggle = handler(&ui, &app, App::toggle_graph);
    ui.on_graph_toggled(move |index, shown| toggle((index, shown)));
    ui.on_filter_changed(handler(&ui, &app, |app, ui, text: SharedString| {
        app.filter = text.to_string();
        app.rebuild_rows(ui);
    }));
    let toggle_column = handler(&ui, &app, App::toggle_column);
    ui.on_column_toggled(move |index, shown| toggle_column((index, shown)));
    let resize_column = handler(&ui, &app, App::resize_process_column);
    ui.on_process_column_resized(move |position, width| resize_column((position, width)));
    let drop_column = handler(&ui, &app, App::drop_process_column);
    ui.on_process_column_dropped(move |position, x| drop_column((position, x)));
    let restart = handler(&ui, &app, |app, ui, ()| app.restart_elevated(ui));
    ui.on_restart_elevated(move || restart(()));
    pages::connect(&ui, &app);
    actions::connect(&ui, &app);
    let close = handler(&ui, &app, |app, ui, ()| app.close_requested(ui));
    ui.window().on_close_requested(move || {
        close(());
        slint::CloseRequestResponse::HideWindow
    });
    ui.on_close_to_tray_toggled(handler(&ui, &app, App::set_close_to_tray));
    ui.on_history_range_chosen(handler(&ui, &app, App::choose_history_range));
    let dropped = handler(&ui, &app, App::history_graph_dropped);
    ui.on_history_graph_dropped(move |index, column, y| dropped((index, column, y)));
    let resized = handler(&ui, &app, App::history_graph_resized);
    ui.on_history_graph_resized(move |index, span, height| resized((index, span, height)));
    ui.on_history_graph_removed(handler(&ui, &app, App::history_graph_removed));
    let programs = handler(&ui, &app, |app, ui, ()| app.toggle_history_programs(ui));
    ui.on_history_programs_toggled(move || programs(()));
    ui.on_history_programs_resized(handler(&ui, &app, App::history_programs_resized));
    let layouts = handler(&ui, &app, |app, ui, ()| app.list_history_layouts(ui));
    ui.on_history_layouts_requested(move || layouts(()));
    ui.on_history_layout_chosen(handler(&ui, &app, App::choose_history_layout));
    ui.on_picker_requested(handler(&ui, &app, App::open_picker));
    let picked = handler(&ui, &app, App::picker_toggled);
    ui.on_picker_toggled(move |key, shown| picked((key, shown)));
    ui.on_series_menu_requested(handler(&ui, &app, App::series_menu_requested));
    let chosen = handler(&ui, &app, App::series_menu_chosen);
    ui.on_series_menu_chosen(move |key, target| chosen((key, target)));
    let moved = handler(&ui, &app, App::header_graph_moved);
    ui.on_header_graph_moved(move |from, to| moved((from, to)));
    ui.on_windows_hidden_toggled(handler(&ui, &app, |app, ui, include: bool| {
        app.pages.windows_hidden = include;
        app.load_windows(ui);
    }));
    let history_service = handler(&ui, &app, |app, ui, ()| app.history_service_clicked(ui));
    ui.on_history_service_clicked(move || history_service(()));
    ui.on_start_at_sign_in_toggled(handler(&ui, &app, App::set_start_at_sign_in));
    ui.on_replace_task_manager_toggled(handler(&ui, &app, App::set_replace_task_manager));

    let tray = TrayIcon::add("Ferrotop", |event| {
        // Inside the icon's window procedure: act from the event loop instead.
        let _ = slint::invoke_from_event_loop(move || with_app(|app, ui| app.tray_event(ui, event)));
    });
    let has_tray = tray.is_ok();
    app.borrow_mut().tray = tray.ok();

    ui.invoke_show_page(start_page);
    // Without a tray icon there'd be no way back to a hidden window.
    if !start_in_tray || !has_tray {
        // Show first: the native window (and so the title bar) only exists once it's shown.
        ui.show()?;
        when_native_window(ui.as_weak(), 0);
        app.borrow_mut().visible = true;
    }
    // Keeps running while the window is hidden in the tray, until Exit.
    let result = slint::run_event_loop_until_quit();
    let _ = ui.hide();
    // Remove the tray icon, and stop the sampler threads (dropping them joins) before exiting.
    app.borrow_mut().tray.take();
    APP.with(|cell| cell.borrow_mut().take());
    app.borrow_mut().sampler.take();
    result
}
