//! TCP and UDP endpoints with their owning processes (`GetExtendedTcpTable`/`GetExtendedUdpTable`).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use windows_sys::Win32::Foundation::ERROR_INSUFFICIENT_BUFFER;
use windows_sys::Win32::NetworkManagement::IpHelper::{
    GetExtendedTcpTable, GetExtendedUdpTable, TCP_TABLE_OWNER_PID_ALL, UDP_TABLE_OWNER_PID,
};
use windows_sys::Win32::Networking::WinSock::{AF_INET, AF_INET6};

use crate::buffer::AlignedBuffer;
use crate::{Error, Result};

const MAX_TABLE: usize = 64 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Protocol {
    Tcp,
    Udp,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Connection {
    pub protocol: Protocol,
    pub local: (IpAddr, u16),
    /// `None` for UDP and listening TCP sockets.
    pub remote: Option<(IpAddr, u16)>,
    /// `MIB_TCP_STATE` (1 closed … 2 listen … 5 established … 12 delete TCB); 0 for UDP.
    pub state: u32,
    pub pid: u32,
}

/// Every TCP and UDP endpoint on the machine, IPv4 and IPv6.
pub fn connections() -> Result<Vec<Connection>> {
    let mut all = Vec::new();
    for (family, parse) in [(AF_INET, parse_tcp4 as fn(&[u8]) -> Option<Vec<Connection>>), (AF_INET6, parse_tcp6)] {
        let bytes = table(|buffer, size| {
            // SAFETY: the pointer/size pair describes memory owned by `buffer`; `size` is in/out.
            unsafe { GetExtendedTcpTable(buffer, size, 0, u32::from(family), TCP_TABLE_OWNER_PID_ALL, 0) }
        })?;
        all.extend(parse(&bytes).ok_or(Error::Malformed { call: "GetExtendedTcpTable" })?);
    }
    for (family, parse) in [(AF_INET, parse_udp4 as fn(&[u8]) -> Option<Vec<Connection>>), (AF_INET6, parse_udp6)] {
        let bytes = table(|buffer, size| {
            // SAFETY: as above.
            unsafe { GetExtendedUdpTable(buffer, size, 0, u32::from(family), UDP_TABLE_OWNER_PID, 0) }
        })?;
        all.extend(parse(&bytes).ok_or(Error::Malformed { call: "GetExtendedUdpTable" })?);
    }
    Ok(all)
}

/// Calls a size-negotiating IP Helper table function until the table fits.
fn table(mut call: impl FnMut(*mut std::ffi::c_void, &mut u32) -> u32) -> Result<Vec<u8>> {
    let mut buffer = AlignedBuffer::new(64 * 1024);
    for _ in 0..6 {
        let mut size = buffer.capacity_u32();
        match call(buffer.as_mut_ptr(), &mut size) {
            0 => return Ok(buffer.bytes(size as usize).to_vec()),
            ERROR_INSUFFICIENT_BUFFER if (size as usize) < MAX_TABLE => {
                // Connections come and go between calls; leave some headroom.
                buffer = AlignedBuffer::new(size as usize + 16 * 1024);
            }
            code => return Err(Error::Win32 { call: "GetExtendedTable", code }),
        }
    }
    Err(Error::Malformed { call: "GetExtendedTable" })
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    let b = bytes.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// Ports are stored in network byte order in the low 16 bits of a DWORD.
fn port_at(bytes: &[u8], at: usize) -> Option<u16> {
    let b = bytes.get(at..at.checked_add(2)?)?;
    Some(u16::from_be_bytes([b[0], b[1]]))
}

fn ipv4_at(bytes: &[u8], at: usize) -> Option<IpAddr> {
    let b: [u8; 4] = bytes.get(at..at.checked_add(4)?)?.try_into().ok()?;
    Some(IpAddr::V4(Ipv4Addr::from(b)))
}

fn ipv6_at(bytes: &[u8], at: usize) -> Option<IpAddr> {
    let b: [u8; 16] = bytes.get(at..at.checked_add(16)?)?.try_into().ok()?;
    Some(IpAddr::V6(Ipv6Addr::from(b)))
}

/// Rows after a `DWORD` count, each `row_size` bytes; the count may not exceed the buffer.
fn rows(bytes: &[u8], row_size: usize) -> Option<impl Iterator<Item = &[u8]>> {
    let count = u32_at(bytes, 0)? as usize;
    let body = bytes.get(4..)?;
    (count <= body.len() / row_size).then(|| body.chunks_exact(row_size).take(count))
}

const LISTEN: u32 = 2;

/// `MIB_TCPROW_OWNER_PID`: state, local addr, local port, remote addr, remote port, pid.
pub(crate) fn parse_tcp4(bytes: &[u8]) -> Option<Vec<Connection>> {
    rows(bytes, 24)?
        .map(|row| {
            let state = u32_at(row, 0)?;
            Some(Connection {
                protocol: Protocol::Tcp,
                local: (ipv4_at(row, 4)?, port_at(row, 8)?),
                remote: (state != LISTEN).then(|| Some((ipv4_at(row, 12)?, port_at(row, 16)?))).flatten(),
                state,
                pid: u32_at(row, 20)?,
            })
        })
        .collect()
}

/// `MIB_TCP6ROW_OWNER_PID`: local addr, scope, port, remote addr, scope, port, state, pid.
pub(crate) fn parse_tcp6(bytes: &[u8]) -> Option<Vec<Connection>> {
    rows(bytes, 56)?
        .map(|row| {
            let state = u32_at(row, 48)?;
            Some(Connection {
                protocol: Protocol::Tcp,
                local: (ipv6_at(row, 0)?, port_at(row, 20)?),
                remote: (state != LISTEN).then(|| Some((ipv6_at(row, 24)?, port_at(row, 44)?))).flatten(),
                state,
                pid: u32_at(row, 52)?,
            })
        })
        .collect()
}

/// `MIB_UDPROW_OWNER_PID`: local addr, local port, pid.
pub(crate) fn parse_udp4(bytes: &[u8]) -> Option<Vec<Connection>> {
    rows(bytes, 12)?
        .map(|row| {
            Some(Connection {
                protocol: Protocol::Udp,
                local: (ipv4_at(row, 0)?, port_at(row, 4)?),
                remote: None,
                state: 0,
                pid: u32_at(row, 8)?,
            })
        })
        .collect()
}

/// `MIB_UDP6ROW_OWNER_PID`: local addr, scope, port, pid.
pub(crate) fn parse_udp6(bytes: &[u8]) -> Option<Vec<Connection>> {
    rows(bytes, 28)?
        .map(|row| {
            Some(Connection {
                protocol: Protocol::Udp,
                local: (ipv6_at(row, 0)?, port_at(row, 20)?),
                remote: None,
                state: 0,
                pid: u32_at(row, 24)?,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One established IPv4 connection 192.0.2.10:50000 → 198.51.100.7:443 (documentation
    /// addresses) owned by PID 4321.
    fn tcp4_sample() -> Vec<u8> {
        let mut bytes = 1u32.to_le_bytes().to_vec();
        bytes.extend(5u32.to_le_bytes());
        bytes.extend([192, 0, 2, 10]);
        bytes.extend([0xC3, 0x50, 0, 0]);
        bytes.extend([198, 51, 100, 7]);
        bytes.extend([0x01, 0xBB, 0, 0]);
        bytes.extend(4321u32.to_le_bytes());
        bytes
    }

    #[test]
    fn decodes_tcp4_rows() {
        let parsed = parse_tcp4(&tcp4_sample()).unwrap();
        assert_eq!(
            parsed,
            [Connection {
                protocol: Protocol::Tcp,
                local: ("192.0.2.10".parse().unwrap(), 50000),
                remote: Some(("198.51.100.7".parse().unwrap(), 443)),
                state: 5,
                pid: 4321,
            }]
        );
    }

    #[test]
    fn rejects_counts_past_the_end() {
        let mut bytes = tcp4_sample();
        bytes[0] = 2;
        assert_eq!(parse_tcp4(&bytes), None);
        assert_eq!(parse_udp6(&[0, 0]), None);
        assert_eq!(parse_udp4(&0u32.to_le_bytes()), Some(Vec::new()));
    }

    #[test]
    fn lists_live_endpoints() {
        // Bind a UDP socket so there's at least one endpoint owned by this process.
        let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = socket.local_addr().unwrap().port();
        let all = connections().expect("connections");
        assert!(all.iter().any(|c| c.protocol == Protocol::Udp && c.local.1 == port && c.pid == std::process::id()));
    }

    #[test]
    fn parsers_never_panic() {
        crate::fuzz::mutate(&tcp4_sample(), 5_000, |bytes| {
            let _ = (parse_tcp4(bytes), parse_tcp6(bytes), parse_udp4(bytes), parse_udp6(bytes));
        });
    }
}
