//! Physical disk identity: model, bus, SSD or spinning, and size.

use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::Storage::FileSystem::{CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::{IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, IOCTL_STORAGE_QUERY_PROPERTY};

use crate::buffer::wide;
use crate::{Error, Result};

const STORAGE_DEVICE_PROPERTY: u32 = 0;
const STORAGE_DEVICE_SEEK_PENALTY_PROPERTY: u32 = 7;
const PROPERTY_STANDARD_QUERY: u32 = 0;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskInfo {
    pub number: u32,
    pub model: String,
    /// Bus type, e.g. `NVMe`, `SATA`, `USB`, `RAID`.
    pub bus: &'static str,
    /// `Some(true)` for solid-state, `Some(false)` for spinning, `None` if the drive won't say.
    pub ssd: Option<bool>,
    pub removable: bool,
    pub size: Option<u64>,
}

impl DiskInfo {
    /// Task Manager's type label: `SSD (NVMe)`, `HDD (SATA)`, `USB (USB)`.
    pub fn kind(&self) -> String {
        let kind = match (self.bus, self.ssd) {
            ("USB", _) => "USB",
            (_, Some(true)) => "SSD",
            (_, Some(false)) => "HDD",
            _ if self.removable => "Removable",
            _ => "Disk",
        };
        format!("{kind} ({})", self.bus)
    }
}

/// `STORAGE_PROPERTY_QUERY` with no additional parameters.
#[repr(C)]
struct PropertyQuery {
    property_id: u32,
    query_type: u32,
    additional: [u8; 4],
}

/// Reads model, bus, media type and size for `\\.\PhysicalDrive{number}`. Opening the drive
/// with no access rights is enough for these queries, so this works without elevation.
pub fn disk_info(number: u32) -> Result<DiskInfo> {
    let path = wide(&format!(r"\\.\PhysicalDrive{number}"));
    // SAFETY: `path` is NUL-terminated; zero access rights, no security attributes and no
    // template are all permitted.
    let raw = unsafe {
        CreateFileW(
            path.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if raw == INVALID_HANDLE_VALUE {
        return Err(Error::last_win32("CreateFileW"));
    }
    // SAFETY: `raw` is a valid handle we just opened, and nothing else owns it.
    let drive = unsafe { OwnedHandle::from_raw_handle(raw) };

    let descriptor = query_property(&drive, STORAGE_DEVICE_PROPERTY)?;
    let field =
        |offset: usize| descriptor.get(offset..offset + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    let ascii = |offset: Option<u32>| match offset {
        Some(start) if start != 0 => descriptor
            .get(start as usize..)
            .map(|tail| {
                let end = tail.iter().position(|&b| b == 0).unwrap_or(tail.len());
                String::from_utf8_lossy(&tail[..end]).trim().to_owned()
            })
            .unwrap_or_default(),
        _ => String::new(),
    };
    let vendor = ascii(field(12));
    let product = ascii(field(16));
    let model = [vendor, product].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" ");

    let ssd = query_property(&drive, STORAGE_DEVICE_SEEK_PENALTY_PROPERTY)
        .ok()
        .and_then(|penalty| penalty.get(8).map(|&incurs| incurs == 0));

    let mut geometry = [0u8; 256];
    let size = ioctl(&drive, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, &[], &mut geometry)
        .ok()
        .filter(|&written| written >= 32)
        .map(|_| u64::from_le_bytes(geometry[24..32].try_into().unwrap_or_default()));

    Ok(DiskInfo {
        number,
        model,
        bus: bus_name(field(28).unwrap_or(0)),
        ssd,
        removable: descriptor.get(10).is_some_and(|&b| b != 0),
        size,
    })
}

fn query_property(drive: &OwnedHandle, property_id: u32) -> Result<Vec<u8>> {
    let query = PropertyQuery { property_id, query_type: PROPERTY_STANDARD_QUERY, additional: [0; 4] };
    // SAFETY: `PropertyQuery` is plain integers with no padding, so viewing it as bytes is sound.
    let input =
        unsafe { std::slice::from_raw_parts((&raw const query).cast::<u8>(), std::mem::size_of::<PropertyQuery>()) };
    let mut output = vec![0u8; 1024];
    let written = ioctl(drive, IOCTL_STORAGE_QUERY_PROPERTY, input, &mut output)?;
    output.truncate(written);
    Ok(output)
}

/// `DeviceIoControl` with byte buffers; returns the number of bytes written.
fn ioctl(device: &OwnedHandle, code: u32, input: &[u8], output: &mut [u8]) -> Result<usize> {
    let mut written = 0u32;
    // SAFETY: `device` is a live handle, both buffers are valid for their lengths, the output is
    // writable, and the call is synchronous (no OVERLAPPED).
    let ok = unsafe {
        DeviceIoControl(
            device.as_raw_handle(),
            code,
            input.as_ptr().cast(),
            input.len() as u32,
            output.as_mut_ptr().cast(),
            output.len() as u32,
            &mut written,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 { Err(Error::last_win32("DeviceIoControl")) } else { Ok((written as usize).min(output.len())) }
}

fn bus_name(bus: u32) -> &'static str {
    match bus {
        1 => "SCSI",
        2 => "ATAPI",
        3 => "ATA",
        4 => "1394",
        6 => "Fibre",
        7 => "USB",
        8 => "RAID",
        9 => "iSCSI",
        10 => "SAS",
        11 => "SATA",
        12 => "SD",
        13 => "MMC",
        14 | 15 => "Virtual",
        16 => "Spaces",
        17 => "NVMe",
        18 => "SCM",
        19 => "UFS",
        _ => "Unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describes_some_disk() {
        let found = (0..8).filter_map(|n| disk_info(n).ok()).collect::<Vec<_>>();
        assert!(!found.is_empty(), "no physical drives found");
        assert!(found.iter().any(|d| d.size.is_some_and(|s| s > 1 << 30)), "{found:?}");
    }
}
