//! Per-process disk and network activity, from the kernel's event trace (see
//! [`ferrotop_sys::etw`]). Only available when Ferrotop runs as administrator.

use std::collections::HashMap;

use ferrotop_sys::etw::{self, IoTrace};
use ferrotop_sys::process::ProcessInfo;

/// Whether per-process disk and network activity can be shown.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ActivityStatus {
    #[default]
    NeedsAdmin,
    /// The trace couldn't start for another reason (another tool holding every system logger
    /// session, say).
    Failed,
    Running,
}

/// One process's bytes since the previous sample.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProcessActivity {
    pub disk_read: u64,
    pub disk_written: u64,
    pub sent: u64,
    pub received: u64,
}

/// Owns the trace; lives on the process sampler thread.
#[derive(Debug)]
pub struct ActivityTracer {
    trace: Option<IoTrace>,
    status: ActivityStatus,
}

impl Default for ActivityTracer {
    fn default() -> Self {
        Self::start()
    }
}

impl ActivityTracer {
    /// Starts tracing if it can. Elevation doesn't change while a process runs, so there's no
    /// point retrying later.
    pub fn start() -> Self {
        match IoTrace::start() {
            Ok(trace) => Self { trace: Some(trace), status: ActivityStatus::Running },
            Err(error) if etw::is_access_denied(&error) => Self { trace: None, status: ActivityStatus::NeedsAdmin },
            Err(_) => Self { trace: None, status: ActivityStatus::Failed },
        }
    }

    pub fn status(&self) -> ActivityStatus {
        self.status
    }

    /// Each process's activity since the last call, or `None` when not tracing. Disk I/O is
    /// counted by the thread that issued it, so `processes` (whose thread lists say which thread
    /// is whose) attributes it; I/O by threads that have since exited is dropped.
    pub fn take(&self, processes: &[ProcessInfo]) -> Option<HashMap<u32, ProcessActivity>> {
        let totals = self.trace.as_ref()?.drain();
        Some(attribute(&totals.disk_by_thread, &totals.network_by_process, processes))
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
        let tracer = ActivityTracer::start();
        assert_eq!(tracer.take(&[]).is_some(), tracer.status() == ActivityStatus::Running);
    }
}
