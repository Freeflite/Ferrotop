//! Entry points for the cargo-fuzz harness (`fuzz/`). Built only with the `fuzzing` feature.

/// The JSON parser on arbitrary text.
pub fn json(data: &[u8]) {
    if let Ok(text) = std::str::from_utf8(data) {
        let _ = crate::json::Json::parse(text);
    }
}

/// LibreHardwareMonitor's reply handling: HTTP framing, de-chunking, JSON and the sensor tree.
pub fn lhm_response(data: &[u8]) {
    crate::lhm::parse_response(data);
}

/// The history files the service writes: per-second records, then busiest-process minutes.
pub fn archive(data: &[u8]) {
    let _ = crate::archive::parse_seconds(data);
    let _ = crate::archive::parse_minutes(data);
}
