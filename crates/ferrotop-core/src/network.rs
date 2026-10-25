//! The Network page: TCP and UDP endpoints with their owning processes.

use std::collections::HashMap;
use std::net::IpAddr;

use ferrotop_sys::connections::{Connection, Protocol};

use crate::names;
use crate::properties::stable_id;
use crate::table::{Cell, Column, Table, Tone};

/// `192.0.2.1`, or `[2001:db8::1]`-style IPv6 without brackets; `*` for the unspecified address.
fn address(ip: IpAddr) -> String {
    if ip.is_unspecified() { "*".into() } else { ip.to_string() }
}

pub fn connections_table(connections: &[Connection], names: &HashMap<u32, String>) -> Table {
    let mut table = Table::new(&[
        Column::text("Process", 190),
        Column::number("PID", 70),
        Column::text("Protocol", 80),
        Column::text("Local address", 220),
        Column::number("Local port", 80),
        Column::text("Remote address", 220),
        Column::number("Remote port", 90),
        Column::text("State", 110),
    ]);
    for connection in connections {
        let protocol = match (connection.protocol, connection.local.0) {
            (Protocol::Tcp, IpAddr::V4(_)) => "TCP",
            (Protocol::Tcp, IpAddr::V6(_)) => "TCPv6",
            (Protocol::Udp, IpAddr::V4(_)) => "UDP",
            (Protocol::Udp, IpAddr::V6(_)) => "UDPv6",
        };
        let process = match connection.pid {
            0 => "System Idle Process".to_owned(),
            pid => names.get(&pid).cloned().unwrap_or_else(|| "Non-existent process".into()),
        };
        let tone = match connection.state {
            2 => Tone::Muted,
            5 => Tone::Good,
            _ => Tone::Plain,
        };
        let (remote_address, remote_port) = match connection.remote {
            Some((ip, port)) => (Cell::text(address(ip)), Cell::number(f64::from(port), port.to_string())),
            None => (Cell::text(""), Cell::text("")),
        };
        let key = format!("{protocol}|{:?}|{:?}|{}", connection.local, connection.remote, connection.pid);
        table.push(
            stable_id(&key),
            vec![
                Cell::text(process),
                Cell::number(f64::from(connection.pid), connection.pid.to_string()),
                Cell::text(protocol),
                Cell::text(address(connection.local.0)),
                Cell::number(f64::from(connection.local.1), connection.local.1.to_string()),
                remote_address,
                remote_port,
                Cell::text(names::tcp_state(connection.state)),
            ],
            tone,
        );
    }
    table
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tabulates_connections_with_process_names() {
        let connections = [
            Connection {
                protocol: Protocol::Tcp,
                local: ("192.0.2.10".parse().unwrap(), 50000),
                remote: Some(("198.51.100.7".parse().unwrap(), 443)),
                state: 5,
                pid: 4321,
            },
            Connection {
                protocol: Protocol::Udp,
                local: ("::".parse().unwrap(), 5353),
                remote: None,
                state: 0,
                pid: 999,
            },
        ];
        let names = HashMap::from([(4321, "test.exe".to_owned())]);
        let table = connections_table(&connections, &names);
        let texts = |row: usize| table.rows[row].cells.iter().map(|c| c.text.as_str()).collect::<Vec<_>>();
        assert_eq!(texts(0), ["test.exe", "4321", "TCP", "192.0.2.10", "50000", "198.51.100.7", "443", "Established"]);
        assert_eq!(texts(1), ["Non-existent process", "999", "UDPv6", "*", "5353", "", "", ""]);
        assert_ne!(table.rows[0].id, table.rows[1].id);
    }
}
