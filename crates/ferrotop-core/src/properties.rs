//! The process properties view: what each tab loads (on a worker thread, since some queries are
//! slow) and how the loaded data becomes a [`Table`] (on the UI thread, with live context).

use std::collections::HashMap;
use std::time::Instant;

use ferrotop_sys::control;
use ferrotop_sys::environment;
use ferrotop_sys::handles::{self, HandleInfo, HandleTarget};
use ferrotop_sys::memory::{self, MemoryRegion};
use ferrotop_sys::modules::{self, ModuleInfo, symbolize};
use ferrotop_sys::service::{self, ServiceDetails};
use ferrotop_sys::signature::Signature;
use ferrotop_sys::stack::{self, StackFrame};
use ferrotop_sys::threads::{self, ThreadInfo};
use ferrotop_sys::token::{self, TokenDetails, account_name};
use ferrotop_sys::version::{VersionInfo, version_info};

use crate::format;
use crate::names;
use crate::provider::ProcessItem;
use crate::table::{Cell, Column, Table, Tone};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    General,
    Threads,
    Modules,
    Memory,
    Handles,
    Environment,
    Token,
    Services,
}

impl Tab {
    pub const ALL: [Tab; 8] = [
        Tab::General,
        Tab::Threads,
        Tab::Modules,
        Tab::Memory,
        Tab::Handles,
        Tab::Environment,
        Tab::Token,
        Tab::Services,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Tab::General => "General",
            Tab::Threads => "Threads",
            Tab::Modules => "Modules",
            Tab::Memory => "Memory",
            Tab::Handles => "Handles",
            Tab::Environment => "Environment",
            Tab::Token => "Token",
            Tab::Services => "Services",
        }
    }

    /// Tabs that change from second to second and so refresh with every sample.
    pub fn is_live(self) -> bool {
        matches!(self, Tab::General | Tab::Threads)
    }

    /// What one row of the tab is, for counts like "14 threads".
    pub fn unit(self, count: usize) -> String {
        let (one, many) = match self {
            Tab::General => ("property", "properties"),
            Tab::Threads => ("thread", "threads"),
            Tab::Modules => ("module", "modules"),
            Tab::Memory => ("region", "regions"),
            Tab::Handles => ("handle", "handles"),
            Tab::Environment => ("variable", "variables"),
            Tab::Token => ("entry", "entries"),
            Tab::Services => ("service", "services"),
        };
        format!("{} {}", crate::format::count(count as u64), if count == 1 { one } else { many })
    }
}

/// Facts for the General tab that aren't in the process list.
#[derive(Debug, Clone, Default)]
pub struct GeneralInfo {
    pub command_line: Option<String>,
    pub priority_class: Option<u32>,
    pub affinity: Option<(u64, u64)>,
    pub critical: Option<bool>,
    pub integrity: Option<String>,
    pub signature: Option<Signature>,
}

#[derive(Debug, Clone)]
pub struct ThreadsSnapshot {
    pub threads: Vec<ThreadInfo>,
    /// Win32 start address of each thread, as `module+offset`.
    pub start: HashMap<u32, String>,
    pub taken_at: Instant,
}

#[derive(Debug, Clone)]
pub struct TokenSnapshot {
    pub details: TokenDetails,
    pub user: String,
    pub groups: Vec<String>,
}

/// One tab's raw data.
#[derive(Debug, Clone)]
pub enum Loaded {
    General(GeneralInfo),
    Threads(ThreadsSnapshot),
    Modules(Vec<(ModuleInfo, VersionInfo)>),
    Memory(Vec<MemoryRegion>),
    Handles(Vec<HandleInfo>),
    Environment(Vec<(String, String)>),
    Token(TokenSnapshot),
    Services(Vec<ServiceDetails>),
}

/// A friendly explanation for a failed query.
pub fn describe_error(error: &ferrotop_sys::Error) -> String {
    match error {
        e if e.is_access_denied() => "Access denied. Run Ferrotop as administrator to see this.".to_owned(),
        ferrotop_sys::Error::ProcessGone => "The process has exited.".to_owned(),
        e => e.to_string(),
    }
}

/// Loads one tab for process `pid`. Runs on a worker thread.
pub fn load(tab: Tab, pid: u32, image_path: Option<&str>) -> Result<Loaded, String> {
    let fail = |e: ferrotop_sys::Error| describe_error(&e);
    Ok(match tab {
        Tab::General => Loaded::General(GeneralInfo {
            command_line: ferrotop_sys::process::command_line(pid).ok(),
            priority_class: control::priority_class(pid).ok(),
            affinity: control::affinity(pid).ok(),
            critical: control::is_critical(pid),
            integrity: token::token_details(pid)
                .ok()
                .and_then(|t| t.integrity)
                .map(|sid| names::integrity(&sid.to_string())),
            signature: image_path.map(ferrotop_sys::signature::verify),
        }),
        Tab::Threads => {
            let threads = threads::threads(pid).map_err(fail)?.ok_or("The process has exited.")?;
            let modules = modules::modules(pid).unwrap_or_default();
            let start = threads
                .iter()
                .filter_map(|t| Some((t.tid, symbolize(&modules, threads::win32_start_address(pid, t.tid).ok()?))))
                .collect();
            Loaded::Threads(ThreadsSnapshot { threads, start, taken_at: Instant::now() })
        }
        Tab::Modules => {
            let mut versions: HashMap<String, VersionInfo> = HashMap::new();
            Loaded::Modules(
                modules::modules(pid)
                    .map_err(fail)?
                    .into_iter()
                    .map(|module| {
                        let version = versions
                            .entry(module.path.to_ascii_lowercase())
                            .or_insert_with(|| version_info(&module.path).unwrap_or_default())
                            .clone();
                        (module, version)
                    })
                    .collect(),
            )
        }
        Tab::Memory => Loaded::Memory(memory::regions(pid).map_err(fail)?),
        Tab::Handles => Loaded::Handles(handles::handles(pid).map_err(fail)?),
        Tab::Environment => Loaded::Environment(environment::environment(pid).map_err(fail)?),
        Tab::Token => {
            let details = token::token_details(pid).map_err(fail)?;
            let name = |sid: &token::Sid| account_name(sid).unwrap_or_else(|_| sid.to_string());
            Loaded::Token(TokenSnapshot {
                user: name(&details.user),
                groups: details.groups.iter().map(|g| name(&g.sid)).collect(),
                details,
            })
        }
        Tab::Services => Loaded::Services(
            service::all_services().map_err(fail)?.into_iter().filter(|s| s.pid == pid && pid != 0).collect(),
        ),
    })
}

/// A thread's call stack, for the Threads tab's "Stack" action.
pub fn load_stack(pid: u32, tid: u32) -> Result<Vec<StackFrame>, String> {
    stack::thread_stack(pid, tid).map_err(|e| describe_error(&e))
}

pub fn stack_table(frames: &[StackFrame]) -> Table {
    let mut table = Table::new(&[Column::number("#", 40), Column::text("Address", 150), Column::text("Function", 500)]);
    for (i, frame) in frames.iter().enumerate() {
        table.push(
            i as u64,
            vec![Cell::count(i as u64), Cell::hex(frame.address), Cell::text(&frame.symbol)],
            Tone::Plain,
        );
    }
    table
}

/// What tabulating a tab can draw on besides its own data.
#[derive(Debug, Clone, Copy)]
pub struct Context<'a> {
    pub item: Option<&'a ProcessItem>,
    /// Process names by PID, for naming the targets of process and thread handles.
    pub names: &'a HashMap<u32, String>,
    /// The previous threads snapshot, for per-thread CPU usage.
    pub previous_threads: Option<&'a ThreadsSnapshot>,
    pub cpu_count: usize,
}

pub fn table(loaded: &Loaded, context: Context<'_>) -> Table {
    match loaded {
        Loaded::General(info) => general_table(info, context),
        Loaded::Threads(snapshot) => threads_table(snapshot, context),
        Loaded::Modules(modules) => modules_table(modules),
        Loaded::Memory(regions) => memory_table(regions),
        Loaded::Handles(handles) => handles_table(handles, context.names),
        Loaded::Environment(variables) => environment_table(variables),
        Loaded::Token(token) => token_table(token),
        Loaded::Services(services) => services_table(services, &HashMap::new()),
    }
}

fn general_table(info: &GeneralInfo, context: Context<'_>) -> Table {
    let mut table = Table::new(&[Column::text("Property", 170), Column::text("Value", 600)]);
    let mut row = |label: &str, value: String| {
        let id = table.rows.len() as u64;
        table.push(id, vec![Cell::text(label), Cell::text(value)], Tone::Plain);
    };
    if let Some(item) = context.item {
        let details = item.details.as_deref();
        row("Name", item.name.clone());
        row("PID", item.key.pid.to_string());
        let parent = context.names.get(&item.parent_pid).map_or_else(
            || format!("Non-existent process ({})", item.parent_pid),
            |name| format!("{name} ({})", item.parent_pid),
        );
        row("Parent", parent);
        row("Image path", details.and_then(|d| d.image_path.clone()).unwrap_or_default());
        row("Command line", info.command_line.clone().unwrap_or_default());
        row("Description", details.and_then(|d| d.description.clone()).unwrap_or_default());
        row("Company", details.and_then(|d| d.company.clone()).unwrap_or_default());
        row(
            "Signature",
            match &info.signature {
                Some(Signature::Verified(signer)) => format!("Verified: {signer}"),
                Some(Signature::Unsigned) => "Not signed".into(),
                Some(Signature::Untrusted(status)) => format!("Not trusted (0x{:08X})", *status as u32),
                None => String::new(),
            },
        );
        let user = details.and_then(|d| d.user.clone()).unwrap_or_default();
        let elevated = if details.is_some_and(|d| d.elevated) { " (elevated)" } else { "" };
        row("User", format!("{user}{elevated}"));
        row("Integrity", info.integrity.clone().unwrap_or_default());
        row("Session", item.session_id.to_string());
        row(
            "Priority",
            match info.priority_class.and_then(|c| control::PRIORITY_CLASSES.iter().find(|(class, _)| *class == c)) {
                Some((_, name)) => format!("{name} (base {})", item.base_priority),
                None => format!("Base {}", item.base_priority),
            },
        );
        row(
            "Affinity",
            match info.affinity {
                Some((mine, system)) if mine == system => format!("All {} processors", system.count_ones()),
                Some((mine, _)) => format!("{} processors: {}", mine.count_ones(), cpu_list(mine)),
                None => String::new(),
            },
        );
        row(
            "Critical",
            match info.critical {
                Some(true) => "Yes: Windows stops with a bugcheck if this process ends".into(),
                Some(false) => "No".into(),
                None => String::new(),
            },
        );
        row("Services", item.services.join(", "));
        row(
            "CPU",
            format!(
                "{}% (kernel {}%)",
                fallback(format::cpu(item.cpu_usage)),
                fallback(format::cpu(item.kernel_usage))
            ),
        );
        row("Private bytes", fallback(format::bytes(item.private_bytes)));
        row("Working set", fallback(format::bytes(item.working_set)));
        row("Threads", format::count(u64::from(item.threads)));
        row("Handles", format::count(u64::from(item.handles)));
        row(
            "I/O",
            format!(
                "read {}  ·  write {}  ·  other {}",
                fallback(format::rate(item.io_read_rate)),
                fallback(format::rate(item.io_write_rate)),
                fallback(format::rate(item.io_other_rate))
            ),
        );
        if item.suspended {
            row("State", "Suspended".into());
        }
    }
    table
}

fn fallback(text: String) -> String {
    if text.is_empty() { "0".into() } else { text }
}

/// `0-3, 8, 10-11` for an affinity mask.
pub fn cpu_list(mask: u64) -> String {
    let mut ranges: Vec<String> = Vec::new();
    let mut bit = 0;
    while bit < 64 {
        if mask & (1 << bit) == 0 {
            bit += 1;
            continue;
        }
        let start = bit;
        while bit + 1 < 64 && mask & (1 << (bit + 1)) != 0 {
            bit += 1;
        }
        ranges.push(if start == bit { start.to_string() } else { format!("{start}-{bit}") });
        bit += 1;
    }
    ranges.join(", ")
}

/// 100 ns ticks as `h:mm:ss.fff`.
fn cpu_time(ticks: u64) -> String {
    let ms = ticks / 10_000;
    format!("{}:{:02}:{:02}.{:03}", ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000)
}

fn threads_table(snapshot: &ThreadsSnapshot, context: Context<'_>) -> Table {
    let mut table = Table::new(&[
        Column::number("TID", 70),
        Column::number("CPU", 60),
        Column::text("Start address", 300),
        Column::text("Priority", 110),
        Column::text("State", 170),
        Column::number("Switches", 90),
        Column::number("Switch Δ", 80),
        Column::number("Kernel time", 100),
        Column::number("User time", 100),
    ]);
    let process_base = context.item.map_or(8, |item| item.base_priority);
    let previous: HashMap<u32, &ThreadInfo> =
        context.previous_threads.map(|p| p.threads.iter().map(|t| (t.tid, t)).collect()).unwrap_or_default();
    let elapsed = context
        .previous_threads
        .map(|p| snapshot.taken_at.saturating_duration_since(p.taken_at).as_secs_f64())
        .filter(|s| *s > 0.0);
    for thread in &snapshot.threads {
        let before = previous.get(&thread.tid).filter(|t| t.create_time == thread.create_time);
        let usage = match (before, elapsed) {
            (Some(before), Some(secs)) => {
                let delta =
                    (thread.kernel_time + thread.user_time).saturating_sub(before.kernel_time + before.user_time);
                (delta as f64 / 1e7 / secs / context.cpu_count.max(1) as f64) as f32
            }
            _ => 0.0,
        };
        let switches = before.map_or(0, |b| thread.context_switches.saturating_sub(b.context_switches));
        let tone = if thread.is_suspended() { Tone::Warning } else { Tone::Plain };
        table.push(
            u64::from(thread.tid),
            vec![
                Cell::number(f64::from(thread.tid), thread.tid.to_string()),
                Cell::number(f64::from(usage), format::cpu(usage)),
                Cell::text(
                    snapshot.start.get(&thread.tid).cloned().unwrap_or_else(|| format!("0x{:x}", thread.start_address)),
                ),
                Cell::number(
                    f64::from(thread.base_priority),
                    names::thread_priority(thread.base_priority, process_base),
                ),
                Cell::text(names::thread_state(thread.state, thread.wait_reason)),
                Cell::count(u64::from(thread.context_switches)),
                Cell::number(
                    f64::from(switches),
                    if switches == 0 { String::new() } else { format::count(u64::from(switches)) },
                ),
                Cell::number(thread.kernel_time as f64, cpu_time(thread.kernel_time)),
                Cell::number(thread.user_time as f64, cpu_time(thread.user_time)),
            ],
            tone,
        );
    }
    table
}

fn modules_table(modules: &[(ModuleInfo, VersionInfo)]) -> Table {
    let mut table = Table::new(&[
        Column::text("Name", 200),
        Column::number("Base address", 140),
        Column::number("Size", 90),
        Column::text("Description", 260),
        Column::text("Company", 180),
        Column::text("Path", 400),
    ]);
    for (module, version) in modules {
        table.push(
            module.base,
            vec![
                Cell::text(&module.name),
                Cell::hex(module.base),
                Cell::bytes(u64::from(module.size)),
                Cell::text(version.description.clone().unwrap_or_default()),
                Cell::text(version.company.clone().unwrap_or_default()),
                Cell::text(&module.path),
            ],
            Tone::Plain,
        );
    }
    table
}

fn memory_table(regions: &[MemoryRegion]) -> Table {
    let mut table = Table::new(&[
        Column::number("Base address", 140),
        Column::text("Type", 70),
        Column::number("Size", 90),
        Column::text("Protection", 90),
        Column::text("State", 70),
        Column::text("Allocation", 90),
        Column::text("Mapped file", 400),
    ]);
    for region in regions {
        // Writable and executable at once is how injected or JIT code lives; worth a glance.
        let tone = match region.protect & 0xFF {
            0x40 | 0x80 => Tone::Warning,
            _ if region.state == 0x2000 => Tone::Muted,
            _ => Tone::Plain,
        };
        table.push(
            region.base,
            vec![
                Cell::hex(region.base),
                Cell::text(names::memory_type(region.kind)),
                Cell::bytes(region.size),
                Cell::text(names::protection(region.protect)),
                Cell::text(names::memory_state(region.state)),
                Cell::text(names::protection(region.allocation_protect)),
                Cell::text(region.mapped_file.clone().unwrap_or_default()),
            ],
            tone,
        );
    }
    table
}

fn handles_table(handles: &[HandleInfo], names: &HashMap<u32, String>) -> Table {
    let mut table = Table::new(&[
        Column::text("Type", 170),
        Column::text("Name", 460),
        Column::number("Handle", 90),
        Column::number("Access", 100),
        Column::text("Attributes", 110),
    ]);
    let process =
        |pid: u32| names.get(&pid).map_or_else(|| format!("Non-existent process ({pid})"), |n| format!("{n} ({pid})"));
    for handle in handles {
        let name = match &handle.target {
            Some(HandleTarget::Process(pid)) => process(*pid),
            Some(HandleTarget::Thread { tid, pid }) => format!("{}: thread {tid}", process(*pid)),
            None => handle.name.clone().unwrap_or_default(),
        };
        let attributes = match (handle.attributes & 0x2 != 0, handle.attributes & 0x1 != 0) {
            (true, true) => "Inherit, protect",
            (true, false) => "Inherit",
            (false, true) => "Protect",
            (false, false) => "",
        };
        let tone = if name.is_empty() { Tone::Muted } else { Tone::Plain };
        table.push(
            handle.handle,
            vec![
                Cell::text(&handle.type_name),
                Cell::text(name),
                Cell::hex(handle.handle),
                Cell::number(f64::from(handle.granted_access), format!("0x{:08x}", handle.granted_access)),
                Cell::text(attributes),
            ],
            tone,
        );
    }
    table
}

fn environment_table(variables: &[(String, String)]) -> Table {
    let mut table = Table::new(&[Column::text("Variable", 220), Column::text("Value", 700)]);
    for (i, (name, value)) in variables.iter().enumerate() {
        // The hidden `=C:` style per-drive directories are plumbing, not configuration.
        let tone = if name.starts_with('=') { Tone::Muted } else { Tone::Plain };
        table.push(i as u64, vec![Cell::text(name), Cell::text(value)], tone);
    }
    table
}

fn token_table(token: &TokenSnapshot) -> Table {
    let mut table = Table::new(&[Column::text("Kind", 140), Column::text("Name", 380), Column::text("Flags", 300)]);
    let details = &token.details;
    let mut id = 0u64;
    let mut push = |table: &mut Table, kind: &str, name: String, flags: String, tone: Tone| {
        table.push(id, vec![Cell::text(kind), Cell::text(name), Cell::text(flags)], tone);
        id += 1;
    };
    push(&mut table, "User", token.user.clone(), details.user.to_string(), Tone::Plain);
    let integrity = details.integrity.as_ref().map(|sid| names::integrity(&sid.to_string())).unwrap_or_default();
    push(&mut table, "Integrity", integrity, String::new(), Tone::Plain);
    push(&mut table, "Session", details.session_id.to_string(), String::new(), Tone::Plain);
    let elevation = format!(
        "{}{}",
        if details.elevated { "Elevated" } else { "Not elevated" },
        match names::elevation_type(details.elevation_type) {
            "" => String::new(),
            kind => format!(" ({kind} token)"),
        }
    );
    push(&mut table, "Elevation", elevation, String::new(), Tone::Plain);
    let virtualization = match (details.virtualization_allowed, details.virtualization_enabled) {
        (_, true) => "Enabled",
        (true, false) => "Allowed, disabled",
        (false, false) => "Not allowed",
    };
    push(&mut table, "UAC virtualization", virtualization.into(), String::new(), Tone::Plain);
    for (group, name) in details.groups.iter().zip(&token.groups) {
        let deny_or_off = group.attributes & 0x10 != 0 || group.attributes & 0x4 == 0;
        let tone = if deny_or_off { Tone::Muted } else { Tone::Plain };
        push(&mut table, "Group", name.clone(), names::group_flags(group.attributes), tone);
    }
    for privilege in &details.privileges {
        let tone = if privilege.attributes & 0x2 != 0 { Tone::Good } else { Tone::Muted };
        push(
            &mut table,
            "Privilege",
            privilege.name.clone(),
            names::privilege_flags(privilege.attributes).into(),
            tone,
        );
    }
    table
}

/// The services table, shared by the Services tab and the Services page. `names` maps host PIDs
/// to process names (empty for the tab, where every service shares one host).
pub fn services_table(services: &[ServiceDetails], names: &HashMap<u32, String>) -> Table {
    let mut table = Table::new(&[
        Column::text("Name", 170),
        Column::text("Display name", 260),
        Column::text("Status", 80),
        Column::text("Startup", 140),
        Column::text("Host process", 160),
        Column::text("Type", 130),
        Column::text("Account", 150),
        Column::text("Description", 360),
        Column::text("Path", 400),
    ]);
    for service in services {
        let tone = match service.state {
            4 => Tone::Good,
            1 => Tone::Muted,
            _ => Tone::Warning,
        };
        let host = match (service.pid, names.get(&service.pid)) {
            (0, _) => String::new(),
            (pid, Some(name)) => format!("{name} ({pid})"),
            (pid, None) => pid.to_string(),
        };
        table.push(
            stable_id(&service.name),
            vec![
                Cell::text(&service.name),
                Cell::text(&service.display_name),
                Cell::text(names::service_state(service.state)),
                Cell::text(names::service_start(service.start_type, service.delayed_start)),
                Cell::number(f64::from(service.pid), host),
                Cell::text(names::service_kind(service.kind)),
                Cell::text(service.account.clone().unwrap_or_default()),
                Cell::text(service.description.clone().unwrap_or_default()),
                Cell::text(service.binary_path.clone().unwrap_or_default()),
            ],
            tone,
        );
    }
    table
}

/// A deterministic 64-bit id for a string key (FNV-1a), so selections survive refreshes.
pub fn stable_id(key: &str) -> u64 {
    key.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_affinity_ranges() {
        assert_eq!(cpu_list(0b1011_1101), "0, 2-5, 7");
        assert_eq!(cpu_list(u64::MAX), "0-63");
        assert_eq!(cpu_list(0), "");
    }

    #[test]
    fn formats_cpu_time() {
        assert_eq!(cpu_time(0), "0:00:00.000");
        assert_eq!(cpu_time(36_610_000_000 + 1_234 * 10_000), "1:01:02.234");
    }

    #[test]
    fn loads_every_tab_for_this_process() {
        let me = std::process::id();
        let path = ferrotop_sys::process::image_path(me).ok();
        for tab in Tab::ALL {
            // A child process would need admin for some tabs; our own never does.
            if tab == Tab::Services {
                continue;
            }
            let loaded = load(tab, me, path.as_deref()).unwrap_or_else(|e| panic!("{tab:?}: {e}"));
            let names = HashMap::new();
            let table = table(&loaded, Context { item: None, names: &names, previous_threads: None, cpu_count: 1 });
            if tab != Tab::General {
                assert!(!table.rows.is_empty(), "{tab:?} produced no rows");
            }
            assert!(table.rows.iter().all(|r| r.cells.len() == table.columns.len()), "{tab:?}");
        }
    }

    #[test]
    fn thread_cpu_comes_from_deltas() {
        let thread = |kernel| ThreadInfo { tid: 7, kernel_time: kernel, create_time: 1, ..ThreadInfo::default() };
        let t0 = Instant::now();
        let before = ThreadsSnapshot { threads: vec![thread(0)], start: HashMap::new(), taken_at: t0 };
        let after = ThreadsSnapshot {
            threads: vec![thread(5_000_000)],
            start: HashMap::new(),
            taken_at: t0 + std::time::Duration::from_secs(1),
        };
        let names = HashMap::new();
        let table =
            threads_table(&after, Context { item: None, names: &names, previous_threads: Some(&before), cpu_count: 2 });
        // Half a second of CPU in one second on a two-CPU machine = 25%.
        assert_eq!(table.rows[0].cells[1].text, "25.00");
    }
}
