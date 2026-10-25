//! The table pages: process properties, Services, Network and Find handles or DLLs. Slow queries
//! run on worker threads; results come back through [`crate::background`] and are dropped if the
//! user has moved on (each request carries a generation number).

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use ferrotop_core::find;
use ferrotop_core::network::connections_table;
use ferrotop_core::properties::{self, Context, Loaded, Tab, ThreadsSnapshot, services_table};
use ferrotop_core::provider::{ProcessKey, ProcessState};
use ferrotop_core::table::Sort;
use ferrotop_sys::desktop::{self, WindowInfo};
use ferrotop_sys::service::ServiceDetails;
use slint::{ModelRc, SharedString, VecModel};

use crate::tables::TableView;
use crate::{App, FIND_PAGE, MainWindow, PROPERTIES_PAGE, TableEvent, TableId, background, handler};

const SERVICES_REFRESH: Duration = Duration::from_secs(5);
/// How long General-tab facts (priority, affinity...) are trusted before re-reading them.
const GENERAL_REFRESH: Duration = Duration::from_secs(5);

/// The process whose properties are open.
struct Properties {
    key: ProcessKey,
    name: String,
    image_path: Option<String>,
    tab: Tab,
    loaded: Option<Loaded>,
    previous_threads: Option<ThreadsSnapshot>,
    loaded_at: Option<Instant>,
    /// The page to go back to.
    from_page: i32,
}

pub struct Pages {
    services: TableView,
    network: TableView,
    find: TableView,
    properties: TableView,
    stack: TableView,
    properties_state: Option<Properties>,
    tab_titles: Rc<VecModel<SharedString>>,
    actions: Rc<VecModel<SharedString>>,
    /// Bumped by every request; a result for an older generation is stale.
    generation: u64,
    loading: bool,
    services_loaded: Option<Instant>,
    services_loading: bool,
    services_data: Vec<ServiceDetails>,
    network_loading: bool,
    find_cancel: Option<Arc<AtomicBool>>,
}

impl Default for Pages {
    fn default() -> Self {
        Self {
            services: TableView::new(Some(Sort { column: 0, ascending: true })),
            network: TableView::new(Some(Sort { column: 0, ascending: true })),
            find: TableView::new(None),
            properties: TableView::new(None),
            stack: TableView::new(None),
            properties_state: None,
            tab_titles: Rc::new(VecModel::from(
                Tab::ALL.iter().map(|tab| SharedString::from(tab.title())).collect::<Vec<_>>(),
            )),
            actions: Rc::new(VecModel::default()),
            generation: 0,
            loading: false,
            services_loaded: None,
            services_loading: false,
            services_data: Vec::new(),
            network_loading: false,
            find_cancel: None,
        }
    }
}

impl Pages {
    /// Hands the table models to the window (once, at startup).
    pub fn bind(&mut self, ui: &MainWindow) {
        ui.set_services_columns(self.services.columns_model());
        ui.set_services_rows(self.services.rows_model());
        ui.set_network_columns(self.network.columns_model());
        ui.set_network_rows(self.network.rows_model());
        ui.set_find_columns(self.find.columns_model());
        ui.set_find_rows(self.find.rows_model());
        ui.set_properties_columns(self.properties.columns_model());
        ui.set_properties_rows(self.properties.rows_model());
        ui.set_stack_columns(self.stack.columns_model());
        ui.set_stack_rows(self.stack.rows_model());
        ui.set_properties_tabs(ModelRc::from(Rc::clone(&self.tab_titles)));
        ui.set_properties_actions(ModelRc::from(Rc::clone(&self.actions)));
        self.services.empty_text = "Loading services…".into();
        self.network.empty_text = "Loading connections…".into();
        self.find.empty_text = "Type part of a name and press Search. Every process is searched for windows, loaded DLLs and named handles (files, registry keys, mutexes, sections…). Run as administrator to include other users' and system processes. Or press Pick a window and click any window to find its process.".into();
        for id in [TableId::Services, TableId::Network, TableId::Find, TableId::Properties, TableId::Stack] {
            self.push(ui, id);
        }
    }

    fn view(&mut self, id: TableId) -> &mut TableView {
        match id {
            TableId::Services => &mut self.services,
            TableId::Network => &mut self.network,
            TableId::Find => &mut self.find,
            TableId::Properties => &mut self.properties,
            TableId::Stack => &mut self.stack,
        }
    }

    /// Sends a table's metadata (sort, selection, status) to the window.
    fn push(&mut self, ui: &MainWindow, id: TableId) {
        let meta = self.view(id).meta();
        match id {
            TableId::Services => ui.set_services_meta(meta),
            TableId::Network => ui.set_network_meta(meta),
            TableId::Find => ui.set_find_meta(meta),
            TableId::Properties => ui.set_properties_meta(meta),
            TableId::Stack => ui.set_stack_meta(meta),
        }
    }
}

/// Wires the page callbacks.
pub fn connect(ui: &MainWindow, app: &Rc<RefCell<App>>) {
    let event = handler(ui, app, |app, ui, (id, event, value): (TableId, TableEvent, i32)| {
        app.table_event(ui, id, event, value)
    });
    ui.on_table_event(move |id, kind, value| event((id, kind, value)));
    let filter = handler(ui, app, |app, ui, (id, text): (TableId, SharedString)| {
        app.pages.view(id).set_filter(&text);
        app.pages.push(ui, id);
    });
    ui.on_table_filter(move |id, text| filter((id, text)));
    ui.on_properties_tab_chosen(handler(ui, app, App::choose_properties_tab));
    let back = handler(ui, app, |app, ui, ()| app.close_properties(ui));
    ui.on_properties_back(move || back(()));
    ui.on_find_start(handler(ui, app, |app, ui, query: SharedString| app.start_find(ui, query.to_string())));
    let cancel = handler(ui, app, |app, _ui, ()| {
        if let Some(cancel) = &app.pages.find_cancel {
            cancel.store(true, Ordering::Relaxed);
        }
    });
    ui.on_find_cancel(move || cancel(()));
    let pick = handler(ui, app, |app, ui, ()| app.start_window_pick(ui));
    ui.on_pick_window(move || pick(()));
}

impl App {
    /// Shows `page` and does what showing it does (the Slint callback can't reach the app while
    /// an app method is running).
    pub(crate) fn switch_page(&mut self, ui: &MainWindow, page: i32) {
        ui.invoke_show_page(page);
        self.page_shown(ui, page);
    }

    fn table_event(&mut self, ui: &MainWindow, id: TableId, event: TableEvent, value: i32) {
        match event {
            TableEvent::Sort => {
                if let Ok(column) = usize::try_from(value) {
                    self.pages.view(id).sort_by(column);
                }
            }
            TableEvent::Click => self.pages.view(id).select_index(value),
            TableEvent::Move => self.pages.view(id).move_selection(value),
            TableEvent::Activate => {
                self.pages.view(id).select_index(value);
                self.table_activated(ui, id);
            }
            TableEvent::Action => self.table_action(ui, id, value),
        }
        self.pages.push(ui, id);
    }

    /// Double-click or Enter on a row.
    fn table_activated(&mut self, ui: &MainWindow, id: TableId) {
        match id {
            TableId::Services | TableId::Network | TableId::Find => {
                if let Some(pid) = self.selected_pid(id) {
                    self.go_to_process(ui, pid);
                }
            }
            TableId::Properties if self.pages.properties_state.as_ref().is_some_and(|p| p.tab == Tab::Threads) => {
                self.show_stack(ui);
            }
            _ => {}
        }
    }

    /// The PID column of the selected row in the Services, Network or Find table.
    fn selected_pid(&mut self, id: TableId) -> Option<u32> {
        let column = match id {
            TableId::Services => 4,
            TableId::Network | TableId::Find => 1,
            _ => return None,
        };
        let row = self.pages.view(id).selected_row()?;
        match row.cells.get(column)?.key {
            ferrotop_core::table::SortKey::Number(pid) if pid > 0.0 => Some(pid as u32),
            _ => None,
        }
    }

    fn table_action(&mut self, ui: &MainWindow, id: TableId, action: i32) {
        match (id, action) {
            (TableId::Services, 0..=2) => {
                let Some(name) = self.pages.services.selected_row().map(|row| row.cells[0].text.clone()) else {
                    return;
                };
                match action {
                    0 => self.service_command(ui, name, ServiceCommand::Start),
                    1 => self.confirm_service(ui, name, ServiceCommand::Stop),
                    _ => self.confirm_service(ui, name, ServiceCommand::Restart),
                }
            }
            (TableId::Services | TableId::Network | TableId::Find, _) => {
                if let Some(pid) = self.selected_pid(id) {
                    self.go_to_process(ui, pid);
                }
            }
            (TableId::Properties, _) => {
                let tab = self.pages.properties_state.as_ref().map(|p| p.tab);
                match tab {
                    Some(Tab::Threads) => self.show_stack(ui),
                    Some(Tab::Modules) => {
                        if let Some(path) = self.pages.properties.selected_row().map(|r| r.cells[5].text.clone())
                            && let Err(error) = ferrotop_sys::control::open_file_location(&path)
                        {
                            self.show_message(ui, format!("Couldn't open the file location: {error}"));
                        }
                    }
                    // The Windows tab's row IDs are window handles.
                    Some(Tab::Windows) => {
                        if let Some(handle) = self.pages.properties.selected_row().map(|r| r.id as usize)
                            && let Err(error) = ferrotop_sys::desktop::bring_to_front(handle)
                        {
                            self.show_message(ui, format!("Couldn't bring the window to the front: {error}"));
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    // ----- Process properties -----

    pub(crate) fn open_properties_at(&mut self, ui: &MainWindow, index: i32) {
        self.select_index(ui, index);
        if let Some(key) = self.selected {
            self.open_properties(ui, key);
        }
    }

    pub(crate) fn open_properties(&mut self, ui: &MainWindow, key: ProcessKey) {
        let Some(item) = self.provider.get(&key) else { return };
        if ferrotop_core::provider::is_pseudo_process(key.pid) {
            return self.show_message(ui, "Interrupts is a counter, not a process".into());
        }
        let from_page = if self.page == PROPERTIES_PAGE { 0 } else { self.page };
        self.pages.properties_state = Some(Properties {
            key,
            name: item.name.clone(),
            image_path: item.details.as_ref().and_then(|d| d.image_path.clone()),
            tab: Tab::General,
            loaded: None,
            previous_threads: None,
            loaded_at: None,
            from_page,
        });
        ui.set_properties_icon(self.icon_for(item));
        ui.set_properties_title(format!("{} ({})", item.name, key.pid).into());
        let path = item.details.as_ref().and_then(|d| d.image_path.clone()).unwrap_or_default();
        let signer = item.file.as_ref().map(|f| format!("  ·  {}", f.signer())).unwrap_or_default();
        ui.set_properties_subtitle(format!("{path}{signer}").into());
        self.pages.properties.selected = None;
        self.pages.properties.set_filter("");
        self.switch_page(ui, PROPERTIES_PAGE);
        self.show_properties_tab(ui, Tab::General);
    }

    fn close_properties(&mut self, ui: &MainWindow) {
        let back = self.pages.properties_state.take().map_or(0, |p| p.from_page);
        self.pages.generation += 1;
        self.switch_page(ui, back);
    }

    fn choose_properties_tab(&mut self, ui: &MainWindow, index: i32) {
        if let Some(&tab) = usize::try_from(index).ok().and_then(|i| Tab::ALL.get(i)) {
            self.show_properties_tab(ui, tab);
        }
    }

    fn show_properties_tab(&mut self, ui: &MainWindow, tab: Tab) {
        let Some(state) = self.pages.properties_state.as_mut() else { return };
        if state.tab != tab {
            state.tab = tab;
            state.loaded = None;
            state.previous_threads = None;
            self.pages.properties.selected = None;
        }
        ui.set_properties_tab(Tab::ALL.iter().position(|t| *t == tab).unwrap_or(0) as i32);
        let actions: &[&str] = match tab {
            Tab::Threads => &["Stack"],
            Tab::Modules => &["Open file location"],
            Tab::Windows => &["Bring to front"],
            _ => &[],
        };
        crate::sync_model(&self.pages.actions, actions.iter().map(|a| SharedString::from(*a)).collect());
        self.pages.properties.clear("Loading…");
        self.pages.properties.status = String::new();
        self.pages.push(ui, TableId::Properties);
        ui.invoke_focus_table(TableId::Properties);
        // A different tab: start now even if another load is still running (its result will be
        // discarded as stale).
        self.pages.loading = false;
        self.load_properties();
    }

    /// Starts (re)loading the open tab on a worker thread, unless a load is already running.
    fn load_properties(&mut self) {
        let Some(state) = self.pages.properties_state.as_ref() else { return };
        if self.pages.loading {
            return;
        }
        let (tab, pid, path) = (state.tab, state.key.pid, state.image_path.clone());
        self.pages.generation += 1;
        self.pages.loading = true;
        let generation = self.pages.generation;
        background(
            move || properties::load(tab, pid, path.as_deref()),
            move |app, ui, result| app.properties_loaded(ui, generation, tab, result),
        );
    }

    fn properties_loaded(&mut self, ui: &MainWindow, generation: u64, tab: Tab, result: Result<Loaded, String>) {
        if generation != self.pages.generation {
            return;
        }
        self.pages.loading = false;
        if self.pages.properties_state.as_ref().is_none_or(|s| s.tab != tab) {
            return;
        }
        match result {
            Ok(loaded) => {
                let state = self.pages.properties_state.as_mut().expect("checked above");
                if let (Loaded::Threads(_), Some(Loaded::Threads(previous))) = (&loaded, state.loaded.take()) {
                    state.previous_threads = Some(previous);
                }
                state.loaded = Some(loaded);
                state.loaded_at = Some(Instant::now());
                self.retabulate_properties();
            }
            Err(message) => {
                self.pages.properties.clear(&message);
            }
        }
        self.pages.push(ui, TableId::Properties);
    }

    /// Rebuilds the open tab's table from its loaded data and the live process list.
    fn retabulate_properties(&mut self) {
        let names = self.process_names();
        let cpu_count = self.provider.stats().cpu_count;
        let Some(state) = self.pages.properties_state.as_ref() else { return };
        let Some(loaded) = state.loaded.as_ref() else { return };
        let item = self.provider.get(&state.key);
        let context = Context { item, names: &names, previous_threads: state.previous_threads.as_ref(), cpu_count };
        let table = properties::table(loaded, context);
        let count = table.rows.len();
        let tab = state.tab;
        let exited = item.is_none_or(|i| i.state == ProcessState::Removed);
        self.pages.properties.empty_text = match tab {
            Tab::Services => "This process doesn't host any services.".into(),
            Tab::Windows => "This process has no windows on this desktop.".into(),
            Tab::General => String::new(),
            _ => "Nothing to show.".into(),
        };
        self.pages.properties.status = match (exited, tab) {
            (true, _) => "The process has exited".into(),
            (false, Tab::General) => String::new(),
            (false, _) => tab.unit(count),
        };
        self.pages.properties.set_table(table);
    }

    // ----- Thread stacks -----

    fn show_stack(&mut self, ui: &MainWindow) {
        let Some(state) = self.pages.properties_state.as_ref() else { return };
        let Some(tid) = self.pages.properties.selected_row().map(|row| row.id as u32) else {
            return self.show_message(ui, "Select a thread first".into());
        };
        let (pid, name) = (state.key.pid, state.name.clone());
        ui.set_stack_title(format!("Thread {tid} of {name} ({pid})").into());
        self.pages.stack.clear("Walking the stack…");
        self.pages.push(ui, TableId::Stack);
        ui.set_stack_open(true);
        background(
            move || properties::load_stack(pid, tid),
            move |app, ui, result| {
                match result {
                    Ok(frames) => app.pages.stack.set_table(properties::stack_table(&frames)),
                    Err(message) => app.pages.stack.clear(&message),
                }
                app.pages.push(ui, TableId::Stack);
            },
        );
    }

    // ----- Periodic refresh -----

    /// Called after every sample.
    pub(crate) fn refresh_pages(&mut self, ui: &MainWindow) {
        match self.page {
            2 if self.pages.services_loaded.is_none_or(|at| at.elapsed() > SERVICES_REFRESH) => self.load_services(ui),
            2 => {
                // Host process names can change between full reloads.
                self.tabulate_services(ui);
            }
            3 => self.load_network(ui),
            PROPERTIES_PAGE => {
                let Some(state) = self.pages.properties_state.as_ref() else { return };
                match state.tab {
                    Tab::Threads => self.load_properties(),
                    Tab::General if state.loaded_at.is_some_and(|at| at.elapsed() > GENERAL_REFRESH) => {
                        self.load_properties()
                    }
                    Tab::General => {
                        self.retabulate_properties();
                        self.pages.push(ui, TableId::Properties);
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    // ----- Services -----

    pub(crate) fn load_services(&mut self, ui: &MainWindow) {
        if self.pages.services_loading {
            return;
        }
        self.pages.services_loading = true;
        let _ = ui;
        background(ferrotop_sys::service::all_services, |app, ui, result| {
            app.pages.services_loading = false;
            app.pages.services_loaded = Some(Instant::now());
            match result {
                Ok(services) => {
                    app.pages.services_data = services;
                    app.tabulate_services(ui);
                }
                Err(error) => {
                    app.pages.services.clear(&format!("Couldn't list services: {error}"));
                    app.pages.push(ui, TableId::Services);
                }
            }
        });
    }

    fn tabulate_services(&mut self, ui: &MainWindow) {
        let names = self.process_names();
        let table = services_table(&self.pages.services_data, &names);
        let running = self.pages.services_data.iter().filter(|s| s.state == 4).count();
        self.pages.services.status =
            format!("{} services and drivers, {running} running", self.pages.services_data.len());
        self.pages.services.empty_text = "No services".into();
        self.pages.services.set_table(table);
        self.pages.push(ui, TableId::Services);
    }

    fn confirm_service(&mut self, ui: &MainWindow, name: String, command: ServiceCommand) {
        let verb = if command == ServiceCommand::Stop { "Stop" } else { "Restart" };
        self.confirm(
            ui,
            format!("{verb} {name}?"),
            "Programs and other services that depend on it may stop working until it's running again.".into(),
            verb,
            crate::actions::Pending::Service(name, command),
        );
    }

    pub(crate) fn service_command(&mut self, ui: &MainWindow, name: String, command: ServiceCommand) {
        self.show_message(ui, format!("{} {name}…", command.progress()));
        background(
            move || {
                use ferrotop_sys::service::{start, stop};
                let run = || match command {
                    ServiceCommand::Start => start(&name),
                    ServiceCommand::Stop => stop(&name),
                    ServiceCommand::Restart => {
                        stop(&name)?;
                        // Give it up to ten seconds to stop before starting it again.
                        for _ in 0..40 {
                            std::thread::sleep(Duration::from_millis(250));
                            if start(&name).is_ok() {
                                return Ok(());
                            }
                        }
                        start(&name)
                    }
                };
                let result = run();
                result.map(|()| name.clone()).map_err(|e| (name, e))
            },
            |app, ui, result| {
                let message = match result {
                    Ok(name) => format!("{name}: done"),
                    Err((name, e)) => format!("{name}: {}", ferrotop_core::properties::describe_error(&e)),
                };
                app.show_message(ui, message);
                app.pages.services_loaded = None;
                app.load_services(ui);
            },
        );
    }

    // ----- Network -----

    pub(crate) fn load_network(&mut self, ui: &MainWindow) {
        if self.pages.network_loading {
            return;
        }
        self.pages.network_loading = true;
        let _ = ui;
        background(ferrotop_sys::connections::connections, |app, ui, result| {
            app.pages.network_loading = false;
            match result {
                Ok(connections) => {
                    let names = app.process_names();
                    let established = connections.iter().filter(|c| c.state == 5).count();
                    let listening = connections.iter().filter(|c| c.state == 2).count();
                    app.pages.network.status =
                        format!("{} endpoints · {established} established · {listening} listening", connections.len());
                    app.pages.network.empty_text = "No connections".into();
                    app.pages.network.set_table(connections_table(&connections, &names));
                }
                Err(error) => app.pages.network.clear(&format!("Couldn't list connections: {error}")),
            }
            app.pages.push(ui, TableId::Network);
        });
    }

    // ----- Pick a window -----

    fn start_window_pick(&mut self, ui: &MainWindow) {
        let picking = desktop::pick_window(|picked| {
            // This runs inside the mouse hook: finish from the event loop instead.
            let _ = slint::invoke_from_event_loop(move || crate::with_app(|app, ui| app.window_picked(ui, picked)));
        });
        match picking {
            Ok(()) => self.show_message(ui, "Click any window to find its process (right-click to cancel)".into()),
            Err(error) => self.show_message(ui, format!("Couldn't watch for a click: {error}")),
        }
    }

    fn window_picked(&mut self, ui: &MainWindow, picked: Option<WindowInfo>) {
        let Some(window) = picked else { return self.show_message(ui, "Cancelled".into()) };
        self.go_to_process(ui, window.pid);
        let name = self.provider.items().find(|item| item.key.pid == window.pid).map(|item| item.name.clone());
        let title = if window.title.is_empty() { &window.class } else { &window.title };
        self.show_message(ui, format!("“{title}” belongs to {} (PID {})", name.unwrap_or_default(), window.pid));
    }

    // ----- Find handles, DLLs or windows -----

    fn start_find(&mut self, ui: &MainWindow, query: String) {
        if self.pages.find_cancel.is_some() {
            return;
        }
        if query.trim().len() < 2 {
            return self.show_message(ui, "Type at least two characters to search for".into());
        }
        let processes: Vec<(u32, String)> = self
            .provider
            .items()
            .filter(|item| {
                item.state != ProcessState::Removed && !ferrotop_core::provider::is_pseudo_process(item.key.pid)
            })
            .map(|item| (item.key.pid, item.name.clone()))
            .collect();
        let cancel = Arc::new(AtomicBool::new(false));
        self.pages.find_cancel = Some(Arc::clone(&cancel));
        ui.set_find_running(true);
        self.pages.find.clear("Searching…");
        self.pages.find.status = format!("Searching {} processes…", processes.len());
        self.pages.push(ui, TableId::Find);
        if self.page != FIND_PAGE {
            self.switch_page(ui, FIND_PAGE);
        }
        background(
            move || {
                let mut last_report = Instant::now();
                find::search(&query, &processes, |done, total| {
                    if last_report.elapsed() > Duration::from_millis(200) {
                        last_report = Instant::now();
                        let _ = slint::invoke_from_event_loop(move || {
                            crate::with_app(|app, ui| {
                                app.pages.find.status = format!("Searched {done} of {total} processes…");
                                app.pages.push(ui, TableId::Find);
                            });
                        });
                    }
                    !cancel.load(Ordering::Relaxed)
                })
            },
            |app, ui, matches| {
                let cancelled = app.pages.find_cancel.take().is_some_and(|c| c.load(Ordering::Relaxed));
                ui.set_find_running(false);
                app.pages.find.status = format!(
                    "{} match{}{}",
                    matches.len(),
                    if matches.len() == 1 { "" } else { "es" },
                    if cancelled { " (cancelled)" } else { "" }
                );
                app.pages.find.empty_text = "Nothing found.".into();
                app.pages.find.set_table(find::matches_table(&matches));
                app.pages.push(ui, TableId::Find);
            },
        );
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceCommand {
    Start,
    Stop,
    Restart,
}

impl ServiceCommand {
    fn progress(self) -> &'static str {
        match self {
            ServiceCommand::Start => "Starting",
            ServiceCommand::Stop => "Stopping",
            ServiceCommand::Restart => "Restarting",
        }
    }
}
