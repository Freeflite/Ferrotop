//! Checks a running history service from the outside, as a Ferrotop window would: prints what
//! the live section and the history files hold, and fails if either is missing or stale.
//! `cargo run -p ferrotop-core --example history_check` (CI runs it after installing the service).
//! With `--window-trace` it first runs a window's own kernel trace for a moment, as an elevated
//! Ferrotop would, which must leave the service's trace alone.

use ferrotop_core::archive;
use ferrotop_sys::live::LiveReader;
use ferrotop_sys::service_host;

fn main() {
    if std::env::args().any(|a| a == "--window-trace") {
        let trace = ferrotop_sys::etw::IoTrace::start(ferrotop_sys::etw::TraceRole::Window)
            .expect("window trace (needs admin)");
        std::thread::sleep(std::time::Duration::from_secs(2));
        drop(trace);
        println!("ran and stopped a window trace");
    }
    let snapshot = match LiveReader::open() {
        Ok(reader) => reader.read(),
        Err(error) => {
            println!("live: can't open the section: {error}");
            None
        }
    };
    let now_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
    match &snapshot {
        Some(live) => println!(
            "live: updated {} ms ago, {} processes with disk or network activity, temperatures: {}",
            now_ms.saturating_sub(live.updated_unix_ms),
            live.processes.len(),
            if live.temps.is_some() { "yes" } else { "no" }
        ),
        None => println!("live: none"),
    }
    let dir = service_host::data_dir().expect("data folder");
    let now = archive::now_unix();
    let seconds = archive::read_seconds(&dir, now - 600, now);
    let minutes = archive::read_minutes(&dir, now - 600, now);
    println!(
        "history in {}: trusted={}, {} seconds and {} minutes in the last 10 minutes",
        dir.display(),
        service_host::is_trusted_dir(&dir),
        seconds.len(),
        minutes.len()
    );
    if let Some(last) = seconds.last() {
        println!("latest second: cpu {:.1}%, memory {} bytes", last.cpu * 100.0, last.memory_used);
    }
    let fresh = snapshot.is_some_and(|live| now_ms.saturating_sub(live.updated_unix_ms) < 5000);
    if !fresh || seconds.len() < 5 || !service_host::is_trusted_dir(&dir) {
        eprintln!("the history service isn't working as it should");
        std::process::exit(1);
    }
}
