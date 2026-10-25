//! Bounds-checked access to buffers filled in by the kernel.

use std::ffi::c_void;
use std::mem::size_of;

use windows_sys::Wdk::System::SystemInformation::{NtQuerySystemInformation, SYSTEM_INFORMATION_CLASS};
use windows_sys::Win32::Foundation::{STATUS_BUFFER_OVERFLOW, STATUS_BUFFER_TOO_SMALL, STATUS_INFO_LENGTH_MISMATCH};

use crate::ntapi::{
    EnumServiceStatusProcess, ObjectTypeInformation, ProcessBasicInformation, ProcessHandleTableEntryInfo,
    QueryServiceConfig, SidAndAttributes, SystemProcessInformation, SystemProcessorPerformanceInformation,
    SystemThreadInformation, UnicodeString,
};
use crate::{Error, Result};

/// Largest buffer we'll allocate for a single query, so a bogus size can't exhaust memory.
const MAX_BUFFER: usize = 256 * 1024 * 1024;

/// A zeroed, 8-byte-aligned buffer for NT query APIs.
pub(crate) struct AlignedBuffer {
    words: Vec<u64>,
}

impl AlignedBuffer {
    pub fn new(bytes: usize) -> Self {
        Self { words: vec![0; bytes.div_ceil(8).max(1)] }
    }

    pub fn capacity(&self) -> usize {
        self.words.len() * 8
    }

    /// The capacity to report to the kernel. Under-reporting is always safe.
    pub fn capacity_u32(&self) -> u32 {
        u32::try_from(self.capacity()).unwrap_or(u32::MAX)
    }

    pub fn as_mut_ptr(&mut self) -> *mut c_void {
        self.words.as_mut_ptr().cast()
    }

    /// The first `len` bytes (clamped to the capacity).
    pub fn bytes(&self, len: usize) -> &[u8] {
        let len = len.min(self.capacity());
        // SAFETY: the Vec owns `capacity()` initialized bytes (it is zero-filled and u64 has no
        // padding), `len` is clamped to that, and the slice borrows `self` so it can't dangle.
        unsafe { std::slice::from_raw_parts(self.words.as_ptr().cast::<u8>(), len) }
    }
}

/// True for the statuses NT uses to say "your buffer is too small".
pub(crate) fn is_size_status(status: i32) -> bool {
    matches!(status, STATUS_INFO_LENGTH_MISMATCH | STATUS_BUFFER_TOO_SMALL | STATUS_BUFFER_OVERFLOW)
}

/// Grows `buffer` until `class` fits, then returns how many bytes the kernel wrote.
pub(crate) fn query_system_information(class: SYSTEM_INFORMATION_CLASS, buffer: &mut AlignedBuffer) -> Result<usize> {
    const CALL: &str = "NtQuerySystemInformation";
    for _ in 0..8 {
        let capacity = buffer.capacity_u32();
        let mut needed = 0u32;
        // SAFETY: the pointer/length pair describes memory owned by `buffer` that is writable for
        // `capacity` bytes, and `needed` is a valid out-pointer.
        let status = unsafe { NtQuerySystemInformation(class, buffer.as_mut_ptr(), capacity, &mut needed) };
        if status >= 0 {
            return Ok(if needed == 0 { capacity } else { needed.min(capacity) } as usize);
        }
        if !is_size_status(status) {
            return Err(Error::Nt { call: CALL, status });
        }
        // Processes can start between calls, so leave some headroom.
        let wanted = (needed as usize).max(buffer.capacity()).saturating_add(0x2_0000);
        if wanted > MAX_BUFFER {
            return Err(Error::Malformed { call: CALL });
        }
        *buffer = AlignedBuffer::new(wanted);
    }
    Err(Error::Nt { call: CALL, status: STATUS_INFO_LENGTH_MISMATCH })
}

/// Marker for `repr(C)` types made only of integers and raw pointers.
///
/// # Safety
/// Every bit pattern must be a valid value of the type.
pub(crate) unsafe trait Pod: Copy {}

// SAFETY: integers and a raw pointer; every bit pattern is valid.
unsafe impl Pod for UnicodeString {}
// SAFETY: integers, raw pointers and a nested `UnicodeString`; every bit pattern is valid.
unsafe impl Pod for SystemProcessInformation {}
// SAFETY: integers only; every bit pattern is valid.
unsafe impl Pod for SystemProcessorPerformanceInformation {}
// SAFETY: a plain integer; every bit pattern is valid.
unsafe impl Pod for u64 {}
// SAFETY: a raw pointer and an integer; every bit pattern is valid.
unsafe impl Pod for SidAndAttributes {}
// SAFETY: raw pointers and integers; every bit pattern is valid.
unsafe impl Pod for EnumServiceStatusProcess {}
// SAFETY: integers and a nested struct of integers; every bit pattern is valid.
unsafe impl Pod for SystemThreadInformation {}
// SAFETY: integers only; every bit pattern is valid.
unsafe impl Pod for ProcessBasicInformation {}
// SAFETY: integers only; every bit pattern is valid.
unsafe impl Pod for ProcessHandleTableEntryInfo {}
// SAFETY: integers and a nested `UnicodeString`; every bit pattern is valid.
unsafe impl Pod for ObjectTypeInformation {}
// SAFETY: integers and raw pointers; every bit pattern is valid.
unsafe impl Pod for QueryServiceConfig {}
// SAFETY: a plain integer; every bit pattern is valid.
unsafe impl Pod for u32 {}
// SAFETY: a plain integer; every bit pattern is valid.
unsafe impl Pod for usize {}

/// Reads a `T` at `offset`, or `None` if it would run past the end of `bytes`.
pub(crate) fn read_pod<T: Pod>(bytes: &[u8], offset: usize) -> Option<T> {
    let end = offset.checked_add(size_of::<T>())?;
    let src = bytes.get(offset..end)?;
    // SAFETY: `src` is exactly `size_of::<T>()` readable bytes, `read_unaligned` has no alignment
    // requirement, and `T: Pod` means any bit pattern is a valid `T`.
    Some(unsafe { src.as_ptr().cast::<T>().read_unaligned() })
}

/// Decodes a `UNICODE_STRING` whose characters live inside `bytes`.
///
/// The kernel points `buffer` back into the same allocation; anything pointing elsewhere is
/// rejected rather than dereferenced.
pub(crate) fn unicode_string_in(bytes: &[u8], string: &UnicodeString) -> Option<String> {
    if string.buffer.is_null() || string.length == 0 {
        return Some(String::new());
    }
    let start = string.buffer.addr().checked_sub(bytes.as_ptr().addr())?;
    let end = start.checked_add(usize::from(string.length))?;
    let units: Vec<u16> =
        bytes.get(start..end)?.as_chunks::<2>().0.iter().map(|&pair| u16::from_le_bytes(pair)).collect();
    Some(String::from_utf16_lossy(&units))
}

/// Byte offset of `ptr` inside `bytes`, or `None` if it points elsewhere.
pub(crate) fn offset_in<T>(bytes: &[u8], ptr: *const T) -> Option<usize> {
    let offset = ptr.addr().checked_sub(bytes.as_ptr().addr())?;
    (offset < bytes.len()).then_some(offset)
}

/// Decodes a NUL-terminated UTF-16 string that lives inside `bytes`. An unterminated string
/// (one that would run off the end of the buffer) is rejected.
pub(crate) fn wide_cstr_in(bytes: &[u8], ptr: *const u16) -> Option<String> {
    if ptr.is_null() {
        return Some(String::new());
    }
    let units: Vec<u16> = bytes
        .get(offset_in(bytes, ptr)?..)?
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&pair| u16::from_le_bytes(pair))
        .take_while(|&unit| unit != 0)
        .collect();
    let terminator = offset_in(bytes, ptr)? + units.len() * 2;
    (bytes.get(terminator..terminator + 2) == Some(&[0, 0])).then(|| String::from_utf16_lossy(&units))
}

/// Decodes UTF-16 bytes, stopping at the first NUL.
pub(crate) fn utf16_until_nul(raw: &[u8]) -> String {
    let units: Vec<u16> =
        raw.as_chunks::<2>().0.iter().map(|&pair| u16::from_le_bytes(pair)).take_while(|&unit| unit != 0).collect();
    String::from_utf16_lossy(&units)
}

/// A NUL-terminated UTF-16 copy of `text` for passing to Win32.
pub(crate) fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_cstr_must_be_terminated_inside_the_buffer() {
        let mut bytes = vec![0u8; 12];
        bytes[2..8].copy_from_slice(&[b'h', 0, b'i', 0, 0, 0]);
        let at = |offset: usize| bytes[offset..].as_ptr().cast::<u16>();
        assert_eq!(wide_cstr_in(&bytes, at(2)).as_deref(), Some("hi"));

        let mut unterminated = vec![b'x'; 8];
        unterminated[0] = b'a';
        assert_eq!(wide_cstr_in(&unterminated, unterminated.as_ptr().cast()), None);

        let elsewhere = [0u16; 2];
        assert_eq!(wide_cstr_in(&bytes, elsewhere.as_ptr()), None);
    }

    #[test]
    fn read_pod_rejects_out_of_bounds() {
        let bytes = [0u8; 20];
        assert!(read_pod::<UnicodeString>(&bytes, 0).is_some());
        assert!(read_pod::<UnicodeString>(&bytes, 5).is_none());
        assert!(read_pod::<UnicodeString>(&bytes, usize::MAX).is_none());
    }

    #[test]
    fn unicode_string_must_point_inside_the_buffer() {
        let mut bytes = vec![0u8; 32];
        bytes[16..22].copy_from_slice(&[b'a', 0, b'b', 0, b'c', 0]);
        let inside = UnicodeString { length: 6, maximum_length: 6, buffer: bytes[16..].as_ptr().cast() };
        assert_eq!(unicode_string_in(&bytes, &inside).as_deref(), Some("abc"));

        let overrun = UnicodeString { length: 64, maximum_length: 64, buffer: bytes[16..].as_ptr().cast() };
        assert_eq!(unicode_string_in(&bytes, &overrun), None);

        let outside = [0u16; 4];
        let elsewhere = UnicodeString { length: 8, maximum_length: 8, buffer: outside.as_ptr() };
        assert_eq!(unicode_string_in(&bytes, &elsewhere), None);
    }
}
