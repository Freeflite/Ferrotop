//! Per-process disk and network activity, from the kernel's event trace (see
//! [`ferrotop_sys::etw`]) when Ferrotop runs as administrator, or else from the history
//! service's live figures when the service is running.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ferrotop_sys::etw::{self, IoTrace, TraceRole};
use ferrotop_sys::live::{LiveReader, LiveSnapshot};
use ferrotop_sys::process::ProcessInfo;

/// How long to wait before looking for the service's live figures again.
const LIVE_RETRY: Duration = Duration::from_secs(10);
/// Figures older than this mean the service has stopped.
const LIVE_STALE_MS: u64 = 3000;

/// Whether per-process disk and network activity can be shown.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ActivityStatus {
    #[default]
    NeedsAdmin,
    /// The trace couldn't start for another reason (another tool holding every system logger
    /// session, say).
    Failed,
    Running,
    /// From the history service, which traces on this process's behalf.
    Service,
}

/// One process's bytes since the previous sample.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProcessActivity {
    pub disk_read: u64,
    pub disk_written: u64,
    pub sent: u64,
    pub received: u64,
}

/// The history service's live figures, for a process that can't read them itself.
#[derive(Debug, Default)]
pub struct LiveSource {
    reader: RefCell<Option<LiveReader>>,
    retry_at: Cell<Option<Instant>>,
}

impl LiveSource {
    /// The service's latest figures, if it's publishing.
    pub fn latest(&self) -> Option<LiveSnapshot> {
        let now = Instant::now();
        if self.reader.borrow().is_none() {
            if self.retry_at.get().is_some_and(|at| now < at) {
                return None;
            }
            match LiveReader::open() {
                Ok(reader) => *self.reader.borrow_mut() = Some(reader),
                Err(_) => {
                    self.retry_at.set(Some(now + LIVE_RETRY));
                    return None;
                }
            }
        }
        let snapshot = self.reader.borrow().as_ref()?.read()?;
        let now_ms = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
        (now_ms.saturating_sub(snapshot.updated_unix_ms) < LIVE_STALE_MS).then_some(snapshot)
    }
}

/// Owns the trace; lives on the process sampler thread.
#[derive(Debug)]
pub struct ActivityTracer {
    trace: Option<IoTrace>,
    status: Cell<ActivityStatus>,
    /// The service's figures when there's no trace; `None` in the service itself.
    live: Option<LiveSource>,
    last_take: Cell<Option<Instant>>,
}

impl ActivityTracer {
    /// Starts tracing if it can. A window that can't relies on the history service instead;
    /// the service never reads its own figures. Elevation doesn't change while a process runs,
    /// so there's no point retrying the trace.
    pub fn start(role: TraceRole) -> Self {
        let (trace, status) = match IoTrace::start(role) {
            Ok(trace) => (Some(trace), ActivityStatus::Running),
            Err(error) if etw::is_access_denied(&error) => (None, ActivityStatus::NeedsAdmin),
            Err(_) => (None, ActivityStatus::Failed),
        };
        let live = (trace.is_none() && role == TraceRole::Window).then(LiveSource::default);
        Self { trace, status: Cell::new(status), live, last_take: Cell::new(None) }
    }

    pub fn status(&self) -> ActivityStatus {
        self.status.get()
    }

    /// Each process's activity since the last call, or `None` when there's neither a trace nor
    /// the service. Disk I/O is counted by the thread that issued it, so `processes` (whose
    /// thread lists say which thread is whose) attributes it; I/O by threads that have since
    /// exited is dropped.
    pub fn take(&self, processes: &[ProcessInfo]) -> Option<HashMap<u32, ProcessActivity>> {
        let now = Instant::now();
        let elapsed = self.last_take.replace(Some(now)).map_or(1.0, |at| now.duration_since(at).as_secs_f64());
        if let Some(trace) = &self.trace {
            let totals = trace.drain();
            return Some(attribute(&totals.disk_by_thread, &totals.network_by_process, processes));
        }
        let Some(snapshot) = self.live.as_ref().and_then(LiveSource::latest) else {
            if self.status.get() == ActivityStatus::Service {
                self.status.set(ActivityStatus::NeedsAdmin);
            }
            return None;
        };
        self.status.set(ActivityStatus::Service);
        // The service publishes rates; turn them into bytes over this sample's interval.
        let bytes = |rate: f32| (f64::from(rate) * elapsed) as u64;
        Some(
            snapshot
                .processes
                .iter()
                .map(|p| {
                    let activity = ProcessActivity {
                        disk_read: bytes(p.disk_read),
                        disk_written: bytes(p.disk_write),
                        sent: bytes(p.sent),
                        received: bytes(p.received),
                    };
                    (p.pid, activity)
                })
                .collect(),
        )
    }
}

pub(crate) fn attribute(
    disk_by_thread: &HashMap<u32, (u64, u64)>,
    network_by_process: &HashMap<u32, (u64, u64)>,
    processes: &[ProcessInfo],
) -> HashMap<u32, ProcessActivity> {
    let mut activity: HashMap<u32, ProcessActivity> = HashMap::new();
    if !disk_by_thread.is_empty() {
        let owner: HashMap<u32, u32> =
            processes.iter().flat_map(|p| p.thread_ids.iter().map(move |&tid| (tid, p.pid))).collect();
        for (thread, &(read, written)) in disk_by_thread {
            if let Some(&pid) = owner.get(thread) {
                let entry = activity.entry(pid).or_default();
                entry.disk_read = entry.disk_read.saturating_add(read);
                entry.disk_written = entry.disk_written.saturating_add(written);
            }
        }
    }
    for (&pid, &(sent, received)) in network_by_process {
        let entry = activity.entry(pid).or_default();
        entry.sent = entry.sent.saturating_add(sent);
        entry.received = entry.received.saturating_add(received);
    }
    activity
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attributes_disk_by_thread_and_network_by_process() {
        let processes = vec![
            ProcessInfo { pid: 10, thread_ids: vec![100, 101], ..ProcessInfo::default() },
            ProcessInfo { pid: 20, thread_ids: vec![200], ..ProcessInfo::default() },
        ];
        let disk = HashMap::from([(100, (4096, 0)), (101, (0, 512)), (200, (1, 1)), (999, (7, 7))]);
        let network = HashMap::from([(10, (1500, 60)), (30, (5, 5))]);
        let activity = attribute(&disk, &network, &processes);
        assert_eq!(activity[&10], ProcessActivity { disk_read: 4096, disk_written: 512, sent: 1500, received: 60 });
        assert_eq!(activity[&20], ProcessActivity { disk_read: 1, disk_written: 1, sent: 0, received: 0 });
        // A process that only sent is still counted; an unknown thread isn't.
        assert_eq!(activity[&30].sent, 5);
        assert_eq!(activity.len(), 3);
    }

    #[test]
    fn starts_or_says_why_not() {
        let tracer = ActivityTracer::start(TraceRole::Service);
        assert_eq!(tracer.take(&[]).is_some(), tracer.status() == ActivityStatus::Running);
    }
}
