//! LibreHardwareMonitor web-server replies: HTTP framing, de-chunking, JSON and the sensor tree.
#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| ferrotop_core::fuzzing::lhm_response(data));
