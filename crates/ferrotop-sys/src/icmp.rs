//! ICMP echo (ping) through the IP Helper API, for Ping and Trace route. No administrator rights
//! are needed, and nothing is sent anywhere but the address asked for.

use std::mem::offset_of;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::Duration;

use windows_sys::Win32::Foundation::{GetLastError, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::NetworkManagement::IpHelper::{
    ICMP_ECHO_REPLY, ICMPV6_ECHO_REPLY_LH, IP_DEST_HOST_UNREACHABLE, IP_DEST_NET_UNREACHABLE, IP_DEST_PORT_UNREACHABLE,
    IP_DEST_PROT_UNREACHABLE, IP_DEST_UNREACHABLE, IP_OPTION_INFORMATION, IP_REQ_TIMED_OUT, IP_SUCCESS,
    IP_TIME_EXCEEDED, IP_TTL_EXPIRED_REASSEM, IP_TTL_EXPIRED_TRANSIT, IPV6_ADDRESS_EX, Icmp6CreateFile, Icmp6SendEcho2,
    IcmpCloseHandle, IcmpCreateFile, IcmpSendEcho2,
};
use windows_sys::Win32::Networking::WinSock::{AF_INET6, IN6_ADDR, IN6_ADDR_0, SOCKADDR_IN6};

use crate::buffer::AlignedBuffer;
use crate::{Error, Result};

/// The 32-byte payload Windows' own ping sends.
const PAYLOAD: &[u8; 32] = b"abcdefghijklmnopqrstuvwabcdefghi";
/// Room for one reply, its echoed payload and any ICMP error details.
const REPLY_BUFFER: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EchoStatus {
    /// The target answered.
    Reply,
    /// A router on the way dropped the packet as its time to live ran out (what Trace route
    /// relies on); the reply comes from that router.
    TtlExpired,
    TimedOut,
    NetUnreachable,
    HostUnreachable,
    /// Another `IP_*` status code.
    Other(u32),
}

impl EchoStatus {
    fn from_code(code: u32) -> Self {
        match code {
            IP_SUCCESS => EchoStatus::Reply,
            IP_TTL_EXPIRED_TRANSIT | IP_TTL_EXPIRED_REASSEM | IP_TIME_EXCEEDED => EchoStatus::TtlExpired,
            IP_REQ_TIMED_OUT => EchoStatus::TimedOut,
            IP_DEST_NET_UNREACHABLE => EchoStatus::NetUnreachable,
            IP_DEST_HOST_UNREACHABLE | IP_DEST_UNREACHABLE | IP_DEST_PROT_UNREACHABLE | IP_DEST_PORT_UNREACHABLE => {
                EchoStatus::HostUnreachable
            }
            other => EchoStatus::Other(other),
        }
    }

    pub fn describe(self) -> String {
        match self {
            EchoStatus::Reply => "Reply".into(),
            EchoStatus::TtlExpired => "TTL expired in transit".into(),
            EchoStatus::TimedOut => "Timed out".into(),
            EchoStatus::NetUnreachable => "Network unreachable".into(),
            EchoStatus::HostUnreachable => "Host unreachable".into(),
            EchoStatus::Other(code) => format!("Error {code}"),
        }
    }
}

/// One echo's outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Echo {
    /// Who answered: the target, or a router on the way.
    pub from: Option<IpAddr>,
    pub status: EchoStatus,
    pub round_trip_ms: u32,
}

/// An open ICMP handle for one address family.
#[derive(Debug)]
pub struct Pinger {
    handle: HANDLE,
    v6: bool,
}

impl Drop for Pinger {
    fn drop(&mut self) {
        // SAFETY: `handle` came from Icmp(6)CreateFile and is closed exactly once, here.
        unsafe { IcmpCloseHandle(self.handle) };
    }
}

impl Pinger {
    /// Opens a pinger for IPv6 targets (`v6`) or IPv4 ones.
    pub fn open(v6: bool) -> Result<Self> {
        let handle = if v6 {
            // SAFETY: no preconditions.
            unsafe { Icmp6CreateFile() }
        } else {
            // SAFETY: no preconditions.
            unsafe { IcmpCreateFile() }
        };
        if handle == INVALID_HANDLE_VALUE || handle.is_null() {
            return Err(Error::last_win32(if v6 { "Icmp6CreateFile" } else { "IcmpCreateFile" }));
        }
        Ok(Self { handle, v6 })
    }

    /// Sends one echo request to `target` with time to live `ttl` and waits up to `timeout` for
    /// the answer. A timeout or an unreachable target is an `Echo`, not an error.
    pub fn echo(&self, target: IpAddr, ttl: u8, timeout: Duration) -> Result<Echo> {
        let options =
            IP_OPTION_INFORMATION { Ttl: ttl, Tos: 0, Flags: 0, OptionsSize: 0, OptionsData: std::ptr::null_mut() };
        let timeout_ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX).max(1);
        let mut reply = AlignedBuffer::new(REPLY_BUFFER);
        let count = match (target, self.v6) {
            (IpAddr::V4(v4), false) => {
                // The address in network byte order, i.e. its octets as they sit in memory.
                let destination = u32::from_ne_bytes(v4.octets());
                // SAFETY: the payload, options and reply buffer are live for the call and their
                // sizes are passed; no event or APC, so the call completes synchronously.
                unsafe {
                    IcmpSendEcho2(
                        self.handle,
                        std::ptr::null_mut(),
                        None,
                        std::ptr::null(),
                        destination,
                        PAYLOAD.as_ptr().cast(),
                        PAYLOAD.len() as u16,
                        &options,
                        reply.as_mut_ptr(),
                        reply.capacity_u32(),
                        timeout_ms,
                    )
                }
            }
            (IpAddr::V6(v6), true) => {
                let source = socket_address(Ipv6Addr::UNSPECIFIED);
                let destination = socket_address(v6);
                // SAFETY: as above, plus both socket addresses are live for the call.
                unsafe {
                    Icmp6SendEcho2(
                        self.handle,
                        std::ptr::null_mut(),
                        None,
                        std::ptr::null(),
                        &source,
                        &destination,
                        PAYLOAD.as_ptr().cast(),
                        PAYLOAD.len() as u16,
                        &options,
                        reply.as_mut_ptr(),
                        reply.capacity_u32(),
                        timeout_ms,
                    )
                }
            }
            _ => return Err(Error::Malformed { call: "Pinger::echo (address family)" }),
        };
        if count == 0 {
            // SAFETY: no preconditions; reads this thread's last error, set by the failed call.
            let code = unsafe { GetLastError() };
            return Ok(Echo { from: None, status: EchoStatus::from_code(code), round_trip_ms: 0 });
        }
        let bytes = reply.bytes(REPLY_BUFFER);
        if self.v6 { parse_reply_v6(bytes) } else { parse_reply_v4(bytes) }
            .ok_or(Error::Malformed { call: "IcmpSendEcho2" })
    }
}

fn socket_address(address: Ipv6Addr) -> SOCKADDR_IN6 {
    SOCKADDR_IN6 {
        sin6_family: AF_INET6,
        sin6_addr: IN6_ADDR { u: IN6_ADDR_0 { Byte: address.octets() } },
        ..SOCKADDR_IN6::default()
    }
}

fn u32_at(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(offset..offset + 4)?.try_into().ok()?))
}

/// Reads an `ICMP_ECHO_REPLY`.
fn parse_reply_v4(bytes: &[u8]) -> Option<Echo> {
    let address = bytes.get(offset_of!(ICMP_ECHO_REPLY, Address)..)?.first_chunk::<4>()?;
    Some(Echo {
        from: Some(IpAddr::V4(Ipv4Addr::from(*address))),
        status: EchoStatus::from_code(u32_at(bytes, offset_of!(ICMP_ECHO_REPLY, Status))?),
        round_trip_ms: u32_at(bytes, offset_of!(ICMP_ECHO_REPLY, RoundTripTime))?,
    })
}

/// Reads an `ICMPV6_ECHO_REPLY_LH`; its address is a byte-packed `IPV6_ADDRESS_EX`.
fn parse_reply_v6(bytes: &[u8]) -> Option<Echo> {
    let at = offset_of!(ICMPV6_ECHO_REPLY_LH, Address) + offset_of!(IPV6_ADDRESS_EX, sin6_addr);
    let address = bytes.get(at..)?.first_chunk::<16>()?;
    Some(Echo {
        from: Some(IpAddr::V6(Ipv6Addr::from(*address))),
        status: EchoStatus::from_code(u32_at(bytes, offset_of!(ICMPV6_ECHO_REPLY_LH, Status))?),
        round_trip_ms: u32_at(bytes, offset_of!(ICMPV6_ECHO_REPLY_LH, RoundTripTime))?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pings_loopback() {
        for target in [IpAddr::V4(Ipv4Addr::LOCALHOST), IpAddr::V6(Ipv6Addr::LOCALHOST)] {
            let Ok(pinger) = Pinger::open(target.is_ipv6()) else { continue };
            let echo = pinger.echo(target, 64, Duration::from_secs(2)).expect("echo");
            assert_eq!(echo.status, EchoStatus::Reply, "{target}");
            assert_eq!(echo.from, Some(target));
        }
    }

    #[test]
    fn a_family_mismatch_is_an_error() {
        let pinger = Pinger::open(false).expect("open");
        assert!(pinger.echo(IpAddr::V6(Ipv6Addr::LOCALHOST), 64, Duration::from_millis(10)).is_err());
    }

    #[test]
    fn parses_replies_and_rejects_short_ones() {
        let mut v4 = vec![0u8; size_of::<ICMP_ECHO_REPLY>()];
        v4[..4].copy_from_slice(&[10, 0, 0, 1]);
        v4[offset_of!(ICMP_ECHO_REPLY, Status)..][..4].copy_from_slice(&IP_TTL_EXPIRED_TRANSIT.to_le_bytes());
        v4[offset_of!(ICMP_ECHO_REPLY, RoundTripTime)..][..4].copy_from_slice(&7u32.to_le_bytes());
        let echo = parse_reply_v4(&v4).expect("v4");
        assert_eq!(echo.from, Some(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))));
        assert_eq!((echo.status, echo.round_trip_ms), (EchoStatus::TtlExpired, 7));
        assert_eq!(parse_reply_v4(&v4[..10]), None);

        let mut v6 = vec![0u8; size_of::<ICMPV6_ECHO_REPLY_LH>()];
        let at = offset_of!(ICMPV6_ECHO_REPLY_LH, Address) + offset_of!(IPV6_ADDRESS_EX, sin6_addr);
        v6[at..at + 16].copy_from_slice(&Ipv6Addr::LOCALHOST.octets());
        let echo = parse_reply_v6(&v6).expect("v6");
        assert_eq!((echo.from, echo.status), (Some(IpAddr::V6(Ipv6Addr::LOCALHOST)), EchoStatus::Reply));
        assert_eq!(parse_reply_v6(&v6[..20]), None);
    }
}
