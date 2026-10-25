//! Builds the Performance page (sidebar tiles and the selected device's detail pane) from the
//! performance provider's histories.

use std::net::IpAddr;

use ferrotop_core::format;
use ferrotop_core::history::{History, nice_ceiling};
use ferrotop_core::perf::{CpuTempSource, PawnIoState, PerformanceProvider, cpu_temperature};
use ferrotop_core::provider::SystemStats;
use ferrotop_core::series::{slug, uniform_ccds};
use ferrotop_core::settings::CpuGraph;

use crate::{CellGroup, DeviceTile, GraphData, GraphPair, GraphToggle, Stat};

/// A device in the sidebar, identified stably across refreshes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Device {
    Cpu,
    Memory,
    Disk(u32),
    Network(u32),
    Gpu(usize),
}

impl Device {
    /// Stable id for the settings file: `cpu`, `memory`, `disk:1`, `net:18`, `gpu:0`.
    pub fn id(self) -> String {
        match self {
            Device::Cpu => "cpu".into(),
            Device::Memory => "memory".into(),
            Device::Disk(number) => format!("disk:{number}"),
            Device::Network(index) => format!("net:{index}"),
            Device::Gpu(index) => format!("gpu:{index}"),
        }
    }

    pub fn parse(id: &str) -> Option<Self> {
        Some(match id.split_once(':') {
            None if id == "cpu" => Device::Cpu,
            None if id == "memory" => Device::Memory,
            Some(("disk", number)) => Device::Disk(number.parse().ok()?),
            Some(("net", index)) => Device::Network(index.parse().ok()?),
            Some(("gpu", index)) => Device::Gpu(index.parse().ok()?),
            _ => return None,
        })
    }
}

// Device kinds, matching `Theme.device-color`.
const CPU: i32 = 0;
const MEMORY: i32 = 1;
const DISK: i32 = 2;
const NETWORK: i32 = 3;
const GPU: i32 = 4;

#[derive(Debug, Default)]
pub struct Detail {
    pub title: String,
    pub model: String,
    pub big: Vec<GraphData>,
    pub pairs: Vec<GraphPair>,
    pub small: Vec<GraphData>,
    pub top: Vec<Stat>,
    pub bottom: Vec<Stat>,
    /// Sensor readings (per-CCD or per-core temperatures), shown as a third row of figures.
    pub sensors: Vec<Stat>,
    pub info: Vec<Stat>,
    /// A grid of small graphs (per core, per thread, per CCX) and how many columns it has.
    pub cells: Vec<GraphData>,
    pub cell_columns: i32,
    pub cells_title: String,
    pub cell_groups: Vec<CellGroup>,
    /// The device's graphs for the View menu, and whether each is shown.
    pub toggles: Vec<GraphToggle>,
    /// Every graph is hidden: show the figures large instead.
    pub figures_only: bool,
}

/// View-menu label for a graph id.
fn graph_label(id: &str) -> &'static str {
    match id {
        "cpu.utilization" => "Utilization",
        "cpu.temperature" => "Temperature",
        "memory.usage" => "Memory usage",
        "memory.commit" => "Committed",
        "disk.active" => "Active time",
        "disk.transfer" => "Transfer rate",
        "network.throughput" => "Throughput",
        "gpu.engines" => "Engines",
        "gpu.dedicated" => "Dedicated memory",
        "gpu.shared" => "Shared memory",
        "gpu.temperature" => "Temperature",
        _ => "Graph",
    }
}

/// Lists the detail's graphs as View-menu toggles and removes the ones in `hidden`.
fn apply_hidden(detail: &mut Detail, hidden: &[String]) {
    let mut ids: Vec<String> = Vec::new();
    let all = detail.big.iter().chain(detail.pairs.iter().map(|p| &p.left)).chain(&detail.small).chain(&detail.cells);
    for graph in all {
        if !ids.iter().any(|id| id == graph.id.as_str()) {
            ids.push(graph.id.to_string());
        }
    }
    let is_hidden = |id: &str| hidden.iter().any(|h| h == id);
    detail.toggles = ids
        .iter()
        .map(|id| GraphToggle { id: id.as_str().into(), title: graph_label(id).into(), shown: !is_hidden(id) })
        .collect();
    detail.big.retain(|g| !is_hidden(&g.id));
    detail.small.retain(|g| !is_hidden(&g.id));
    detail.pairs.retain(|p| !is_hidden(&p.left.id));
    if detail.cells.first().is_some_and(|g| is_hidden(&g.id)) {
        detail.cells.clear();
        detail.cell_groups.clear();
        detail.cells_title.clear();
    }
    detail.figures_only = !ids.is_empty()
        && detail.big.is_empty()
        && detail.small.is_empty()
        && detail.pairs.is_empty()
        && detail.cells.is_empty();
}

/// Sidebar order: CPU, memory, disks, network adapters, GPUs (like Task Manager).
pub fn devices(perf: &PerformanceProvider) -> Vec<Device> {
    let mut devices = vec![Device::Cpu, Device::Memory];
    devices.extend(perf.disks.keys().map(|&n| Device::Disk(n)));
    devices.extend(perf.networks.keys().map(|&i| Device::Network(i)));
    devices.extend((0..perf.gpus.len()).map(Device::Gpu));
    devices
}

fn paths(history: &History, max: f64) -> (String, String) {
    history.svg_paths(100.0, 100.0, max)
}

fn graph(id: &str, kind: i32, title: &str, corner: String, a: &History, max: f64, b: Option<&History>) -> GraphData {
    let (line_a, fill_a) = paths(a, max);
    GraphData {
        id: id.into(),
        kind,
        title: title.into(),
        corner: corner.into(),
        line_a: line_a.into(),
        fill_a: fill_a.into(),
        line_b: b.map(|b| paths(b, max).0).unwrap_or_default().into(),
        history: Default::default(),
    }
}

/// What right-clicking a graph offers to show in History or the header: graph keys (one series
/// id, or two joined by `+`) separated by `;`.
fn keyed(mut graph: GraphData, keys: impl Into<String>) -> GraphData {
    graph.history = keys.into().into();
    graph
}

fn stat(label: &str, value: impl Into<String>) -> Stat {
    Stat { label: label.into(), value: value.into().into(), history: Default::default() }
}

fn keyed_stat(label: &str, value: impl Into<String>, keys: impl Into<String>) -> Stat {
    Stat { history: keys.into().into(), ..stat(label, value) }
}

fn gib(bytes: u64) -> f64 {
    bytes as f64 / f64::from(1u32 << 30)
}

/// `"2.2/8.0 GB"`, or without decimals for big totals: `"110/192 GB"`.
fn used_of(used: u64, total: u64) -> String {
    if gib(total) >= 100.0 {
        format!("{:.0}/{:.0} GB", gib(used), gib(total))
    } else {
        format!("{:.1}/{:.1} GB", gib(used), gib(total))
    }
}

fn percent(fraction: f64) -> String {
    format!("{:.0}%", fraction * 100.0)
}

fn rate(bytes_per_second: f64) -> String {
    match format::rate(bytes_per_second) {
        empty if empty.is_empty() => "0 KB/s".to_owned(),
        rate => rate,
    }
}

fn disk_title(number: u32, letters: &str) -> String {
    if letters.is_empty() { format!("Disk {number}") } else { format!("Disk {number} ({letters})") }
}

fn network_title(wireless: bool) -> &'static str {
    if wireless { "Wi-Fi" } else { "Ethernet" }
}

pub fn tile(perf: &PerformanceProvider, stats: &SystemStats, device: Device) -> DeviceTile {
    let keys = match device {
        Device::Cpu => "cpu;cpu.temp".to_owned(),
        Device::Memory => "memory;memory.commit".to_owned(),
        Device::Disk(n) => format!("disk.{n}.active;disk.{n}.read+disk.{n}.write"),
        Device::Network(i) => format!("net.{i}.receive+net.{i}.send"),
        Device::Gpu(n) => format!("gpu.{n}.load;gpu.{n}.temp+gpu.{n}.hotspot;gpu.{n}.temp;gpu.{n}.dedicated"),
    };
    let (kind, title, subtitle, value, history, max) = match device {
        Device::Cpu => {
            let speed = perf.cpu_speed_mhz.map(format::mhz).unwrap_or_default();
            let temperature = perf.cpu_temps.as_ref().and_then(cpu_temperature).map(|t| format!("  ({t:.0} °C)"));
            (
                CPU,
                "CPU".into(),
                format!("{}  {speed}{}", percent(f64::from(stats.cpu_usage)), temperature.unwrap_or_default()),
                String::new(),
                &perf.cpu,
                1.0,
            )
        }
        Device::Memory => {
            let (used, total) =
                perf.memory.map_or((0, 0), |m| (m.physical_total - m.physical_available, m.physical_total));
            let share = if total == 0 { 0.0 } else { used as f64 / total as f64 };
            (
                MEMORY,
                "Memory".into(),
                format!("{} ({})", used_of(used, total), percent(share)),
                String::new(),
                &perf.memory_used,
                total as f64,
            )
        }
        Device::Disk(number) => {
            let Some(disk) = perf.disks.get(&number) else { return DeviceTile::default() };
            (
                DISK,
                disk_title(number, &disk.latest.letters),
                disk.latest.info.kind(),
                format!("{:.0}%", disk.latest.active),
                &disk.active,
                1.0,
            )
        }
        Device::Network(index) => {
            let Some(net) = perf.networks.get(&index) else { return DeviceTile::default() };
            let max = nice_ceiling(net.receive.peak().max(net.send.peak()));
            (
                NETWORK,
                network_title(net.adapter.wireless).into(),
                net.adapter.alias.clone(),
                format!(
                    "S: {}  R: {}",
                    format::bits_per_second(net.send_rate),
                    format::bits_per_second(net.receive_rate)
                ),
                &net.receive,
                max,
            )
        }
        Device::Gpu(index) => {
            let Some(gpu) = perf.gpus.get(index) else { return DeviceTile::default() };
            let temperature = match (gpu.latest.edge_c(), gpu.latest.hotspot_c()) {
                (Some(edge), Some(hotspot)) => format!("  ({edge:.0} °C, hotspot {hotspot:.0} °C)"),
                (Some(edge), None) => format!("  ({edge:.0} °C)"),
                _ => String::new(),
            };
            (
                GPU,
                format!("GPU {index}"),
                gpu.info.name.clone(),
                format!("{:.0}%{temperature}", gpu.utilization_now()),
                &gpu.utilization,
                1.0,
            )
        }
    };
    let (line, fill) = paths(history, max);
    DeviceTile {
        kind,
        title: title.into(),
        subtitle: subtitle.into(),
        value: value.into(),
        line: line.into(),
        fill: fill.into(),
        history: keys.into(),
    }
}

pub fn detail(
    perf: &PerformanceProvider,
    stats: &SystemStats,
    device: Device,
    cpu_graph: CpuGraph,
    hidden: &[String],
) -> Detail {
    let mut detail = match device {
        Device::Cpu => cpu_detail(perf, stats, cpu_graph),
        Device::Memory => memory_detail(perf),
        Device::Disk(number) => disk_detail(perf, number),
        Device::Network(index) => network_detail(perf, index),
        Device::Gpu(index) => gpu_detail(perf, index),
    };
    apply_hidden(&mut detail, hidden);
    detail
}

/// Columns for a grid of `count` small graphs: one row up to 4, then 4 or 8 wide, 16 for huge chips.
fn grid_columns(count: usize) -> i32 {
    match count {
        0..=4 => count.max(1) as i32,
        5..=8 => 4,
        9..=64 => 8,
        _ => 16,
    }
}

/// The CPU graph grid for the per-core, per-thread and per-CCX views.
struct CellGrid {
    title: String,
    cells: Vec<GraphData>,
    columns: i32,
    /// Boxes drawn around runs of cells, e.g. the CCXs of each CCD.
    groups: Vec<CellGroup>,
}

fn cpu_cells(perf: &PerformanceProvider, graph: CpuGraph) -> CellGrid {
    let cpu = &perf.machine.cpu;
    let cell = |label: String, history: &History, keys: String| {
        keyed(graph_cell(CPU, label, format!("{:.0}%", history.latest().unwrap_or(0.0) * 100.0), history), keys)
    };
    let temps = perf.cpu_temps.as_ref();
    let span = |items: &[usize]| match (items.first(), items.last()) {
        (Some(first), Some(last)) if first != last => format!("{first}–{last}"),
        (Some(only), _) => only.to_string(),
        _ => String::new(),
    };
    let grid = |title: String, cells: Vec<GraphData>| CellGrid {
        title,
        columns: grid_columns(cells.len()),
        cells,
        groups: Vec::new(),
    };
    match graph {
        CpuGraph::Overall => grid(String::new(), Vec::new()),
        CpuGraph::Cores => grid(
            format!("Utilization per core ({} cores)", perf.cores.len()),
            perf.cores
                .iter()
                .enumerate()
                .map(|(i, history)| {
                    let celsius = temps.and_then(|t| t.cores.get(i)).map(|t| format!(" · {t:.0} °C"));
                    cell(
                        format!("Core {i}{}", celsius.unwrap_or_default()),
                        history,
                        format!("cpu.core.{i};cpu.core.{i}.temp"),
                    )
                })
                .collect(),
        ),
        CpuGraph::Logical => grid(
            format!("Utilization per logical processor ({} threads)", perf.logical.len()),
            perf.logical
                .iter()
                .enumerate()
                .map(|(i, history)| cell(format!("CPU {i}"), history, String::new()))
                .collect(),
        ),
        CpuGraph::Ccx => {
            // Name each CCX by its cores, found through its threads.
            let cores_of = |ccx: usize| -> Vec<usize> {
                let threads = cpu.l3_groups.get(ccx).cloned().unwrap_or_default();
                cpu.core_threads
                    .iter()
                    .enumerate()
                    .filter(|(_, core)| core.iter().any(|t| threads.contains(t)))
                    .map(|(core, _)| core)
                    .collect()
            };
            let ccx_count = perf.l3_groups.len();
            // How many CCDs the CCXs make up, each holding the next run of CCXs. The CPU says on
            // Zen to Zen 2, so the CCDs show without a temperature driver; otherwise there's one
            // per CCD temperature sensor.
            let sensors = temps.map_or(&[][..], |t| &t.ccds[..]);
            let ccd_count = match uniform_ccds(&cpu.l3_ccds) {
                Some(count) if count > 1 && cpu.l3_ccds.len() == ccx_count => count,
                _ => sensors.len(),
            };
            let per_ccd = ccx_count.checked_div(ccd_count).filter(|n| *n > 0 && ccx_count.is_multiple_of(ccd_count));
            // Sensor slots can be sparse (a part with CCDs in alternate positions reports slots
            // 0, 2, 4 and 6), so the n-th sensor is the n-th CCD's. Only shown when every CCD
            // has one.
            let celsius = |n: usize| match sensors.get(n) {
                Some((_, t)) if sensors.len() == ccd_count => format!(" · {t:.0} °C"),
                _ => String::new(),
            };
            match per_ccd {
                // One CCX per CCD (Zen 3 and later): the CCX is the CCD, so say so on the cell.
                Some(1) => grid(
                    format!("Utilization per CCD ({ccx_count} CCDs, each with its own L3 cache)"),
                    perf.l3_groups
                        .iter()
                        .enumerate()
                        .map(|(i, history)| {
                            let label = format!("CCD {} · cores {}{}", i + 1, span(&cores_of(i)), celsius(i));
                            cell(label, history, format!("cpu.ccx.{i};cpu.ccd.{}.temp", i + 1))
                        })
                        .collect(),
                ),
                // Several CCXs per CCD (Zen 2): box each CCD's CCXs together, with its temperature.
                Some(per_ccd) => {
                    let cells: Vec<GraphData> = perf
                        .l3_groups
                        .iter()
                        .enumerate()
                        .map(|(i, history)| {
                            // The CCX, and the whole CCD it's part of.
                            let ccd = i / per_ccd + 1;
                            let keys = format!("cpu.ccx.{i};cpu.ccd.{ccd};cpu.ccd.{ccd}.temp");
                            cell(format!("CCX {i} · cores {}", span(&cores_of(i))), history, keys)
                        })
                        .collect();
                    // Whole CCDs per row, so a box never wraps.
                    let columns = (grid_columns(cells.len()) as usize / per_ccd).max(1) * per_ccd;
                    let groups = (0..ccd_count)
                        .map(|n| {
                            let first = n * per_ccd;
                            let ccx_cores: Vec<usize> = (first..first + per_ccd).flat_map(cores_of).collect();
                            CellGroup {
                                title: format!("CCD {} · cores {}{}", n + 1, span(&ccx_cores), celsius(n)).into(),
                                column: (first % columns) as i32,
                                row: (first / columns) as i32,
                                columns: per_ccd as i32,
                                history: format!("cpu.ccd.{};cpu.ccd.{}.temp", n + 1, n + 1).into(),
                            }
                        })
                        .collect();
                    CellGrid {
                        title: format!(
                            "Utilization per CCX (cores sharing an L3 cache), boxed by CCD: {ccd_count} CCDs × {per_ccd} CCXs"
                        ),
                        cells,
                        columns: columns as i32,
                        groups,
                    }
                }
                // CCDs unknown (a CPU that doesn't say, without PawnIO or not elevated): just the CCXs.
                None => grid(
                    format!("Utilization per CCX: cores sharing an L3 cache ({ccx_count} groups)"),
                    perf.l3_groups
                        .iter()
                        .enumerate()
                        .map(|(i, history)| {
                            cell(format!("CCX {i} · cores {}", span(&cores_of(i))), history, format!("cpu.ccx.{i}"))
                        })
                        .collect(),
                ),
            }
        }
    }
}

fn graph_cell(kind: i32, title: String, corner: String, history: &History) -> GraphData {
    let (line_a, fill_a) = paths(history, 1.0);
    GraphData {
        id: "cpu.utilization".into(),
        kind,
        title: title.into(),
        corner: corner.into(),
        line_a: line_a.into(),
        fill_a: fill_a.into(),
        line_b: Default::default(),
        history: Default::default(),
    }
}

fn cpu_detail(perf: &PerformanceProvider, stats: &SystemStats, cpu_graph: CpuGraph) -> Detail {
    let cpu = &perf.machine.cpu;
    let cache = |bytes: u64| if bytes == 0 { "—".to_owned() } else { format::bytes(bytes) };
    let grid = cpu_cells(perf, cpu_graph);
    let big = if grid.cells.is_empty() {
        vec![keyed(
            graph("cpu.utilization", CPU, "% Utilization over 60 seconds", "100%".into(), &perf.cpu, 1.0, None),
            "cpu",
        )]
    } else {
        Vec::new()
    };
    let temperature = perf.cpu_temps.as_ref().and_then(cpu_temperature);
    let mut top = vec![
        keyed_stat("Utilization", percent(f64::from(stats.cpu_usage)), "cpu"),
        keyed_stat("Speed", perf.cpu_speed_mhz.map(format::mhz).unwrap_or_else(|| "—".into()), "cpu.speed"),
    ];
    let mut small = Vec::new();
    if let Some(celsius) = temperature {
        let label = match perf.cpu_temps.as_ref() {
            Some(t) if t.die_c.is_some() => "Temperature (Tdie)",
            Some(t) if t.package_c.is_some() && t.cores.is_empty() => "Temperature (Tctl)",
            _ => "Temperature",
        };
        top.push(keyed_stat(label, format!("{celsius:.0} °C"), "cpu.temp"));
        let ceiling = nice_ceiling(perf.cpu_temp.peak()).max(100.0);
        let temperature =
            graph("cpu.temperature", CPU, label, format!("{ceiling:.0} °C"), &perf.cpu_temp, ceiling, None);
        small.push(keyed(temperature, "cpu.temp"));
    }
    let bottom = vec![
        stat("Processes", format::count(stats.processes as u64)),
        stat("Threads", format::count(stats.threads)),
        stat("Handles", format::count(stats.handles)),
        stat("Up time", format::uptime(perf.uptime.as_secs())),
    ];
    let mut sensors = Vec::new();
    if let Some(temps) = perf.cpu_temps.as_ref() {
        // Number CCDs by position: their sensor slots can be sparse.
        for (n, (_, celsius)) in temps.ccds.iter().enumerate() {
            sensors.push(keyed_stat(
                &format!("CCD {}", n + 1),
                format!("{celsius:.0} °C"),
                format!("cpu.ccd.{}.temp", n + 1),
            ));
        }
        if let (Some(hottest), Some(coolest)) =
            (temps.cores.iter().copied().reduce(f32::max), temps.cores.iter().copied().reduce(f32::min))
        {
            sensors.push(stat("Hottest core", format!("{hottest:.0} °C")));
            sensors.push(stat("Coolest core", format!("{coolest:.0} °C")));
        }
    }
    let source = match (perf.cpu_temp_source, perf.pawnio) {
        (CpuTempSource::PawnIo, _) => "PawnIO driver".to_owned(),
        (CpuTempSource::Service, _) => "Ferrotop's history service (PawnIO)".to_owned(),
        (CpuTempSource::LibreHardwareMonitor, _) => "LibreHardwareMonitor".to_owned(),
        (CpuTempSource::None, PawnIoState::NeedsAdmin) => "Run as administrator to read it through PawnIO".to_owned(),
        (CpuTempSource::None, PawnIoState::Unsupported) => "Not supported on this CPU".to_owned(),
        (CpuTempSource::None, PawnIoState::Failed) => "PawnIO isn't responding".to_owned(),
        (CpuTempSource::None, _) => "Install PawnIO, or turn on LibreHardwareMonitor's web server".to_owned(),
    };
    Detail {
        title: "CPU".into(),
        model: cpu.name.clone(),
        big,
        cell_columns: grid.columns,
        cells: grid.cells,
        cells_title: grid.title,
        cell_groups: grid.groups,
        small,
        top,
        bottom,
        sensors,
        info: vec![
            stat("Temperature source", source),
            stat("Base speed", cpu.base_mhz.map(|mhz| format::mhz(f64::from(mhz))).unwrap_or_else(|| "—".into())),
            stat("Sockets", cpu.sockets.to_string()),
            stat("Cores", cpu.cores.to_string()),
            stat("Logical processors", cpu.logical.to_string()),
            stat("L1 cache", cache(cpu.l1_cache)),
            stat("L2 cache", cache(cpu.l2_cache)),
            stat("L3 cache", cache(cpu.l3_cache)),
        ],
        ..Detail::default()
    }
}

fn memory_detail(perf: &PerformanceProvider) -> Detail {
    let Some(memory) = perf.memory else { return Detail { title: "Memory".into(), ..Detail::default() } };
    let machine = &perf.machine;
    let used = memory.physical_total.saturating_sub(memory.physical_available);
    let populated: Vec<_> = machine.slots.iter().filter(|slot| slot.size > 0).collect();
    let installed = machine.installed_memory.unwrap_or(memory.physical_total);
    let kind = populated.first().and_then(|slot| slot.memory_type).unwrap_or_default();
    let speed = populated.first().and_then(|slot| slot.configured_speed.or(slot.speed));
    Detail {
        title: "Memory".into(),
        model: format!("{} {kind}", format::bytes(installed)).trim().to_owned(),
        big: vec![keyed(
            graph(
                "memory.usage",
                MEMORY,
                "Memory usage",
                format::bytes(memory.physical_total),
                &perf.memory_used,
                memory.physical_total as f64,
                None,
            ),
            "memory",
        )],
        small: vec![keyed(
            graph(
                "memory.commit",
                MEMORY,
                "Committed",
                format::bytes(memory.commit_limit),
                &perf.commit,
                memory.commit_limit as f64,
                None,
            ),
            "memory.commit",
        )],
        top: vec![
            keyed_stat("In use", format::bytes(used), "memory"),
            stat("Available", format::bytes(memory.physical_available)),
        ],
        bottom: vec![
            keyed_stat("Committed", used_of(memory.commit_total, memory.commit_limit), "memory.commit"),
            stat("Cached", format::bytes(memory.system_cache)),
            stat("Paged pool", format::bytes(memory.paged_pool)),
            stat("Non-paged pool", format::bytes(memory.nonpaged_pool)),
        ],
        info: vec![
            stat("Speed", speed.map(|s| format!("{s} MT/s")).unwrap_or_else(|| "—".into())),
            stat(
                "Slots used",
                if machine.slots.is_empty() {
                    "—".to_owned()
                } else {
                    format!("{} of {}", populated.len(), machine.slots.len())
                },
            ),
            stat("Form factor", populated.first().and_then(|slot| slot.form_factor).unwrap_or("—")),
            stat("Hardware reserved", format::bytes(installed.saturating_sub(memory.physical_total))),
        ],
        ..Detail::default()
    }
}

fn disk_detail(perf: &PerformanceProvider, number: u32) -> Detail {
    let Some(disk) = perf.disks.get(&number) else { return Detail::default() };
    let latest = &disk.latest;
    let max = nice_ceiling(disk.read.peak().max(disk.write.peak()).max(1024.0 * 1024.0));
    let system_drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
    Detail {
        title: disk_title(number, &latest.letters),
        model: latest.info.model.clone(),
        big: vec![keyed(
            graph("disk.active", DISK, "Active time", "100%".into(), &disk.active, 1.0, None),
            format!("disk.{number}.active"),
        )],
        small: vec![keyed(
            graph(
                "disk.transfer",
                DISK,
                "Disk transfer rate (read, write)",
                format!("{}/s", format::bytes(max as u64)),
                &disk.read,
                max,
                Some(&disk.write),
            ),
            format!("disk.{number}.read+disk.{number}.write;disk.{number}.read;disk.{number}.write"),
        )],
        top: vec![
            keyed_stat("Active time", format!("{:.0}%", latest.active), format!("disk.{number}.active")),
            stat("Average response time", format!("{:.1} ms", latest.response_ms)),
        ],
        bottom: vec![
            keyed_stat("Read speed", rate(latest.read), format!("disk.{number}.read")),
            keyed_stat("Write speed", rate(latest.write), format!("disk.{number}.write")),
        ],
        info: vec![
            stat("Capacity", latest.info.size.map(format::bytes).unwrap_or_else(|| "—".into())),
            stat("Type", latest.info.kind()),
            stat(
                "System disk",
                if latest.letters.split_whitespace().any(|l| l.eq_ignore_ascii_case(&system_drive)) {
                    "Yes"
                } else {
                    "No"
                },
            ),
        ],
        ..Detail::default()
    }
}

fn network_detail(perf: &PerformanceProvider, index: u32) -> Detail {
    let Some(net) = perf.networks.get(&index) else { return Detail::default() };
    let adapter = &net.adapter;
    let max = nice_ceiling(net.receive.peak().max(net.send.peak()).max(100_000.0));
    let first = |v4: bool| {
        adapter
            .addresses
            .iter()
            .find(|ip| matches!(ip, IpAddr::V4(_)) == v4 && !matches!(ip, IpAddr::V6(v6) if v6.is_unicast_link_local()))
            .map_or_else(|| "—".to_owned(), ToString::to_string)
    };
    Detail {
        title: network_title(adapter.wireless).into(),
        model: adapter.description.clone(),
        big: vec![keyed(
            graph(
                "network.throughput",
                NETWORK,
                "Throughput (receive, send)",
                format::bits_per_second(max),
                &net.receive,
                max,
                Some(&net.send),
            ),
            format!("net.{index}.receive+net.{index}.send;net.{index}.receive;net.{index}.send"),
        )],
        top: vec![
            keyed_stat("Send", format::bits_per_second(net.send_rate), format!("net.{index}.send")),
            keyed_stat("Receive", format::bits_per_second(net.receive_rate), format!("net.{index}.receive")),
        ],
        info: vec![
            stat("Adapter name", adapter.alias.clone()),
            stat("Connection type", network_title(adapter.wireless)),
            stat("Link speed", format::bits_per_second(adapter.link_speed as f64)),
            stat("IPv4 address", first(true)),
            stat("IPv6 address", first(false)),
        ],
        ..Detail::default()
    }
}

fn gpu_detail(perf: &PerformanceProvider, index: usize) -> Detail {
    let Some(gpu) = perf.gpus.get(index) else { return Detail::default() };
    let info = &gpu.info;
    let latest = &gpu.latest;
    let engine_graphs: Vec<GraphData> = gpu
        .engines
        .iter()
        .map(|(label, history)| {
            let now = latest.engines.iter().find(|(l, _)| l == label).map_or(0.0, |(_, v)| *v);
            keyed(
                graph("gpu.engines", GPU, label, format!("{now:.0}%"), history, 1.0, None),
                format!("gpu.{index}.engine.{}", slug(label)),
            )
        })
        .collect();
    let pairs = engine_graphs
        .chunks(2)
        .map(|pair| GraphPair {
            left: pair[0].clone(),
            right: pair.get(1).cloned().unwrap_or_default(),
            has_right: pair.len() == 2,
        })
        .collect();
    let total = info.dedicated_memory + info.shared_memory;
    let celsius = |value: Option<f32>| value.map_or_else(|| "—".to_owned(), |t| format!("{t:.0} °C"));
    let vendor = latest.vendor.unwrap_or_default();

    let mut small = vec![
        keyed(
            graph(
                "gpu.dedicated",
                GPU,
                "Dedicated GPU memory",
                format::bytes(info.dedicated_memory),
                &gpu.dedicated,
                info.dedicated_memory.max(1) as f64,
                None,
            ),
            format!("gpu.{index}.dedicated"),
        ),
        keyed(
            graph(
                "gpu.shared",
                GPU,
                "Shared GPU memory",
                format::bytes(info.shared_memory),
                &gpu.shared,
                info.shared_memory.max(1) as f64,
                None,
            ),
            format!("gpu.{index}.shared"),
        ),
    ];
    if !gpu.temperature.is_empty() {
        let ceiling = nice_ceiling(gpu.hotspot.peak().max(gpu.temperature.peak())).max(100.0);
        let title = if gpu.hotspot.is_empty() { "Temperature" } else { "Temperature (edge, hotspot)" };
        let second = (!gpu.hotspot.is_empty()).then_some(&gpu.hotspot);
        let keys = if gpu.hotspot.is_empty() {
            format!("gpu.{index}.temp")
        } else {
            format!("gpu.{index}.temp+gpu.{index}.hotspot;gpu.{index}.temp;gpu.{index}.hotspot")
        };
        let temperature =
            graph("gpu.temperature", GPU, title, format!("{ceiling:.0} °C"), &gpu.temperature, ceiling, second);
        small.push(keyed(temperature, keys));
    }

    let mut bottom = vec![
        stat("GPU memory", used_of(latest.dedicated_used + latest.shared_used, total)),
        keyed_stat("GPU temperature", celsius(latest.edge_c()), format!("gpu.{index}.temp")),
    ];
    if latest.vendor.is_some() {
        bottom.push(keyed_stat("Hotspot", celsius(vendor.hotspot_c), format!("gpu.{index}.hotspot")));
        bottom.push(keyed_stat("Memory temp", celsius(vendor.memory_c), format!("gpu.{index}.memory-temp")));
    }

    let mut details = vec![
        stat("Driver version", info.driver_version.clone().unwrap_or_else(|| "—".into())),
        stat("Driver date", info.driver_date.clone().unwrap_or_else(|| "—".into())),
    ];
    if latest.vendor.is_some() {
        let or_dash = |value: Option<String>| value.unwrap_or_else(|| "—".into());
        let fan = match (vendor.fan_rpm, vendor.fan_percent) {
            (Some(0), _) | (None, Some(0)) => Some("Stopped (zero-RPM idle)".to_owned()),
            (Some(rpm), _) => Some(format!("{rpm} RPM")),
            (None, Some(percent)) => Some(format!("{percent}%")),
            (None, None) => None,
        };
        details.push(stat("Fan", or_dash(fan)));
        details.push(stat("Board power", or_dash(vendor.power_w.map(|w| format!("{w} W")))));
        details.push(stat("Core clock", or_dash(vendor.core_clock_mhz.map(|mhz| format!("{mhz} MHz")))));
        details.push(stat("Memory clock", or_dash(vendor.memory_clock_mhz.map(|mhz| format!("{mhz} MHz")))));
    }

    Detail {
        title: format!("GPU {index}"),
        model: info.name.clone(),
        pairs,
        small,
        top: vec![
            keyed_stat("Utilization", format!("{:.0}%", gpu.utilization_now()), format!("gpu.{index}.load")),
            keyed_stat(
                "Dedicated GPU memory",
                used_of(latest.dedicated_used, info.dedicated_memory),
                format!("gpu.{index}.dedicated"),
            ),
            keyed_stat(
                "Shared GPU memory",
                used_of(latest.shared_used, info.shared_memory),
                format!("gpu.{index}.shared"),
            ),
        ],
        bottom,
        info: details
            .into_iter()
            .chain([stat(
                "Physical location",
                info.location.map_or_else(
                    || "—".to_owned(),
                    |(bus, device, function)| format!("PCI bus {bus}, device {device}, function {function}"),
                ),
            )])
            .collect(),
        ..Detail::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ccds_must_be_even_runs_of_ccxs() {
        assert_eq!(uniform_ccds(&[0, 0, 1, 1]), Some(2));
        // Keys are opaque: the socket's bits ride above the CCD's.
        assert_eq!(uniform_ccds(&[0, 0, 8, 8]), Some(2));
        assert_eq!(uniform_ccds(&[5]), Some(1));
        assert_eq!(uniform_ccds(&[0, 0, 1]), None, "uneven");
        assert_eq!(uniform_ccds(&[0, 1, 0, 1]), None, "a CCD's CCXs apart");
        assert_eq!(uniform_ccds(&[]), None);
    }
}
