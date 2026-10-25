//! Entry points for the cargo-fuzz harness (`fuzz/`). Built only with the `fuzzing` feature.

/// Feeds `data` to one of the buffer parsers, chosen by its first byte.
pub fn parse(data: &[u8]) {
    let Some((&which, bytes)) = data.split_first() else { return };
    match which % 10 {
        0 => {
            let _ = crate::process::parse_processes(bytes);
        }
        1 => {
            let _ = crate::threads::parse_threads(bytes, 4);
        }
        2 => {
            let _ = crate::handles::parse_snapshot(bytes);
        }
        3 => {
            let _ = crate::handles::parse_object_types(bytes);
        }
        4 => {
            let _ = (
                crate::connections::parse_tcp4(bytes),
                crate::connections::parse_tcp6(bytes),
                crate::connections::parse_udp4(bytes),
                crate::connections::parse_udp6(bytes),
            );
        }
        5 => {
            let units: Vec<u16> = bytes.as_chunks::<2>().0.iter().map(|&pair| u16::from_le_bytes(pair)).collect();
            let _ = crate::environment::parse_environment(&units);
        }
        6 => {
            let _ = (crate::token::parse_groups(bytes), crate::token::parse_privileges(bytes));
        }
        7 => {
            let mut info = crate::hardware::CpuInfo::default();
            crate::hardware::parse_topology(bytes, &[0, 64], &mut info);
        }
        8 => {
            let _ = crate::hardware::parse_memory_slots(bytes);
        }
        _ => {
            let _ = (crate::net::parse_sockaddr(bytes), crate::token::Sid::parse(bytes));
        }
    }
}
