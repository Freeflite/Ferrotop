//! A process's virtual address space, region by region, and reading its memory.

use std::collections::HashMap;
use std::os::windows::io::{AsRawHandle, OwnedHandle};

use windows_sys::Win32::Storage::FileSystem::{GetLogicalDrives, QueryDosDeviceW};
use windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory;
use windows_sys::Win32::System::Memory::{MEM_FREE, MEMORY_BASIC_INFORMATION, VirtualQueryEx};
use windows_sys::Win32::System::ProcessStatus::K32GetMappedFileNameW;
use windows_sys::Win32::System::Threading::PROCESS_QUERY_INFORMATION;

use crate::buffer::wide;
use crate::process::open_process;
use crate::{Error, Result};

/// Even huge processes (browsers, JITs) stay well under this many regions.
const MAX_REGIONS: usize = 200_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryRegion {
    pub base: u64,
    pub allocation_base: u64,
    pub size: u64,
    /// `MEM_COMMIT` (0x1000) or `MEM_RESERVE` (0x2000).
    pub state: u32,
    /// `PAGE_*` protection of the region, and of the allocation when it was made.
    pub protect: u32,
    pub allocation_protect: u32,
    /// `MEM_IMAGE` (0x1000000), `MEM_MAPPED` (0x40000) or `MEM_PRIVATE` (0x20000).
    pub kind: u32,
    /// The file behind an image or mapped region, as a DOS path where possible.
    pub mapped_file: Option<String>,
}

/// Every allocated (non-free) region of `pid`'s address space.
pub fn regions(pid: u32) -> Result<Vec<MemoryRegion>> {
    let process = open_process(pid, PROCESS_QUERY_INFORMATION)?;
    let devices = dos_devices();
    let mut names: HashMap<u64, Option<String>> = HashMap::new();
    let mut regions = Vec::new();
    let mut address = 0usize;
    while regions.len() < MAX_REGIONS {
        // SAFETY: MEMORY_BASIC_INFORMATION is plain integers and pointers; all-zero is valid.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: `process` is live, `info` is writable for the size passed, and VirtualQueryEx
        // only inspects the target's address space (any address is acceptable).
        let written = unsafe {
            VirtualQueryEx(
                process.as_raw_handle(),
                address as *const _,
                &mut info,
                size_of::<MEMORY_BASIC_INFORMATION>(),
            )
        };
        if written == 0 {
            break;
        }
        let base = info.BaseAddress as usize;
        if info.State != MEM_FREE {
            let allocation_base = info.AllocationBase as u64;
            let mapped_file = if info.Type == 0x100_0000 || info.Type == 0x4_0000 {
                names
                    .entry(allocation_base)
                    .or_insert_with(|| mapped_file_name(&process, allocation_base, &devices))
                    .clone()
            } else {
                None
            };
            regions.push(MemoryRegion {
                base: base as u64,
                allocation_base,
                size: info.RegionSize as u64,
                state: info.State,
                protect: info.Protect,
                allocation_protect: info.AllocationProtect,
                kind: info.Type,
                mapped_file,
            });
        }
        match base.checked_add(info.RegionSize) {
            Some(next) if next > address => address = next,
            _ => break,
        }
    }
    if regions.is_empty() {
        return Err(Error::last_win32("VirtualQueryEx"));
    }
    Ok(regions)
}

fn mapped_file_name(process: &OwnedHandle, address: u64, devices: &[(String, String)]) -> Option<String> {
    let mut name = vec![0u16; 1024];
    // SAFETY: `process` is live and `name` is writable for the length passed.
    let len = unsafe {
        K32GetMappedFileNameW(process.as_raw_handle(), address as *const _, name.as_mut_ptr(), name.len() as u32)
    };
    (len > 0).then(|| dos_path(&String::from_utf16_lossy(&name[..(len as usize).min(name.len())]), devices))
}

/// `(device path, drive)` pairs such as `(\Device\HarddiskVolume3, C:)`.
pub fn dos_devices() -> Vec<(String, String)> {
    // SAFETY: no preconditions.
    let drives = unsafe { GetLogicalDrives() };
    (0..26u8)
        .filter(|bit| drives & (1 << bit) != 0)
        .filter_map(|bit| {
            let drive = format!("{}:", char::from(b'A' + bit));
            let name = wide(&drive);
            let mut target = vec![0u16; 1024];
            // SAFETY: `name` is NUL-terminated and `target` is writable for the length passed.
            let len = unsafe { QueryDosDeviceW(name.as_ptr(), target.as_mut_ptr(), target.len() as u32) };
            // The result is a list of NUL-terminated strings; the first is the current target.
            let first: Vec<u16> =
                target[..(len as usize).min(target.len())].iter().copied().take_while(|&c| c != 0).collect();
            (!first.is_empty()).then(|| (String::from_utf16_lossy(&first), drive))
        })
        .collect()
}

/// Rewrites a kernel device path (`\Device\HarddiskVolume3\x.dll`) as a DOS path (`C:\x.dll`).
pub fn dos_path(path: &str, devices: &[(String, String)]) -> String {
    for (device, drive) in devices {
        if let Some(rest) = path.strip_prefix(device.as_str())
            && (rest.is_empty() || rest.starts_with('\\'))
        {
            return format!("{drive}{rest}");
        }
    }
    path.to_owned()
}

/// Reads exactly `buffer.len()` bytes at `address` in the process.
pub(crate) fn read(process: &OwnedHandle, address: u64, buffer: &mut [u8]) -> Result<()> {
    let mut read = 0usize;
    // SAFETY: `process` is live; `buffer` is writable for its length; ReadProcessMemory copies
    // from the *target's* address space and fails cleanly on unreadable addresses.
    let ok = unsafe {
        ReadProcessMemory(
            process.as_raw_handle(),
            address as *const _,
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut read,
        )
    };
    if ok == 0 {
        return Err(Error::last_win32("ReadProcessMemory"));
    }
    if read != buffer.len() {
        return Err(Error::Malformed { call: "ReadProcessMemory" });
    }
    Ok(())
}

pub(crate) fn read_u64(process: &OwnedHandle, address: u64) -> Result<u64> {
    let mut bytes = [0u8; 8];
    read(process, address, &mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_device_paths_to_drives() {
        let devices = vec![(r"\Device\HarddiskVolume3".to_owned(), "C:".to_owned())];
        assert_eq!(dos_path(r"\Device\HarddiskVolume3\Windows\x.dll", &devices), r"C:\Windows\x.dll");
        assert_eq!(dos_path(r"\Device\HarddiskVolume30\x.dll", &devices), r"\Device\HarddiskVolume30\x.dll");
        assert_eq!(dos_path(r"\Device\NamedPipe\x", &devices), r"\Device\NamedPipe\x");
    }

    #[test]
    fn walks_own_address_space() {
        let regions = regions(std::process::id()).expect("regions");
        assert!(regions.len() > 10);
        assert!(regions.windows(2).all(|w| w[0].base < w[1].base), "ascending");
        let exe = regions
            .iter()
            .find(|r| r.kind == 0x100_0000 && r.mapped_file.as_deref().is_some_and(|f| f.ends_with(".exe")));
        assert!(exe.is_some(), "the exe image is mapped from a file");
        assert!(exe.unwrap().mapped_file.as_deref().unwrap().contains(":\\"), "as a DOS path");
    }
}
