//! The process provider: turns successive OS snapshots into per-process usage figures.
//!
//! Ported from System Informer's `procprv.c`, including its default cycle-based CPU accounting
//! (with the time-based method as a fallback when cycle counters can't be read).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ferrotop_sys::process::ProcessInfo;
use ferrotop_sys::system::{CpuTimes, CycleTimes, MemoryStatus};

use crate::activity::{ActivityStatus, ActivityTracer, ProcessActivity};
use crate::delta::Delta;
use crate::details::{DetailsCollector, FileFacts, ProcessDetails, file_key};
use crate::history::History;
use crate::perf::ProcessGpu;

/// How many refreshes a new or exited process stays highlighted.
const HIGHLIGHT_REFRESHES: u8 = 2;
/// Samples kept for the system graphs.
const HISTORY_SAMPLES: usize = 90;
/// Time per sample spent looking up details for newly seen processes.
const DETAILS_BUDGET: Duration = Duration::from_millis(250);

/// PID of the "Interrupts" pseudo-process that carries CPU spent in DPCs and interrupts.
/// It's `-3` as a 32-bit value, matching System Informer's `INTERRUPTS_PROCESS_ID`.
pub const INTERRUPTS_PID: u32 = u32::MAX - 2;
const SYSTEM_PID: u32 = 4;

/// True for rows that aren't real processes (and so can't be opened or terminated).
pub fn is_pseudo_process(pid: u32) -> bool {
    pid == INTERRUPTS_PID
}

/// Identifies a process across refreshes. PIDs are recycled, so the creation time is part of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ProcessKey {
    pub pid: u32,
    pub create_time: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessState {
    /// Appeared since the provider started; highlighted for a few refreshes.
    New,
    Normal,
    /// Exited; kept (and highlighted) for a few refreshes before it disappears.
    Removed,
}

/// Everything the UI shows for one process.
#[derive(Debug, Clone)]
pub struct ProcessItem {
    pub key: ProcessKey,
    pub parent_pid: u32,
    pub name: String,
    pub state: ProcessState,
    pub threads: u32,
    pub handles: u32,
    pub session_id: u32,
    pub base_priority: i32,
    pub working_set: u64,
    pub private_bytes: u64,
    /// Share of total machine CPU over the last interval, `0.0..=1.0`.
    pub cpu_usage: f32,
    pub kernel_usage: f32,
    pub user_usage: f32,
    /// Bytes per second over the last interval.
    pub io_read_rate: f64,
    pub io_write_rate: f64,
    pub io_other_rate: f64,
    /// Disk and network bytes per second, from the kernel trace; zero when not tracing.
    pub disk_read_rate: f64,
    pub disk_write_rate: f64,
    pub net_send_rate: f64,
    pub net_receive_rate: f64,
    /// User, path and version info; `None` until the sampler has looked it up.
    pub details: Option<Arc<ProcessDetails>>,
    /// Names of the services this process hosts.
    pub services: Vec<String>,
    /// GPU use from the GPU performance counters; default when the process isn't using a GPU.
    pub gpu: ProcessGpu,
    /// Every thread is suspended.
    pub suspended: bool,
    /// Icon and signature of the image file, once the sampler has checked it.
    pub file: Option<Arc<FileFacts>>,
    highlight: u8,
    cpu_kernel: Delta,
    cpu_user: Delta,
    cycles: Delta,
    io_read: Delta,
    io_write: Delta,
    io_other: Delta,
}

impl ProcessItem {
    pub fn io_total_rate(&self) -> f64 {
        self.io_read_rate + self.io_write_rate + self.io_other_rate
    }

    pub fn disk_rate(&self) -> f64 {
        self.disk_read_rate + self.disk_write_rate
    }

    pub fn network_rate(&self) -> f64 {
        self.net_send_rate + self.net_receive_rate
    }

    pub fn user(&self) -> &str {
        self.details.as_ref().and_then(|d| d.user.as_deref()).unwrap_or_default()
    }

    pub fn description(&self) -> &str {
        self.details.as_ref().and_then(|d| d.description.as_deref()).unwrap_or_default()
    }

    /// The verified signer (or why there isn't one); empty until checked.
    pub fn signer(&self) -> String {
        self.file.as_ref().map(|f| f.signer()).unwrap_or_default()
    }
}

/// One snapshot of the OS state, taken at `taken_at`.
#[derive(Debug, Clone)]
pub struct Sample {
    pub processes: Vec<ProcessInfo>,
    pub cpus: Vec<CpuTimes>,
    /// `None` if the cycle counters couldn't be read; CPU usage then falls back to time-based.
    pub cycles: Option<CycleTimes>,
    pub memory: MemoryStatus,
    pub details: HashMap<ProcessKey, Arc<ProcessDetails>>,
    pub services: Arc<HashMap<u32, Vec<String>>>,
    /// Final cycle counts of processes that exited since the previous sample.
    pub exited_cycles: HashMap<ProcessKey, u64>,
    /// Image-file facts found since the previous sample, keyed by [`file_key`].
    pub files: Vec<(String, Arc<FileFacts>)>,
    /// Disk and network bytes by process since the previous sample; `None` when not tracing.
    pub activity: Option<HashMap<u32, ProcessActivity>>,
    pub activity_status: ActivityStatus,
    pub taken_at: Instant,
}

impl Sample {
    /// The OS state without per-process details (user, path, icon, signature), with disk and
    /// network activity from `tracer`: what the history recorder needs.
    pub fn capture_basic(tracer: &ActivityTracer) -> ferrotop_sys::Result<Self> {
        let cpus = ferrotop_sys::system::processor_times()?;
        let cycles = ferrotop_sys::system::cycle_times().ok();
        let processes = ferrotop_sys::process::enumerate()?;
        let memory = ferrotop_sys::system::memory_status()?;
        let taken_at = Instant::now();
        let activity = tracer.take(&processes);
        Ok(Self {
            processes,
            cpus,
            cycles,
            memory,
            details: HashMap::new(),
            services: Arc::default(),
            exited_cycles: HashMap::new(),
            files: Vec::new(),
            activity,
            activity_status: tracer.status(),
            taken_at,
        })
    }

    /// Captures the OS state, filling in per-process details from `collector` and disk and
    /// network activity from `tracer`.
    pub fn capture(collector: &mut DetailsCollector, tracer: &ActivityTracer) -> ferrotop_sys::Result<Self> {
        let cpus = ferrotop_sys::system::processor_times()?;
        let cycles = ferrotop_sys::system::cycle_times().ok();
        let processes = ferrotop_sys::process::enumerate()?;
        let memory = ferrotop_sys::system::memory_status()?;
        let taken_at = Instant::now();
        let details = collector.collect(&processes, DETAILS_BUDGET);
        let services = collector.services();
        let exited_cycles = collector.take_exited_cycles();
        let files = collector.take_new_files();
        let activity = tracer.take(&processes);
        let activity_status = tracer.status();
        Ok(Self {
            processes,
            cpus,
            cycles,
            memory,
            details,
            services,
            exited_cycles,
            files,
            activity,
            activity_status,
            taken_at,
        })
    }
}

/// System-wide figures from the latest refresh.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemStats {
    pub cpu_usage: f32,
    pub kernel_usage: f32,
    pub user_usage: f32,
    pub cpu_count: usize,
    /// True when CPU usage came from cycle counters rather than time accounting.
    pub cycle_based: bool,
    pub memory: MemoryStatus,
    pub processes: usize,
    pub threads: u64,
    pub handles: u64,
    /// Whether per-process disk and network activity is being traced.
    pub activity: ActivityStatus,
}

impl SystemStats {
    pub fn memory_used(&self) -> u64 {
        self.memory.total_physical.saturating_sub(self.memory.available_physical)
    }
}

#[derive(Debug)]
pub struct ProcessProvider {
    items: HashMap<ProcessKey, ProcessItem>,
    sys_kernel: Delta,
    sys_user: Delta,
    sys_idle: Delta,
    sys_idle_cycles: Delta,
    last_sample: Option<Instant>,
    run_count: u64,
    stats: SystemStats,
    cpu_history: History,
    memory_history: History,
    files: HashMap<String, Arc<FileFacts>>,
    /// Per-process GPU use by PID, from the latest Performance sample.
    gpu: HashMap<u32, ProcessGpu>,
}

impl Default for ProcessProvider {
    fn default() -> Self {
        Self::new()
    }
}

fn ratio(part: u64, total: u64) -> f32 {
    if total == 0 { 0.0 } else { (part as f64 / total as f64) as f32 }
}

/// Splits `usage` into (kernel, user) in proportion to the time deltas, as System Informer does
/// for cycle-based usage. With no time deltas, a process that has ever run user code is split
/// evenly; one that hasn't is all kernel.
fn split_usage(usage: f32, kernel_delta: u64, user_delta: u64, ever_user: bool) -> (f32, f32) {
    match kernel_delta + user_delta {
        0 if ever_user => (usage / 2.0, usage / 2.0),
        0 => (usage, 0.0),
        total => (usage * ratio(kernel_delta, total), usage * ratio(user_delta, total)),
    }
}

impl ProcessProvider {
    pub fn new() -> Self {
        Self {
            items: HashMap::new(),
            sys_kernel: Delta::default(),
            sys_user: Delta::default(),
            sys_idle: Delta::default(),
            sys_idle_cycles: Delta::default(),
            last_sample: None,
            run_count: 0,
            stats: SystemStats::default(),
            cpu_history: History::new(HISTORY_SAMPLES),
            memory_history: History::new(HISTORY_SAMPLES),
            files: HashMap::new(),
            gpu: HashMap::new(),
        }
    }

    /// Per-process GPU use by PID (from a Performance sample, which arrives separately), shown
    /// from the next [`update`](Self::update).
    pub fn set_gpu_usage(&mut self, gpu: HashMap<u32, ProcessGpu>) {
        self.gpu = gpu;
    }

    pub fn items(&self) -> impl Iterator<Item = &ProcessItem> {
        self.items.values()
    }

    pub fn get(&self, key: &ProcessKey) -> Option<&ProcessItem> {
        self.items.get(key)
    }

    pub fn stats(&self) -> &SystemStats {
        &self.stats
    }

    pub fn cpu_history(&self) -> &History {
        &self.cpu_history
    }

    pub fn memory_history(&self) -> &History {
        &self.memory_history
    }

    /// Folds a new snapshot into the provider.
    pub fn update(&mut self, sample: Sample) {
        let first_run = self.run_count == 0;
        let elapsed = self
            .last_sample
            .map(|last| sample.taken_at.saturating_duration_since(last).as_secs_f64())
            .filter(|secs| *secs > 0.0);
        self.last_sample = Some(sample.taken_at);

        // System time: kernel (minus idle) + user + idle across every logical processor.
        let totals = sample.cpus.iter().fold(CpuTimes::default(), |acc, cpu| CpuTimes {
            idle: acc.idle + cpu.idle,
            kernel: acc.kernel + cpu.kernel,
            user: acc.user + cpu.user,
            dpc: acc.dpc + cpu.dpc,
            interrupt: acc.interrupt + cpu.interrupt,
        });
        self.sys_kernel.update(totals.kernel);
        self.sys_user.update(totals.user);
        self.sys_idle.update(totals.idle);
        let sys_time = self.sys_kernel.delta + self.sys_user.delta + self.sys_idle.delta;

        let mut processes = sample.processes;
        for info in processes.iter_mut().filter(|info| info.pid == 0) {
            // The idle "process" has no image name. Its time and cycles are the machine's idle
            // time and cycles (System Informer makes the same substitution).
            if info.name.is_empty() {
                info.name = "System Idle Process".into();
            }
            info.kernel_time = totals.idle;
            info.user_time = 0;
            if let Some(cycles) = sample.cycles {
                info.cycle_time = cycles.idle;
            }
        }
        processes.push(ProcessInfo {
            pid: INTERRUPTS_PID,
            parent_pid: SYSTEM_PID,
            name: "Interrupts".into(),
            kernel_time: totals.dpc + totals.interrupt,
            cycle_time: sample.cycles.map_or(0, |cycles| cycles.interrupt),
            ..ProcessInfo::default()
        });

        // Age highlights and drop processes whose exit has been shown long enough.
        self.items.retain(|_, item| {
            item.highlight = item.highlight.saturating_sub(1);
            match (item.state, item.highlight) {
                (ProcessState::Removed, 0) => false,
                (ProcessState::New, 0) => {
                    item.state = ProcessState::Normal;
                    true
                }
                _ => true,
            }
        });

        // Cycle-based accounting: the machine's total is the sum of every process's cycle delta,
        // counting a new process's whole cycle count, plus the cycles processes that exited
        // mid-interval used before they went (read through the handle the sampler kept open).
        let sys_cycles = sample.cycles.map(|cycles| {
            self.sys_idle_cycles.update(cycles.idle);
            let running = processes.iter().map(|info| {
                let key = ProcessKey { pid: info.pid, create_time: info.create_time };
                match self.items.get(&key) {
                    Some(item) if item.state != ProcessState::Removed => {
                        info.cycle_time.saturating_sub(item.cycles.value)
                    }
                    _ => info.cycle_time,
                }
            });
            let exited = sample.exited_cycles.iter().filter_map(|(key, last)| {
                let item = self.items.get(key).filter(|item| item.state != ProcessState::Removed)?;
                Some(last.saturating_sub(item.cycles.value))
            });
            running.chain(exited).fold(0u64, u64::saturating_add)
        });
        if self.files.len() > 4096 {
            self.files.clear();
            for item in self.items.values_mut() {
                item.file = None;
            }
        }
        self.files.extend(sample.files.iter().cloned());

        let mut seen = HashSet::with_capacity(processes.len());
        let mut threads = 0u64;
        let mut handles = 0u64;

        for info in processes {
            threads += u64::from(info.threads);
            handles += u64::from(info.handles);
            let key = ProcessKey { pid: info.pid, create_time: info.create_time };
            seen.insert(key);

            let item = self.items.entry(key).or_insert_with(|| ProcessItem {
                key,
                parent_pid: info.parent_pid,
                name: info.name.clone(),
                state: if first_run { ProcessState::Normal } else { ProcessState::New },
                threads: 0,
                handles: 0,
                session_id: info.session_id,
                base_priority: 0,
                working_set: 0,
                private_bytes: 0,
                cpu_usage: 0.0,
                kernel_usage: 0.0,
                user_usage: 0.0,
                io_read_rate: 0.0,
                io_write_rate: 0.0,
                io_other_rate: 0.0,
                disk_read_rate: 0.0,
                disk_write_rate: 0.0,
                net_send_rate: 0.0,
                net_receive_rate: 0.0,
                details: None,
                services: Vec::new(),
                gpu: ProcessGpu::default(),
                suspended: false,
                file: None,
                highlight: if first_run { 0 } else { HIGHLIGHT_REFRESHES },
                cpu_kernel: Delta::new(info.kernel_time),
                cpu_user: Delta::new(info.user_time),
                cycles: Delta::new(info.cycle_time),
                io_read: Delta::new(info.io_read_bytes),
                io_write: Delta::new(info.io_write_bytes),
                io_other: Delta::new(info.io_other_bytes),
            });

            item.cpu_kernel.update(info.kernel_time);
            item.cpu_user.update(info.user_time);
            item.cycles.update(info.cycle_time);
            item.io_read.update(info.io_read_bytes);
            item.io_write.update(info.io_write_bytes);
            item.io_other.update(info.io_other_bytes);

            let (kernel, user) = match sys_cycles {
                Some(total) => split_usage(
                    ratio(item.cycles.delta, total),
                    item.cpu_kernel.delta,
                    item.cpu_user.delta,
                    info.user_time != 0,
                ),
                None => (ratio(item.cpu_kernel.delta, sys_time), ratio(item.cpu_user.delta, sys_time)),
            };
            item.kernel_usage = kernel;
            item.user_usage = user;
            item.cpu_usage = kernel + user;

            let per_second = |delta: u64| elapsed.map_or(0.0, |secs| delta as f64 / secs);
            item.io_read_rate = per_second(item.io_read.delta);
            item.io_write_rate = per_second(item.io_write.delta);
            item.io_other_rate = per_second(item.io_other.delta);
            let activity = sample.activity.as_ref().and_then(|a| a.get(&info.pid)).copied().unwrap_or_default();
            item.disk_read_rate = per_second(activity.disk_read);
            item.disk_write_rate = per_second(activity.disk_written);
            item.net_send_rate = per_second(activity.sent);
            item.net_receive_rate = per_second(activity.received);

            item.threads = info.threads;
            item.handles = info.handles;
            item.base_priority = info.base_priority;
            item.working_set = info.working_set;
            item.private_bytes = info.private_bytes;
            if let Some(details) = sample.details.get(&key) {
                item.details = Some(Arc::clone(details));
            }
            if item.file.is_none()
                && let Some(path) = item.details.as_ref().and_then(|d| d.image_path.as_deref())
            {
                item.file = self.files.get(&file_key(path)).cloned();
            }
            item.suspended = info.suspended;
            item.services = sample.services.get(&info.pid).cloned().unwrap_or_default();
            item.gpu = self.gpu.get(&info.pid).cloned().unwrap_or_default();
        }

        // Anything we didn't see this time has exited.
        for item in self.items.values_mut() {
            if item.state != ProcessState::Removed && !seen.contains(&item.key) {
                item.state = ProcessState::Removed;
                item.highlight = HIGHLIGHT_REFRESHES;
                item.cpu_usage = 0.0;
                item.kernel_usage = 0.0;
                item.user_usage = 0.0;
                item.io_read_rate = 0.0;
                item.io_write_rate = 0.0;
                item.io_other_rate = 0.0;
                item.disk_read_rate = 0.0;
                item.disk_write_rate = 0.0;
                item.net_send_rate = 0.0;
                item.net_receive_rate = 0.0;
            }
        }

        let (cpu_usage, kernel_usage, user_usage) = match sys_cycles {
            Some(total) => {
                let busy = 1.0 - ratio(self.sys_idle_cycles.delta, total);
                let (kernel, user) = split_usage(busy, self.sys_kernel.delta, self.sys_user.delta, true);
                (busy, kernel, user)
            }
            None => {
                let (kernel, user) = (ratio(self.sys_kernel.delta, sys_time), ratio(self.sys_user.delta, sys_time));
                (kernel + user, kernel, user)
            }
        };
        self.stats = SystemStats {
            cpu_usage: cpu_usage.clamp(0.0, 1.0),
            kernel_usage,
            user_usage,
            cpu_count: sample.cpus.len(),
            cycle_based: sys_cycles.is_some(),
            memory: sample.memory,
            // Every sample includes the Interrupts pseudo-process.
            processes: seen.len().saturating_sub(1),
            threads,
            handles,
            activity: sample.activity_status,
        };
        if !first_run {
            self.cpu_history.push(f64::from(self.stats.cpu_usage));
        }
        let total = sample.memory.total_physical.max(1);
        self.memory_history.push(self.stats.memory_used() as f64 / total as f64);
        self.run_count += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn process(pid: u32, parent_pid: u32, create_time: i64, cpu_time: u64) -> ProcessInfo {
        ProcessInfo {
            pid,
            parent_pid,
            name: format!("p{pid}.exe"),
            create_time,
            user_time: cpu_time,
            threads: 1,
            ..ProcessInfo::default()
        }
    }

    pub(crate) fn sample(
        start: Instant,
        secs: u64,
        cpu: CpuTimes,
        cycles: Option<CycleTimes>,
        processes: Vec<ProcessInfo>,
    ) -> Sample {
        Sample {
            processes,
            cpus: vec![cpu],
            cycles,
            memory: MemoryStatus::default(),
            details: HashMap::new(),
            services: Arc::default(),
            exited_cycles: HashMap::new(),
            files: Vec::new(),
            activity: None,
            activity_status: ActivityStatus::default(),
            taken_at: start + Duration::from_secs(secs),
        }
    }

    #[test]
    fn time_based_cpu_is_share_of_total_machine_time() {
        let t0 = Instant::now();
        let mut p = ProcessProvider::new();
        let cpu = |idle, user| CpuTimes { idle, user, ..Default::default() };
        p.update(sample(t0, 0, cpu(1000, 0), None, vec![process(10, 4, 1, 0)]));
        // 1000 ticks pass: 250 user (all from pid 10), 750 idle.
        p.update(sample(t0, 1, cpu(1750, 250), None, vec![process(10, 4, 1, 250)]));
        let item = p.get(&ProcessKey { pid: 10, create_time: 1 }).unwrap();
        assert!((item.cpu_usage - 0.25).abs() < 1e-6, "got {}", item.cpu_usage);
        assert!((p.stats().cpu_usage - 0.25).abs() < 1e-6);
        assert!(!p.stats().cycle_based);
    }

    #[test]
    fn cycle_based_cpu_divides_by_all_cycles_spent() {
        let t0 = Instant::now();
        let mut p = ProcessProvider::new();
        let idle = ProcessInfo { name: String::new(), ..process(0, 0, 0, 0) };
        let busy = |cycles, user| ProcessInfo { cycle_time: cycles, user_time: user, ..process(10, 4, 1, 0) };
        let cycles = |idle, interrupt| Some(CycleTimes { idle, interrupt });
        p.update(sample(t0, 0, CpuTimes::default(), cycles(5_000, 100), vec![idle.clone(), busy(1_000, 10)]));
        // Next interval: 600 idle cycles, 300 in pid 10, 100 in interrupts = 1000 total.
        p.update(sample(t0, 1, CpuTimes::default(), cycles(5_600, 200), vec![idle, busy(1_300, 20)]));
        let item = p.get(&ProcessKey { pid: 10, create_time: 1 }).unwrap();
        assert!((item.cpu_usage - 0.3).abs() < 1e-6, "got {}", item.cpu_usage);
        let interrupts = p.get(&ProcessKey { pid: INTERRUPTS_PID, create_time: 0 }).unwrap();
        assert!((interrupts.cpu_usage - 0.1).abs() < 1e-6, "got {}", interrupts.cpu_usage);
        assert!((p.stats().cpu_usage - 0.4).abs() < 1e-6, "got {}", p.stats().cpu_usage);
        assert!(p.stats().cycle_based);
        assert_eq!(p.stats().processes, 2, "the pseudo-process isn't counted");
    }

    #[test]
    fn exited_processes_final_cycles_count_toward_the_total() {
        let t0 = Instant::now();
        let mut p = ProcessProvider::new();
        let idle = ProcessInfo { name: String::new(), ..process(0, 0, 0, 0) };
        let busy = |pid, cycles| ProcessInfo { cycle_time: cycles, ..process(pid, 4, 1, 0) };
        let cycles = |idle| Some(CycleTimes { idle, interrupt: 0 });
        p.update(sample(t0, 0, CpuTimes::default(), cycles(0), vec![idle.clone(), busy(10, 0), busy(20, 0)]));
        // pid 20 exits after using 500 cycles; pid 10 used 250; idle 250. Total 1000.
        let mut s = sample(t0, 1, CpuTimes::default(), cycles(250), vec![idle, busy(10, 250)]);
        s.exited_cycles = HashMap::from([(ProcessKey { pid: 20, create_time: 1 }, 500)]);
        p.update(s);
        let item = p.get(&ProcessKey { pid: 10, create_time: 1 }).unwrap();
        assert!((item.cpu_usage - 0.25).abs() < 1e-6, "got {}", item.cpu_usage);
        assert!((p.stats().cpu_usage - 0.75).abs() < 1e-6, "got {}", p.stats().cpu_usage);
    }

    #[test]
    fn idle_process_reports_machine_idle_time() {
        let t0 = Instant::now();
        let mut p = ProcessProvider::new();
        let idle = ProcessInfo { name: String::new(), ..process(0, 0, 0, 0) };
        p.update(sample(t0, 0, CpuTimes::default(), None, vec![idle.clone()]));
        p.update(sample(t0, 1, CpuTimes { idle: 900, user: 100, ..Default::default() }, None, vec![idle]));
        let item = p.get(&ProcessKey { pid: 0, create_time: 0 }).unwrap();
        assert_eq!(item.name, "System Idle Process");
        assert!((item.cpu_usage - 0.9).abs() < 1e-6);
    }

    #[test]
    fn new_and_exited_processes_are_highlighted_then_settle() {
        let t0 = Instant::now();
        let cpu = CpuTimes::default();
        let mut p = ProcessProvider::new();
        p.update(sample(t0, 0, cpu, None, vec![process(4, 0, 0, 0), process(10, 4, 5, 0)]));
        let old = ProcessKey { pid: 10, create_time: 5 };
        assert_eq!(p.get(&old).unwrap().state, ProcessState::Normal, "first run isn't highlighted");

        // PID 10 is reused by a different process.
        p.update(sample(t0, 1, cpu, None, vec![process(4, 0, 0, 0), process(10, 4, 9, 0)]));
        let new = ProcessKey { pid: 10, create_time: 9 };
        assert_eq!(p.get(&old).unwrap().state, ProcessState::Removed);
        assert_eq!(p.get(&new).unwrap().state, ProcessState::New);

        for secs in 2..=3 {
            p.update(sample(t0, secs, cpu, None, vec![process(4, 0, 0, 0), process(10, 4, 9, 0)]));
        }
        assert!(p.get(&old).is_none(), "exited process dropped after its highlight");
        assert_eq!(p.get(&new).unwrap().state, ProcessState::Normal);
    }

    #[test]
    fn io_rates_are_per_second() {
        let t0 = Instant::now();
        let cpu = CpuTimes::default();
        let mut p = ProcessProvider::new();
        p.update(sample(t0, 0, cpu, None, vec![ProcessInfo { io_read_bytes: 1000, ..process(10, 4, 1, 0) }]));
        p.update(sample(t0, 2, cpu, None, vec![ProcessInfo { io_read_bytes: 5000, ..process(10, 4, 1, 0) }]));
        let item = p.get(&ProcessKey { pid: 10, create_time: 1 }).unwrap();
        assert!((item.io_read_rate - 2000.0).abs() < 1e-9);
    }

    #[test]
    fn services_attach_to_their_host() {
        let t0 = Instant::now();
        let mut p = ProcessProvider::new();
        let mut s = sample(t0, 0, CpuTimes::default(), None, vec![process(700, 4, 1, 0)]);
        s.services = Arc::new(HashMap::from([(700, vec!["Dnscache".to_owned()])]));
        p.update(s);
        assert_eq!(p.get(&ProcessKey { pid: 700, create_time: 1 }).unwrap().services, ["Dnscache"]);
    }
}
