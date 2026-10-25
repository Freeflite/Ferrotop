//! Mutation tests for every parser that reads kernel- or firmware-supplied buffers.
//!
//! Each parser gets thousands of truncated, bit-flipped, spliced and random variants of a real
//! buffer captured on the test machine (nothing is stored in the repository) and must reject or
//! survive them without panicking, looping forever or reading out of bounds. (The pure parsers'
//! own mutation tests also run under Miri in CI; these need live seeds from Windows.)

use crate::fuzz::mutate;

const ROUNDS: usize = 4_000;

fn system_process_buffer() -> Vec<u8> {
    let mut buffer = crate::buffer::AlignedBuffer::new(0x10_0000);
    let len = crate::buffer::query_system_information(
        windows_sys::Wdk::System::SystemInformation::SystemProcessInformation,
        &mut buffer,
    )
    .expect("process snapshot");
    // The first few entries are plenty; mutations reach every field.
    buffer.bytes(len.min(16 * 1024)).to_vec()
}

#[test]
fn process_and_thread_parsers_survive_mutation() {
    let seed = system_process_buffer();
    mutate(&seed, ROUNDS, |bytes| {
        let _ = crate::process::parse_processes(bytes);
        let _ = crate::threads::parse_threads(bytes, 4);
    });
}

#[test]
fn topology_parser_survives_mutation() {
    let seed = crate::hardware::logical_processor_records().expect("topology");
    let seed = &seed[..seed.len().min(8 * 1024)];
    let bases = [0usize, 64];
    mutate(seed, ROUNDS, |bytes| {
        let mut info = crate::hardware::CpuInfo::default();
        crate::hardware::parse_topology(bytes, &bases, &mut info);
    });
}

#[test]
fn smbios_parser_survives_mutation() {
    let mut table = vec![0, 3, 4, 0];
    let mut data = vec![17u8, 0x22, 0x40, 0x00];
    data.resize(0x22, 0);
    data[0x0C..0x0E].copy_from_slice(&0x7FFFu16.to_le_bytes());
    data[0x10] = 1;
    data.extend_from_slice(b"SLOT\0\0");
    table.extend_from_slice(&(data.len() as u32).to_le_bytes());
    table.extend_from_slice(&data);
    mutate(&table, ROUNDS, |bytes| {
        let _ = crate::hardware::parse_memory_slots(bytes);
    });
}

#[test]
fn small_parsers_survive_mutation() {
    mutate(&[2, 0, 0, 80, 192, 0, 2, 1, 0, 0, 0, 0, 0, 0, 0, 0], ROUNDS, |bytes| {
        let _ = crate::net::parse_sockaddr(bytes);
    });
    mutate(&[1, 2, 0, 0, 0, 0, 0, 5, 21, 0, 0, 0, 0xE9, 3, 0, 0], ROUNDS, |bytes| {
        let _ = crate::token::Sid::parse(bytes);
    });
}
