//! A client for the PawnIO driver (<https://pawnio.eu>), used only to read CPU temperatures.
//!
//! Ferrotop doesn't install or ship the driver. If the user has installed it, Ferrotop opens
//! its device, loads one of the official signed modules bundled in `modules/pawnio`, and calls
//! that module's read-only functions. This talks to the device's documented I/O-control
//! interface directly, so PawnIO's user-mode DLL is never loaded into Ferrotop.
//!
//! The driver only accepts modules signed by its author, and only Administrators and SYSTEM may
//! open the device, so all of this needs Ferrotop to run elevated.

use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::DeviceIoControl;

use crate::buffer::wide;
use crate::{Error, Result};

/// The official AMD Zen module (SMN and MSR reads, families 17h to 1Ah).
pub const AMD_FAMILY_17_MODULE: &[u8] = include_bytes!("../modules/pawnio/AMDFamily17.bin");
/// The official Intel MSR module.
pub const INTEL_MSR_MODULE: &[u8] = include_bytes!("../modules/pawnio/IntelMSR.bin");

const DEVICE_PATH: &str = r"\\?\GLOBALROOT\Device\PawnIO";
const DEVICE_TYPE: u32 = 41394;
/// `CTL_CODE(DEVICE_TYPE, function, METHOD_BUFFERED, FILE_ANY_ACCESS)`.
const fn ctl_code(function: u32) -> u32 {
    (DEVICE_TYPE << 16) | (function << 2)
}
const IOCTL_LOAD_BINARY: u32 = ctl_code(0x821);
const IOCTL_EXECUTE_FN: u32 = ctl_code(0x841);
/// Function names are passed as a fixed, NUL-padded 32-byte field.
const FUNCTION_NAME_LEN: usize = 32;

const ERROR_FILE_NOT_FOUND: u32 = 2;
const ERROR_PATH_NOT_FOUND: u32 = 3;

/// An open PawnIO device handle with (once [`PawnIo::load`] succeeds) one module loaded.
#[derive(Debug)]
pub struct PawnIo {
    device: OwnedHandle,
}

impl PawnIo {
    /// Opens the device. Fails with a not-found error when PawnIO isn't installed and with
    /// access denied when Ferrotop isn't elevated.
    pub fn open() -> Result<Self> {
        let path = wide(DEVICE_PATH);
        // SAFETY: `path` is NUL-terminated; the other arguments are plain flags or null.
        let raw = unsafe {
            CreateFileW(
                path.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        if raw == INVALID_HANDLE_VALUE {
            return Err(Error::last_win32("CreateFile(PawnIO)"));
        }
        // SAFETY: `raw` is a handle we just opened, and nothing else owns it.
        Ok(Self { device: unsafe { OwnedHandle::from_raw_handle(raw) } })
    }

    /// Loads a signed module into this handle. The driver verifies the signature and runs the
    /// module's `main`, which fails (`ERROR_NOT_SUPPORTED`) on the wrong CPU vendor or family.
    pub fn load(&self, module: &[u8]) -> Result<()> {
        let len = u32::try_from(module.len()).map_err(|_| Error::Malformed { call: "PawnIO load" })?;
        let mut returned = 0u32;
        // SAFETY: the device handle is live, the input pointer/length describe `module`, there is
        // no output buffer, and `returned` is a valid out-pointer. The handle isn't overlapped,
        // so the call completes before returning.
        let ok = unsafe {
            DeviceIoControl(
                self.device.as_raw_handle(),
                IOCTL_LOAD_BINARY,
                module.as_ptr().cast(),
                len,
                std::ptr::null_mut(),
                0,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(Error::last_win32("PawnIO load"));
        }
        Ok(())
    }

    /// Calls a function of the loaded module, returning how many output values it wrote.
    pub fn execute(&self, function: &str, input: &[u64], output: &mut [u64]) -> Result<usize> {
        let request = execute_request(function, input).ok_or(Error::Malformed { call: "PawnIO execute" })?;
        let in_len = u32::try_from(request.len()).map_err(|_| Error::Malformed { call: "PawnIO execute" })?;
        let out_len = u32::try_from(size_of_val(output)).map_err(|_| Error::Malformed { call: "PawnIO execute" })?;
        let mut returned = 0u32;
        // SAFETY: the device handle is live; the input pointer/length describe `request` and the
        // output pointer/length describe `output`, both of which outlive this synchronous call.
        let ok = unsafe {
            DeviceIoControl(
                self.device.as_raw_handle(),
                IOCTL_EXECUTE_FN,
                request.as_ptr().cast(),
                in_len,
                output.as_mut_ptr().cast(),
                out_len,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(Error::last_win32("PawnIO execute"));
        }
        Ok((returned as usize / size_of::<u64>()).min(output.len()))
    }
}

/// True when opening the device failed because PawnIO isn't installed.
pub fn is_not_installed(error: &Error) -> bool {
    matches!(error, Error::Win32 { code: ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND, .. })
}

/// The execute request: the function name in a NUL-padded 32-byte field, then the inputs.
fn execute_request(function: &str, input: &[u64]) -> Option<Vec<u8>> {
    // The name needs room for its terminating NUL.
    if function.len() >= FUNCTION_NAME_LEN || function.bytes().any(|b| b == 0) {
        return None;
    }
    let mut request = vec![0u8; FUNCTION_NAME_LEN];
    request[..function.len()].copy_from_slice(function.as_bytes());
    for value in input {
        request.extend_from_slice(&value.to_le_bytes());
    }
    Some(request)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SHA-256 through Windows CNG, to pin the bundled modules to the verified release.
    fn sha256(data: &[u8]) -> String {
        use windows_sys::Win32::Security::Cryptography::{BCRYPT_SHA256_ALG_HANDLE, BCryptHash};
        let mut digest = [0u8; 32];
        // SAFETY: the pseudo-handle needs no setup; input and output describe live buffers.
        let status = unsafe {
            BCryptHash(
                BCRYPT_SHA256_ALG_HANDLE,
                std::ptr::null(),
                0,
                data.as_ptr(),
                data.len() as u32,
                digest.as_mut_ptr(),
                digest.len() as u32,
            )
        };
        assert_eq!(status, 0, "BCryptHash");
        digest.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn bundled_modules_are_the_verified_release() {
        assert_eq!(sha256(AMD_FAMILY_17_MODULE), "dae74615761b78bdf064dfb3e136252ddcc6fc727d88f14738d0e5800d427a91");
        assert_eq!(sha256(INTEL_MSR_MODULE), "d6ed85d65ab17a22f813ef98207d6d537155ee2ded5976a21cb48413c9b92e5f");
    }

    #[test]
    fn requests_carry_a_padded_name_then_inputs() {
        let request = execute_request("ioctl_read_smn", &[0x59800]).unwrap();
        assert_eq!(request.len(), 40);
        assert_eq!(&request[..14], b"ioctl_read_smn");
        assert!(request[14..32].iter().all(|&b| b == 0));
        assert_eq!(u64::from_le_bytes(request[32..].try_into().unwrap()), 0x59800);
        assert!(execute_request(&"x".repeat(32), &[]).is_none(), "no room for the NUL");
        assert!(execute_request("a\0b", &[]).is_none());
    }

    #[test]
    fn ioctl_codes_match_the_driver() {
        assert_eq!(IOCTL_LOAD_BINARY, 0xA1B2_2084);
        assert_eq!(IOCTL_EXECUTE_FN, 0xA1B2_2104);
    }
}
