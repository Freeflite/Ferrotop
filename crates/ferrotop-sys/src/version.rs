//! File version resources: the description and company shown next to a process.

use std::ffi::c_void;

use windows_sys::Win32::Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW};

use crate::buffer::{AlignedBuffer, offset_in, utf16_until_nul, wide};
use crate::{Error, Result};

/// Largest version resource we'll load.
const MAX_VERSION_INFO: u32 = 4 * 1024 * 1024;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VersionInfo {
    pub description: Option<String>,
    pub company: Option<String>,
}

/// Reads `FileDescription` and `CompanyName` from an executable's version resource.
pub fn version_info(path: &str) -> Result<VersionInfo> {
    let path = wide(path);
    let mut ignored = 0u32;
    // SAFETY: `path` is NUL-terminated and `ignored` is a valid out-pointer.
    let size = unsafe { GetFileVersionInfoSizeW(path.as_ptr(), &mut ignored) };
    if size == 0 {
        return Err(Error::last_win32("GetFileVersionInfoSizeW"));
    }
    if size > MAX_VERSION_INFO {
        return Err(Error::Malformed { call: "GetFileVersionInfoSizeW" });
    }
    let mut block = AlignedBuffer::new(size as usize);
    // SAFETY: `path` is NUL-terminated and `block` is writable for `size` bytes.
    if unsafe { GetFileVersionInfoW(path.as_ptr(), 0, size, block.as_mut_ptr()) } == 0 {
        return Err(Error::last_win32("GetFileVersionInfoW"));
    }
    let size = size as usize;

    // Try the languages the file declares, then the usual fallbacks.
    let mut languages: Vec<String> = query(&mut block, size, r"\VarFileInfo\Translation")
        .map(|raw| {
            raw.as_chunks::<4>()
                .0
                .iter()
                .map(|t| format!("{:04x}{:04x}", u16::from_le_bytes([t[0], t[1]]), u16::from_le_bytes([t[2], t[3]])))
                .collect()
        })
        .unwrap_or_default();
    languages.extend(["040904b0", "040904e4", "000004b0"].map(String::from));

    let mut string = |name: &str| {
        languages.iter().find_map(|language| {
            let raw = query(&mut block, size, &format!(r"\StringFileInfo\{language}\{name}"))?;
            Some(utf16_until_nul(&raw).trim().to_owned()).filter(|s| !s.is_empty())
        })
    };
    Ok(VersionInfo { description: string("FileDescription"), company: string("CompanyName") })
}

/// Looks up `sub_block`, returning a copy of the bytes it points at inside the block.
fn query(block: &mut AlignedBuffer, size: usize, sub_block: &str) -> Option<Vec<u8>> {
    let sub_block = wide(sub_block);
    let mut value: *mut c_void = std::ptr::null_mut();
    let mut len = 0u32;
    // SAFETY: `block` holds the version resource GetFileVersionInfoW filled in, `sub_block` is
    // NUL-terminated, and both out-pointers are valid.
    let ok = unsafe { VerQueryValueW(block.as_mut_ptr(), sub_block.as_ptr(), &mut value, &mut len) };
    if ok == 0 || value.is_null() {
        return None;
    }
    let bytes = block.bytes(size);
    let start = offset_in(bytes, value.cast::<u8>())?;
    // `len` is in characters for strings and bytes for binary values; take the larger reading and
    // let the caller stop at the terminator. Either way, never read past the block.
    let end = start.saturating_add(len as usize * 2).min(bytes.len());
    Some(bytes[start..end].to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_system_binary() {
        let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
        let info = version_info(&format!(r"{root}\System32\ntoskrnl.exe")).expect("ntoskrnl version info");
        assert!(info.company.as_deref().unwrap_or_default().contains("Microsoft"), "{info:?}");
        assert!(info.description.is_some(), "{info:?}");
    }
}
