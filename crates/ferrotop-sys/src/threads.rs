//! A process's threads, from the same kernel snapshot as the process list.

use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

use windows_sys::Wdk::System::SystemInformation::SystemProcessInformation;
use windows_sys::Wdk::System::Threading::{NtQueryInformationThread, ThreadQuerySetWin32StartAddress};
use windows_sys::Win32::System::Threading::{
    GetProcessIdOfThread, OpenThread, THREAD_ACCESS_RIGHTS, THREAD_QUERY_INFORMATION,
};

use crate::buffer::{AlignedBuffer, query_system_information, read_pod};
use crate::ntapi::{SystemProcessInformation as RawProcess, SystemThreadInformation as RawThread};
use crate::process::non_negative;
use crate::{Error, Result};

/// `KTHREAD_STATE::Waiting` and the two "suspended" wait reasons.
pub const STATE_WAITING: u32 = 5;
const WAIT_SUSPENDED: u32 = 5;
const WAIT_WR_SUSPENDED: u32 = 12;

/// One thread. Times are in 100 ns units; `create_time` counts from 1601-01-01 UTC.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ThreadInfo {
    pub tid: u32,
    pub create_time: i64,
    pub kernel_time: u64,
    pub user_time: u64,
    /// The kernel's start address (usually `RtlUserThreadStart`); see [`win32_start_address`].
    pub start_address: u64,
    pub priority: i32,
    pub base_priority: i32,
    pub context_switches: u32,
    pub state: u32,
    pub wait_reason: u32,
}

impl ThreadInfo {
    pub fn is_suspended(&self) -> bool {
        self.state == STATE_WAITING && matches!(self.wait_reason, WAIT_SUSPENDED | WAIT_WR_SUSPENDED)
    }
}

/// The threads of process `pid`, or `None` if it isn't running.
pub fn threads(pid: u32) -> Result<Option<Vec<ThreadInfo>>> {
    let mut buffer = AlignedBuffer::new(0x20_0000);
    let len = query_system_information(SystemProcessInformation, &mut buffer)?;
    parse_threads(buffer.bytes(len), pid).ok_or(Error::Malformed { call: "NtQuerySystemInformation(threads)" })
}

/// Walks the process list to `pid` and decodes its thread array. `None` means the buffer is
/// malformed; `Some(None)` that the process isn't in it.
pub(crate) fn parse_threads(bytes: &[u8], pid: u32) -> Option<Option<Vec<ThreadInfo>>> {
    let mut offset = 0usize;
    loop {
        let raw: RawProcess = read_pod(bytes, offset)?;
        if raw.unique_process_id == pid as usize {
            return Some(Some(thread_array(bytes, offset, &raw)?));
        }
        match raw.next_entry_offset {
            0 => return Some(None),
            next => offset = offset.checked_add(next as usize)?,
        }
    }
}

/// The thread array following the process entry at `offset`.
pub(crate) fn thread_array(bytes: &[u8], offset: usize, raw: &RawProcess) -> Option<Vec<ThreadInfo>> {
    let start = offset.checked_add(size_of::<RawProcess>())?;
    // Never trust the count further than the buffer (or the next entry) allows.
    let limit = match raw.next_entry_offset {
        0 => bytes.len(),
        next => offset.checked_add(next as usize)?.min(bytes.len()),
    };
    let fits = limit.saturating_sub(start) / size_of::<RawThread>();
    let count = (raw.number_of_threads as usize).min(fits);
    (0..count)
        .map(|i| {
            let thread: RawThread = read_pod(bytes, start + i * size_of::<RawThread>())?;
            Some(ThreadInfo {
                tid: u32::try_from(thread.client_id.unique_thread).ok()?,
                create_time: thread.create_time,
                kernel_time: non_negative(thread.kernel_time),
                user_time: non_negative(thread.user_time),
                start_address: thread.start_address as u64,
                priority: thread.priority,
                base_priority: thread.base_priority,
                context_switches: thread.context_switches,
                state: thread.thread_state,
                wait_reason: thread.wait_reason,
            })
        })
        .collect()
}

/// True when every thread in the entry is suspended (how Task Manager decides "Suspended").
pub(crate) fn all_suspended(bytes: &[u8], offset: usize, raw: &RawProcess) -> bool {
    thread_array(bytes, offset, raw)
        .is_some_and(|threads| !threads.is_empty() && threads.iter().all(ThreadInfo::is_suspended))
}

pub(crate) fn open_thread(tid: u32, access: THREAD_ACCESS_RIGHTS) -> Result<OwnedHandle> {
    // SAFETY: OpenThread has no memory-safety preconditions; it returns null on failure.
    let raw = unsafe { OpenThread(access, 0, tid) };
    if raw.is_null() {
        return Err(Error::last_win32("OpenThread"));
    }
    // SAFETY: `raw` is a valid thread handle we just opened, and nothing else owns it.
    Ok(unsafe { OwnedHandle::from_raw_handle(raw) })
}

/// The address the thread's creator passed as its start routine, which (unlike the kernel's
/// start address) says what the thread is for. Checks the thread still belongs to `pid`.
pub fn win32_start_address(pid: u32, tid: u32) -> Result<u64> {
    // The start address needs full query rights; limited rights only get as far as the owner check.
    let thread = open_thread(tid, THREAD_QUERY_INFORMATION)?;
    // SAFETY: `thread` is a live thread handle.
    if unsafe { GetProcessIdOfThread(thread.as_raw_handle()) } != pid {
        return Err(Error::ProcessGone);
    }
    let mut address = 0usize;
    // SAFETY: `thread` is live and `address` is a writable pointer-sized out-buffer of the size passed.
    let status = unsafe {
        NtQueryInformationThread(
            thread.as_raw_handle(),
            ThreadQuerySetWin32StartAddress,
            (&raw mut address).cast(),
            size_of::<usize>() as u32,
            std::ptr::null_mut(),
        )
    };
    if status < 0 {
        return Err(Error::Nt { call: "NtQueryInformationThread(Win32StartAddress)", status });
    }
    Ok(address as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_this_process_threads_with_start_addresses() {
        let pid = std::process::id();
        let threads = threads(pid).expect("query").expect("own process present");
        assert!(!threads.is_empty());
        assert!(threads.iter().all(|t| t.tid != 0 && !t.is_suspended()));
        let first = &threads[0];
        assert_ne!(win32_start_address(pid, first.tid).expect("start address"), 0);
        assert_eq!(win32_start_address(pid.wrapping_add(4), first.tid), Err(Error::ProcessGone));
    }

    #[test]
    fn missing_processes_are_none() {
        assert_eq!(threads(u32::MAX - 7).expect("query"), None);
    }
}
