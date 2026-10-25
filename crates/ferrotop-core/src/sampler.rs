//! Takes OS snapshots on a background thread so the UI never waits on a syscall.

use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::details::DetailsCollector;
use crate::perf::PerfCollector;
use crate::provider::Sample;

enum Command {
    RefreshNow,
    Stop,
}

/// Owns the sampling thread. Dropping it stops the thread and waits for it.
#[derive(Debug)]
pub struct Sampler {
    commands: Sender<Command>,
    worker: Option<JoinHandle<()>>,
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
    /// Captures a sample immediately and then every `interval`, handing each to `deliver` on the
    /// sampling thread. `lhm_port` is where LibreHardwareMonitor's web server is looked for.
    pub fn spawn<F>(interval: Duration, lhm_port: u16, deliver: F) -> std::io::Result<Self>
    where
        F: Fn(ferrotop_sys::Result<Sample>) + Send + 'static,
    {
        let (commands, inbox) = mpsc::channel();
        let worker = thread::Builder::new().name("ferrotop-sampler".into()).spawn(move || {
            let mut collector = DetailsCollector::new();
            let mut perf = PerfCollector::new();
            perf.set_lhm_port(lhm_port);
            loop {
                deliver(Sample::capture(&mut collector, &mut perf));
                match inbox.recv_timeout(interval) {
                    Ok(Command::RefreshNow) | Err(RecvTimeoutError::Timeout) => {}
                    Ok(Command::Stop) | Err(RecvTimeoutError::Disconnected) => break,
                }
            }
        })?;
        Ok(Self { commands, worker: Some(worker) })
    }

    /// Takes the next sample now instead of waiting out the interval.
    pub fn refresh_now(&self) {
        let _ = self.commands.send(Command::RefreshNow);
    }
}

impl Drop for Sampler {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Stop);
        if let Some(worker) = self.worker.take() {
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
        let sampler = Sampler::spawn(Duration::from_millis(50), crate::lhm::DEFAULT_PORT, move |sample| {
            let _ = tx.send(sample.map(|s| s.processes.len()));
        })
        .expect("spawn");
        let first = rx.recv_timeout(Duration::from_secs(10)).expect("first sample").expect("capture");
        assert!(first > 10);
        sampler.refresh_now();
        rx.recv_timeout(Duration::from_secs(10)).expect("second sample").expect("capture");
        drop(sampler);
        // The thread has stopped, so the sender is gone once any in-flight sample drains.
        while rx.recv_timeout(Duration::from_secs(2)).is_ok() {}
    }
}
