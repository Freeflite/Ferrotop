//! Services: the running ones and their host processes, the full list with configuration, and
//! starting and stopping them.

use std::mem::size_of;

use windows_sys::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, ERROR_MORE_DATA};
use windows_sys::Win32::System::Services::{
    CloseServiceHandle, ControlService, ENUM_SERVICE_STATE, ENUM_SERVICE_TYPE, EnumServicesStatusExW, OpenSCManagerW,
    OpenServiceW, QueryServiceConfig2W, QueryServiceConfigW, SC_ENUM_PROCESS_INFO, SC_HANDLE, SC_MANAGER_CONNECT,
    SC_MANAGER_ENUMERATE_SERVICE, SERVICE_ACTIVE, SERVICE_CONFIG_DELAYED_AUTO_START_INFO, SERVICE_CONFIG_DESCRIPTION,
    SERVICE_CONTROL_STOP, SERVICE_DRIVER, SERVICE_QUERY_CONFIG, SERVICE_QUERY_STATUS, SERVICE_START, SERVICE_STATE_ALL,
    SERVICE_STATUS, SERVICE_STOP, SERVICE_WIN32, StartServiceW,
};

use crate::buffer::{AlignedBuffer, offset_in, read_pod, wide, wide_cstr_in};
use crate::ntapi::{EnumServiceStatusProcess, QueryServiceConfig};
use crate::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceInfo {
    pub pid: u32,
    pub name: String,
    pub display_name: String,
}

/// A Service Control Manager handle (not a kernel handle, so `OwnedHandle` doesn't fit).
struct ScManager(SC_HANDLE);

impl Drop for ScManager {
    fn drop(&mut self) {
        // SAFETY: `self.0` came from a successful OpenSCManagerW and is closed exactly once, here.
        unsafe { CloseServiceHandle(self.0) };
    }
}

fn open_manager(access: u32) -> Result<ScManager> {
    // SAFETY: null machine and database names mean the local active database; no other preconditions.
    let raw = unsafe { OpenSCManagerW(std::ptr::null(), std::ptr::null(), access) };
    if raw.is_null() {
        return Err(Error::last_win32("OpenSCManagerW"));
    }
    Ok(ScManager(raw))
}

fn open_service(manager: &ScManager, name: &str, access: u32) -> Result<ScManager> {
    let name = wide(name);
    // SAFETY: `manager` is a live SCM handle and `name` is NUL-terminated.
    let raw = unsafe { OpenServiceW(manager.0, name.as_ptr(), access) };
    if raw.is_null() {
        return Err(Error::last_win32("OpenServiceW"));
    }
    Ok(ScManager(raw))
}

/// One entry of `EnumServicesStatusExW`.
struct Enumerated {
    name: String,
    display_name: String,
    kind: u32,
    state: u32,
    pid: u32,
}

/// Every running Win32 service with the PID of its host process.
pub fn running_services() -> Result<Vec<ServiceInfo>> {
    let manager = open_manager(SC_MANAGER_ENUMERATE_SERVICE)?;
    Ok(enumerate(&manager, SERVICE_WIN32, SERVICE_ACTIVE)?
        .into_iter()
        .map(|s| ServiceInfo { pid: s.pid, name: s.name, display_name: s.display_name })
        .collect())
}

fn enumerate(manager: &ScManager, kinds: ENUM_SERVICE_TYPE, states: ENUM_SERVICE_STATE) -> Result<Vec<Enumerated>> {
    const CALL: &str = "EnumServicesStatusExW";

    let mut buffer = AlignedBuffer::new(64 * 1024);
    for _ in 0..4 {
        let (mut needed, mut returned, mut resume) = (0u32, 0u32, 0u32);
        // SAFETY: `manager` is a live SCM handle, the buffer pointer/length describe memory owned by
        // `buffer`, all out-pointers are valid, and a null group name means "all groups".
        let ok = unsafe {
            EnumServicesStatusExW(
                manager.0,
                SC_ENUM_PROCESS_INFO,
                kinds,
                states,
                buffer.as_mut_ptr().cast(),
                buffer.capacity_u32(),
                &mut needed,
                &mut returned,
                &mut resume,
                std::ptr::null(),
            )
        };
        if ok == 0 {
            let error = Error::last_win32(CALL);
            if error != (Error::Win32 { call: CALL, code: ERROR_MORE_DATA }) {
                return Err(error);
            }
            // Services can start between calls; ask for the full list again with headroom.
            buffer = AlignedBuffer::new(buffer.capacity() + needed as usize + 16 * 1024);
            continue;
        }
        let bytes = buffer.bytes(buffer.capacity());
        let malformed = || Error::Malformed { call: CALL };
        return (0..returned as usize)
            .map(|i| {
                let entry: EnumServiceStatusProcess =
                    read_pod(bytes, i * size_of::<EnumServiceStatusProcess>()).ok_or_else(malformed)?;
                Ok(Enumerated {
                    name: wide_cstr_in(bytes, entry.service_name).ok_or_else(malformed)?,
                    display_name: wide_cstr_in(bytes, entry.display_name).ok_or_else(malformed)?,
                    kind: entry.service_type,
                    state: entry.current_state,
                    pid: entry.process_id,
                })
            })
            .collect();
    }
    Err(Error::Malformed { call: CALL })
}

/// A service (or driver) with its configuration, for the Services page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceDetails {
    pub name: String,
    pub display_name: String,
    /// `SERVICE_*` type bits: 0x1 kernel driver, 0x2 file-system driver, 0x10 own process,
    /// 0x20 shared process (plus 0x40/0x80 for per-user templates and instances).
    pub kind: u32,
    /// `SERVICE_STOPPED` (1) through `SERVICE_PAUSED` (7).
    pub state: u32,
    pub pid: u32,
    /// `SERVICE_BOOT_START` (0) through `SERVICE_DISABLED` (4); `None` if unreadable.
    pub start_type: Option<u32>,
    pub delayed_start: bool,
    pub binary_path: Option<String>,
    pub account: Option<String>,
    pub description: Option<String>,
}

/// Every service and driver, with configuration where Ferrotop may read it.
pub fn all_services() -> Result<Vec<ServiceDetails>> {
    let manager = open_manager(SC_MANAGER_CONNECT | SC_MANAGER_ENUMERATE_SERVICE)?;
    Ok(enumerate(&manager, SERVICE_WIN32 | SERVICE_DRIVER, SERVICE_STATE_ALL)?
        .into_iter()
        .map(|service| {
            let mut details = ServiceDetails {
                start_type: None,
                delayed_start: false,
                binary_path: None,
                account: None,
                description: None,
                name: service.name,
                display_name: service.display_name,
                kind: service.kind,
                state: service.state,
                pid: service.pid,
            };
            if let Ok(handle) = open_service(&manager, &details.name, SERVICE_QUERY_CONFIG) {
                if let Some((start_type, binary_path, account)) = query_config(&handle) {
                    details.start_type = Some(start_type);
                    details.binary_path = binary_path;
                    details.account = account;
                }
                details.description = query_config2(&handle, SERVICE_CONFIG_DESCRIPTION, |bytes| {
                    // SERVICE_DESCRIPTIONW: one pointer to the text, which follows in the buffer.
                    let text: usize = read_pod(bytes, 0)?;
                    let offset = offset_in(bytes, std::ptr::without_provenance::<u16>(text))?;
                    let units: Vec<u16> = bytes[offset..]
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .map(|&pair| u16::from_le_bytes(pair))
                        .take_while(|&unit| unit != 0)
                        .collect();
                    Some(String::from_utf16_lossy(&units)).filter(|d| !d.is_empty())
                });
                details.delayed_start =
                    query_config2(&handle, SERVICE_CONFIG_DELAYED_AUTO_START_INFO, |bytes| read_pod::<u32>(bytes, 0))
                        .is_some_and(|flag| flag != 0);
            }
            details
        })
        .collect())
}

/// `QueryServiceConfigW`: start type, binary path and account.
fn query_config(service: &ScManager) -> Option<(u32, Option<String>, Option<String>)> {
    let mut needed = 0u32;
    // SAFETY: a null buffer with zero length asks for the size; `needed` is a valid out-pointer.
    unsafe { QueryServiceConfigW(service.0, std::ptr::null_mut(), 0, &mut needed) };
    if needed == 0 || needed > 64 * 1024 {
        return None;
    }
    let mut buffer = AlignedBuffer::new(needed as usize);
    // SAFETY: the buffer is writable for `needed` bytes; `needed` is a valid out-pointer.
    if unsafe { QueryServiceConfigW(service.0, buffer.as_mut_ptr().cast(), buffer.capacity_u32(), &mut needed) } == 0 {
        return None;
    }
    let bytes = buffer.bytes(buffer.capacity());
    let config: QueryServiceConfig = read_pod(bytes, 0)?;
    let text = |ptr: *const u16| wide_cstr_in(bytes, ptr).filter(|s| !s.is_empty());
    Some((config.start_type, text(config.binary_path_name), text(config.service_start_name)))
}

/// `QueryServiceConfig2W` for one information level, decoded by `decode` while the buffer (which
/// its pointers refer to) is still alive.
fn query_config2<T>(service: &ScManager, level: u32, decode: impl FnOnce(&[u8]) -> Option<T>) -> Option<T> {
    const CALL: &str = "QueryServiceConfig2W";
    let mut needed = 0u32;
    // SAFETY: a null buffer with zero length asks for the size; `needed` is a valid out-pointer.
    let ok = unsafe { QueryServiceConfig2W(service.0, level, std::ptr::null_mut(), 0, &mut needed) };
    if ok == 0 && Error::last_win32(CALL) != (Error::Win32 { call: CALL, code: ERROR_INSUFFICIENT_BUFFER }) {
        return None;
    }
    if needed == 0 || needed > 64 * 1024 {
        return None;
    }
    let mut buffer = AlignedBuffer::new(needed as usize);
    // SAFETY: the buffer is writable for `needed` bytes; `needed` is a valid out-pointer.
    if unsafe { QueryServiceConfig2W(service.0, level, buffer.as_mut_ptr().cast(), buffer.capacity_u32(), &mut needed) }
        == 0
    {
        return None;
    }
    decode(buffer.bytes(buffer.capacity()))
}

/// Starts a stopped service. Needs administrator rights for most services.
pub fn start(name: &str) -> Result<()> {
    let manager = open_manager(SC_MANAGER_CONNECT)?;
    let service = open_service(&manager, name, SERVICE_START | SERVICE_QUERY_STATUS)?;
    // SAFETY: `service` is live; no arguments are passed.
    if unsafe { StartServiceW(service.0, 0, std::ptr::null()) } == 0 {
        return Err(Error::last_win32("StartServiceW"));
    }
    Ok(())
}

/// Asks a running service to stop.
pub fn stop(name: &str) -> Result<()> {
    let manager = open_manager(SC_MANAGER_CONNECT)?;
    let service = open_service(&manager, name, SERVICE_STOP | SERVICE_QUERY_STATUS)?;
    // SAFETY: SERVICE_STATUS is plain integers; all-zero is valid.
    let mut status: SERVICE_STATUS = unsafe { std::mem::zeroed() };
    // SAFETY: `service` is live and `status` is a writable SERVICE_STATUS.
    if unsafe { ControlService(service.0, SERVICE_CONTROL_STOP, &mut status) } == 0 {
        return Err(Error::last_win32("ControlService"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_running_services_with_host_pids() {
        let services = running_services().expect("services");
        assert!(services.len() > 20, "only {} services", services.len());
        assert!(services.iter().all(|s| !s.name.is_empty()));
        assert!(services.iter().any(|s| s.pid != 0));
    }

    #[test]
    fn lists_all_services_and_drivers_with_configuration() {
        let services = all_services().expect("services");
        assert!(services.iter().any(|s| s.kind & 0x3 != 0), "drivers included");
        assert!(services.iter().any(|s| s.state == 1), "stopped services included");
        let event_log = services.iter().find(|s| s.name.eq_ignore_ascii_case("EventLog")).expect("EventLog");
        assert_eq!(event_log.start_type, Some(2), "automatic");
        assert!(event_log.binary_path.as_deref().is_some_and(|p| p.to_ascii_lowercase().contains("svchost")));
        assert!(event_log.description.is_some());
    }
}
