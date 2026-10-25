//! System-wide counters (per-processor times and cycles, memory) and our own process's
//! privileges, elevation and theme.

use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_NOT_ALL_ASSIGNED, ERROR_SUCCESS, LUID};
use windows_sys::Win32::Security::{
    AdjustTokenPrivileges, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW, SE_PRIVILEGE_ENABLED, TOKEN_ADJUST_PRIVILEGES,
    TOKEN_PRIVILEGES, TOKEN_QUERY,
};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, REG_SZ, RRF_RT_REG_DWORD, RRF_RT_REG_SZ, RegDeleteKeyValueW,
    RegGetValueW, RegSetKeyValueW,
};
use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows_sys::Win32::System::Threading::{
    GetActiveProcessorCount, GetActiveProcessorGroupCount, GetCurrentProcess, OpenProcessToken,
};
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

use crate::buffer::{Pod, wide};
use crate::ntapi::{
    NtQuerySystemInformationEx, SYSTEM_PROCESSOR_CYCLE_TIME_INFORMATION, SYSTEM_PROCESSOR_IDLE_CYCLE_TIME_INFORMATION,
    SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION, SystemProcessorPerformanceInformation,
};
use crate::process::non_negative;
use crate::{Error, Result};

/// Cumulative time one logical processor has spent in each state, in 100 ns units.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CpuTimes {
    pub idle: u64,
    /// Kernel time *excluding* idle time.
    pub kernel: u64,
    pub user: u64,
    pub dpc: u64,
    pub interrupt: u64,
}

/// Runs a per-processor information class across every processor group (the `Ex` form takes the
/// group as input), returning one `T` per logical processor.
fn per_processor<T: Pod + Default>(class: i32, call: &'static str) -> Result<Vec<T>> {
    // SAFETY: no preconditions.
    let groups = unsafe { GetActiveProcessorGroupCount() };
    let mut all = Vec::new();
    for group in 0..groups {
        // SAFETY: no preconditions; returns 0 for a group that doesn't exist.
        let count = unsafe { GetActiveProcessorCount(group) } as usize;
        let mut info = vec![T::default(); count];
        let capacity = u32::try_from(count * size_of::<T>()).map_err(|_| Error::Malformed { call })?;
        let mut written = 0u32;
        // SAFETY: the input is a `u16` of the stated size, the output buffer holds `count`
        // writable `T`s (the element type this class returns, and `T: Pod` accepts any bytes),
        // and `written` is a valid out-pointer.
        let status = unsafe {
            NtQuerySystemInformationEx(
                class,
                (&raw const group).cast(),
                size_of::<u16>() as u32,
                info.as_mut_ptr().cast(),
                capacity,
                &mut written,
            )
        };
        if status < 0 {
            return Err(Error::Nt { call, status });
        }
        let filled = match written {
            0 => count,
            bytes => (bytes as usize / size_of::<T>()).min(count),
        };
        info.truncate(filled);
        all.extend(info);
    }
    Ok(all)
}

/// Times for every logical processor, across all processor groups.
pub fn processor_times() -> Result<Vec<CpuTimes>> {
    let info: Vec<SystemProcessorPerformanceInformation> = per_processor(
        SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION,
        "NtQuerySystemInformationEx(SystemProcessorPerformanceInformation)",
    )?;
    Ok(info
        .iter()
        .map(|cpu| CpuTimes {
            idle: non_negative(cpu.idle_time),
            kernel: non_negative(cpu.kernel_time).saturating_sub(non_negative(cpu.idle_time)),
            user: non_negative(cpu.user_time),
            dpc: non_negative(cpu.dpc_time),
            interrupt: non_negative(cpu.interrupt_time),
        })
        .collect())
}

/// CPU cycle counters summed over every logical processor.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CycleTimes {
    /// Cycles spent idle.
    pub idle: u64,
    /// Cycles spent in DPCs and interrupts, which no process is charged for.
    pub interrupt: u64,
}

pub fn cycle_times() -> Result<CycleTimes> {
    let sum = |cycles: Vec<u64>| cycles.into_iter().fold(0u64, u64::wrapping_add);
    Ok(CycleTimes {
        idle: sum(per_processor(
            SYSTEM_PROCESSOR_IDLE_CYCLE_TIME_INFORMATION,
            "NtQuerySystemInformationEx(SystemProcessorIdleCycleTimeInformation)",
        )?),
        interrupt: sum(per_processor(
            SYSTEM_PROCESSOR_CYCLE_TIME_INFORMATION,
            "NtQuerySystemInformationEx(SystemProcessorCycleTimeInformation)",
        )?),
    })
}

/// Physical memory and commit charge, in bytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MemoryStatus {
    pub total_physical: u64,
    pub available_physical: u64,
    pub commit_total: u64,
    pub commit_limit: u64,
}

pub fn memory_status() -> Result<MemoryStatus> {
    let mut status = MEMORYSTATUSEX {
        dwLength: size_of::<MEMORYSTATUSEX>() as u32,
        dwMemoryLoad: 0,
        ullTotalPhys: 0,
        ullAvailPhys: 0,
        ullTotalPageFile: 0,
        ullAvailPageFile: 0,
        ullTotalVirtual: 0,
        ullAvailVirtual: 0,
        ullAvailExtendedVirtual: 0,
    };
    // SAFETY: `status` is a writable MEMORYSTATUSEX with `dwLength` set, as the API requires.
    if unsafe { GlobalMemoryStatusEx(&mut status) } == 0 {
        return Err(Error::last_win32("GlobalMemoryStatusEx"));
    }
    Ok(MemoryStatus {
        total_physical: status.ullTotalPhys,
        available_physical: status.ullAvailPhys,
        commit_total: status.ullTotalPageFile.saturating_sub(status.ullAvailPageFile),
        commit_limit: status.ullTotalPageFile,
    })
}

/// True when Ferrotop is running elevated (as Administrator).
pub fn is_elevated() -> bool {
    crate::token::current_token().is_ok_and(|token| token.elevated)
}

/// Turns on `SeDebugPrivilege` for Ferrotop, as System Informer does when elevated. It lets us
/// query processes owned by other users and services. Only an elevated token holds the privilege,
/// so this fails harmlessly otherwise.
pub fn enable_debug_privilege() -> Result<()> {
    // SAFETY: returns a pseudo-handle that needs no closing; no preconditions.
    let process = unsafe { GetCurrentProcess() };
    let mut token = std::ptr::null_mut();
    // SAFETY: `process` is the current-process pseudo-handle and `token` is a valid out-pointer.
    if unsafe { OpenProcessToken(process, TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY, &mut token) } == 0 {
        return Err(Error::last_win32("OpenProcessToken"));
    }
    // SAFETY: `token` is a handle we just opened, and nothing else owns it.
    let token = unsafe { OwnedHandle::from_raw_handle(token) };

    let name = wide("SeDebugPrivilege");
    let mut luid = LUID { LowPart: 0, HighPart: 0 };
    // SAFETY: `name` is NUL-terminated, `luid` is a valid out-pointer, and a null system name means local.
    if unsafe { LookupPrivilegeValueW(std::ptr::null(), name.as_ptr(), &mut luid) } == 0 {
        return Err(Error::last_win32("LookupPrivilegeValueW"));
    }
    let privileges = TOKEN_PRIVILEGES {
        PrivilegeCount: 1,
        Privileges: [LUID_AND_ATTRIBUTES { Luid: luid, Attributes: SE_PRIVILEGE_ENABLED }],
    };
    // SAFETY: `token` was opened with TOKEN_ADJUST_PRIVILEGES, `privileges` is a valid
    // TOKEN_PRIVILEGES, and no previous-state output is requested.
    let ok = unsafe {
        AdjustTokenPrivileges(token.as_raw_handle(), 0, &privileges, 0, std::ptr::null_mut(), std::ptr::null_mut())
    };
    // AdjustTokenPrivileges "succeeds" even when the token doesn't hold the privilege; the real
    // answer is in the last error.
    let result = Error::last_win32("AdjustTokenPrivileges");
    match (ok, result) {
        (0, error) => Err(error),
        (_, Error::Win32 { code: ERROR_SUCCESS, .. }) => Ok(()),
        (_, Error::Win32 { code: ERROR_NOT_ALL_ASSIGNED, .. }) => Err(result),
        (_, error) => Err(error),
    }
}

/// Starts a new, elevated copy of Ferrotop through the UAC prompt. The caller should exit once
/// this succeeds; `--replace` tells the new copy to take over rather than defer to this one.
pub fn relaunch_elevated() -> Result<()> {
    let exe = std::env::current_exe().map_err(|_| Error::Malformed { call: "current_exe" })?;
    let exe: Vec<u16> = exe.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    let verb = wide("runas");
    let parameters = wide("--replace");
    // SAFETY: the verb, file and parameters are NUL-terminated and outlive the call; a null
    // window and directory are permitted.
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            exe.as_ptr(),
            parameters.as_ptr(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    // ShellExecuteW reports success with a value above 32.
    if result.addr() > 32 { Ok(()) } else { Err(Error::last_win32("ShellExecuteW")) }
}

/// Windows' "app mode" setting: `Some(true)` for light, `Some(false)` for dark.
pub fn apps_use_light_theme() -> Option<bool> {
    read_dword(HKEY_CURRENT_USER, r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize", "AppsUseLightTheme")
        .map(|value| value != 0)
}

fn read_dword(root: HKEY, key: &str, value: &str) -> Option<u32> {
    let (key, value) = (wide(key), wide(value));
    let mut data = 0u32;
    let mut size = size_of::<u32>() as u32;
    // SAFETY: both strings are NUL-terminated, `data` is a writable DWORD whose size is in `size`,
    // and a null type pointer is permitted.
    let status = unsafe {
        RegGetValueW(
            root,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            (&raw mut data).cast(),
            &mut size,
        )
    };
    (status == ERROR_SUCCESS).then_some(data)
}

/// A DWORD value under `HKEY_LOCAL_MACHINE`.
pub(crate) fn registry_dword(key: &str, value: &str) -> Option<u32> {
    read_dword(HKEY_LOCAL_MACHINE, key, value)
}

/// A string value (up to 1023 characters) under `HKEY_LOCAL_MACHINE`.
pub(crate) fn registry_string(key: &str, value: &str) -> Option<String> {
    string_value(HKEY_LOCAL_MACHINE, key, value)
}

/// Where Windows keeps the programs it starts when you sign in.
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "Ferrotop";

/// The command Windows runs for Ferrotop when you sign in, if there is one.
pub fn sign_in_command() -> Option<String> {
    string_value(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE)
}

/// Makes this copy of Ferrotop start in the tray when you sign in, or stops that. Only your own
/// sign-in is affected (the current user's Run key).
pub fn set_start_at_sign_in(enabled: bool) -> Result<()> {
    let (key, value) = (wide(RUN_KEY), wide(RUN_VALUE));
    if !enabled {
        // SAFETY: both strings are NUL-terminated.
        let status = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), value.as_ptr()) };
        return match status {
            ERROR_SUCCESS | ERROR_FILE_NOT_FOUND => Ok(()),
            code => Err(Error::Win32 { call: "RegDeleteKeyValueW", code }),
        };
    }
    let exe = std::env::current_exe().map_err(|_| Error::Malformed { call: "current_exe" })?;
    let command: Vec<u16> = "\""
        .encode_utf16()
        .chain(exe.as_os_str().encode_wide())
        .chain("\" --tray".encode_utf16())
        .chain(std::iter::once(0))
        .collect();
    let bytes = u32::try_from(command.len() * 2).map_err(|_| Error::Malformed { call: "sign-in command" })?;
    // SAFETY: the key and value names are NUL-terminated, and `command` is a NUL-terminated
    // REG_SZ of `bytes` bytes.
    let status = unsafe {
        RegSetKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), value.as_ptr(), REG_SZ, command.as_ptr().cast(), bytes)
    };
    if status == ERROR_SUCCESS { Ok(()) } else { Err(Error::Win32 { call: "RegSetKeyValueW", code: status }) }
}

fn string_value(root: HKEY, key: &str, value: &str) -> Option<String> {
    let (key, value) = (wide(key), wide(value));
    let mut data = vec![0u16; 1024];
    let mut size = (data.len() * 2) as u32;
    // SAFETY: both strings are NUL-terminated, `data` is writable for `size` bytes, and a null
    // type pointer is permitted. RRF_RT_REG_SZ guarantees a NUL-terminated result.
    let status = unsafe {
        RegGetValueW(
            root,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            data.as_mut_ptr().cast(),
            &mut size,
        )
    };
    if status != ERROR_SUCCESS {
        return None;
    }
    let units = (size as usize / 2).min(data.len());
    let text = String::from_utf16_lossy(&data[..units]);
    Some(text.trim_end_matches('\0').to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_entry_per_logical_processor() {
        let times = processor_times().expect("processor times");
        let expected = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
        assert_eq!(times.len(), expected);
        assert!(times.iter().any(|cpu| cpu.idle > 0));
    }

    #[test]
    fn cycle_counters_advance() {
        let first = cycle_times().expect("cycle times");
        std::thread::sleep(std::time::Duration::from_millis(20));
        let second = cycle_times().expect("cycle times");
        assert!(second.idle > first.idle, "{first:?} -> {second:?}");
    }

    #[test]
    fn memory_is_plausible() {
        let memory = memory_status().expect("memory status");
        assert!(memory.total_physical > 0);
        assert!(memory.available_physical <= memory.total_physical);
        assert!(memory.commit_total <= memory.commit_limit);
    }
}
