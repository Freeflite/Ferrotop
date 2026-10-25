//! CPU temperatures from LibreHardwareMonitor's built-in web server, for machines without PawnIO.
//!
//! LibreHardwareMonitor dropped its WMI interface in version 0.9.5, so this reads the JSON
//! sensor tree its "Remote Web Server" serves (Options → Remote Web Server → Run). Ferrotop only
//! ever connects to the loopback address, sends one fixed request, caps how much it reads and
//! how long it waits, and treats the reply as untrusted input.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::time::Duration;

use ferrotop_sys::cpu_temp::CpuTemps;

use crate::json::Json;

/// LibreHardwareMonitor's default web server port.
pub const DEFAULT_PORT: u16 = 8085;
const TIMEOUT: Duration = Duration::from_millis(400);
/// The full sensor tree of a large workstation is well under 1 MiB.
const MAX_RESPONSE: u64 = 8 << 20;

/// Fetches and parses the CPU temperatures from LibreHardwareMonitor on `127.0.0.1:port`.
/// `Err` means nothing answered (so the caller can back off); `Ok(None)` means it answered
/// but reported no CPU temperatures.
pub fn cpu_temps(port: u16) -> std::io::Result<Option<CpuTemps>> {
    Ok(parse_response(&fetch(port)?))
}

/// Everything after the bytes arrive: HTTP framing, JSON, and finding the CPU's sensors.
pub(crate) fn parse_response(response: &[u8]) -> Option<CpuTemps> {
    http_body(response)
        .and_then(|body| String::from_utf8(body).ok())
        .and_then(|text| Json::parse(&text))
        .and_then(|tree| parse_cpu_temps(&tree))
}

fn fetch(port: u16) -> std::io::Result<Vec<u8>> {
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut stream = TcpStream::connect_timeout(&address, TIMEOUT)?;
    stream.set_read_timeout(Some(TIMEOUT))?;
    stream.set_write_timeout(Some(TIMEOUT))?;
    // HTTP/1.0 so the reply isn't chunked and the server closes the connection when done.
    write!(stream, "GET /data.json HTTP/1.0\r\nHost: 127.0.0.1:{port}\r\nAccept: application/json\r\n\r\n")?;
    let mut response = Vec::new();
    stream.take(MAX_RESPONSE).read_to_end(&mut response)?;
    Ok(response)
}

/// The body of a `200` response, de-chunked if the server chunked it anyway.
fn http_body(response: &[u8]) -> Option<Vec<u8>> {
    let split = response.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = std::str::from_utf8(&response[..split]).ok()?;
    let body = &response[split + 4..];
    let mut lines = head.split("\r\n");
    let status = lines.next()?;
    if status.split_whitespace().nth(1) != Some("200") {
        return None;
    }
    let chunked = lines.any(|line| {
        let (name, value) = line.split_once(':').unwrap_or((line, ""));
        name.trim().eq_ignore_ascii_case("transfer-encoding") && value.to_ascii_lowercase().contains("chunked")
    });
    if chunked { dechunk(body) } else { Some(body.to_vec()) }
}

fn dechunk(mut body: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let line_end = body.windows(2).position(|w| w == b"\r\n")?;
        let size_text = std::str::from_utf8(&body[..line_end]).ok()?;
        let size_text = size_text.split(';').next()?.trim();
        let size = usize::from_str_radix(size_text, 16).ok()?;
        body = &body[line_end + 2..];
        if size == 0 {
            return Some(out);
        }
        let chunk = body.get(..size)?;
        out.extend_from_slice(chunk);
        body = body.get(size..)?.strip_prefix(b"\r\n")?;
    }
}

/// `"45.5 °C"` or (in comma-decimal locales) `"45,5 °C"` → 45.5.
fn parse_celsius(text: &str) -> Option<f32> {
    let number: String =
        text.trim().chars().take_while(|c| c.is_ascii_digit() || matches!(c, '-' | '.' | ',')).collect();
    let value: f32 = number.replace(',', ".").parse().ok()?;
    (value.is_finite() && (-50.0..=200.0).contains(&value) && text.contains("°C")).then_some(value)
}

/// `"CCD1 (Tdie)"`, `"CCD #2"` → 0, 1 (LibreHardwareMonitor counts from 1).
fn ccd_index(name: &str) -> Option<u32> {
    let rest = name.strip_prefix("CCD")?.trim_start_matches([' ', '#']);
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse::<u32>().ok()?.checked_sub(1)
}

/// `"CPU Core #3"` → 2 (but not the "Distance to TjMax" variants).
fn core_index(name: &str) -> Option<usize> {
    let rest = name.strip_prefix("CPU Core #")?;
    if !rest.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    rest.parse::<usize>().ok()?.checked_sub(1)
}

/// Is this node a CPU in the hardware tree?
fn is_cpu_hardware(node: &Json) -> bool {
    let field = |key| node.get(key).and_then(Json::as_str).unwrap_or_default();
    field("ImageURL").ends_with("cpu.png")
        || field("HardwareId").starts_with("/amdcpu")
        || field("HardwareId").starts_with("/intelcpu")
}

/// Collects every temperature sensor below the first CPU in the tree.
fn parse_cpu_temps(tree: &Json) -> Option<CpuTemps> {
    let cpu = find(tree, 0, &is_cpu_hardware)?;
    let mut sensors = Vec::new();
    collect_sensors(cpu, 0, &mut sensors);

    let mut temps = CpuTemps::default();
    let mut cores: Vec<(usize, f32)> = Vec::new();
    for (name, value) in sensors {
        match name.as_str() {
            "Core (Tctl/Tdie)" | "Core (Tctl)" | "CPU Package" | "Package" => temps.package_c = Some(value),
            "Core (Tdie)" => temps.die_c = Some(value),
            _ => {
                if let Some(ccd) = ccd_index(&name) {
                    if !temps.ccds.iter().any(|(i, _)| *i == ccd) {
                        temps.ccds.push((ccd, value));
                    }
                } else if let Some(core) = core_index(&name) {
                    cores.push((core, value));
                }
            }
        }
    }
    temps.ccds.sort_by_key(|(ccd, _)| *ccd);
    cores.sort_by_key(|(core, _)| *core);
    temps.cores = cores.into_iter().map(|(_, value)| value).collect();
    let any = temps.package_c.is_some() || temps.die_c.is_some() || !temps.ccds.is_empty() || !temps.cores.is_empty();
    any.then_some(temps)
}

const MAX_TREE_DEPTH: usize = 32;

fn find<'a>(node: &'a Json, depth: usize, predicate: &dyn Fn(&Json) -> bool) -> Option<&'a Json> {
    if depth > MAX_TREE_DEPTH {
        return None;
    }
    if predicate(node) {
        return Some(node);
    }
    node.get("Children")?.items().iter().find_map(|child| find(child, depth + 1, predicate))
}

/// `(sensor name, °C)` for every temperature sensor below `node`.
fn collect_sensors(node: &Json, depth: usize, out: &mut Vec<(String, f32)>) {
    if depth > MAX_TREE_DEPTH {
        return;
    }
    let field = |key| node.get(key).and_then(Json::as_str);
    let is_temperature = field("Type") == Some("Temperature")
        || field("SensorId").is_some_and(|id| id.contains("/temperature/"))
        || field("Value").is_some_and(|v| v.ends_with("°C"));
    if is_temperature && let (Some(name), Some(value)) = (field("Text"), field("Value").and_then(parse_celsius)) {
        out.push((name.to_owned(), value));
    }
    for child in node.get("Children").map(Json::items).unwrap_or_default() {
        collect_sensors(child, depth + 1, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape of LibreHardwareMonitor's `/data.json`, with made-up values.
    const SAMPLE: &str = r#"{"id":0,"Text":"Sensor","Children":[{"id":1,"Text":"TEST-PC","Children":[
        {"id":2,"Text":"Test Motherboard","ImageURL":"images_icon/mainboard.png","Children":[
            {"id":3,"Text":"Temperatures","Children":[{"id":4,"Text":"CPU","Value":"40.0 °C","Type":"Temperature"}]}]},
        {"id":5,"Text":"AMD Ryzen Test","ImageURL":"images_icon/cpu.png","Children":[
            {"id":6,"Text":"Temperatures","ImageURL":"images_icon/temperature.png","Children":[
                {"id":7,"Text":"Core (Tctl/Tdie)","Value":"61.3 °C","SensorId":"/amdcpu/0/temperature/2","Type":"Temperature"},
                {"id":8,"Text":"CCD2 (Tdie)","Value":"55,5 °C","SensorId":"/amdcpu/0/temperature/4","Type":"Temperature"},
                {"id":9,"Text":"CCD1 (Tdie)","Value":"58.0 °C","SensorId":"/amdcpu/0/temperature/3","Type":"Temperature"}]},
            {"id":10,"Text":"Clocks","Children":[{"id":11,"Text":"Core #1","Value":"4200 MHz","Type":"Clock"}]}]}]}]}"#;

    fn response(body: &str) -> Vec<u8> {
        format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len())
            .into_bytes()
    }

    #[test]
    fn reads_amd_temperatures_from_the_cpu_node_only() {
        let body = http_body(&response(SAMPLE)).unwrap();
        let temps = parse_cpu_temps(&Json::parse(std::str::from_utf8(&body).unwrap()).unwrap()).unwrap();
        assert_eq!(temps.package_c, Some(61.3), "not the motherboard's CPU sensor");
        assert_eq!(temps.ccds, [(0, 58.0), (1, 55.5)]);
    }

    #[test]
    fn reads_intel_package_and_cores() {
        let tree = Json::parse(
            r#"{"Text":"x","Children":[{"Text":"Intel Test","HardwareId":"/intelcpu/0","Children":[
            {"Text":"CPU Core #2","Value":"52 °C","Type":"Temperature"},
            {"Text":"CPU Core #1","Value":"50 °C","Type":"Temperature"},
            {"Text":"CPU Core #1 Distance to TjMax","Value":"50 °C","Type":"Temperature"},
            {"Text":"CPU Package","Value":"55 °C","Type":"Temperature"}]}]}"#,
        )
        .unwrap();
        let temps = parse_cpu_temps(&tree).unwrap();
        assert_eq!(temps.package_c, Some(55.0));
        assert_eq!(temps.cores, [50.0, 52.0]);
    }

    #[test]
    fn handles_chunked_and_error_responses() {
        assert_eq!(dechunk(b"4\r\nWiki\r\n5;x=y\r\npedia\r\n0\r\n\r\n").unwrap(), b"Wikipedia");
        assert!(dechunk(b"ff\r\nshort\r\n").is_none());
        let chunked = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n{}\r\n0\r\n\r\n";
        assert_eq!(http_body(chunked).unwrap(), b"{}");
        assert!(http_body(b"HTTP/1.1 404 Not Found\r\n\r\n{}").is_none());
        assert!(http_body(b"garbage").is_none());
    }

    #[test]
    fn parses_celsius_values() {
        assert_eq!(parse_celsius("45.5 °C"), Some(45.5));
        assert_eq!(parse_celsius("45,5 °C"), Some(45.5));
        assert_eq!(parse_celsius("4200 MHz"), None);
        assert_eq!(parse_celsius("9999 °C"), None);
        assert_eq!(parse_celsius(""), None);
    }

    #[test]
    fn no_server_is_an_error_not_a_hang() {
        // Port 1 is never LibreHardwareMonitor; connecting must fail fast.
        let started = std::time::Instant::now();
        assert!(cpu_temps(1).is_err());
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn never_panics_on_arbitrary_responses() {
        crate::fuzz::mutate(&response(SAMPLE), 5_000, |bytes| {
            if let Some(body) = http_body(bytes)
                && let Ok(text) = String::from_utf8(body)
                && let Some(tree) = Json::parse(&text)
            {
                let _ = parse_cpu_temps(&tree);
            }
        });
        crate::fuzz::mutate(b"4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n", 5_000, |bytes| {
            let _ = dechunk(bytes);
        });
    }
}
