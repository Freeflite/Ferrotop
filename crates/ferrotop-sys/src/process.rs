//! Process enumeration and per-process queries.

use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::sync::atomic::{AtomicUsize, Ordering};

use windows_sys::Wdk::System::SystemInformation::SystemProcessInformation;
use windows_sys::Wdk::System::Threading::{NtQueryInformationProcess, ProcessCommandLineInformation};
use windows_sys::Win32::Foundation::FILETIME;
use windows_sys::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, PROCESS_ACCESS_RIGHTS, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_TERMINATE, QueryFullProcessImageNameW, TerminateProcess,
};
use windows_sys::Win32::System::WindowsProgramming::QueryProcessCycleTime;

use crate::buffer::{AlignedBuffer, is_size_status, query_system_information, read_pod, unicode_string_in};
use crate::ntapi::{SystemProcessInformation as RawProcess, UnicodeString};
use crate::{Error, Result};

/// One process as reported by `NtQuerySystemInformation(SystemProcessInformation)`.
///
/// Times are in 100 ns units; `create_time` counts from 1601-01-01 UTC.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProcessInfo {
    pub pid: u32,
    pub parent_pid: u32,
    pub name: String,
    pub create_time: i64,
    pub kernel_time: u64,
    pub user_time: u64,
    pub cycle_time: u64,
    pub threads: u32,
    pub handles: u32,
    pub session_id: u32,
    pub base_priority: i32,
    pub working_set: u64,
    pub private_working_set: u64,
    pub private_bytes: u64,
    pub peak_working_set: u64,
    pub virtual_size: u64,
    pub page_faults: u32,
    pub hard_faults: u32,
    pub io_read_bytes: u64,
    pub io_write_bytes: u64,
    pub io_other_bytes: u64,
    pub io_read_ops: u64,
    pub io_write_ops: u64,
    pub io_other_ops: u64,
    /// Every thread is suspended (what Task Manager shows as "Suspended").
    pub suspended: bool,
    /// The process's thread IDs.
    pub thread_ids: Vec<u32>,
}

/// Remembers the last buffer size so steady-state refreshes need a single syscall.
static PROCESS_BUFFER_HINT: AtomicUsize = AtomicUsize::new(0x10_0000);

/// Lists every process on the system.
pub fn enumerate() -> Result<Vec<ProcessInfo>> {
    let mut buffer = AlignedBuffer::new(PROCESS_BUFFER_HINT.load(Ordering::Relaxed));
    let len = query_system_information(SystemProcessInformation, &mut buffer)?;
    PROCESS_BUFFER_HINT.store(buffer.capacity(), Ordering::Relaxed);
    parse_processes(buffer.bytes(len))
}

pub(crate) fn parse_processes(bytes: &[u8]) -> Result<Vec<ProcessInfo>> {
    let malformed = || Error::Malformed { call: "NtQuerySystemInformation(SystemProcessInformation)" };
    let mut processes = Vec::with_capacity(512);
    let mut offset = 0usize;
    loop {
        let raw: RawProcess = read_pod(bytes, offset).ok_or_else(malformed)?;
        let name = unicode_string_in(bytes, &raw.image_name).ok_or_else(malformed)?;
        let mut info = ProcessInfo::from_raw(&raw, name).ok_or_else(malformed)?;
        if let Some(threads) = crate::threads::thread_array(bytes, offset, &raw) {
            // What Task Manager calls "Suspended": every thread is.
            info.suspended = !threads.is_empty() && threads.iter().all(crate::threads::ThreadInfo::is_suspended);
            info.thread_ids = threads.iter().map(|thread| thread.tid).collect();
        }
        processes.push(info);
        match raw.next_entry_offset {
            0 => break,
            next => offset = offset.checked_add(next as usize).ok_or_else(malformed)?,
        }
    }
    Ok(processes)
}

impl ProcessInfo {
    fn from_raw(raw: &RawProcess, name: String) -> Option<Self> {
        Some(Self {
            pid: u32::try_from(raw.unique_process_id).ok()?,
            parent_pid: u32::try_from(raw.inherited_from_unique_process_id).ok()?,
            name,
            create_time: raw.create_time,
            kernel_time: non_negative(raw.kernel_time),
            user_time: non_negative(raw.user_time),
            cycle_time: raw.cycle_time,
            threads: raw.number_of_threads,
            handles: raw.handle_count,
            session_id: raw.session_id,
            base_priority: raw.base_priority,
            working_set: raw.working_set_size as u64,
            private_working_set: raw.working_set_private_size,
            private_bytes: raw.pagefile_usage as u64,
            peak_working_set: raw.peak_working_set_size as u64,
            virtual_size: raw.virtual_size as u64,
            page_faults: raw.page_fault_count,
            hard_faults: raw.hard_fault_count,
            io_read_bytes: non_negative(raw.read_transfer_count),
            io_write_bytes: non_negative(raw.write_transfer_count),
            io_other_bytes: non_negative(raw.other_transfer_count),
            io_read_ops: non_negative(raw.read_operation_count),
            io_write_ops: non_negative(raw.write_operation_count),
            io_other_ops: non_negative(raw.other_operation_count),
            suspended: false,
            thread_ids: Vec::new(),
        })
    }
}

pub(crate) fn non_negative(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

pub(crate) fn open_process(pid: u32, access: PROCESS_ACCESS_RIGHTS) -> Result<OwnedHandle> {
    // SAFETY: OpenProcess has no memory-safety preconditions; it returns null on failure.
    let raw = unsafe { OpenProcess(access, 0, pid) };
    if raw.is_null() {
        return Err(Error::last_win32("OpenProcess"));
    }
    // SAFETY: `raw` is a valid process handle we just opened, and nothing else owns it.
    Ok(unsafe { OwnedHandle::from_raw_handle(raw) })
}

/// Opens `pid` with `access`, but only if it is still the process created at `create_time`.
///
/// PIDs are recycled, so every action on a process the user picked goes through this: checking
/// the creation time on the open handle guarantees it can't land on an unrelated process that
/// inherited the PID after the original exited.
pub(crate) fn open_verified(pid: u32, create_time: i64, access: PROCESS_ACCESS_RIGHTS) -> Result<OwnedHandle> {
    let process = open_process(pid, access | PROCESS_QUERY_LIMITED_INFORMATION)?;
    if self::create_time(&process)? != create_time {
        return Err(Error::ProcessGone);
    }
    Ok(process)
}

/// A query-only handle to keep for a process's lifetime, so its final figures can still be read
/// after it exits (the handle keeps the exited process's accounting alive).
pub fn open_for_query(pid: u32, create_time: i64) -> Result<OwnedHandle> {
    open_verified(pid, create_time, PROCESS_QUERY_LIMITED_INFORMATION)
}

/// Total CPU cycles the process has used, including after it has exited.
pub fn cycle_time(process: &OwnedHandle) -> Result<u64> {
    let mut cycles = 0u64;
    // SAFETY: `process` is a live process handle and `cycles` is a valid out-pointer.
    if unsafe { QueryProcessCycleTime(process.as_raw_handle(), &mut cycles) } == 0 {
        return Err(Error::last_win32("QueryProcessCycleTime"));
    }
    Ok(cycles)
}

/// Full Win32 path of the process image, e.g. `C:\Windows\explorer.exe`.
pub fn image_path(pid: u32) -> Result<String> {
    let process = open_process(pid, PROCESS_QUERY_LIMITED_INFORMATION)?;
    let mut path = vec![0u16; 32 * 1024];
    let mut len = path.len() as u32;
    // SAFETY: `process` is a live handle, `path` is writable for `len` UTF-16 units, and `len`
    // is a valid in/out pointer.
    let ok =
        unsafe { QueryFullProcessImageNameW(process.as_raw_handle(), PROCESS_NAME_WIN32, path.as_mut_ptr(), &mut len) };
    if ok == 0 {
        return Err(Error::last_win32("QueryFullProcessImageNameW"));
    }
    Ok(String::from_utf16_lossy(&path[..(len as usize).min(path.len())]))
}

/// The command line the process was started with.
pub fn command_line(pid: u32) -> Result<String> {
    const CALL: &str = "NtQueryInformationProcess(ProcessCommandLineInformation)";
    let process = open_process(pid, PROCESS_QUERY_LIMITED_INFORMATION)?;
    let mut buffer = AlignedBuffer::new(0x1000);
    for _ in 0..4 {
        let capacity = buffer.capacity_u32();
        let mut needed = 0u32;
        // SAFETY: `process` is a live handle, the pointer/length pair describes memory owned by
        // `buffer`, and `needed` is a valid out-pointer.
        let status = unsafe {
            NtQueryInformationProcess(
                process.as_raw_handle(),
                ProcessCommandLineInformation,
                buffer.as_mut_ptr(),
                capacity,
                &mut needed,
            )
        };
        if status >= 0 {
            let bytes = buffer.bytes(capacity as usize);
            let header: UnicodeString = read_pod(bytes, 0).ok_or(Error::Malformed { call: CALL })?;
            return unicode_string_in(bytes, &header).ok_or(Error::Malformed { call: CALL });
        }
        if !is_size_status(status) || needed as usize <= buffer.capacity() {
            return Err(Error::Nt { call: CALL, status });
        }
        buffer = AlignedBuffer::new(needed as usize);
    }
    Err(Error::Malformed { call: CALL })
}

pub(crate) fn create_time(process: &OwnedHandle) -> Result<i64> {
    let zero = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
    // SAFETY: `process` is a live handle and all four out-pointers are valid FILETIMEs.
    let ok = unsafe { GetProcessTimes(process.as_raw_handle(), &mut created, &mut exited, &mut kernel, &mut user) };
    if ok == 0 {
        return Err(Error::last_win32("GetProcessTimes"));
    }
    Ok(((u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime)) as i64)
}

/// Terminates the process, but only if it is still the one created at `expected_create_time`.
///
/// PIDs are recycled, so checking the creation time on the open handle guarantees we never kill
/// an unrelated process that inherited the PID after the original exited.
pub fn terminate(pid: u32, expected_create_time: i64) -> Result<()> {
    let process = open_verified(pid, expected_create_time, PROCESS_TERMINATE)?;
    // SAFETY: `process` is a live handle opened with PROCESS_TERMINATE.
    let ok = unsafe { TerminateProcess(process.as_raw_handle(), 1) };
    if ok == 0 {
        return Err(Error::last_win32("TerminateProcess"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enumerate_finds_this_process() {
        let processes = enumerate().expect("enumerate processes");
        let me = processes.iter().find(|p| p.pid == std::process::id()).expect("own process listed");
        assert!(me.name.to_ascii_lowercase().ends_with(".exe"), "name was {:?}", me.name);
        assert!(me.threads >= 1);
        assert!(processes.iter().any(|p| p.pid == 4), "System process missing");
    }

    #[test]
    fn queries_own_path_and_command_line() {
        let pid = std::process::id();
        let path = image_path(pid).expect("image path");
        assert!(path.to_ascii_lowercase().ends_with(".exe"));
        assert!(!command_line(pid).expect("command line").is_empty());
    }

    #[test]
    fn query_handles_outlive_the_check_and_read_cycles() {
        let me = enumerate().unwrap().into_iter().find(|p| p.pid == std::process::id()).unwrap();
        let handle = open_for_query(me.pid, me.create_time).expect("open");
        assert!(cycle_time(&handle).expect("cycles") > 0);
        assert_eq!(open_for_query(me.pid, me.create_time + 1).err(), Some(Error::ProcessGone));
    }

    #[test]
    fn terminate_refuses_a_stale_create_time() {
        let pid = std::process::id();
        assert_eq!(terminate(pid, 1), Err(Error::ProcessGone));
    }
}
