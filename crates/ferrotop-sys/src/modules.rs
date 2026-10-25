//! The DLLs (and main image) loaded in a process.

use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, MODULEENTRY32W, Module32FirstW, Module32NextW, TH32CS_SNAPMODULE, TH32CS_SNAPMODULE32,
};

use crate::buffer::utf16_until_nul;
use crate::{Error, Result};

const ERROR_BAD_LENGTH: u32 = 24;
/// No process loads anywhere near this many modules; stop rather than loop on a broken snapshot.
const MAX_MODULES: usize = 16_384;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleInfo {
    pub base: u64,
    pub size: u32,
    pub name: String,
    pub path: String,
}

impl ModuleInfo {
    pub fn contains(&self, address: u64) -> bool {
        address >= self.base && address - self.base < u64::from(self.size)
    }
}

fn snapshot(pid: u32) -> Result<OwnedHandle> {
    // The snapshot fails with ERROR_BAD_LENGTH while the target is loading or unloading modules;
    // retrying is the documented remedy.
    for _ in 0..5 {
        // SAFETY: no memory-safety preconditions; returns INVALID_HANDLE_VALUE on failure.
        let raw = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPMODULE | TH32CS_SNAPMODULE32, pid) };
        if raw != INVALID_HANDLE_VALUE {
            // SAFETY: `raw` is a snapshot handle we just created, and nothing else owns it.
            return Ok(unsafe { OwnedHandle::from_raw_handle(raw) });
        }
        let error = Error::last_win32("CreateToolhelp32Snapshot");
        if error != (Error::Win32 { call: "CreateToolhelp32Snapshot", code: ERROR_BAD_LENGTH }) {
            return Err(error);
        }
    }
    Err(Error::Malformed { call: "CreateToolhelp32Snapshot" })
}

/// The modules loaded in `pid`, main image first, as Windows lists them.
pub fn modules(pid: u32) -> Result<Vec<ModuleInfo>> {
    let snapshot = snapshot(pid)?;
    // SAFETY: MODULEENTRY32W is plain data (integers, pointers, arrays); all-zero is valid.
    let mut entry: MODULEENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = size_of::<MODULEENTRY32W>() as u32;
    let mut modules = Vec::new();
    // SAFETY: `snapshot` is a live snapshot handle and `entry` is writable with `dwSize` set.
    let mut ok = unsafe { Module32FirstW(snapshot.as_raw_handle(), &mut entry) };
    while ok != 0 && modules.len() < MAX_MODULES {
        modules.push(ModuleInfo {
            base: entry.modBaseAddr as u64,
            size: entry.modBaseSize,
            name: utf16_until_nul(bytes_of(&entry.szModule)),
            path: utf16_until_nul(bytes_of(&entry.szExePath)),
        });
        // SAFETY: as above.
        ok = unsafe { Module32NextW(snapshot.as_raw_handle(), &mut entry) };
    }
    if modules.is_empty() {
        return Err(Error::last_win32("Module32FirstW"));
    }
    Ok(modules)
}

/// A fixed UTF-16 array as bytes, for [`utf16_until_nul`].
fn bytes_of(units: &[u16]) -> &[u8] {
    // SAFETY: u16 has no padding and any byte is a valid u8; the slice covers exactly the
    // array's memory and borrows from it.
    unsafe { std::slice::from_raw_parts(units.as_ptr().cast::<u8>(), size_of_val(units)) }
}

/// `module.dll+0x1a2b` for an address, given the process's modules.
pub fn symbolize(modules: &[ModuleInfo], address: u64) -> String {
    match modules.iter().find(|m| m.contains(address)) {
        Some(module) => format!("{}+0x{:x}", module.name, address - module.base),
        None => format!("0x{address:x}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_own_modules_with_the_exe_first() {
        let modules = modules(std::process::id()).expect("modules");
        assert!(modules[0].name.to_ascii_lowercase().ends_with(".exe"));
        assert!(modules.iter().any(|m| m.name.eq_ignore_ascii_case("ntdll.dll")));
        let ntdll = modules.iter().find(|m| m.name.eq_ignore_ascii_case("ntdll.dll")).unwrap();
        assert_eq!(symbolize(&modules, ntdll.base + 0x10), format!("{}+0x10", ntdll.name));
        assert_eq!(symbolize(&[], 0x1234), "0x1234");
    }
}
