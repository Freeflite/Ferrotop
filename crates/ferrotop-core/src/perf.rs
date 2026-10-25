//! Data for the Performance view: CPU, memory, disks, network adapters and GPUs.
//!
//! [`PerfCollector`] lives on the sampler thread (it owns the PDH query and GPU handles) and
//! produces [`PerfSample`]s; [`PerformanceProvider`] turns those into graph histories on the UI
//! thread. GPU engine and memory figures come from the same performance counters Task Manager
//! reads, which also give the per-process GPU columns.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ferrotop_sys::amd::Adl;
use ferrotop_sys::cpu_temp::{CpuTemps, CpuThermal, Unavailable};
use ferrotop_sys::disk::{DiskInfo, disk_info};
use ferrotop_sys::gpu::VendorSensors;
use ferrotop_sys::gpu::{Adapters, GpuInfo};
use ferrotop_sys::hardware::{self, CpuInfo, MemoryCounters, MemorySlot};
use ferrotop_sys::net::{self, NetAdapter};
use ferrotop_sys::nvidia::Nvml;
use ferrotop_sys::pdh::{CounterId, Query};
use ferrotop_sys::system::CpuTimes;

use crate::history::History;
use crate::lhm;

/// Samples kept for Performance graphs: one minute, like Task Manager.
pub const PERF_HISTORY: usize = 60;

/// A GPU adapter LUID as `(high, low)`.
type Luid = (i32, u32);

/// How often to retry PawnIO after it wasn't available (it may get installed meanwhile).
const PAWNIO_RETRY: Duration = Duration::from_secs(30);
/// How long to leave LibreHardwareMonitor alone after nothing answered on its port.
const LHM_RETRY: Duration = Duration::from_secs(10);
/// Consecutive failed PawnIO reads before the device is reopened.
const PAWNIO_MAX_FAILURES: u32 = 10;

/// Where the CPU temperatures came from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CpuTempSource {
    #[default]
    None,
    PawnIo,
    LibreHardwareMonitor,
}

/// Whether the PawnIO driver can be used, for explaining missing CPU temperatures.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PawnIoState {
    #[default]
    NotInstalled,
    NeedsAdmin,
    Unsupported,
    Failed,
    Ready,
}

/// The single figure to show for "CPU temperature": Tdie where it differs from Tctl, else Tctl or
/// the package sensor, else the hottest CCD or core.
pub fn cpu_temperature(temps: &CpuTemps) -> Option<f32> {
    temps
        .die_c
        .or(temps.package_c)
        .or_else(|| temps.ccds.iter().map(|(_, t)| *t).reduce(f32::max))
        .or_else(|| temps.cores.iter().copied().reduce(f32::max))
}

/// Facts that don't change while Ferrotop runs.
#[derive(Debug, Clone, Default)]
pub struct Machine {
    pub cpu: CpuInfo,
    pub installed_memory: Option<u64>,
    pub slots: Vec<MemorySlot>,
    pub gpus: Vec<GpuInfo>,
}

#[derive(Debug, Clone)]
pub struct DiskSample {
    pub info: Arc<DiskInfo>,
    /// Drive letters on this disk, e.g. `C:` or `E: F:`.
    pub letters: String,
    /// Percent of time busy, `0..=100`.
    pub active: f64,
    /// Bytes per second.
    pub read: f64,
    pub write: f64,
    pub response_ms: f64,
}

#[derive(Debug, Clone, Default)]
pub struct GpuSample {
    /// `(engine label, percent busy)`, in engine order.
    pub engines: Vec<(String, f64)>,
    pub dedicated_used: u64,
    pub shared_used: u64,
    pub temperature_c: Option<f32>,
    /// Hotspot/memory temperatures, fan, power and clocks from the GPU vendor's driver library
    /// (AMD ADL or NVIDIA NVML), when available.
    pub vendor: Option<VendorSensors>,
}

impl GpuSample {
    /// The best available overall GPU temperature: the vendor's main die sensor, else the
    /// reading Windows itself reports (which Task Manager shows).
    pub fn edge_c(&self) -> Option<f32> {
        self.vendor.and_then(|v| v.edge_c).or(self.temperature_c)
    }

    pub fn hotspot_c(&self) -> Option<f32> {
        self.vendor.and_then(|v| v.hotspot_c)
    }
}

/// One process's GPU use, summed across adapters.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProcessGpu {
    /// Percent of its busiest engine, `0..=100`.
    pub usage: f64,
    /// That engine, e.g. `GPU 0 - 3D`.
    pub engine: String,
    pub dedicated: u64,
    pub shared: u64,
}

#[derive(Debug, Clone, Default)]
pub struct PerfSample {
    pub machine: Arc<Machine>,
    pub cpu_speed_mhz: Option<f64>,
    pub memory: Option<MemoryCounters>,
    pub uptime: Duration,
    pub disks: Vec<DiskSample>,
    pub networks: Vec<NetAdapter>,
    /// Parallel to `machine.gpus`.
    pub gpus: Vec<GpuSample>,
    pub gpu_processes: HashMap<u32, ProcessGpu>,
    pub cpu_temps: Option<CpuTemps>,
    pub cpu_temp_source: CpuTempSource,
    pub pawnio: PawnIoState,
}

#[derive(Debug, Default)]
struct Counters {
    cpu_performance: Option<CounterId>,
    disk_idle: Option<CounterId>,
    disk_read: Option<CounterId>,
    disk_write: Option<CounterId>,
    disk_latency: Option<CounterId>,
    gpu_engine: Option<CounterId>,
    gpu_process_dedicated: Option<CounterId>,
    gpu_process_shared: Option<CounterId>,
    gpu_adapter_dedicated: Option<CounterId>,
    gpu_adapter_shared: Option<CounterId>,
}

/// Owns the performance-counter query and GPU handles; lives on the sampler thread.
#[derive(Debug)]
pub struct PerfCollector {
    query: Option<Query>,
    counters: Counters,
    adapters: Option<Adapters>,
    /// Vendor driver libraries, opened only when a GPU from that vendor is present.
    amd: Option<Adl>,
    nvidia: Option<Nvml>,
    machine: Arc<Machine>,
    disks: HashMap<u32, Arc<DiskInfo>>,
    thermal: Option<CpuThermal>,
    pawnio: PawnIoState,
    /// When to try PawnIO again; `None` once it's known not to support this CPU.
    pawnio_retry: Option<Instant>,
    pawnio_failures: u32,
    lhm_port: u16,
    lhm_retry: Instant,
}

impl Default for PerfCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl PerfCollector {
    pub fn new() -> Self {
        let adapters = Adapters::open().ok();
        let machine = Machine {
            cpu: hardware::cpu_info(),
            installed_memory: hardware::installed_memory(),
            slots: hardware::memory_slots().unwrap_or_default(),
            gpus: adapters.as_ref().map(|a| a.infos().cloned().collect()).unwrap_or_default(),
        };
        let mut query = Query::open().ok();
        let mut add = |path: &str| query.as_mut().and_then(|q| q.add(path).ok());
        let counters = Counters {
            cpu_performance: add(r"\Processor Information(_Total)\% Processor Performance"),
            disk_idle: add(r"\PhysicalDisk(*)\% Idle Time"),
            disk_read: add(r"\PhysicalDisk(*)\Disk Read Bytes/sec"),
            disk_write: add(r"\PhysicalDisk(*)\Disk Write Bytes/sec"),
            disk_latency: add(r"\PhysicalDisk(*)\Avg. Disk sec/Transfer"),
            gpu_engine: add(r"\GPU Engine(*)\Utilization Percentage"),
            gpu_process_dedicated: add(r"\GPU Process Memory(*)\Dedicated Usage"),
            gpu_process_shared: add(r"\GPU Process Memory(*)\Shared Usage"),
            gpu_adapter_dedicated: add(r"\GPU Adapter Memory(*)\Dedicated Usage"),
            gpu_adapter_shared: add(r"\GPU Adapter Memory(*)\Shared Usage"),
        };
        // Rate counters need a previous collection before they report anything.
        if let Some(query) = query.as_mut() {
            let _ = query.collect();
        }
        let has_gpu = |names: &[&str]| machine.gpus.iter().any(|gpu| names.iter().any(|n| gpu.name.contains(n)));
        let amd = has_gpu(&["AMD", "Radeon"]).then(Adl::open).flatten();
        let nvidia = has_gpu(&["NVIDIA", "GeForce", "Quadro", "RTX"]).then(Nvml::open).flatten();
        Self {
            query,
            counters,
            adapters,
            amd,
            nvidia,
            machine: Arc::new(machine),
            disks: HashMap::new(),
            thermal: None,
            pawnio: PawnIoState::NotInstalled,
            pawnio_retry: Some(Instant::now()),
            pawnio_failures: 0,
            lhm_port: lhm::DEFAULT_PORT,
            lhm_retry: Instant::now(),
        }
    }

    /// The port LibreHardwareMonitor's web server listens on, if not the default.
    pub fn set_lhm_port(&mut self, port: u16) {
        self.lhm_port = port;
    }

    /// CPU temperatures from PawnIO if it's usable, else from LibreHardwareMonitor if it's running.
    fn cpu_temperatures(&mut self) -> (Option<CpuTemps>, CpuTempSource) {
        let now = Instant::now();
        if self.thermal.is_none() && self.pawnio_retry.is_some_and(|at| now >= at) {
            let cpu = &self.machine.cpu;
            match CpuThermal::open(&cpu.name, &cpu.core_threads) {
                Ok(thermal) => {
                    self.thermal = Some(thermal);
                    self.pawnio = PawnIoState::Ready;
                    self.pawnio_failures = 0;
                }
                Err(reason) => {
                    self.pawnio = match reason {
                        Unavailable::NotInstalled => PawnIoState::NotInstalled,
                        Unavailable::NeedsAdmin => PawnIoState::NeedsAdmin,
                        Unavailable::Unsupported => PawnIoState::Unsupported,
                        Unavailable::Failed(_) => PawnIoState::Failed,
                    };
                    self.pawnio_retry = (reason != Unavailable::Unsupported).then(|| now + PAWNIO_RETRY);
                }
            }
        }
        if let Some(thermal) = self.thermal.as_mut() {
            match thermal.read() {
                Some(temps) => {
                    self.pawnio_failures = 0;
                    return (Some(temps), CpuTempSource::PawnIo);
                }
                None => {
                    self.pawnio_failures += 1;
                    if self.pawnio_failures >= PAWNIO_MAX_FAILURES {
                        self.thermal = None;
                        self.pawnio = PawnIoState::Failed;
                        self.pawnio_retry = Some(now + PAWNIO_RETRY);
                    }
                }
            }
        }
        if now >= self.lhm_retry {
            match lhm::cpu_temps(self.lhm_port) {
                Ok(Some(temps)) => return (Some(temps), CpuTempSource::LibreHardwareMonitor),
                Ok(None) => {}
                Err(_) => self.lhm_retry = now + LHM_RETRY,
            }
        }
        (None, CpuTempSource::None)
    }

    pub fn collect(&mut self) -> PerfSample {
        let (cpu_temps, cpu_temp_source) = self.cpu_temperatures();
        let mut sample = PerfSample {
            cpu_temps,
            cpu_temp_source,
            pawnio: self.pawnio,
            machine: Arc::clone(&self.machine),
            memory: hardware::memory_counters().ok(),
            uptime: hardware::uptime(),
            networks: net::adapters().unwrap_or_default(),
            ..PerfSample::default()
        };
        let Some(query) = self.query.as_mut() else { return sample };
        if query.collect().is_err() {
            return sample;
        }
        let query = &*query;
        let values = |id: Option<CounterId>| id.map(|id| query.values(id)).unwrap_or_default();

        sample.cpu_speed_mhz = values(self.counters.cpu_performance)
            .first()
            .zip(self.machine.cpu.base_mhz)
            .map(|((_, percent), base)| percent * f64::from(base) / 100.0);

        // Disks: `0 C:` style instances, keyed by disk number.
        let by_disk = |id| -> HashMap<u32, (String, f64)> {
            values(id)
                .into_iter()
                .filter_map(|(name, value)| {
                    let (number, letters) = parse_disk_instance(&name)?;
                    Some((number, (letters, value)))
                })
                .collect()
        };
        let (read, write, latency) =
            (by_disk(self.counters.disk_read), by_disk(self.counters.disk_write), by_disk(self.counters.disk_latency));
        let mut disks: Vec<_> = by_disk(self.counters.disk_idle).into_iter().collect();
        disks.sort_by_key(|(number, _)| *number);
        for (number, (letters, idle)) in disks {
            let info = match self.disks.get(&number) {
                Some(info) => Arc::clone(info),
                None => match disk_info(number) {
                    Ok(info) => Arc::clone(self.disks.entry(number).or_insert_with(|| Arc::new(info))),
                    Err(_) => continue,
                },
            };
            let value = |map: &HashMap<u32, (String, f64)>| map.get(&number).map_or(0.0, |(_, v)| *v);
            sample.disks.push(DiskSample {
                info,
                letters,
                active: (100.0 - idle).clamp(0.0, 100.0),
                read: value(&read),
                write: value(&write),
                response_ms: value(&latency) * 1000.0,
            });
        }

        let (gpus, processes) = aggregate_gpu(
            &self.machine.gpus,
            &values(self.counters.gpu_engine),
            &values(self.counters.gpu_process_dedicated),
            &values(self.counters.gpu_process_shared),
            &values(self.counters.gpu_adapter_dedicated),
            &values(self.counters.gpu_adapter_shared),
        );
        sample.gpus = gpus;
        sample.gpu_processes = processes;
        if let Some(adapters) = &self.adapters {
            for (index, gpu) in sample.gpus.iter_mut().enumerate() {
                gpu.temperature_c = adapters.sensors(index).temperature_c;
            }
        }
        for (gpu, info) in sample.gpus.iter_mut().zip(&self.machine.gpus) {
            let Some(location) = info.location else { continue };
            gpu.vendor = self
                .amd
                .as_ref()
                .and_then(|amd| amd.sensors(location))
                .or_else(|| self.nvidia.as_ref().and_then(|nvidia| nvidia.sensors(location)));
        }
        sample
    }
}

/// `0 C:` → `(0, "C:")`; `_Total` → `None`.
fn parse_disk_instance(name: &str) -> Option<(u32, String)> {
    let (number, letters) = name.split_once(' ').unwrap_or((name, ""));
    Some((number.parse().ok()?, letters.trim().to_owned()))
}

fn parse_luid(high: &str, low: &str) -> Option<Luid> {
    let hex = |s: &str| u32::from_str_radix(s.strip_prefix("0x")?, 16).ok();
    Some((hex(high)? as i32, hex(low)?))
}

#[derive(Debug, Clone, PartialEq)]
struct EngineInstance {
    pid: u32,
    luid: Luid,
    engine: u32,
    kind: String,
}

/// `pid_4321_luid_0x00000000_0x0000ABCD_phys_0_eng_2_engtype_Compute 0`.
fn parse_engine_instance(name: &str) -> Option<EngineInstance> {
    let lower = name.to_ascii_lowercase();
    let (head, kind) = lower.split_once("_engtype_")?;
    match head.split('_').collect::<Vec<_>>()[..] {
        ["pid", pid, "luid", high, low, "phys", _, "eng", engine] => Some(EngineInstance {
            pid: pid.parse().ok()?,
            luid: parse_luid(high, low)?,
            engine: engine.parse().ok()?,
            kind: pretty_engine(kind),
        }),
        _ => None,
    }
}

/// `pid_1234_luid_0x00000000_0x0000ABCD_phys_0` → `(1234, luid)`.
fn parse_process_memory_instance(name: &str) -> Option<(u32, Luid)> {
    let lower = name.to_ascii_lowercase();
    match lower.split('_').collect::<Vec<_>>()[..] {
        ["pid", pid, "luid", high, low, "phys", _] => Some((pid.parse().ok()?, parse_luid(high, low)?)),
        _ => None,
    }
}

/// `luid_0x00000000_0x0000ABCD_phys_0` → luid.
fn parse_adapter_memory_instance(name: &str) -> Option<Luid> {
    let lower = name.to_ascii_lowercase();
    match lower.split('_').collect::<Vec<_>>()[..] {
        ["luid", high, low, "phys", _] => parse_luid(high, low),
        _ => None,
    }
}

/// `video codec engine` → `Video Codec Engine`, `3d` → `3D`.
fn pretty_engine(kind: &str) -> String {
    kind.split_whitespace()
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().chain(chars.map(|c| c.to_ascii_lowercase())).collect::<String>(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
        .replace("3d", "3D")
}

/// Engines that are always idle housekeeping, which Task Manager doesn't graph either.
fn is_hidden_engine(kind: &str) -> bool {
    kind.starts_with("Timer") || kind.starts_with("Security")
}

#[allow(clippy::type_complexity)]
fn aggregate_gpu(
    gpus: &[GpuInfo],
    engines: &[(String, f64)],
    process_dedicated: &[(String, f64)],
    process_shared: &[(String, f64)],
    adapter_dedicated: &[(String, f64)],
    adapter_shared: &[(String, f64)],
) -> (Vec<GpuSample>, HashMap<u32, ProcessGpu>) {
    let index_of: HashMap<Luid, usize> = gpus.iter().enumerate().map(|(i, g)| (g.luid, i)).collect();

    // Per adapter, per engine: kind and the summed utilization of every process using it.
    let mut adapter_engines: Vec<BTreeMap<u32, (String, f64)>> = vec![BTreeMap::new(); gpus.len()];
    // Per process, per (adapter, engine): summed utilization.
    let mut process_engines: HashMap<u32, HashMap<(usize, u32), f64>> = HashMap::new();
    for (name, value) in engines {
        let Some(instance) = parse_engine_instance(name) else { continue };
        let Some(&gpu) = index_of.get(&instance.luid) else { continue };
        if is_hidden_engine(&instance.kind) {
            continue;
        }
        let entry = adapter_engines[gpu].entry(instance.engine).or_insert_with(|| (instance.kind.clone(), 0.0));
        entry.1 += value;
        *process_engines.entry(instance.pid).or_default().entry((gpu, instance.engine)).or_default() += value;
    }

    // Label engines like Task Manager: a kind that appears twice becomes "Copy" and "Copy 1".
    let labels: Vec<BTreeMap<u32, String>> = adapter_engines
        .iter()
        .map(|engines| {
            let mut seen: HashMap<&str, usize> = HashMap::new();
            engines
                .iter()
                .map(|(&engine, (kind, _))| {
                    let n = seen.entry(kind.as_str()).or_default();
                    let label = if *n == 0 { kind.clone() } else { format!("{kind} {n}") };
                    *n += 1;
                    (engine, label)
                })
                .collect()
        })
        .collect();

    let mut samples: Vec<GpuSample> = adapter_engines
        .iter()
        .zip(&labels)
        .map(|(engines, labels)| GpuSample {
            engines: engines.iter().map(|(engine, (_, sum))| (labels[engine].clone(), sum.min(100.0))).collect(),
            ..GpuSample::default()
        })
        .collect();
    for (name, value) in adapter_dedicated {
        if let Some(&gpu) = parse_adapter_memory_instance(name).and_then(|luid| index_of.get(&luid)) {
            samples[gpu].dedicated_used += *value as u64;
        }
    }
    for (name, value) in adapter_shared {
        if let Some(&gpu) = parse_adapter_memory_instance(name).and_then(|luid| index_of.get(&luid)) {
            samples[gpu].shared_used += *value as u64;
        }
    }

    let mut processes: HashMap<u32, ProcessGpu> = process_engines
        .into_iter()
        .filter_map(|(pid, engines)| {
            let (&(gpu, engine), &usage) = engines.iter().max_by(|a, b| a.1.total_cmp(b.1))?;
            Some((
                pid,
                ProcessGpu {
                    usage: usage.min(100.0),
                    engine: format!("GPU {gpu} - {}", labels[gpu][&engine]),
                    ..ProcessGpu::default()
                },
            ))
        })
        .collect();
    for (name, value) in process_dedicated {
        if let Some((pid, _)) = parse_process_memory_instance(name).filter(|(_, luid)| index_of.contains_key(luid)) {
            processes.entry(pid).or_default().dedicated += *value as u64;
        }
    }
    for (name, value) in process_shared {
        if let Some((pid, _)) = parse_process_memory_instance(name).filter(|(_, luid)| index_of.contains_key(luid)) {
            processes.entry(pid).or_default().shared += *value as u64;
        }
    }
    (samples, processes)
}

/// A disk's graphs.
#[derive(Debug, Clone)]
pub struct DiskView {
    pub latest: DiskSample,
    /// Fraction busy, `0..=1`.
    pub active: History,
    /// Bytes per second.
    pub read: History,
    pub write: History,
}

/// A network adapter's graphs.
#[derive(Debug, Clone)]
pub struct NetView {
    pub adapter: NetAdapter,
    /// Bits per second.
    pub send: History,
    pub receive: History,
    pub send_rate: f64,
    pub receive_rate: f64,
}

/// A GPU's graphs.
#[derive(Debug, Clone)]
pub struct GpuView {
    pub info: GpuInfo,
    pub latest: GpuSample,
    /// Busiest engine, fraction `0..=1`.
    pub utilization: History,
    /// Per engine, fraction `0..=1`, in the order engines first appeared.
    pub engines: Vec<(String, History)>,
    /// Bytes.
    pub dedicated: History,
    pub shared: History,
    /// Degrees Celsius.
    pub temperature: History,
    pub hotspot: History,
}

impl GpuView {
    pub fn utilization_now(&self) -> f64 {
        self.latest.engines.iter().map(|(_, value)| *value).fold(0.0, f64::max)
    }
}

/// Graph histories for the Performance view.
#[derive(Debug)]
pub struct PerformanceProvider {
    pub machine: Arc<Machine>,
    pub cpu: History,
    /// Per logical processor, per physical core, and per L3 group (CCX): fraction busy.
    pub logical: Vec<History>,
    pub cores: Vec<History>,
    pub l3_groups: Vec<History>,
    last_cpus: Vec<CpuTimes>,
    pub cpu_speed_mhz: Option<f64>,
    pub cpu_temps: Option<CpuTemps>,
    pub cpu_temp_source: CpuTempSource,
    pub pawnio: PawnIoState,
    /// Degrees Celsius: the headline CPU temperature, each CCD's (by CCD index) and each core's.
    pub cpu_temp: History,
    pub ccd_temps: Vec<(u32, History)>,
    pub core_temps: Vec<History>,
    pub uptime: Duration,
    pub memory: Option<MemoryCounters>,
    /// Bytes in use.
    pub memory_used: History,
    pub commit: History,
    pub disks: BTreeMap<u32, DiskView>,
    pub networks: BTreeMap<u32, NetView>,
    pub gpus: Vec<GpuView>,
    last: Option<Instant>,
}

impl Default for PerformanceProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl PerformanceProvider {
    pub fn new() -> Self {
        Self {
            machine: Arc::default(),
            cpu: History::new(PERF_HISTORY),
            logical: Vec::new(),
            cores: Vec::new(),
            l3_groups: Vec::new(),
            last_cpus: Vec::new(),
            cpu_speed_mhz: None,
            cpu_temps: None,
            cpu_temp_source: CpuTempSource::None,
            pawnio: PawnIoState::NotInstalled,
            cpu_temp: History::new(PERF_HISTORY),
            ccd_temps: Vec::new(),
            core_temps: Vec::new(),
            uptime: Duration::ZERO,
            memory: None,
            memory_used: History::new(PERF_HISTORY),
            commit: History::new(PERF_HISTORY),
            disks: BTreeMap::new(),
            networks: BTreeMap::new(),
            gpus: Vec::new(),
            last: None,
        }
    }

    /// Folds in a sample taken at `taken_at`; `cpu_usage` is the machine-wide CPU fraction from
    /// the process provider (so both views agree).
    /// Per-processor busy fractions since the previous sample, then averaged into cores and L3
    /// groups using the CPU topology.
    fn update_processors(&mut self, cpus: &[CpuTimes]) {
        let busy: Vec<f64> = cpus
            .iter()
            .enumerate()
            .map(|(i, now)| {
                let before = self.last_cpus.get(i).copied().unwrap_or(*now);
                let work = (now.kernel + now.user).saturating_sub(before.kernel + before.user);
                let total = work + now.idle.saturating_sub(before.idle);
                if total == 0 { 0.0 } else { work as f64 / total as f64 }
            })
            .collect();
        self.last_cpus = cpus.to_vec();

        let average = |threads: &[usize]| {
            let values: Vec<f64> = threads.iter().filter_map(|&t| busy.get(t).copied()).collect();
            if values.is_empty() { 0.0 } else { values.iter().sum::<f64>() / values.len() as f64 }
        };
        let push_all = |histories: &mut Vec<History>, values: Vec<f64>| {
            histories.resize_with(values.len(), || History::new(PERF_HISTORY));
            for (history, value) in histories.iter_mut().zip(values) {
                history.push(value);
            }
        };
        let cores = self.machine.cpu.core_threads.iter().map(|t| average(t)).collect();
        let groups = self.machine.cpu.l3_groups.iter().map(|t| average(t)).collect();
        push_all(&mut self.logical, busy.clone());
        push_all(&mut self.cores, cores);
        push_all(&mut self.l3_groups, groups);
    }

    fn update_temperatures(&mut self, sample: &PerfSample) {
        self.cpu_temp_source = sample.cpu_temp_source;
        self.pawnio = sample.pawnio;
        self.cpu_temps = sample.cpu_temps.clone();
        let Some(temps) = &sample.cpu_temps else { return };
        if let Some(headline) = cpu_temperature(temps) {
            self.cpu_temp.push(f64::from(headline));
        }
        for &(ccd, celsius) in &temps.ccds {
            match self.ccd_temps.iter_mut().find(|(index, _)| *index == ccd) {
                Some((_, history)) => history.push(f64::from(celsius)),
                None => {
                    let mut history = History::new(PERF_HISTORY);
                    history.push(f64::from(celsius));
                    self.ccd_temps.push((ccd, history));
                    self.ccd_temps.sort_by_key(|(index, _)| *index);
                }
            }
        }
        if self.core_temps.len() < temps.cores.len() {
            self.core_temps.resize_with(temps.cores.len(), || History::new(PERF_HISTORY));
        }
        for (history, celsius) in self.core_temps.iter_mut().zip(&temps.cores) {
            history.push(f64::from(*celsius));
        }
    }

    pub fn update(&mut self, sample: &PerfSample, cpu_usage: f32, cpus: &[CpuTimes], taken_at: Instant) {
        let elapsed = self.last.map(|last| taken_at.saturating_duration_since(last).as_secs_f64()).filter(|s| *s > 0.0);
        self.last = Some(taken_at);
        self.machine = Arc::clone(&sample.machine);
        self.cpu.push(f64::from(cpu_usage));
        self.update_processors(cpus);
        self.cpu_speed_mhz = sample.cpu_speed_mhz;
        self.update_temperatures(sample);
        self.uptime = sample.uptime;
        if let Some(memory) = sample.memory {
            self.memory = Some(memory);
            self.memory_used.push(memory.physical_total.saturating_sub(memory.physical_available) as f64);
            self.commit.push(memory.commit_total as f64);
        }

        self.disks.retain(|number, _| sample.disks.iter().any(|d| d.info.number == *number));
        for disk in &sample.disks {
            let view = self.disks.entry(disk.info.number).or_insert_with(|| DiskView {
                latest: disk.clone(),
                active: History::new(PERF_HISTORY),
                read: History::new(PERF_HISTORY),
                write: History::new(PERF_HISTORY),
            });
            view.latest = disk.clone();
            view.active.push(disk.active / 100.0);
            view.read.push(disk.read);
            view.write.push(disk.write);
        }

        self.networks.retain(|index, _| sample.networks.iter().any(|a| a.index == *index));
        for adapter in &sample.networks {
            let view = self.networks.entry(adapter.index).or_insert_with(|| NetView {
                adapter: adapter.clone(),
                send: History::new(PERF_HISTORY),
                receive: History::new(PERF_HISTORY),
                send_rate: 0.0,
                receive_rate: 0.0,
            });
            let rate =
                |now: u64, before: u64| elapsed.map_or(0.0, |secs| now.saturating_sub(before) as f64 * 8.0 / secs);
            view.send_rate = rate(adapter.sent, view.adapter.sent);
            view.receive_rate = rate(adapter.received, view.adapter.received);
            view.adapter = adapter.clone();
            view.send.push(view.send_rate);
            view.receive.push(view.receive_rate);
        }

        if self.gpus.len() != sample.gpus.len() {
            self.gpus = sample
                .machine
                .gpus
                .iter()
                .zip(&sample.gpus)
                .map(|(info, latest)| GpuView {
                    info: info.clone(),
                    latest: latest.clone(),
                    utilization: History::new(PERF_HISTORY),
                    engines: Vec::new(),
                    dedicated: History::new(PERF_HISTORY),
                    shared: History::new(PERF_HISTORY),
                    temperature: History::new(PERF_HISTORY),
                    hotspot: History::new(PERF_HISTORY),
                })
                .collect();
        }
        for (view, gpu) in self.gpus.iter_mut().zip(&sample.gpus) {
            view.latest = gpu.clone();
            view.utilization.push(view.utilization_now() / 100.0);
            for (label, value) in &gpu.engines {
                match view.engines.iter_mut().find(|(existing, _)| existing == label) {
                    Some((_, history)) => history.push(value / 100.0),
                    None => {
                        let mut history = History::new(PERF_HISTORY);
                        history.push(value / 100.0);
                        view.engines.push((label.clone(), history));
                    }
                }
            }
            view.dedicated.push(gpu.dedicated_used as f64);
            view.shared.push(gpu.shared_used as f64);
            if let Some(edge) = gpu.edge_c() {
                view.temperature.push(f64::from(edge));
            }
            if let Some(hotspot) = gpu.hotspot_c() {
                view.hotspot.push(f64::from(hotspot));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gpu(luid: Luid) -> GpuInfo {
        GpuInfo {
            luid,
            name: "Test GPU".into(),
            dedicated_memory: 8 << 30,
            shared_memory: 16 << 30,
            location: None,
            driver_version: None,
            driver_date: None,
        }
    }

    #[test]
    fn parses_counter_instances() {
        assert_eq!(parse_disk_instance("1 C:"), Some((1, "C:".into())));
        assert_eq!(parse_disk_instance("3 E: F:"), Some((3, "E: F:".into())));
        assert_eq!(parse_disk_instance("2"), Some((2, String::new())));
        assert_eq!(parse_disk_instance("_Total"), None);
        assert_eq!(
            parse_engine_instance("pid_4321_luid_0x00000000_0x0000ABCD_phys_0_eng_2_engtype_Compute 0"),
            Some(EngineInstance { pid: 4321, luid: (0, 0xABCD), engine: 2, kind: "Compute 0".into() })
        );
        assert_eq!(
            parse_engine_instance("pid_1_luid_0x0_0x1_phys_0_eng_7_engtype_video codec engine").unwrap().kind,
            "Video Codec Engine"
        );
        assert_eq!(parse_engine_instance("pid_1_luid_0x0_0x1_phys_0_eng_0_engtype_3d").unwrap().kind, "3D");
        assert_eq!(parse_process_memory_instance("pid_42_luid_0x00000000_0x0000abcd_phys_0"), Some((42, (0, 0xabcd))));
        assert_eq!(parse_adapter_memory_instance("luid_0x00000000_0x0000abcd_phys_0"), Some((0, 0xabcd)));
        assert_eq!(parse_engine_instance("garbage"), None);
    }

    #[test]
    fn aggregates_engines_like_task_manager() {
        let luid = (0, 0xabcd);
        let engine = |pid: u32, eng: u32, kind: &str, value: f64| {
            (format!("pid_{pid}_luid_0x00000000_0x0000abcd_phys_0_eng_{eng}_engtype_{kind}"), value)
        };
        let engines = [
            engine(10, 0, "3D", 30.0),
            engine(20, 0, "3D", 25.0),
            engine(20, 1, "Copy", 5.0),
            engine(20, 3, "Copy", 1.0),
            engine(10, 4, "Timer 0", 99.0),
        ];
        let memory = [("pid_20_luid_0x00000000_0x0000abcd_phys_0".to_owned(), 1024.0)];
        let adapter = [("luid_0x00000000_0x0000abcd_phys_0".to_owned(), 4096.0)];
        let (gpus, processes) = aggregate_gpu(&[gpu(luid)], &engines, &memory, &[], &adapter, &[]);

        assert_eq!(gpus[0].engines, [("3D".into(), 55.0), ("Copy".into(), 5.0), ("Copy 1".into(), 1.0)]);
        assert_eq!(gpus[0].dedicated_used, 4096);
        assert_eq!(processes[&10], ProcessGpu { usage: 30.0, engine: "GPU 0 - 3D".into(), dedicated: 0, shared: 0 });
        assert_eq!(processes[&20].usage, 25.0);
        assert_eq!(processes[&20].dedicated, 1024);
    }

    #[test]
    fn network_rates_are_bits_per_second() {
        let adapter = |sent, received| NetAdapter {
            index: 7,
            alias: "Ethernet".into(),
            description: String::new(),
            wireless: false,
            connected: true,
            link_speed: 1_000_000_000,
            received,
            sent,
            addresses: Vec::new(),
        };
        let t0 = Instant::now();
        let mut provider = PerformanceProvider::new();
        provider.update(&PerfSample { networks: vec![adapter(1000, 0)], ..PerfSample::default() }, 0.0, &[], t0);
        provider.update(
            &PerfSample { networks: vec![adapter(3000, 500)], ..PerfSample::default() },
            0.0,
            &[],
            t0 + Duration::from_secs(2),
        );
        let view = &provider.networks[&7];
        assert_eq!(view.send_rate, 8000.0);
        assert_eq!(view.receive_rate, 2000.0);
    }

    #[test]
    fn per_processor_usage_rolls_up_into_cores_and_l3_groups() {
        let machine = Machine {
            cpu: CpuInfo {
                core_threads: vec![vec![0, 1], vec![2, 3]],
                l3_groups: vec![vec![0, 1, 2, 3]],
                ..CpuInfo::default()
            },
            ..Machine::default()
        };
        let sample = PerfSample { machine: Arc::new(machine), ..PerfSample::default() };
        let cpu = |idle, user| CpuTimes { idle, user, ..CpuTimes::default() };
        let t0 = Instant::now();
        let mut provider = PerformanceProvider::new();
        provider.update(&sample, 0.0, &[cpu(0, 0); 4], t0);
        // Over the next interval: CPU 0 fully busy, CPU 1 idle, CPU 2 half busy, CPU 3 idle.
        let next = [cpu(0, 100), cpu(100, 0), cpu(50, 50), cpu(100, 0)];
        provider.update(&sample, 0.0, &next, t0 + Duration::from_secs(1));
        let latest = |histories: &[History]| histories.iter().map(|h| h.latest().unwrap()).collect::<Vec<_>>();
        assert_eq!(latest(&provider.logical), [1.0, 0.0, 0.5, 0.0]);
        assert_eq!(latest(&provider.cores), [0.5, 0.25]);
        assert_eq!(latest(&provider.l3_groups), [0.375]);
    }
}
