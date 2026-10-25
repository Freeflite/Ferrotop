//! Takes OS snapshots on background threads so the UI never waits on a syscall.
//!
//! Processes (with CPU and memory use) and the Performance view's devices are sampled on separate
//! threads, so a slow device (a driver library loading, a disk spinning up) never holds up the
//! process list, and the Performance view shows each device as soon as it can be read.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::activity::ActivityTracer;
use crate::details::DetailsCollector;
use crate::perf::{PerfCollector, PerfSample};
use crate::provider::Sample;

enum Command {
    RefreshNow,
    Stop,
}

/// Owns the sampling threads. Dropping it stops them and waits for them.
#[derive(Debug)]
pub struct Sampler {
    processes: Sender<Command>,
    devices: Sender<Command>,
    workers: Vec<JoinHandle<()>>,
}

impl std::fmt::Debug for Command {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Command::RefreshNow => "RefreshNow",
            Command::Stop => "Stop",
        })
    }
}

impl Sampler {
    /// Captures a process sample immediately and then every `interval`, handing each to `deliver`,
    /// and on a second thread does the same with Performance view samples and `deliver_perf`.
    /// `lhm_port` is where LibreHardwareMonitor's web server is looked for.
    pub fn spawn<F, P>(interval: Duration, lhm_port: u16, deliver: F, deliver_perf: P) -> std::io::Result<Self>
    where
        F: Fn(ferrotop_sys::Result<Sample>) + Send + 'static,
        P: Fn(PerfSample) + Send + 'static,
    {
        let (processes, process_inbox) = mpsc::channel();
        let (devices, device_inbox) = mpsc::channel();
        // Built first, so dropping it stops whatever started if a later spawn fails.
        let mut sampler = Self { processes, devices, workers: Vec::new() };
        sampler.workers.push(thread::Builder::new().name("ferrotop-sampler".into()).spawn(move || {
            let mut collector = DetailsCollector::new();
            let tracer = ActivityTracer::start(true);
            loop {
                deliver(Sample::capture(&mut collector, &tracer));
                if !wait(&process_inbox, interval) {
                    break;
                }
            }
        })?);
        sampler.workers.push(thread::Builder::new().name("ferrotop-perf".into()).spawn(move || {
            let mut perf = PerfCollector::new();
            perf.set_lhm_port(lhm_port);
            loop {
                deliver_perf(perf.collect());
                // One slow source per round, after the sample, so what's ready shows first.
                perf.set_up_next();
                if !wait(&device_inbox, interval) {
                    break;
                }
            }
        })?);
        Ok(sampler)
    }

    /// Takes the next process sample now instead of waiting out the interval.
    pub fn refresh_now(&self) {
        let _ = self.processes.send(Command::RefreshNow);
    }
}

/// Waits out `interval` unless told to refresh sooner; `false` once told to stop.
fn wait(inbox: &Receiver<Command>, interval: Duration) -> bool {
    match inbox.recv_timeout(interval) {
        Ok(Command::RefreshNow) | Err(RecvTimeoutError::Timeout) => true,
        Ok(Command::Stop) | Err(RecvTimeoutError::Disconnected) => false,
    }
}

impl Drop for Sampler {
    fn drop(&mut self) {
        let _ = self.processes.send(Command::Stop);
        let _ = self.devices.send(Command::Stop);
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;

    #[test]
    fn delivers_samples_until_dropped() {
        let (tx, rx) = mpsc::channel();
        let (perf_tx, perf_rx) = mpsc::channel();
        let sampler = Sampler::spawn(
            Duration::from_millis(50),
            crate::lhm::DEFAULT_PORT,
            move |sample| {
                let _ = tx.send(sample.map(|s| s.processes.len()));
            },
            move |perf| {
                let _ = perf_tx.send(perf.machine.cpu.logical);
            },
        )
        .expect("spawn");
        let first = rx.recv_timeout(Duration::from_secs(10)).expect("first sample").expect("capture");
        assert!(first > 10);
        assert!(perf_rx.recv_timeout(Duration::from_secs(10)).expect("first performance sample") > 0);
        sampler.refresh_now();
        rx.recv_timeout(Duration::from_secs(10)).expect("second sample").expect("capture");
        drop(sampler);
        // The threads have stopped, so the senders are gone once any in-flight samples drain.
        while rx.recv_timeout(Duration::from_secs(2)).is_ok() {}
        while perf_rx.recv_timeout(Duration::from_secs(2)).is_ok() {}
    }
}
