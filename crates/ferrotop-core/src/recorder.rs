//! The history service's work: a sample of the whole machine every second, written to the
//! archive along with each minute's busiest processes, and live figures published for Ferrotop
//! windows that aren't elevated.

use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ferrotop_sys::live::{LiveProcess, LivePublisher, LiveSnapshot};

use crate::activity::ActivityTracer;
use crate::archive::{self, ArchiveWriter, MAX_MINUTE_ENTRIES, MinuteEntry, MinuteRecord, SecondRecord};
use crate::perf::{PerfCollector, PerfSample, PerformanceProvider, cpu_temperature};
use crate::provider::{ProcessProvider, ProcessState, Sample, is_pseudo_process};

/// Busiest processes kept per minute: by CPU, then by disk and by network.
const TOP_CPU: usize = 6;
const TOP_DISK: usize = 3;
const TOP_NETWORK: usize = 3;
const _: () = assert!(TOP_CPU + TOP_DISK + TOP_NETWORK <= MAX_MINUTE_ENTRIES);

/// Records into `dir` until `wait` (given how long until the next second) returns `true`.
pub fn run(dir: &Path, mut wait: impl FnMut(Duration) -> bool) -> io::Result<()> {
    // The service is the source of the live figures, never a reader of them.
    let tracer = ActivityTracer::start(false);
    // Without the section, windows just don't get live figures; history still works.
    let live = LivePublisher::create().ok();
    let mut processes = ProcessProvider::new();
    let mut collector = PerfCollector::new();
    // Temperatures come from PawnIO, which SYSTEM can read. Never from the service's own live
    // figures, nor from LibreHardwareMonitor's port, which any user could be listening on.
    collector.set_use_service(false);
    collector.disable_lhm();
    let mut devices = PerformanceProvider::new();
    let mut writer = ArchiveWriter::open(dir, archive::now_unix())?;
    let mut minute = MinuteTotals::new(archive::now_unix() / 60 * 60);
    let mut pruned_at = 0;
    loop {
        let now = archive::now_unix();
        if now >= pruned_at + 3600 {
            archive::prune(dir, now);
            pruned_at = now;
        }
        if let Ok(sample) = Sample::capture_basic(&tracer) {
            let cpus = sample.cpus.clone();
            processes.update(sample);
            devices.update_cpu(processes.stats().cpu_usage, &cpus);
        }
        let perf = collector.collect();
        devices.update_devices(&perf);
        collector.set_up_next();

        // A full disk shouldn't stop the service; the next second tries again.
        let _ = writer.write_second(&second_record(now, &processes, &devices, &perf));
        let this_minute = now / 60 * 60;
        if this_minute != minute.start {
            if let Some(record) = minute.finish() {
                let _ = writer.write_minute(&record);
            }
            minute = MinuteTotals::new(this_minute);
        }
        minute.add(&processes);
        if let Some(live) = &live {
            live.publish(&live_snapshot(&processes, &perf));
        }

        let into_second = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.subsec_millis());
        if wait(Duration::from_millis(u64::from(1000 - into_second.min(999)))) {
            break;
        }
    }
    if let Some(record) = minute.finish() {
        let _ = writer.write_minute(&record);
    }
    Ok(())
}

fn second_record(
    now: u64,
    processes: &ProcessProvider,
    devices: &PerformanceProvider,
    perf: &PerfSample,
) -> SecondRecord {
    let stats = processes.stats();
    SecondRecord {
        time: now as u32,
        cpu: stats.cpu_usage,
        memory_used: stats.memory_used(),
        commit: perf.memory.map_or(0, |m| m.commit_total),
        disk_read: perf.disks.iter().map(|d| d.read).sum::<f64>() as f32,
        disk_write: perf.disks.iter().map(|d| d.write).sum::<f64>() as f32,
        // Network rates are kept in bits per second for the graphs.
        sent: (devices.networks.values().map(|n| n.send_rate).sum::<f64>() / 8.0) as f32,
        received: (devices.networks.values().map(|n| n.receive_rate).sum::<f64>() / 8.0) as f32,
        gpu: (devices.gpus.iter().map(|g| g.utilization_now()).fold(0.0, f64::max) / 100.0) as f32,
        cpu_temp: perf.cpu_temps.as_ref().and_then(cpu_temperature).unwrap_or(f32::NAN),
    }
}

/// Disk and network rates of every process doing any, and the CPU temperatures.
fn live_snapshot(processes: &ProcessProvider, perf: &PerfSample) -> LiveSnapshot {
    let updated_unix_ms = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
    LiveSnapshot {
        updated_unix_ms,
        temps: perf.cpu_temps.clone(),
        processes: processes
            .items()
            .filter(|item| item.state != ProcessState::Removed && (item.disk_rate() > 0.0 || item.network_rate() > 0.0))
            .map(|item| LiveProcess {
                pid: item.key.pid,
                disk_read: item.disk_read_rate as f32,
                disk_write: item.disk_write_rate as f32,
                sent: item.net_send_rate as f32,
                received: item.net_receive_rate as f32,
            })
            .collect(),
    }
}

/// One process's sums over the current minute.
#[derive(Debug, Default)]
struct Totals {
    name: String,
    cpu: f64,
    disk: f64,
    network: f64,
    private_bytes: u64,
}

/// Per-process sums over one minute.
#[derive(Debug)]
struct MinuteTotals {
    start: u64,
    seconds: u32,
    processes: HashMap<u32, Totals>,
}

impl MinuteTotals {
    fn new(start: u64) -> Self {
        Self { start, seconds: 0, processes: HashMap::new() }
    }

    fn add(&mut self, processes: &ProcessProvider) {
        self.seconds += 1;
        for item in processes.items() {
            // Leave out exited processes, the idle "process" and the interrupts row.
            if item.state == ProcessState::Removed || item.key.pid == 0 || is_pseudo_process(item.key.pid) {
                continue;
            }
            let totals = self
                .processes
                .entry(item.key.pid)
                .or_insert_with(|| Totals { name: item.name.clone(), ..Totals::default() });
            totals.cpu += f64::from(item.cpu_usage);
            totals.disk += item.disk_rate();
            totals.network += item.network_rate();
            totals.private_bytes = totals.private_bytes.max(item.private_bytes);
        }
    }

    /// The minute's busiest processes: the top few by CPU, by disk and by network, averaged
    /// over the seconds recorded. `None` when nothing was recorded.
    fn finish(&self) -> Option<MinuteRecord> {
        if self.seconds == 0 {
            return None;
        }
        let seconds = f64::from(self.seconds);
        let entries: Vec<MinuteEntry> = self
            .processes
            .iter()
            .map(|(&pid, t)| MinuteEntry {
                pid,
                name: t.name.clone(),
                cpu: (t.cpu / seconds) as f32,
                private_bytes: t.private_bytes,
                disk: (t.disk / seconds) as f32,
                network: (t.network / seconds) as f32,
            })
            .collect();
        let mut chosen: Vec<MinuteEntry> = Vec::new();
        let mut pick = |key: fn(&MinuteEntry) -> f32, count: usize| {
            let mut ranked: Vec<&MinuteEntry> = entries.iter().filter(|e| key(e) > 0.0).collect();
            ranked.sort_by(|a, b| key(b).total_cmp(&key(a)).then(a.pid.cmp(&b.pid)));
            for entry in ranked.into_iter().take(count) {
                if !chosen.iter().any(|c| c.pid == entry.pid) {
                    chosen.push(entry.clone());
                }
            }
        };
        pick(|e| e.cpu, TOP_CPU);
        pick(|e| e.disk, TOP_DISK);
        pick(|e| e.network, TOP_NETWORK);
        Some(MinuteRecord { time: self.start as u32, entries: chosen })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_minute_keeps_the_busiest_by_each_measure() {
        let mut minute = MinuteTotals::new(600);
        minute.seconds = 2;
        let mut add = |pid: u32, cpu: f64, disk: f64, network: f64| {
            minute
                .processes
                .insert(pid, Totals { name: format!("p{pid}.exe"), cpu, disk, network, private_bytes: u64::from(pid) });
        };
        for pid in 1..=10 {
            add(pid, f64::from(pid) * 0.02, 0.0, 0.0);
        }
        add(50, 0.0, 9000.0, 0.0);
        add(60, 0.0, 0.0, 400.0);
        add(70, 0.0, 0.0, 0.0);
        let record = minute.finish().expect("record");
        assert_eq!(record.time, 600);
        let pids: Vec<u32> = record.entries.iter().map(|e| e.pid).collect();
        assert_eq!(pids, [10, 9, 8, 7, 6, 5, 50, 60], "six by CPU, then disk, then network; idle ones left out");
        assert!((record.entries[0].cpu - 0.1).abs() < 1e-6, "averaged over the seconds recorded");
        assert_eq!(record.entries[6].disk, 4500.0);
        assert!(MinuteTotals::new(0).finish().is_none());
    }

    #[test]
    fn records_a_few_seconds() {
        let dir = std::env::temp_dir().join(format!("ferrotop-recorder-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let mut rounds = 0;
        run(&dir, |_| {
            rounds += 1;
            // Don't wait for real seconds: three quick rounds are enough.
            std::thread::sleep(Duration::from_millis(200));
            rounds == 3
        })
        .expect("record");
        let now = archive::now_unix();
        let seconds = archive::read_seconds(&dir, now - 60, now + 1);
        assert_eq!(seconds.len(), 3);
        assert!(seconds.iter().all(|s| s.memory_used > 0 && (0.0..=1.0).contains(&s.cpu)));
        // The minute in progress is written when recording stops.
        assert!(!archive::read_minutes(&dir, now - 120, now + 1).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
