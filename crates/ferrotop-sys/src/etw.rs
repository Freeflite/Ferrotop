//! Per-process disk and network byte counts from a kernel event trace (ETW): the source System
//! Informer and Resource Monitor use. Needs administrator rights.
//!
//! The trace is a private "system logger" session (Windows 8 and later), so it doesn't take the
//! single NT Kernel Logger from other tools. Disk events name the thread that issued the I/O
//! (the completion runs in whatever process happens to be current), so disk bytes are counted
//! per thread and attributed to processes by the caller; network events name the process.

use std::collections::HashMap;
use std::mem::offset_of;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, ERROR_SUCCESS};
use windows_sys::Win32::System::Diagnostics::Etw::{
    CONTROLTRACE_HANDLE, CloseTrace, ControlTraceW, EVENT_HEADER_FLAG_32_BIT_HEADER, EVENT_RECORD,
    EVENT_TRACE_CONTROL_STOP, EVENT_TRACE_FLAG_DISK_IO, EVENT_TRACE_FLAG_NETWORK_TCPIP, EVENT_TRACE_LOGFILEW,
    EVENT_TRACE_PROPERTIES, EVENT_TRACE_REAL_TIME_MODE, EVENT_TRACE_SYSTEM_LOGGER_MODE, OpenTraceW,
    PROCESS_TRACE_MODE_EVENT_RECORD, PROCESS_TRACE_MODE_REAL_TIME, PROCESSTRACE_HANDLE, ProcessTrace, StartTraceW,
    WNODE_FLAG_TRACED_GUID,
};
use windows_sys::core::GUID;

use crate::{Error, Result};

/// Who's tracing. The window and the history service each get their own session, so neither
/// mistakes the other's for one left behind by a crash and stops it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TraceRole {
    Window,
    Service,
}

impl TraceRole {
    fn session_name(self) -> &'static str {
        match self {
            TraceRole::Window => "Ferrotop I/O trace",
            TraceRole::Service => "Ferrotop history service I/O trace",
        }
    }

    /// Identifies the session; any fixed GUID will do, one per role.
    fn session_guid(self) -> GUID {
        match self {
            TraceRole::Window => GUID::from_u128(0x9e2f4a1c_7b3d_4e85_a6f0_3c8d2b1e5f47),
            TraceRole::Service => GUID::from_u128(0x4c7d1f90_2e6b_4a83_9d15_b8e0a3f6c271),
        }
    }
}
/// The kernel's classic providers.
const DISK_IO: u128 = 0x3d6fa8d4_fe05_11d0_9dda_00c04fd7ba7c;
const TCP_IP: u128 = 0x9a280ac0_c8e0_11d1_84e2_00c04fb998a2;
const UDP_IP: u128 = 0xbf3a50c5_a9c9_4988_a005_2df0b7c80f80;
/// Event opcodes: disk read and write; network send and receive over IPv4, and (+16) IPv6.
const OPCODE_READ: u8 = 10;
const OPCODE_WRITE: u8 = 11;
const OPCODE_SEND: u8 = 10;
const OPCODE_RECEIVE: u8 = 11;
const OPCODE_SEND_V6: u8 = 26;
const OPCODE_RECEIVE_V6: u8 = 27;
/// `PROCESSTRACE_HANDLE` value OpenTrace returns on failure.
const INVALID_PROCESSTRACE_HANDLE: u64 = u64::MAX;
const ERROR_ACCESS_DENIED: u32 = 5;

/// One I/O event: disk bytes by issuing thread, network bytes by process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IoEvent {
    DiskRead { thread: u32, bytes: u32 },
    DiskWrite { thread: u32, bytes: u32 },
    Send { pid: u32, bytes: u32 },
    Receive { pid: u32, bytes: u32 },
}

/// Bytes counted since the last [`IoTrace::drain`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IoTotals {
    /// `(read, written)` by the thread that issued the disk I/O.
    pub disk_by_thread: HashMap<u32, (u64, u64)>,
    /// `(sent, received)` by process.
    pub network_by_process: HashMap<u32, (u64, u64)>,
}

impl IoTotals {
    fn add(&mut self, event: IoEvent) {
        let add = |slot: &mut u64, bytes: u32| *slot = slot.saturating_add(u64::from(bytes));
        match event {
            IoEvent::DiskRead { thread, bytes } => add(&mut self.disk_by_thread.entry(thread).or_default().0, bytes),
            IoEvent::DiskWrite { thread, bytes } => add(&mut self.disk_by_thread.entry(thread).or_default().1, bytes),
            IoEvent::Send { pid, bytes } => add(&mut self.network_by_process.entry(pid).or_default().0, bytes),
            IoEvent::Receive { pid, bytes } => add(&mut self.network_by_process.entry(pid).or_default().1, bytes),
        }
    }
}

/// A GUID as the `u128` that `GUID::from_u128` takes.
fn guid_value(guid: &GUID) -> u128 {
    (u128::from(guid.data1) << 96)
        | (u128::from(guid.data2) << 80)
        | (u128::from(guid.data3) << 64)
        | u128::from(u64::from_be_bytes(guid.data4))
}

fn u32_at(data: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(data.get(offset..)?.first_chunk::<4>().copied()?))
}

/// Decodes the events Ferrotop counts from a provider, opcode and payload. `pointer_size` is 4
/// for events logged by a 32-bit kernel. Anything else, or a payload too short, is `None`.
pub(crate) fn parse_event(provider: u128, opcode: u8, pointer_size: usize, data: &[u8]) -> Option<IoEvent> {
    match (provider, opcode) {
        // DiskIo_TypeGroup1: DiskNumber, IrpFlags, TransferSize, Reserved (u32 each), ByteOffset
        // (u64), FileObject and Irp (pointers), HighResResponseTime (u64), IssuingThreadId (u32,
        // from version 3, Windows 8).
        (DISK_IO, OPCODE_READ | OPCODE_WRITE) => {
            let bytes = u32_at(data, 8)?;
            let thread = u32_at(data, 24 + 2 * pointer_size + 8)?;
            Some(if opcode == OPCODE_READ {
                IoEvent::DiskRead { thread, bytes }
            } else {
                IoEvent::DiskWrite { thread, bytes }
            })
        }
        // TcpIp and UdpIp send/receive payloads start with the process ID and the size.
        (TCP_IP | UDP_IP, OPCODE_SEND | OPCODE_SEND_V6) => {
            Some(IoEvent::Send { pid: u32_at(data, 0)?, bytes: u32_at(data, 4)? })
        }
        (TCP_IP | UDP_IP, OPCODE_RECEIVE | OPCODE_RECEIVE_V6) => {
            Some(IoEvent::Receive { pid: u32_at(data, 0)?, bytes: u32_at(data, 4)? })
        }
        _ => None,
    }
}

/// `EVENT_TRACE_PROPERTIES` with the session name stored after it, as StartTrace wants.
#[repr(C)]
struct SessionProperties {
    properties: EVENT_TRACE_PROPERTIES,
    name: [u16; 64],
}

impl SessionProperties {
    fn new(role: TraceRole) -> Box<Self> {
        let mut name = [0u16; 64];
        for (slot, unit) in name.iter_mut().take(63).zip(role.session_name().encode_utf16()) {
            *slot = unit;
        }
        let mut session = Box::new(Self { properties: EVENT_TRACE_PROPERTIES::default(), name });
        let properties = &mut session.properties;
        properties.Wnode.BufferSize = size_of::<Self>() as u32;
        properties.Wnode.Guid = role.session_guid();
        // Timestamps from the performance counter.
        properties.Wnode.ClientContext = 1;
        properties.Wnode.Flags = WNODE_FLAG_TRACED_GUID;
        properties.LogFileMode = EVENT_TRACE_REAL_TIME_MODE | EVENT_TRACE_SYSTEM_LOGGER_MODE;
        properties.EnableFlags = EVENT_TRACE_FLAG_DISK_IO | EVENT_TRACE_FLAG_NETWORK_TCPIP;
        // Deliver buffered events at least once a second.
        properties.FlushTimer = 1;
        properties.LoggerNameOffset = offset_of!(Self, name) as u32;
        session
    }
}

/// A running trace. Dropping it stops the session.
#[derive(Debug)]
pub struct IoTrace {
    role: TraceRole,
    /// The session's `CONTROLTRACE_HANDLE` and the consumer's `PROCESSTRACE_HANDLE` values.
    session: u64,
    consumer: u64,
    totals: Arc<Mutex<IoTotals>>,
    worker: Option<JoinHandle<()>>,
}

impl IoTrace {
    /// Starts the trace. Fails with an access-denied error unless run as administrator.
    pub fn start(role: TraceRole) -> Result<Self> {
        let mut properties = SessionProperties::new(role);
        let mut session = CONTROLTRACE_HANDLE::default();
        // SAFETY: `properties` is a correctly sized EVENT_TRACE_PROPERTIES with the name after
        // it, and both pointers are valid for the call.
        let mut status = unsafe { StartTraceW(&mut session, properties.name.as_ptr(), &mut properties.properties) };
        if status == ERROR_ALREADY_EXISTS {
            // Left over from a Ferrotop that didn't exit cleanly: stop it and start afresh.
            stop_session(role, 0);
            properties = SessionProperties::new(role);
            // SAFETY: as above.
            status = unsafe { StartTraceW(&mut session, properties.name.as_ptr(), &mut properties.properties) };
        }
        if status != ERROR_SUCCESS {
            return Err(Error::Win32 { call: "StartTraceW", code: status });
        }

        let totals = Arc::new(Mutex::new(IoTotals::default()));
        let mut logfile = EVENT_TRACE_LOGFILEW {
            LoggerName: properties.name.as_mut_ptr(),
            Context: Arc::as_ptr(&totals).cast_mut().cast(),
            ..EVENT_TRACE_LOGFILEW::default()
        };
        logfile.Anonymous1.ProcessTraceMode = PROCESS_TRACE_MODE_REAL_TIME | PROCESS_TRACE_MODE_EVENT_RECORD;
        logfile.Anonymous2.EventRecordCallback = Some(on_event);
        // SAFETY: `logfile` is fully initialized and its name buffer outlives the call.
        let consumer = unsafe { OpenTraceW(&mut logfile) };
        if consumer.Value == INVALID_PROCESSTRACE_HANDLE {
            let error = Error::last_win32("OpenTraceW");
            stop_session(role, session.Value);
            return Err(error);
        }

        // The callback reads `totals` through the context pointer, so this thread keeps a
        // reference alive until ProcessTrace returns.
        let context = Arc::clone(&totals);
        let spawned = thread::Builder::new().name("ferrotop-etw".into()).spawn(move || {
            let handles = [consumer];
            // SAFETY: `handles` holds the one open consumer handle; ProcessTrace delivers events
            // to `on_event` on this thread until the session stops or the handle is closed.
            unsafe { ProcessTrace(handles.as_ptr(), 1, std::ptr::null(), std::ptr::null()) };
            drop(context);
        });
        let worker = match spawned {
            Ok(worker) => worker,
            Err(_) => {
                close_consumer(consumer.Value);
                stop_session(role, session.Value);
                return Err(Error::Malformed { call: "spawning the ETW consumer thread" });
            }
        };
        Ok(Self { role, session: session.Value, consumer: consumer.Value, totals, worker: Some(worker) })
    }

    /// Takes the byte counts gathered since the last call.
    pub fn drain(&self) -> IoTotals {
        self.totals.lock().map(|mut totals| std::mem::take(&mut *totals)).unwrap_or_default()
    }
}

/// True for the error [`IoTrace::start`] gives without administrator rights.
pub fn is_access_denied(error: &Error) -> bool {
    matches!(error, Error::Win32 { code: ERROR_ACCESS_DENIED, .. })
}

impl Drop for IoTrace {
    fn drop(&mut self) {
        stop_session(self.role, self.session);
        close_consumer(self.consumer);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// Stops the role's session; with a zero handle, finds it by name.
fn stop_session(role: TraceRole, session: u64) {
    let mut properties = SessionProperties::new(role);
    let session = CONTROLTRACE_HANDLE { Value: session };
    // SAFETY: as in `start`; stopping fills `properties` with the session's final statistics.
    unsafe { ControlTraceW(session, properties.name.as_ptr(), &mut properties.properties, EVENT_TRACE_CONTROL_STOP) };
}

fn close_consumer(consumer: u64) {
    // SAFETY: `consumer` came from OpenTraceW and is closed once; this also ends ProcessTrace.
    unsafe { CloseTrace(PROCESSTRACE_HANDLE { Value: consumer }) };
}

/// ETW's per-event callback, on the consumer thread.
unsafe extern "system" fn on_event(record: *mut EVENT_RECORD) {
    // SAFETY: ETW passes a valid record that lives for the duration of the callback.
    let record = unsafe { &*record };
    let header = &record.EventHeader;
    let provider = guid_value(&header.ProviderId);
    if !matches!(provider, DISK_IO | TCP_IP | UDP_IP) || record.UserData.is_null() || record.UserContext.is_null() {
        return;
    }
    let len = usize::from(record.UserDataLength);
    // SAFETY: UserData points at UserDataLength bytes, valid for the duration of the callback.
    let data = unsafe { std::slice::from_raw_parts(record.UserData.cast::<u8>(), len) };
    let pointer_size = if u32::from(header.Flags) & EVENT_HEADER_FLAG_32_BIT_HEADER != 0 { 4 } else { 8 };
    let Some(event) = parse_event(provider, header.EventDescriptor.Opcode, pointer_size, data) else { return };
    let totals: *const Mutex<IoTotals> = record.UserContext.cast();
    // SAFETY: UserContext is the logfile Context: the Mutex inside the Arc the consumer thread
    // holds until ProcessTrace (which calls this) has returned.
    let totals = unsafe { &*totals };
    if let Ok(mut totals) = totals.lock() {
        totals.add(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn disk_payload(pointer_size: usize, bytes: u32, thread: u32) -> Vec<u8> {
        let mut data = vec![0u8; 24 + 2 * pointer_size + 8 + 4];
        data[8..12].copy_from_slice(&bytes.to_le_bytes());
        let at = 24 + 2 * pointer_size + 8;
        data[at..at + 4].copy_from_slice(&thread.to_le_bytes());
        data
    }

    #[test]
    fn parses_disk_and_network_events() {
        assert_eq!(
            parse_event(DISK_IO, OPCODE_READ, 8, &disk_payload(8, 4096, 77)),
            Some(IoEvent::DiskRead { thread: 77, bytes: 4096 })
        );
        assert_eq!(
            parse_event(DISK_IO, OPCODE_WRITE, 4, &disk_payload(4, 512, 9)),
            Some(IoEvent::DiskWrite { thread: 9, bytes: 512 })
        );
        // Before Windows 8 there's no issuing thread.
        assert_eq!(parse_event(DISK_IO, OPCODE_READ, 8, &disk_payload(8, 1, 1)[..40]), None);
        let mut network = vec![0u8; 24];
        network[..4].copy_from_slice(&1234u32.to_le_bytes());
        network[4..8].copy_from_slice(&1500u32.to_le_bytes());
        assert_eq!(parse_event(TCP_IP, OPCODE_SEND, 8, &network), Some(IoEvent::Send { pid: 1234, bytes: 1500 }));
        assert_eq!(
            parse_event(UDP_IP, OPCODE_RECEIVE_V6, 8, &network),
            Some(IoEvent::Receive { pid: 1234, bytes: 1500 })
        );
        assert_eq!(parse_event(TCP_IP, 12, 8, &network), None, "a connect isn't counted");
        assert_eq!(parse_event(TCP_IP, OPCODE_SEND, 8, &network[..6]), None);
        assert_eq!(parse_event(0x1234, OPCODE_SEND, 8, &network), None);
    }

    #[test]
    fn totals_add_up() {
        let mut totals = IoTotals::default();
        totals.add(IoEvent::DiskRead { thread: 5, bytes: 10 });
        totals.add(IoEvent::DiskRead { thread: 5, bytes: 20 });
        totals.add(IoEvent::DiskWrite { thread: 5, bytes: 7 });
        totals.add(IoEvent::Send { pid: 3, bytes: u32::MAX });
        totals.add(IoEvent::Receive { pid: 3, bytes: 1 });
        assert_eq!(totals.disk_by_thread[&5], (30, 7));
        assert_eq!(totals.network_by_process[&3], (u64::from(u32::MAX), 1));
    }

    #[test]
    fn guids_round_trip() {
        assert_eq!(guid_value(&GUID::from_u128(DISK_IO)), DISK_IO);
        assert_eq!(guid_value(&TraceRole::Window.session_guid()), 0x9e2f4a1c_7b3d_4e85_a6f0_3c8d2b1e5f47);
        assert_ne!(TraceRole::Window.session_guid().data1, TraceRole::Service.session_guid().data1);
        assert_ne!(TraceRole::Window.session_name(), TraceRole::Service.session_name());
    }

    #[test]
    fn counts_this_process_or_needs_admin() {
        let trace = match IoTrace::start(TraceRole::Window) {
            Ok(trace) => trace,
            Err(error) => return assert!(is_access_denied(&error), "{error}"),
        };
        // Elevated (as on CI): make some network and disk I/O, and expect to see it.
        let receiver = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind");
        let sender = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind");
        let target = receiver.local_addr().expect("address");
        let path = std::env::temp_dir().join(format!("ferrotop-etw-{}.tmp", std::process::id()));
        let mut seen = IoTotals::default();
        for _ in 0..20 {
            for _ in 0..50 {
                sender.send_to(&[0u8; 1000], target).expect("send");
            }
            // Written and flushed through one handle: flushing needs write access.
            let mut file = std::fs::File::create(&path).expect("create");
            std::io::Write::write_all(&mut file, &vec![1u8; 1 << 20]).expect("write");
            file.sync_all().expect("flush");
            drop(file);
            thread::sleep(std::time::Duration::from_millis(250));
            let totals = trace.drain();
            seen.network_by_process.extend(totals.network_by_process);
            seen.disk_by_thread.extend(totals.disk_by_thread);
            if seen.network_by_process.contains_key(&std::process::id()) && !seen.disk_by_thread.is_empty() {
                break;
            }
        }
        let _ = std::fs::remove_file(&path);
        assert!(seen.network_by_process.get(&std::process::id()).is_some_and(|(sent, _)| *sent > 0), "{seen:?}");
        assert!(!seen.disk_by_thread.is_empty(), "no disk events");
    }
}
