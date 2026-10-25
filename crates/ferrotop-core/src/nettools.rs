//! Ping and Trace route: ICMP echoes to an address the user chose, and only to it.

use std::net::{IpAddr, ToSocketAddrs};
use std::thread;
use std::time::{Duration, Instant};

use ferrotop_sys::icmp::{Echo, EchoStatus, Pinger};

use crate::table::{Cell, Column, Table, Tone};

/// Echoes Ping sends, one a second, as Windows' ping does.
pub const PING_COUNT: u32 = 4;
/// Trace route gives up after this many routers.
pub const MAX_HOPS: u8 = 30;
const PING_INTERVAL: Duration = Duration::from_secs(1);
const TIMEOUT: Duration = Duration::from_secs(2);
/// Ping's time to live: plenty for any route.
const PING_TTL: u8 = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Ping,
    TraceRoute,
}

impl Tool {
    pub fn title(self) -> &'static str {
        match self {
            Tool::Ping => "Ping",
            Tool::TraceRoute => "Trace route",
        }
    }
}

/// One probe: its number (Ping) or hop (Trace route), and what came back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    pub step: u32,
    pub echo: Result<Echo, String>,
}

/// Turns what the user typed into an address: an IPv4 or IPv6 address, or a host name looked
/// up through Windows' resolver.
pub fn resolve(target: &str) -> Result<IpAddr, String> {
    let target = target.trim();
    if target.is_empty() {
        return Err("Type an address or a host name".into());
    }
    if let Ok(address) = target.trim_start_matches('[').trim_end_matches(']').parse::<IpAddr>() {
        return Ok(address);
    }
    let mut found = (target, 0).to_socket_addrs().map_err(|error| format!("Couldn't look up {target}: {error}"))?;
    found.next().map(|address| address.ip()).ok_or_else(|| format!("{target} has no addresses"))
}

/// Runs `tool` against `target`, handing each probe to `report` as it completes. `report`
/// returns `false` to stop early.
pub fn run(tool: Tool, target: IpAddr, mut report: impl FnMut(Probe) -> bool) -> Result<(), String> {
    let pinger = Pinger::open(target.is_ipv6()).map_err(|error| error.to_string())?;
    match tool {
        Tool::Ping => {
            for step in 1..=PING_COUNT {
                let started = Instant::now();
                let echo = pinger.echo(target, PING_TTL, TIMEOUT).map_err(|error| error.to_string());
                if !report(Probe { step, echo }) {
                    break;
                }
                if step < PING_COUNT {
                    thread::sleep(PING_INTERVAL.saturating_sub(started.elapsed()));
                }
            }
        }
        Tool::TraceRoute => {
            for ttl in 1..=MAX_HOPS {
                let echo = pinger.echo(target, ttl, TIMEOUT).map_err(|error| error.to_string());
                // Done once the target answers, or a router says it can't be reached.
                let done = matches!(&echo, Ok(e) if e.status == EchoStatus::Reply
                    || matches!(e.status, EchoStatus::NetUnreachable | EchoStatus::HostUnreachable));
                if !report(Probe { step: u32::from(ttl), echo }) || done {
                    break;
                }
            }
        }
    }
    Ok(())
}

fn milliseconds(ms: u32) -> String {
    if ms == 0 { "<1 ms".into() } else { format!("{ms} ms") }
}

pub fn table(tool: Tool, probes: &[Probe]) -> Table {
    let first = match tool {
        Tool::Ping => "#",
        Tool::TraceRoute => "Hop",
    };
    let mut table = Table::new(&[
        Column::number(first, 60),
        Column::text("Address", 300),
        Column::text("Result", 240),
        Column::number("Time", 90),
    ]);
    for probe in probes {
        let step = Cell::count(u64::from(probe.step));
        let row = match &probe.echo {
            Ok(echo) => {
                let address = echo.from.map_or_else(|| "*".to_owned(), |address| address.to_string());
                let answered = matches!(echo.status, EchoStatus::Reply | EchoStatus::TtlExpired);
                let time = if answered {
                    Cell::number(f64::from(echo.round_trip_ms), milliseconds(echo.round_trip_ms))
                } else {
                    Cell::text("")
                };
                let tone = match echo.status {
                    EchoStatus::Reply => Tone::Good,
                    EchoStatus::TtlExpired => Tone::Plain,
                    EchoStatus::TimedOut => Tone::Muted,
                    _ => Tone::Warning,
                };
                (vec![step, Cell::text(address), Cell::text(echo.status.describe()), time], tone)
            }
            Err(error) => (vec![step, Cell::text(""), Cell::text(error), Cell::text("")], Tone::Warning),
        };
        table.push(u64::from(probe.step), row.0, row.1);
    }
    table
}

/// A one-line result, e.g. `4 sent, 4 received, 0% lost · 1 to 3 ms, average 2 ms`.
pub fn summary(tool: Tool, target: IpAddr, probes: &[Probe]) -> String {
    let replies: Vec<u32> = probes
        .iter()
        .filter_map(|p| p.echo.as_ref().ok().filter(|e| e.status == EchoStatus::Reply).map(|e| e.round_trip_ms))
        .collect();
    match tool {
        Tool::Ping => {
            let sent = probes.len();
            let lost = (sent - replies.len()) * 100 / sent.max(1);
            let mut text = format!("{target}: {sent} sent, {} received, {lost}% lost", replies.len());
            if let (Some(min), Some(max)) = (replies.iter().min(), replies.iter().max()) {
                let average = replies.iter().map(|&ms| u64::from(ms)).sum::<u64>() / replies.len() as u64;
                text += &format!(
                    " · {} to {}, average {}",
                    milliseconds(*min),
                    milliseconds(*max),
                    milliseconds(average as u32)
                );
            }
            text
        }
        Tool::TraceRoute => match probes.last() {
            _ if !replies.is_empty() => {
                format!("{target}: reached in {} hop{}", probes.len(), if probes.len() == 1 { "" } else { "s" })
            }
            Some(Probe { echo: Ok(echo), .. }) if echo.status != EchoStatus::TimedOut => {
                format!("{target}: stopped at hop {}: {}", probes.len(), echo.status.describe())
            }
            _ => format!("{target}: not reached within {} hops", probes.len()),
        },
    }
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, Ipv6Addr};

    use super::*;

    #[test]
    fn resolves_addresses_and_local_names() {
        assert_eq!(resolve(" 127.0.0.1 "), Ok(IpAddr::V4(Ipv4Addr::LOCALHOST)));
        assert_eq!(resolve("[::1]"), Ok(IpAddr::V6(Ipv6Addr::LOCALHOST)));
        assert!(resolve("localhost").expect("localhost").is_loopback());
        assert!(resolve("   ").is_err());
    }

    #[test]
    fn pings_and_traces_loopback() {
        let target = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let mut probes = Vec::new();
        run(Tool::TraceRoute, target, |probe| {
            probes.push(probe);
            true
        })
        .expect("trace route");
        // Loopback is one hop away.
        assert_eq!(probes.len(), 1);
        assert_eq!(summary(Tool::TraceRoute, target, &probes), "127.0.0.1: reached in 1 hop");

        probes.clear();
        // Stop after the first echo, so the test doesn't wait out the interval.
        run(Tool::Ping, target, |probe| {
            probes.push(probe);
            false
        })
        .expect("ping");
        assert_eq!(probes.len(), 1);
        assert!(summary(Tool::Ping, target, &probes).starts_with("127.0.0.1: 1 sent, 1 received, 0% lost"));
        let table = table(Tool::Ping, &probes);
        assert_eq!(table.rows.len(), 1);
        assert_eq!(table.rows[0].cells[2].text, "Reply");
    }
}
