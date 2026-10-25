//! The JSON parser on arbitrary text.
#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| ferrotop_core::fuzzing::json(data));
