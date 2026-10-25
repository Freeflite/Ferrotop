//! Kernel, firmware and IP Helper buffer parsers on arbitrary bytes (the first byte picks one).
#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| ferrotop_sys::fuzzing::parse(data));
