//! Every figure the Performance page shows, by a stable id, so the history service can record
//! them all and the History page and the header can show any of them.
//!
//! Ids are short lower-case words joined by dots: `cpu`, `cpu.ccd.1.temp`, `disk.0.read`,
//! `net.18.receive`, `gpu.0.hotspot`. A graph shows one id, or two joined by `+` (a second line
//! over the first, like a GPU's hotspot over its temperature).

use crate::archive::SecondRecord;
use crate::format;
use crate::history::{History, nice_ceiling};
use crate::json::Json;
use crate::perf::{PerformanceProvider, cpu_temperature};

/// Device kinds, matching `Theme.device-color`.
pub const CPU: i32 = 0;
pub const MEMORY: i32 = 1;
pub const DISK: i32 = 2;
pub const NETWORK: i32 = 3;
pub const GPU: i32 = 4;

/// The longest id kept, and the most graphs a page or the header holds.
pub const MAX_ID: usize = 64;
pub const MAX_GRAPHS: usize = 48;
pub const MAX_HEADER: usize = 8;

/// What a series measures, which says how to show it and how to store it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    /// `0..=1`, shown as a percentage.
    Fraction,
    Celsius,
    Bytes,
    BytesPerSecond,
    BitsPerSecond,
    Watts,
    Megahertz,
    Rpm,
}

impl Unit {
    const ALL: [Unit; 8] = [
        Unit::Fraction,
        Unit::Celsius,
        Unit::Bytes,
        Unit::BytesPerSecond,
        Unit::BitsPerSecond,
        Unit::Watts,
        Unit::Megahertz,
        Unit::Rpm,
    ];

    pub fn code(self) -> u8 {
        Self::ALL.iter().position(|u| *u == self).unwrap_or(0) as u8
    }

    pub fn from_code(code: u8) -> Option<Self> {
        Self::ALL.get(usize::from(code)).copied()
    }

    /// Stored values are multiplied by this, so they fit a half float (which tops out at 65504):
    /// bytes as GiB, rates as millions per second.
    pub fn scale(self) -> f64 {
        match self {
            Unit::Bytes => 1.0 / f64::from(1u32 << 30),
            Unit::BytesPerSecond | Unit::BitsPerSecond => 1e-6,
            _ => 1.0,
        }
    }

    pub fn format(self, value: f64) -> String {
        match self {
            Unit::Fraction => format!("{:.0}%", value * 100.0),
            Unit::Celsius => format!("{value:.0} °C"),
            Unit::Bytes => format::bytes(value.max(0.0) as u64),
            Unit::BytesPerSecond => match format::rate(value) {
                empty if empty.is_empty() => "0 B/s".to_owned(),
                rate => rate,
            },
            Unit::BitsPerSecond => format::bits_per_second(value),
            Unit::Watts => format!("{value:.0} W"),
            Unit::Megahertz => format::mhz(value),
            Unit::Rpm => format!("{value:.0} RPM"),
        }
    }

    /// The top of a graph that peaked at `peak`: the series' limit where it has one.
    pub fn ceiling(self, peak: f64, capacity: Option<f64>) -> f64 {
        match (self, capacity) {
            (Unit::Fraction, _) => 1.0,
            (_, Some(capacity)) if capacity > 0.0 => capacity,
            (Unit::Celsius, _) => nice_ceiling(peak.max(50.0)),
            _ => nice_ceiling(peak),
        }
    }
}

/// One series and its value now.
#[derive(Debug, Clone, PartialEq)]
pub struct Reading {
    pub id: String,
    /// The device it belongs to: `CPU`, `GPU 0`, `Disk 1 (C:)`, `Ethernet (Office)`.
    pub group: String,
    /// What it measures, e.g. `hotspot`.
    pub name: String,
    pub unit: Unit,
    pub kind: i32,
    /// The most it can be (installed memory, a GPU's memory), where there's such a limit.
    pub capacity: Option<f64>,
    /// `None` when the sensor gave nothing this time.
    pub value: Option<f64>,
}

impl Reading {
    pub fn title(&self) -> String {
        format!("{} {}", self.group, self.name)
    }
}

/// Series the service keeps in its `.seconds` files (with a week of seconds); the rest go in
/// the `.series` files.
pub fn is_builtin(id: &str) -> bool {
    matches!(id, "cpu" | "cpu.temp" | "memory" | "memory.commit" | "disk" | "network" | "gpu")
}

/// A built-in series' value in a `.seconds` record.
pub fn builtin_value(record: &SecondRecord, id: &str) -> Option<f64> {
    let value = match id {
        "cpu" => f64::from(record.cpu),
        "cpu.temp" => f64::from(record.cpu_temp),
        "memory" => record.memory_used as f64,
        "memory.commit" => record.commit as f64,
        "disk" => f64::from(record.disk_read) + f64::from(record.disk_write),
        "network" => f64::from(record.sent) + f64::from(record.received),
        "gpu" => f64::from(record.gpu),
        _ => return None,
    };
    value.is_finite().then_some(value)
}

/// A built-in series' unit, for when the device that had it is gone.
pub fn builtin_unit(id: &str) -> Option<Unit> {
    Some(match id {
        "cpu" | "gpu" => Unit::Fraction,
        "cpu.temp" => Unit::Celsius,
        "memory" | "memory.commit" => Unit::Bytes,
        "disk" | "network" => Unit::BytesPerSecond,
        _ => return None,
    })
}

/// The device kind an id belongs to, by its first word.
pub fn kind_of(id: &str) -> i32 {
    match id.split('.').next().unwrap_or_default() {
        "memory" => MEMORY,
        "disk" => DISK,
        "net" | "network" => NETWORK,
        "gpu" => GPU,
        _ => CPU,
    }
}

/// A name short enough for the header's small graphs: `CPU temp`, `VRAM`, `CCD 1 temp`.
pub fn short_label(readings: &[Reading], id: &str) -> String {
    let parts: Vec<&str> = id.split('.').collect();
    let gpus = readings.iter().filter(|r| r.id.starts_with("gpu.") && r.id.ends_with(".load")).count();
    let gpu = |n: &str| if gpus > 1 { format!("GPU {n}") } else { "GPU".to_owned() };
    match parts[..] {
        ["cpu"] => "CPU".into(),
        ["cpu", "temp"] => "CPU temp".into(),
        ["cpu", "speed"] => "CPU clock".into(),
        ["memory"] => "Memory".into(),
        ["memory", "commit"] => "Committed".into(),
        ["disk"] => "Disks".into(),
        ["network"] => "Network".into(),
        ["gpu"] => "GPU".into(),
        ["cpu", "core", i] => format!("Core {i}"),
        ["cpu", "core", i, "temp"] => format!("Core {i} temp"),
        ["cpu", "ccd", n, "temp"] => format!("CCD {n} temp"),
        ["cpu", "ccx", _] => readings
            .iter()
            .find(|r| r.id == id)
            .map_or_else(|| id.to_owned(), |r| r.name.trim_end_matches(" utilization").to_owned()),
        ["disk", n, "active"] => format!("Disk {n}"),
        ["disk", n, metric] => format!("Disk {n} {metric}"),
        ["net", _, "receive"] => "Net in".into(),
        ["net", _, "send"] => "Net out".into(),
        ["gpu", n, "load"] => gpu(n),
        ["gpu", n, "temp"] => format!("{} temp", gpu(n)),
        ["gpu", n, "hotspot"] => format!("{} hotspot", gpu(n)),
        ["gpu", _, "memory-temp"] => "VRAM temp".into(),
        ["gpu", _, "dedicated"] => "VRAM".into(),
        ["gpu", _, "shared"] => "Shared GPU".into(),
        ["gpu", n, "power"] => format!("{} power", gpu(n)),
        ["gpu", n, "fan" | "fan-percent"] => format!("{} fan", gpu(n)),
        ["gpu", n, "clock"] => format!("{} clock", gpu(n)),
        ["gpu", _, "memory-clock"] => "VRAM clock".into(),
        _ => readings.iter().find(|r| r.id == id).map_or_else(|| id.to_owned(), |r| r.name.clone()),
    }
}

/// The Performance page's own graph of a series (the last minute), where it keeps one.
pub fn live_history<'a>(perf: &'a PerformanceProvider, id: &str) -> Option<&'a History> {
    let parts: Vec<&str> = id.split('.').collect();
    let number = |text: &str| text.parse::<usize>().ok();
    match parts[..] {
        ["cpu"] => Some(&perf.cpu),
        ["cpu", "temp"] => Some(&perf.cpu_temp).filter(|h| !h.is_empty()),
        ["memory"] => Some(&perf.memory_used),
        ["memory", "commit"] => Some(&perf.commit),
        ["cpu", "core", i] => perf.cores.get(number(i)?),
        ["cpu", "core", i, "temp"] => perf.core_temps.get(number(i)?),
        ["cpu", "ccx", i] => perf.l3_groups.get(number(i)?),
        ["cpu", "ccd", n, "temp"] => perf.ccd_temps.get(number(n)?.checked_sub(1)?).map(|(_, h)| h),
        ["disk", n, metric] => {
            let disk = perf.disks.get(&n.parse().ok()?)?;
            match metric {
                "active" => Some(&disk.active),
                "read" => Some(&disk.read),
                "write" => Some(&disk.write),
                _ => None,
            }
        }
        ["net", i, direction] => {
            let net = perf.networks.get(&i.parse().ok()?)?;
            match direction {
                "receive" => Some(&net.receive),
                "send" => Some(&net.send),
                _ => None,
            }
        }
        ["gpu", n, metric, ..] => {
            let gpu = perf.gpus.get(number(n)?)?;
            match (metric, parts.get(3)) {
                ("load", None) => Some(&gpu.utilization),
                ("temp", None) => Some(&gpu.temperature),
                ("hotspot", None) => Some(&gpu.hotspot),
                ("dedicated", None) => Some(&gpu.dedicated),
                ("shared", None) => Some(&gpu.shared),
                ("engine", Some(engine)) => {
                    gpu.engines.iter().find(|(label, _)| slug(label) == *engine).map(|(_, h)| h)
                }
                _ => None,
            }
            .filter(|h| !h.is_empty())
        }
        _ => None,
    }
}

/// A valid id: short, lower-case letters, digits, dots and dashes.
pub fn is_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID
        && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-')
}

/// `Video Decode 1` → `video-decode-1`.
pub fn slug(label: &str) -> String {
    let mut slug = String::new();
    for c in label.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    slug.trim_end_matches('-').chars().take(24).collect()
}

/// Every series the provider has now, device by device in the Performance page's order.
pub fn readings(perf: &PerformanceProvider) -> Vec<Reading> {
    let mut out = Vec::new();
    let mut add =
        |id: String, group: &str, name: &str, unit: Unit, kind: i32, capacity: Option<f64>, value: Option<f64>| {
            out.push(Reading {
                id,
                group: group.to_owned(),
                name: name.to_owned(),
                unit,
                kind,
                capacity,
                value: value.filter(|v| v.is_finite()),
            });
        };

    // CPU.
    let temps = perf.cpu_temps.as_ref();
    add("cpu".into(), "CPU", "utilization", Unit::Fraction, CPU, None, perf.cpu.latest());
    if let Some(celsius) = temps.and_then(cpu_temperature) {
        add("cpu.temp".into(), "CPU", "temperature", Unit::Celsius, CPU, None, Some(f64::from(celsius)));
    }
    if let Some(mhz) = perf.cpu_speed_mhz {
        add("cpu.speed".into(), "CPU", "speed", Unit::Megahertz, CPU, None, Some(mhz));
    }
    let ccd_sensors = temps.map_or(0, |t| t.ccds.len());
    if perf.l3_groups.len() > 1 {
        // Each L3 group is a whole CCD when there's one per CCD (Zen 3 and later).
        let mut ccds = perf.machine.cpu.l3_ccds.clone();
        ccds.sort_unstable();
        ccds.dedup();
        let one_per_ccd = ccds.len() == perf.l3_groups.len() || ccd_sensors == perf.l3_groups.len();
        for (i, history) in perf.l3_groups.iter().enumerate() {
            let name = if one_per_ccd { format!("CCD {} utilization", i + 1) } else { format!("CCX {i} utilization") };
            add(format!("cpu.ccx.{i}"), "CPU", &name, Unit::Fraction, CPU, None, history.latest());
        }
    }
    if let Some(temps) = temps {
        // Numbered by position, as on the Performance page: sensor slots can be sparse.
        for (n, (_, celsius)) in temps.ccds.iter().enumerate() {
            let name = format!("CCD {} temperature", n + 1);
            add(format!("cpu.ccd.{}.temp", n + 1), "CPU", &name, Unit::Celsius, CPU, None, Some(f64::from(*celsius)));
        }
    }
    for (i, history) in perf.cores.iter().enumerate() {
        add(
            format!("cpu.core.{i}"),
            "CPU",
            &format!("core {i} utilization"),
            Unit::Fraction,
            CPU,
            None,
            history.latest(),
        );
    }
    if let Some(temps) = temps {
        for (i, celsius) in temps.cores.iter().enumerate() {
            let name = format!("core {i} temperature");
            add(format!("cpu.core.{i}.temp"), "CPU", &name, Unit::Celsius, CPU, None, Some(f64::from(*celsius)));
        }
    }

    // Memory.
    if let Some(memory) = perf.memory {
        let used = memory.physical_total.saturating_sub(memory.physical_available);
        let total = Some(memory.physical_total as f64);
        add("memory".into(), "Memory", "in use", Unit::Bytes, MEMORY, total, Some(used as f64));
        let limit = Some(memory.commit_limit as f64);
        add(
            "memory.commit".into(),
            "Memory",
            "committed",
            Unit::Bytes,
            MEMORY,
            limit,
            Some(memory.commit_total as f64),
        );
    }

    // Disks.
    if !perf.disks.is_empty() {
        let total = perf.disks.values().map(|d| d.latest.read + d.latest.write).sum();
        add("disk".into(), "All disks", "read and write", Unit::BytesPerSecond, DISK, None, Some(total));
    }
    for (number, disk) in &perf.disks {
        let latest = &disk.latest;
        let group = disk_group(*number, &latest.letters);
        add(
            format!("disk.{number}.active"),
            &group,
            "active time",
            Unit::Fraction,
            DISK,
            None,
            Some(latest.active / 100.0),
        );
        add(format!("disk.{number}.read"), &group, "read", Unit::BytesPerSecond, DISK, None, Some(latest.read));
        add(format!("disk.{number}.write"), &group, "write", Unit::BytesPerSecond, DISK, None, Some(latest.write));
    }

    // Network adapters.
    if !perf.networks.is_empty() {
        let total = perf.networks.values().map(|n| n.send_rate + n.receive_rate).sum::<f64>() / 8.0;
        add(
            "network".into(),
            "All network adapters",
            "sent and received",
            Unit::BytesPerSecond,
            NETWORK,
            None,
            Some(total),
        );
    }
    for (index, net) in &perf.networks {
        let kind = if net.adapter.wireless { "Wi-Fi" } else { "Ethernet" };
        let group = format!("{kind} ({})", net.adapter.alias);
        add(
            format!("net.{index}.receive"),
            &group,
            "received",
            Unit::BitsPerSecond,
            NETWORK,
            None,
            Some(net.receive_rate),
        );
        add(format!("net.{index}.send"), &group, "sent", Unit::BitsPerSecond, NETWORK, None, Some(net.send_rate));
    }

    // GPUs.
    if !perf.gpus.is_empty() {
        let busiest = perf.gpus.iter().map(|g| g.utilization_now()).fold(0.0, f64::max) / 100.0;
        add("gpu".into(), "GPU", "(busiest engine of any)", Unit::Fraction, GPU, None, Some(busiest));
    }
    for (n, gpu) in perf.gpus.iter().enumerate() {
        let group = format!("GPU {n}");
        let latest = &gpu.latest;
        let vendor = latest.vendor.unwrap_or_default();
        let celsius = |value: Option<f32>| value.map(f64::from);
        let number = |value: Option<u32>| value.map(f64::from);
        add(format!("gpu.{n}.load"), &group, "load", Unit::Fraction, GPU, None, Some(gpu.utilization_now() / 100.0));
        if let Some(edge) = latest.edge_c() {
            add(format!("gpu.{n}.temp"), &group, "temperature", Unit::Celsius, GPU, None, Some(f64::from(edge)));
        }
        if vendor.hotspot_c.is_some() {
            add(format!("gpu.{n}.hotspot"), &group, "hotspot", Unit::Celsius, GPU, None, celsius(vendor.hotspot_c));
        }
        if vendor.memory_c.is_some() {
            add(
                format!("gpu.{n}.memory-temp"),
                &group,
                "memory temperature",
                Unit::Celsius,
                GPU,
                None,
                celsius(vendor.memory_c),
            );
        }
        let dedicated = Some(gpu.info.dedicated_memory as f64).filter(|m| *m > 0.0);
        add(
            format!("gpu.{n}.dedicated"),
            &group,
            "dedicated memory",
            Unit::Bytes,
            GPU,
            dedicated,
            Some(latest.dedicated_used as f64),
        );
        let shared = Some(gpu.info.shared_memory as f64).filter(|m| *m > 0.0);
        add(
            format!("gpu.{n}.shared"),
            &group,
            "shared memory",
            Unit::Bytes,
            GPU,
            shared,
            Some(latest.shared_used as f64),
        );
        if vendor.power_w.is_some() {
            add(format!("gpu.{n}.power"), &group, "board power", Unit::Watts, GPU, None, number(vendor.power_w));
        }
        if vendor.fan_rpm.is_some() {
            add(format!("gpu.{n}.fan"), &group, "fan", Unit::Rpm, GPU, None, number(vendor.fan_rpm));
        } else if let Some(percent) = vendor.fan_percent {
            add(
                format!("gpu.{n}.fan-percent"),
                &group,
                "fan",
                Unit::Fraction,
                GPU,
                None,
                Some(f64::from(percent) / 100.0),
            );
        }
        if vendor.core_clock_mhz.is_some() {
            add(
                format!("gpu.{n}.clock"),
                &group,
                "core clock",
                Unit::Megahertz,
                GPU,
                None,
                number(vendor.core_clock_mhz),
            );
        }
        if vendor.memory_clock_mhz.is_some() {
            let clock = number(vendor.memory_clock_mhz);
            add(format!("gpu.{n}.memory-clock"), &group, "memory clock", Unit::Megahertz, GPU, None, clock);
        }
        for (label, history) in &gpu.engines {
            let id = format!("gpu.{n}.engine.{}", slug(label));
            if is_id(&id) {
                add(id, &group, &format!("{label} engine"), Unit::Fraction, GPU, None, history.latest());
            }
        }
    }
    out
}

fn disk_group(number: u32, letters: &str) -> String {
    if letters.is_empty() { format!("Disk {number}") } else { format!("Disk {number} ({letters})") }
}

/// A graph's title: its series' title, or for two the device and both names: `GPU 0
/// temperature and hotspot`. Ids not in `readings` (a device that's gone) show as themselves.
pub fn graph_title(readings: &[Reading], series: &[String]) -> String {
    let find = |id: &str| readings.iter().find(|r| r.id == id);
    match series {
        [one] => find(one).map_or_else(|| one.clone(), Reading::title),
        [a, b, ..] => match (find(a), find(b)) {
            (Some(a), Some(b)) if a.group == b.group => format!("{} {} and {}", a.group, a.name, b.name),
            (Some(a), Some(b)) => format!("{} and {}", a.title(), b.title()),
            _ => format!("{a} and {b}"),
        },
        [] => String::new(),
    }
}

// ----- Graph layouts -----

/// One graph on the History page: its series (one or two), how much of the page's width it
/// takes in twelfths, and its height in pixels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphSpec {
    pub series: Vec<String>,
    pub span: u8,
    pub height: u16,
}

pub const MIN_SPAN: u8 = 2;
pub const MAX_SPAN: u8 = 12;
pub const MIN_HEIGHT: u16 = 60;
pub const MAX_HEIGHT: u16 = 900;

impl GraphSpec {
    pub fn new(series: &str, span: u8, height: u16) -> Self {
        Self {
            series: series.split('+').map(str::to_owned).collect(),
            span: span.clamp(MIN_SPAN, MAX_SPAN),
            height: height.clamp(MIN_HEIGHT, MAX_HEIGHT),
        }
    }

    /// `gpu.0.temp+gpu.0.hotspot`.
    pub fn key(&self) -> String {
        self.series.join("+")
    }
}

/// The History page's graphs before the user changes them.
pub fn default_graphs() -> Vec<GraphSpec> {
    ["cpu", "memory", "disk", "network", "gpu", "cpu.temp"].into_iter().map(|id| GraphSpec::new(id, 6, 130)).collect()
}

/// `cpu@6x130|gpu.0.temp+gpu.0.hotspot@12x220` → graphs; anything malformed is skipped.
pub fn parse_graphs(value: &str) -> Vec<GraphSpec> {
    value
        .split('|')
        .filter_map(|graph| {
            let (series, size) = graph.trim().split_once('@')?;
            let (span, height) = size.split_once('x')?;
            let series = parse_key(series)?;
            Some(GraphSpec::new(&series, span.parse().ok()?, height.parse().ok()?))
        })
        .take(MAX_GRAPHS)
        .collect()
}

pub fn format_graphs(graphs: &[GraphSpec]) -> String {
    let graphs: Vec<String> = graphs.iter().map(|g| format!("{}@{}x{}", g.key(), g.span, g.height)).collect();
    graphs.join("|")
}

/// A graph key with one or two valid ids, or `None`.
pub fn parse_key(key: &str) -> Option<String> {
    let ids: Vec<&str> = key.split('+').map(str::trim).collect();
    (matches!(ids.len(), 1 | 2) && ids.iter().all(|id| is_id(id))).then(|| ids.join("+"))
}

/// Where each graph goes: its column (in twelfths), width (twelfths), top and height, flowing
/// left to right and onto a new row when a graph doesn't fit. Rows are as tall as their tallest
/// graph. Returns the places and the height of them all.
pub fn flow(graphs: &[GraphSpec], gap: f32) -> (Vec<Placement>, f32) {
    let mut placements = Vec::with_capacity(graphs.len());
    let (mut column, mut top, mut row_height) = (0u8, 0.0f32, 0.0f32);
    for graph in graphs {
        if column > 0 && column + graph.span > MAX_SPAN {
            top += row_height + gap;
            column = 0;
            row_height = 0.0;
        }
        let height = f32::from(graph.height);
        placements.push(Placement { column, span: graph.span, top, height });
        column += graph.span;
        row_height = row_height.max(height);
    }
    (placements, if graphs.is_empty() { 0.0 } else { top + row_height })
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placement {
    pub column: u8,
    pub span: u8,
    pub top: f32,
    pub height: f32,
}

/// Where a graph dropped with its middle at `(column, y)` (column in twelfths, may be
/// fractional) belongs: before the graph under that point, or after it when it's past that
/// graph's middle. Past every graph is the end.
pub fn drop_index(placements: &[Placement], column: f32, y: f32) -> usize {
    for (i, place) in placements.iter().enumerate() {
        let left = f32::from(place.column);
        let right = left + f32::from(place.span);
        let in_row = y >= place.top && y <= place.top + place.height;
        if in_row && column >= left && column < right {
            return if column < (left + right) / 2.0 { i } else { i + 1 };
        }
        // Above this graph's row (in the gap between rows, or left of its first graph).
        if y < place.top || (in_row && column < left) {
            return i;
        }
    }
    placements.len()
}

/// Moves graph `from` to before position `to` (a [`drop_index`]).
pub fn move_graph(graphs: &mut Vec<GraphSpec>, from: usize, to: usize) {
    if from >= graphs.len() {
        return;
    }
    let graph = graphs.remove(from);
    let to = if to > from { to - 1 } else { to };
    graphs.insert(to.min(graphs.len()), graph);
}

// ----- Layout files and templates -----

/// The largest layout file read.
pub const MAX_LAYOUT_FILE: u64 = 256 * 1024;

/// A History layout as JSON, for saving and sharing: `{"ferrotop": "history-layout",
/// "version": 1, "name": ..., "graphs": [{"series": ["cpu"], "width": 6, "height": 130}, ...]}`,
/// where `width` is in twelfths of the page and `height` in pixels.
pub fn layout_to_json(name: &str, graphs: &[GraphSpec]) -> String {
    let mut name_json = String::from('"');
    for c in name.chars() {
        match c {
            '"' => name_json.push_str("\\\""),
            '\\' => name_json.push_str("\\\\"),
            c if c.is_control() => {}
            c => name_json.push(c),
        }
    }
    name_json.push('"');
    let graphs: Vec<String> = graphs
        .iter()
        .map(|g| {
            let series: Vec<String> = g.series.iter().map(|id| format!("\"{id}\"")).collect();
            format!("    {{ \"series\": [{}], \"width\": {}, \"height\": {} }}", series.join(", "), g.span, g.height)
        })
        .collect();
    format!(
        "{{\n  \"ferrotop\": \"history-layout\",\n  \"version\": 1,\n  \"name\": {name_json},\n  \"graphs\": [\n{}\n  ]\n}}\n",
        graphs.join(",\n")
    )
}

/// A layout file's name (if it gives one) and graphs; `None` if it isn't a layout.
pub fn layout_from_json(text: &str) -> Option<(Option<String>, Vec<GraphSpec>)> {
    let json = Json::parse(text.trim_start_matches(|c: char| u32::from(c) == 0xfeff))?;
    let graphs = json.get("graphs")?;
    if !matches!(graphs, Json::Array(_)) {
        return None;
    }
    let specs = graphs
        .items()
        .iter()
        .filter_map(|graph| {
            let series = match graph.get("series")? {
                Json::String(id) => id.clone(),
                list => {
                    list.items().iter().map(|id| id.as_str().map(str::to_owned)).collect::<Option<Vec<_>>>()?.join("+")
                }
            };
            let series = parse_key(&series)?;
            let number = |key: &str, default: f64| graph.get(key).and_then(Json::as_number).unwrap_or(default);
            let span = number("width", 6.0).clamp(0.0, 255.0) as u8;
            let height = number("height", 130.0).clamp(0.0, 65535.0) as u16;
            Some(GraphSpec::new(&series, span, height))
        })
        .take(MAX_GRAPHS)
        .collect();
    let name = json
        .get("name")
        .and_then(Json::as_str)
        .map(|n| n.chars().filter(|c| !c.is_control()).take(60).collect::<String>())
        .filter(|n| !n.trim().is_empty());
    Some((name, specs))
}

/// Built-in History layouts for this machine's sensors, by name.
pub fn templates(readings: &[Reading]) -> Vec<(&'static str, Vec<GraphSpec>)> {
    let has = |id: &str| readings.iter().any(|r| r.id == id);
    let ids = |prefix: &str, suffix: &str| -> Vec<String> {
        readings.iter().filter(|r| r.id.starts_with(prefix) && r.id.ends_with(suffix)).map(|r| r.id.clone()).collect()
    };
    let gpus: Vec<String> = ids("gpu.", ".load").iter().map(|id| id.trim_end_matches(".load").to_owned()).collect();
    let gpu_temp = |gpu: &str| match (has(&format!("{gpu}.temp")), has(&format!("{gpu}.hotspot"))) {
        (true, true) => Some(format!("{gpu}.temp+{gpu}.hotspot")),
        (true, false) => Some(format!("{gpu}.temp")),
        _ => None,
    };
    let activity = ["cpu", "memory", "disk", "network", "gpu"];
    let present =
        |list: &[&str]| -> Vec<String> { list.iter().filter(|id| has(id)).map(|id| (*id).to_owned()).collect() };
    let graphs = |list: Vec<String>, span: u8, height: u16| -> Vec<GraphSpec> {
        list.iter().map(|key| GraphSpec::new(key, span, height)).collect()
    };

    let mut overview = present(&activity);
    overview.extend(present(&["cpu.temp"]));
    overview.extend(gpus.iter().filter_map(|gpu| gpu_temp(gpu)));

    let mut temperatures = graphs(present(&["cpu.temp"]), 12, 200);
    temperatures.extend(graphs(gpus.iter().filter_map(|gpu| gpu_temp(gpu)).collect(), 12, 200));
    let mut sensors = ids("cpu.ccd.", ".temp");
    sensors.extend(gpus.iter().map(|gpu| format!("{gpu}.memory-temp")).filter(|id| has(id)));
    temperatures.extend(graphs(sensors, 6, 130));
    temperatures.extend(graphs(present(&activity), 4, 100));

    let mut gpu = Vec::new();
    for id in &gpus {
        gpu.extend(graphs(present(&[&format!("{id}.load")]), 6, 160));
        gpu.extend(graphs(gpu_temp(id).into_iter().collect(), 6, 160));
        let more = [format!("{id}.dedicated"), format!("{id}.power"), format!("{id}.fan"), format!("{id}.clock")];
        gpu.extend(graphs(more.into_iter().filter(|m| has(m)).collect(), 6, 110));
    }
    gpu.extend(graphs(present(&["cpu", "cpu.temp", "memory"]), 4, 100));

    let mut cpu = graphs(present(&["cpu"]), 12, 160);
    cpu.extend(graphs(present(&["cpu.temp", "cpu.speed"]), 6, 120));
    cpu.extend(graphs(ids("cpu.ccx.", ""), 6, 110));
    cpu.extend(graphs(ids("cpu.ccd.", ".temp"), 6, 110));

    vec![
        ("Overview", graphs(overview.clone(), 6, 130)),
        ("Temperatures", temperatures),
        ("GPU", gpu),
        ("CPU", cpu),
        ("Compact", graphs(overview, 4, 90)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_files_round_trip() {
        let graphs = vec![GraphSpec::new("cpu", 6, 130), GraphSpec::new("gpu.0.temp+gpu.0.hotspot", 12, 220)];
        let text = layout_to_json("Mine \"best\"", &graphs);
        assert_eq!(layout_from_json(&text), Some((Some("Mine \"best\"".to_owned()), graphs)));
        // A hand-written one: a single id as a string, sizes left out, and junk skipped.
        let text =
            r#"{"graphs": [{"series": "cpu.temp"}, {"series": ["BAD"]}, {"series": ["a", "b", "c"]}, {"width": 3}]}"#;
        assert_eq!(layout_from_json(text), Some((None, vec![GraphSpec::new("cpu.temp", 6, 130)])));
        assert_eq!(layout_from_json("[1, 2]"), None);
        assert_eq!(layout_from_json("not json"), None);
    }

    #[test]
    fn templates_use_only_present_sensors() {
        let reading = |id: &str| Reading {
            id: id.into(),
            group: String::new(),
            name: String::new(),
            unit: Unit::Fraction,
            kind: 0,
            capacity: None,
            value: None,
        };
        let readings: Vec<Reading> =
            ["cpu", "memory", "cpu.temp", "gpu", "gpu.0.load", "gpu.0.temp", "gpu.0.hotspot", "cpu.ccd.1.temp"]
                .into_iter()
                .map(reading)
                .collect();
        let templates = templates(&readings);
        let keys = |name: &str| -> Vec<String> {
            templates
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, g)| g.iter().map(GraphSpec::key).collect())
                .unwrap_or_default()
        };
        assert_eq!(keys("Overview"), ["cpu", "memory", "gpu", "cpu.temp", "gpu.0.temp+gpu.0.hotspot"]);
        assert_eq!(keys("Temperatures")[..3], ["cpu.temp", "gpu.0.temp+gpu.0.hotspot", "cpu.ccd.1.temp"]);
        assert!(
            templates.iter().all(|(_, graphs)| graphs
                .iter()
                .all(|g| g.series.iter().all(|id| readings.iter().any(|r| r.id == *id))))
        );
    }

    #[test]
    fn ids_and_slugs() {
        assert!(is_id("gpu.0.engine.video-decode-1"));
        assert!(!is_id("GPU.0") && !is_id("") && !is_id("a b") && !is_id(&"x".repeat(65)));
        assert_eq!(slug("Video Decode 1"), "video-decode-1");
        assert_eq!(slug("3D"), "3d");
        assert_eq!(slug("  Copy  "), "copy");
    }

    #[test]
    fn units_round_trip_their_codes() {
        for unit in Unit::ALL {
            assert_eq!(Unit::from_code(unit.code()), Some(unit));
        }
        assert_eq!(Unit::from_code(99), None);
        assert_eq!(Unit::Fraction.format(0.456), "46%");
        assert_eq!(Unit::Celsius.format(71.6), "72 °C");
        assert_eq!(Unit::Celsius.ceiling(40.0, None), 50.0);
        assert_eq!(Unit::Bytes.ceiling(3.0, Some(8.0)), 8.0);
        assert_eq!(Unit::Fraction.ceiling(0.2, Some(8.0)), 1.0);
    }

    #[test]
    fn graph_lists_round_trip_and_skip_junk() {
        let graphs = vec![GraphSpec::new("cpu", 6, 130), GraphSpec::new("gpu.0.temp+gpu.0.hotspot", 12, 220)];
        let text = format_graphs(&graphs);
        assert_eq!(text, "cpu@6x130|gpu.0.temp+gpu.0.hotspot@12x220");
        assert_eq!(parse_graphs(&text), graphs);
        let parsed = parse_graphs("cpu@1x10|bad id@6x100|a+b+c@6x100|memory@99x9999|disk@6");
        assert_eq!(parsed, vec![GraphSpec::new("cpu", 2, 60), GraphSpec::new("memory", 12, 900)]);
        assert!(parse_graphs("").is_empty());
    }

    #[test]
    fn graphs_flow_into_rows() {
        let graphs = vec![
            GraphSpec::new("a", 6, 100),
            GraphSpec::new("b", 6, 140),
            GraphSpec::new("c", 4, 100),
            GraphSpec::new("d", 12, 200),
        ];
        let (places, height) = flow(&graphs, 10.0);
        assert_eq!(places[0], Placement { column: 0, span: 6, top: 0.0, height: 100.0 });
        assert_eq!(places[1], Placement { column: 6, span: 6, top: 0.0, height: 140.0 });
        assert_eq!(places[2], Placement { column: 0, span: 4, top: 150.0, height: 100.0 });
        assert_eq!(places[3], Placement { column: 0, span: 12, top: 260.0, height: 200.0 });
        assert_eq!(height, 460.0);
    }

    #[test]
    fn drops_land_before_or_after_the_graph_underneath() {
        let graphs: Vec<GraphSpec> = ["a", "b", "c"].iter().map(|id| GraphSpec::new(id, 6, 100)).collect();
        let (places, _) = flow(&graphs, 10.0);
        assert_eq!(drop_index(&places, 1.0, 50.0), 0);
        assert_eq!(drop_index(&places, 5.0, 50.0), 1);
        assert_eq!(drop_index(&places, 7.0, 50.0), 1);
        assert_eq!(drop_index(&places, 2.0, 160.0), 2);
        assert_eq!(drop_index(&places, 10.0, 160.0), 3, "right of the last graph");
        assert_eq!(drop_index(&places, 3.0, 500.0), 3);
        assert_eq!(drop_index(&places, 3.0, 105.0), 2, "in the gap above the second row");

        let mut list = graphs.clone();
        move_graph(&mut list, 0, 3);
        assert_eq!(list.iter().map(GraphSpec::key).collect::<Vec<_>>(), ["b", "c", "a"]);
        move_graph(&mut list, 2, 0);
        assert_eq!(list.iter().map(GraphSpec::key).collect::<Vec<_>>(), ["a", "b", "c"]);
        move_graph(&mut list, 1, 2);
        assert_eq!(list.iter().map(GraphSpec::key).collect::<Vec<_>>(), ["a", "b", "c"], "dropped on itself");
    }

    #[test]
    fn readings_cover_the_devices() {
        let mut perf = PerformanceProvider::new();
        perf.update_cpu(0.25, &[]);
        let readings = readings(&perf);
        let cpu = readings.iter().find(|r| r.id == "cpu").expect("cpu");
        assert_eq!(cpu.value, Some(0.25));
        assert_eq!(cpu.title(), "CPU utilization");
        assert!(readings.iter().all(|r| is_id(&r.id)));
        assert_eq!(graph_title(&readings, &["cpu".into()]), "CPU utilization");
        assert_eq!(graph_title(&readings, &["gone.0".into()]), "gone.0");
        assert_eq!(short_label(&readings, "cpu"), "CPU");
        assert_eq!(short_label(&readings, "gpu.0.dedicated"), "VRAM");
        assert_eq!(short_label(&readings, "cpu.ccd.2.temp"), "CCD 2 temp");
        assert!(live_history(&perf, "cpu").is_some_and(|h| h.latest() == Some(0.25)));
        assert!(live_history(&perf, "gpu.0.load").is_none());
        assert_eq!(kind_of("net.3.send"), NETWORK);
        assert_eq!(builtin_unit("memory"), Some(Unit::Bytes));
    }
}
