//! Another process's environment variables, read from its process parameters.
//!
//! The block is copied out of the target with `ReadProcessMemory` (never mapped or executed) and
//! parsed with length checks, since the target controls its contents.

use windows_sys::Wdk::System::Threading::{NtQueryInformationProcess, ProcessBasicInformation};
use windows_sys::Win32::System::Threading::{PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ};

use crate::buffer::read_pod;
use crate::memory::{read, read_u64};
use crate::ntapi::ProcessBasicInformation as RawBasicInformation;
use crate::process::open_process;
use crate::{Error, Result};

/// Offsets in the 64-bit PEB and RTL_USER_PROCESS_PARAMETERS (phnt `ntpebteb.h`, `ntrtl.h`).
const PEB_PROCESS_PARAMETERS: u64 = 0x20;
const PARAMETERS_ENVIRONMENT: u64 = 0x80;
const PARAMETERS_ENVIRONMENT_SIZE: u64 = 0x3F0;
/// Windows caps an environment block at 32,767 characters per variable but not overall; a few
/// MiB is far beyond anything real.
const MAX_ENVIRONMENT: u64 = 4 << 20;

/// The process's environment as `(name, value)` pairs, in block order.
pub fn environment(pid: u32) -> Result<Vec<(String, String)>> {
    let process = open_process(pid, PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ)?;
    let mut basic = [0u8; size_of::<RawBasicInformation>()];
    // SAFETY: `process` is live and `basic` is a writable buffer of exactly the size passed.
    let status = unsafe {
        NtQueryInformationProcess(
            std::os::windows::io::AsRawHandle::as_raw_handle(&process),
            ProcessBasicInformation,
            basic.as_mut_ptr().cast(),
            basic.len() as u32,
            std::ptr::null_mut(),
        )
    };
    if status < 0 {
        return Err(Error::Nt { call: "NtQueryInformationProcess(ProcessBasicInformation)", status });
    }
    let basic: RawBasicInformation = read_pod(&basic, 0).ok_or(Error::Malformed { call: "ProcessBasicInformation" })?;
    let peb = basic.peb_base_address as u64;
    if peb == 0 {
        return Err(Error::Malformed { call: "ProcessBasicInformation" });
    }
    let parameters = read_u64(&process, peb + PEB_PROCESS_PARAMETERS)?;
    let block = read_u64(&process, parameters + PARAMETERS_ENVIRONMENT)?;
    let size = read_u64(&process, parameters + PARAMETERS_ENVIRONMENT_SIZE)?;
    if block == 0 || size == 0 || size > MAX_ENVIRONMENT {
        return Err(Error::Malformed { call: "RTL_USER_PROCESS_PARAMETERS" });
    }
    let mut bytes = vec![0u8; size as usize];
    read(&process, block, &mut bytes)?;
    let units: Vec<u16> = bytes.as_chunks::<2>().0.iter().map(|&pair| u16::from_le_bytes(pair)).collect();
    Ok(parse_environment(&units))
}

/// Parses a `NAME=value\0NAME=value\0\0` block. Entries without `=` are skipped; names may start
/// with `=` (the hidden per-drive current directories such as `=C:=C:\Users`).
pub(crate) fn parse_environment(units: &[u16]) -> Vec<(String, String)> {
    units
        .split(|&unit| unit == 0)
        .take_while(|entry| !entry.is_empty())
        .filter_map(|entry| {
            let text = String::from_utf16_lossy(entry);
            // Skip a leading '=' when looking for the separator.
            let split = text.char_indices().skip(1).find(|&(_, c)| c == '=').map(|(i, _)| i)?;
            Some((text[..split].to_owned(), text[split + 1..].to_owned()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(text: &str) -> Vec<u16> {
        text.encode_utf16().collect()
    }

    #[test]
    fn parses_blocks_and_hidden_drive_variables() {
        let parsed = parse_environment(&block("=C:=C:\\x\0PATH=a;b\0EMPTY=\0broken\0\0IGNORED=1\0"));
        assert_eq!(
            parsed,
            [("=C:".into(), "C:\\x".into()), ("PATH".into(), "a;b".into()), ("EMPTY".into(), String::new())]
        );
        assert!(parse_environment(&[]).is_empty());
        assert_eq!(parse_environment(&block("A=1")), [("A".into(), "1".into())], "unterminated tail");
    }

    #[test]
    fn reads_own_environment() {
        // The test process certainly has PATH (or Path) set.
        let env = environment(std::process::id()).expect("environment");
        assert!(env.iter().any(|(name, _)| name.eq_ignore_ascii_case("PATH")), "{} variables", env.len());
    }

    #[test]
    fn never_panics_on_arbitrary_blocks() {
        crate::fuzz::mutate(&[0x41, 0, 0x3D, 0, 0x42, 0, 0, 0, 0, 0], 5_000, |bytes| {
            let units: Vec<u16> = bytes.as_chunks::<2>().0.iter().map(|&p| u16::from_le_bytes(p)).collect();
            let _ = parse_environment(&units);
        });
    }
}
