//! Network adapters and their byte counters, from the IP Helper interface table.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use windows_sys::Win32::NetworkManagement::IpHelper::{
    FreeMibTable, GetIfTable2, GetUnicastIpAddressTable, MIB_IF_ROW2, MIB_IF_TABLE2, MIB_UNICASTIPADDRESS_ROW,
};
use windows_sys::Win32::Networking::WinSock::{AF_UNSPEC, SOCKADDR_INET};

use crate::{Error, Result};

const IF_TYPE_ETHERNET_CSMACD: u32 = 6;
const IF_TYPE_IEEE80211: u32 = 71;
const MEDIA_CONNECT_STATE_CONNECTED: i32 = 1;
// Bit of `InterfaceAndOperStatusFlags` marking a filter-driver layer over another interface.
const FILTER_INTERFACE: u8 = 1 << 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetAdapter {
    pub index: u32,
    /// Friendly name, e.g. `Ethernet` or `vEthernet (Default Switch)`.
    pub alias: String,
    /// Hardware name, e.g. `Hyper-V Virtual Ethernet Adapter`.
    pub description: String,
    pub wireless: bool,
    pub connected: bool,
    /// Link speed in bits per second.
    pub link_speed: u64,
    /// Cumulative bytes.
    pub received: u64,
    pub sent: u64,
    pub addresses: Vec<IpAddr>,
}

/// Frees a table the IP Helper API allocated.
struct MibTable(*const std::ffi::c_void);

impl Drop for MibTable {
    fn drop(&mut self) {
        // SAFETY: the pointer came from a successful IP Helper table call and is freed exactly once.
        unsafe { FreeMibTable(self.0) };
    }
}

/// The adapters worth graphing: connected Ethernet and Wi-Fi interfaces that carry a routable IP
/// address. That skips the filter-driver duplicates every adapter also appears as, Hyper-V switch
/// plumbing, and NICs bound underneath a virtual switch (their traffic shows on the vEthernet
/// adapter instead).
pub fn adapters() -> Result<Vec<NetAdapter>> {
    let mut addresses = addresses().unwrap_or_default();
    let mut raw: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
    // SAFETY: `raw` is a valid out-pointer; on success it receives a table freed by `MibTable`.
    let status = unsafe { GetIfTable2(&mut raw) };
    if status != 0 || raw.is_null() {
        return Err(Error::Win32 { call: "GetIfTable2", code: status });
    }
    let _table = MibTable(raw.cast());
    // SAFETY: `raw` points at a valid MIB_IF_TABLE2 kept alive by `_table`.
    let count = unsafe { (*raw).NumEntries } as usize;
    // SAFETY: `raw` points at a valid table; this only takes the address of its row array.
    let first_row = unsafe { std::ptr::addr_of!((*raw).Table) }.cast::<MIB_IF_ROW2>();
    // SAFETY: the table is a header followed by `NumEntries` contiguous rows, kept alive by
    // `_table` for as long as this slice is used.
    let rows: &[MIB_IF_ROW2] = unsafe { std::slice::from_raw_parts(first_row, count) };

    let mut adapters: Vec<NetAdapter> = rows
        .iter()
        .filter(|row| {
            matches!(row.Type, IF_TYPE_ETHERNET_CSMACD | IF_TYPE_IEEE80211)
                && row.InterfaceAndOperStatusFlags._bitfield & FILTER_INTERFACE == 0
                && row.MediaConnectState == MEDIA_CONNECT_STATE_CONNECTED
        })
        .map(|row| NetAdapter {
            index: row.InterfaceIndex,
            alias: utf16_field(&row.Alias),
            description: utf16_field(&row.Description),
            wireless: row.Type == IF_TYPE_IEEE80211,
            connected: true,
            link_speed: row.ReceiveLinkSpeed.max(row.TransmitLinkSpeed),
            received: row.InOctets,
            sent: row.OutOctets,
            addresses: addresses.remove(&row.InterfaceIndex).unwrap_or_default(),
        })
        .filter(|adapter| adapter.addresses.iter().any(|ip| is_routable(*ip)))
        .collect();
    adapters.sort_by_key(|adapter| adapter.index);
    Ok(adapters)
}

fn is_routable(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => !v4.is_link_local() && !v4.is_loopback() && !v4.is_unspecified(),
        IpAddr::V6(v6) => !v6.is_unicast_link_local() && !v6.is_loopback() && !v6.is_unspecified(),
    }
}

/// Unicast IP addresses by interface index.
fn addresses() -> Result<HashMap<u32, Vec<IpAddr>>> {
    let mut raw = std::ptr::null_mut();
    // SAFETY: `raw` is a valid out-pointer; on success it receives a table freed by `MibTable`.
    let status = unsafe { GetUnicastIpAddressTable(AF_UNSPEC, &mut raw) };
    if status != 0 || raw.is_null() {
        return Err(Error::Win32 { call: "GetUnicastIpAddressTable", code: status });
    }
    let _table = MibTable(raw.cast());
    // SAFETY: `raw` points at a valid table kept alive by `_table`.
    let count = unsafe { (*raw).NumEntries } as usize;
    // SAFETY: `raw` points at a valid table; this only takes the address of its row array.
    let first_row = unsafe { std::ptr::addr_of!((*raw).Table) }.cast::<MIB_UNICASTIPADDRESS_ROW>();
    // SAFETY: `NumEntries` contiguous rows follow the header, kept alive by `_table`.
    let rows: &[MIB_UNICASTIPADDRESS_ROW] = unsafe { std::slice::from_raw_parts(first_row, count) };

    let mut by_interface: HashMap<u32, Vec<IpAddr>> = HashMap::new();
    for row in rows {
        // SAFETY: SOCKADDR_INET is a union of plain socket-address structs that the OS filled in,
        // so all of its bytes are initialized; we only read them as bytes.
        let bytes =
            unsafe { std::slice::from_raw_parts((&raw const row.Address).cast::<u8>(), size_of::<SOCKADDR_INET>()) };
        if let Some(ip) = parse_sockaddr(bytes) {
            by_interface.entry(row.InterfaceIndex).or_default().push(ip);
        }
    }
    Ok(by_interface)
}

/// `SOCKADDR_IN` / `SOCKADDR_IN6` bytes to an address.
pub(crate) fn parse_sockaddr(bytes: &[u8]) -> Option<IpAddr> {
    const AF_INET: u16 = 2;
    const AF_INET6: u16 = 23;
    match u16::from_le_bytes([*bytes.first()?, *bytes.get(1)?]) {
        AF_INET => {
            let v4 = bytes.get(4..8)?;
            Some(IpAddr::V4(Ipv4Addr::new(v4[0], v4[1], v4[2], v4[3])))
        }
        AF_INET6 => Some(IpAddr::V6(Ipv6Addr::from(<[u8; 16]>::try_from(bytes.get(8..24)?).ok()?))),
        _ => None,
    }
}

fn utf16_field(field: &[u16]) -> String {
    let len = field.iter().position(|&c| c == 0).unwrap_or(field.len());
    String::from_utf16_lossy(&field[..len])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_adapters_without_filter_duplicates() {
        let adapters = adapters().expect("adapters");
        assert!(!adapters.is_empty());
        assert!(adapters.iter().all(|a| !a.alias.contains("Filter") && a.addresses.iter().any(|ip| is_routable(*ip))));
    }

    #[test]
    fn parses_socket_addresses() {
        let mut v4 = [0u8; 28];
        v4[0] = 2;
        v4[4..8].copy_from_slice(&[192, 168, 88, 20]);
        assert_eq!(parse_sockaddr(&v4), Some(IpAddr::V4(Ipv4Addr::new(192, 168, 88, 20))));
        let mut v6 = [0u8; 28];
        v6[0] = 23;
        v6[8] = 0xfe;
        v6[9] = 0x80;
        v6[23] = 1;
        assert_eq!(parse_sockaddr(&v6), Some(IpAddr::V6("fe80::1".parse().unwrap())));
        assert_eq!(parse_sockaddr(&[2]), None);
    }
}
