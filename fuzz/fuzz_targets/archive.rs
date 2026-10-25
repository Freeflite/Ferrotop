//! The history files the service writes (per-second records and busiest-process minutes).
#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| ferrotop_core::fuzzing::archive(data));
