//! Process actions: end (and end tree), suspend and resume, priority, affinity, dump files, plus
//! the Run box. Anything that can't be undone, or that can hang the desktop, asks first.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use ferrotop_core::format;
use ferrotop_core::provider::{ProcessKey, ProcessState, is_pseudo_process};
use ferrotop_sys::control::{self, PRIORITY_CLASSES};
use slint::{Model, ModelRc, SharedString, VecModel};

use crate::pages::ServiceCommand;
use crate::{AffinityCpu, App, MainWindow, ProcessAction, background, handler};

/// Processes that freeze the desktop (or Ferrotop itself) when suspended. Windows' critical
/// processes are refused separately, via the kernel's own flag.
const NEVER_SUSPEND: &[&str] = &["dwm.exe", "csrss.exe", "winlogon.exe"];

/// An action waiting for the confirmation dialog.
pub enum Pending {
    Terminate(ProcessKey),
    TerminateTree(Vec<ProcessKey>),
    Suspend(ProcessKey),
    Realtime(ProcessKey),
    Service(String, ServiceCommand),
}

#[derive(Default)]
pub struct Actions {
    pending: Option<Pending>,
    /// The process the open right-click menu acts on, fixed when it opened: the list can
    /// re-sort under the menu, so its row index can't be trusted by the time an item is picked.
    menu_for: Option<ProcessKey>,
    affinity_for: Option<ProcessKey>,
    affinity: Rc<VecModel<AffinityCpu>>,
}

impl Actions {
    pub fn bind(&mut self, ui: &MainWindow) {
        ui.set_affinity_cpus(ModelRc::from(Rc::clone(&self.affinity)));
    }
}

pub fn connect(ui: &MainWindow, app: &Rc<RefCell<App>>) {
    ui.on_process_menu_requested(handler(ui, app, App::prepare_process_menu));
    let menu = handler(ui, app, |app, ui, (action, value): (ProcessAction, i32)| app.process_menu(ui, action, value));
    ui.on_process_menu(move |action, value| menu((action, value)));
    let request = handler(ui, app, |app, ui, ()| app.request_terminate(ui));
    ui.on_request_terminate(move || request(()));
    let confirmed = handler(ui, app, |app, ui, ()| app.confirmed(ui));
    ui.on_confirm_accepted(move || confirmed(()));
    let run =
        handler(ui, app, |app, ui, (command, elevated): (SharedString, bool)| app.run_task(ui, &command, elevated));
    ui.on_run_task(move |command, elevated| run((command, elevated)));
    let toggled = handler(ui, app, |app, _ui, (index, checked): (i32, bool)| {
        let model = &app.actions.affinity;
        if let Some(mut cpu) = usize::try_from(index).ok().and_then(|i| model.row_data(i)) {
            cpu.checked = checked;
            model.set_row_data(index as usize, cpu);
        }
    });
    ui.on_affinity_toggled(move |index, checked| toggled((index, checked)));
    ui.on_affinity_all(handler(ui, app, |app, _ui, checked: bool| {
        let model = &app.actions.affinity;
        for i in 0..model.row_count() {
            if let Some(mut cpu) = model.row_data(i) {
                cpu.checked = checked;
                model.set_row_data(i, cpu);
            }
        }
    }));
    let apply = handler(ui, app, |app, ui, ()| app.apply_affinity(ui));
    ui.on_affinity_apply(move || apply(()));
}

impl App {
    pub(crate) fn confirm(&mut self, ui: &MainWindow, title: String, text: String, button: &str, pending: Pending) {
        ui.set_confirm_title(title.into());
        ui.set_confirm_text(text.into());
        ui.set_confirm_button(button.into());
        self.actions.pending = Some(pending);
        ui.set_confirm_open(true);
    }

    fn confirmed(&mut self, ui: &MainWindow) {
        match self.actions.pending.take() {
            Some(Pending::Terminate(key)) => self.terminate(ui, &[key]),
            Some(Pending::TerminateTree(keys)) => self.terminate(ui, &keys),
            Some(Pending::Suspend(key)) => self.suspend(ui, key),
            Some(Pending::Realtime(key)) => self.set_priority(ui, key, 5),
            Some(Pending::Service(name, command)) => self.service_command(ui, name, command),
            None => {}
        }
    }

    /// A live, real process (not the idle process, System, or the Interrupts row).
    fn actionable(&self, key: ProcessKey) -> Option<String> {
        let item = self.provider.get(&key)?;
        (item.state != ProcessState::Removed && key.pid > 4 && !is_pseudo_process(key.pid)).then(|| item.name.clone())
    }

    /// A row was right-clicked: select it and fix the menu to that process.
    fn prepare_process_menu(&mut self, ui: &MainWindow, index: i32) {
        self.select_index(ui, index);
        self.actions.menu_for = self.selected;
        let item = self.selected.and_then(|key| self.provider.get(&key));
        ui.set_menu_title(item.map_or_else(String::new, |item| format!("{} ({})", item.name, item.key.pid)).into());
        ui.set_menu_suspended(item.is_some_and(|item| item.suspended));
    }

    fn process_menu(&mut self, ui: &MainWindow, action: ProcessAction, value: i32) {
        let Some(key) = self.actions.menu_for.take() else { return };
        if self.provider.get(&key).is_none_or(|item| item.state == ProcessState::Removed) {
            return self.show_message(ui, format!("PID {} has exited", key.pid));
        }
        // Keep the selection on the process the menu was for.
        self.selected = Some(key);
        self.rebuild_rows(ui);
        self.update_details(ui);
        match action {
            ProcessAction::Properties => self.open_properties(ui, key),
            ProcessAction::OpenLocation => {
                let path = self.provider.get(&key).and_then(|item| item.details.as_ref()?.image_path.clone());
                match path {
                    Some(path) => {
                        if let Err(error) = control::open_file_location(&path) {
                            self.show_message(ui, format!("Couldn't open the file location: {error}"));
                        }
                    }
                    None => self.show_message(ui, "The image path isn't known (run as administrator?)".into()),
                }
            }
            ProcessAction::End => self.request_terminate(ui),
            ProcessAction::EndTree => self.request_terminate_tree(ui, key),
            ProcessAction::Suspend => self.request_suspend(ui, key),
            ProcessAction::Resume => self.resume(ui, key),
            ProcessAction::Priority => match value {
                5 => {
                    let Some(name) = self.actionable(key) else { return };
                    self.confirm(
                        ui,
                        format!("Give {name} realtime priority?"),
                        "A realtime process runs ahead of the mouse, keyboard and disk I/O. If it gets busy, the whole machine can stop responding.".into(),
                        "Set realtime",
                        Pending::Realtime(key),
                    );
                }
                class @ 0..=4 => self.set_priority(ui, key, class as usize),
                _ => {}
            },
            ProcessAction::Affinity => self.open_affinity(ui, key),
            ProcessAction::Dump => self.create_dump(ui, key),
        }
    }

    fn request_terminate(&mut self, ui: &MainWindow) {
        let Some(key) = self.selected else { return };
        let Some(name) = self.actionable(key) else { return };
        if control::is_critical(key.pid) == Some(true) {
            return self
                .show_message(ui, format!("{name} is a critical system process: ending it would crash Windows"));
        }
        self.confirm(
            ui,
            format!("End {name}?"),
            format!("PID {} will be terminated immediately. Any unsaved data in it will be lost.", key.pid),
            "End process",
            Pending::Terminate(key),
        );
    }

    /// The process and its descendants, children before parents.
    fn process_tree(&self, root: ProcessKey) -> Vec<ProcessKey> {
        let mut order = vec![root];
        let mut next = 0;
        while next < order.len() && order.len() < 4096 {
            let parent = order[next];
            next += 1;
            for item in self.provider.items() {
                // Same rule as the tree view: a child is created after its parent.
                if item.parent_pid == parent.pid
                    && item.key != parent
                    && item.key.create_time >= parent.create_time
                    && item.state != ProcessState::Removed
                    && !order.contains(&item.key)
                {
                    order.push(item.key);
                }
            }
        }
        order.reverse();
        order
    }

    fn request_terminate_tree(&mut self, ui: &MainWindow, key: ProcessKey) {
        let Some(name) = self.actionable(key) else { return };
        let tree = self.process_tree(key);
        if let Some(critical) = tree.iter().find(|k| control::is_critical(k.pid) == Some(true)) {
            let critical = self.provider.get(critical).map_or_else(String::new, |i| i.name.clone());
            return self.show_message(ui, format!("The tree includes {critical}, a critical system process"));
        }
        let others = tree.len() - 1;
        self.confirm(
            ui,
            format!("End {name} and its descendants?"),
            format!(
                "{name} (PID {}) and {others} process{} started from it will be terminated immediately. Unsaved data in them will be lost.",
                key.pid,
                if others == 1 { "" } else { "es" }
            ),
            "End process tree",
            Pending::TerminateTree(tree),
        );
    }

    fn terminate(&mut self, ui: &MainWindow, keys: &[ProcessKey]) {
        let mut ended = 0;
        let mut failures = Vec::new();
        for key in keys {
            let name = self.provider.get(key).map_or_else(|| "process".to_owned(), |i| i.name.clone());
            match ferrotop_sys::process::terminate(key.pid, key.create_time) {
                Ok(()) => ended += 1,
                // Already gone: that's what we wanted.
                Err(ferrotop_sys::Error::ProcessGone) => ended += 1,
                Err(error) => failures.push(format!("{name}: {}", ferrotop_core::properties::describe_error(&error))),
            }
        }
        let message = match (keys, failures.as_slice()) {
            ([key], []) => format!("Ended PID {}", key.pid),
            (_, []) => format!("Ended {ended} processes"),
            (_, [failure, ..]) => format!("Couldn't end {failure}"),
        };
        self.show_message(ui, message);
        if let Some(sampler) = &self.sampler {
            sampler.refresh_now();
        }
    }

    fn request_suspend(&mut self, ui: &MainWindow, key: ProcessKey) {
        let Some(name) = self.actionable(key) else { return };
        if key.pid == std::process::id() || NEVER_SUSPEND.iter().any(|n| n.eq_ignore_ascii_case(&name)) {
            return self.show_message(ui, format!("Suspending {name} would freeze the desktop"));
        }
        if control::is_critical(key.pid) == Some(true) {
            return self.show_message(ui, format!("{name} is a critical system process"));
        }
        self.confirm(
            ui,
            format!("Suspend {name}?"),
            "Every thread stops until you resume it (right-click it again). Its windows won't respond in the meantime, and programs waiting on it may hang too.".into(),
            "Suspend",
            Pending::Suspend(key),
        );
    }

    fn suspend(&mut self, ui: &MainWindow, key: ProcessKey) {
        let message = match control::suspend(key.pid, key.create_time) {
            Ok(()) => format!("Suspended PID {}", key.pid),
            Err(error) => format!("Couldn't suspend: {}", ferrotop_core::properties::describe_error(&error)),
        };
        self.show_message(ui, message);
    }

    fn resume(&mut self, ui: &MainWindow, key: ProcessKey) {
        if self.actionable(key).is_none() {
            return;
        }
        let message = match control::resume(key.pid, key.create_time) {
            Ok(()) => format!("Resumed PID {}", key.pid),
            Err(error) => format!("Couldn't resume: {}", ferrotop_core::properties::describe_error(&error)),
        };
        self.show_message(ui, message);
    }

    fn set_priority(&mut self, ui: &MainWindow, key: ProcessKey, index: usize) {
        if self.actionable(key).is_none() {
            return;
        }
        let Some(&(class, label)) = PRIORITY_CLASSES.get(index) else { return };
        let message = match control::set_priority_class(key.pid, key.create_time, class) {
            Ok(()) => format!("Priority of PID {} set to {label}", key.pid),
            Err(error) => format!("Couldn't change priority: {}", ferrotop_core::properties::describe_error(&error)),
        };
        self.show_message(ui, message);
    }

    fn open_affinity(&mut self, ui: &MainWindow, key: ProcessKey) {
        let Some(name) = self.actionable(key) else { return };
        let (mine, system) = match control::affinity(key.pid) {
            Ok(masks) => masks,
            Err(error) => {
                return self.show_message(
                    ui,
                    format!("Couldn't read affinity: {}", ferrotop_core::properties::describe_error(&error)),
                );
            }
        };
        let cpus: Vec<AffinityCpu> = (0..64)
            .filter(|bit| system & (1 << bit) != 0)
            .map(|bit| AffinityCpu { label: format!("CPU {bit}").into(), checked: mine & (1 << bit) != 0 })
            .collect();
        crate::sync_model(&self.actions.affinity, cpus);
        self.actions.affinity_for = Some(key);
        let groups = if self.provider.stats().cpu_count > 64 { "  (this processor group)" } else { "" };
        ui.set_affinity_title(format!("Processors {name} may run on{groups}").into());
        ui.set_affinity_open(true);
    }

    fn apply_affinity(&mut self, ui: &MainWindow) {
        let Some(key) = self.actions.affinity_for.take() else { return };
        let mask = self
            .actions
            .affinity
            .iter()
            .filter(|cpu| cpu.checked)
            .filter_map(|cpu| cpu.label.strip_prefix("CPU ")?.parse::<u32>().ok())
            .fold(0u64, |mask, bit| mask | (1 << bit));
        if mask == 0 {
            return self.show_message(ui, "Pick at least one processor".into());
        }
        let message = match control::set_affinity(key.pid, key.create_time, mask) {
            Ok(()) => format!("Affinity of PID {} set to {} processors", key.pid, mask.count_ones()),
            Err(error) => format!("Couldn't set affinity: {}", ferrotop_core::properties::describe_error(&error)),
        };
        self.show_message(ui, message);
    }

    fn create_dump(&mut self, ui: &MainWindow, key: ProcessKey) {
        let Some(name) = self.actionable(key) else { return };
        let Some(folder) =
            std::env::var_os("LOCALAPPDATA").map(|dir| PathBuf::from(dir).join("Ferrotop").join("Dumps"))
        else {
            return self.show_message(ui, "LOCALAPPDATA isn't set".into());
        };
        if let Err(error) = std::fs::create_dir_all(&folder) {
            return self.show_message(ui, format!("Couldn't create {}: {error}", folder.display()));
        }
        let stem = name
            .trim_end_matches(".exe")
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .collect::<String>();
        let path = folder.join(format!("{stem}-{}-{}.dmp", key.pid, format::timestamp_now()));
        self.show_message(ui, format!("Writing a dump of {name}…"));
        background(
            move || {
                let path_text = path.to_string_lossy().into_owned();
                control::create_dump(key.pid, key.create_time, &path_text)
                    .map(|()| (path_text.clone(), std::fs::metadata(&path).map_or(0, |m| m.len())))
            },
            |app, ui, result| {
                let message = match result {
                    Ok((path, size)) => {
                        // Point Explorer at it; the dump holds the process's memory, so it stays
                        // in the user's own profile.
                        let _ = control::open_file_location(&path);
                        format!("Dump written ({}): {path}", format::bytes(size))
                    }
                    Err(error) => {
                        format!("Couldn't write the dump: {}", ferrotop_core::properties::describe_error(&error))
                    }
                };
                app.show_message(ui, message);
            },
        );
    }

    fn run_task(&mut self, ui: &MainWindow, command: &str, elevated: bool) {
        let command = command.trim();
        if command.is_empty() {
            return;
        }
        // Already elevated: a normal launch inherits that.
        let elevate = elevated && !ferrotop_sys::system::is_elevated();
        let message = match control::run(command, elevate) {
            Ok(()) => format!("Started {command}"),
            Err(error) => format!("Couldn't start {command}: {error}"),
        };
        self.show_message(ui, message);
    }
}
