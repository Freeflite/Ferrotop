//! Live figures the history service shares with Ferrotop windows that aren't elevated: each
//! process's disk and network rates (from the kernel trace) and the CPU temperatures (from
//! PawnIO), neither of which an ordinary user can read directly.
//!
//! It's a shared memory section the service writes and everyone else may only read; nothing
//! flows back to the service. Readers treat the contents as untrusted and check them anyway.
//! A sequence number (odd while the service is writing) lets a reader take a consistent copy
//! without locks, and every access is an atomic 32- or 64-bit load or store, so a read racing a
//! write is never undefined behaviour.

use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering, fence};

use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError, HLOCAL, INVALID_HANDLE_VALUE, LocalFree};
use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_KERNEL_OBJECT};
use windows_sys::Win32::Security::{
    IsWellKnownSid, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES, WinLocalSystemSid,
};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_READ, FILE_MAP_WRITE, MEMORY_BASIC_INFORMATION, MEMORY_MAPPED_VIEW_ADDRESS,
    MapViewOfFile, OpenFileMappingW, PAGE_READWRITE, UnmapViewOfFile, VirtualQuery,
};

use crate::buffer::wide;
use crate::cpu_temp::CpuTemps;
use crate::service_host::SecurityDescriptor;
use crate::{Error, Result};

pub const SECTION_NAME: &str = r"Global\Ferrotop.Live";
/// Owned by SYSTEM (stated outright: a SYSTEM process's objects can otherwise default to being
/// owned by Administrators, and readers check the owner); SYSTEM and Administrators get full
/// access, interactive users may only read.
const SECTION_SDDL: &str = "O:SYD:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GR;;;IU)";
/// The same access without the owner, which only SYSTEM may set: for tests.
#[cfg(test)]
const TEST_SECTION_SDDL: &str = "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GR;;;IU)";
const MAGIC: u32 = u32::from_le_bytes(*b"FTLV");
const VERSION: u32 = 1;
pub const MAX_PROCESSES: usize = 8192;
const MAX_CCDS: usize = 16;
const MAX_CORES: usize = 128;

// Layout, in 32-bit words.
const W_MAGIC: usize = 0;
const W_VERSION: usize = 1;
/// A u64 at words 2 and 3: odd while being written.
const W_SEQUENCE: usize = 2;
/// A u64 at words 4 and 5: when it was last written, in Unix milliseconds.
const W_UPDATED: usize = 4;
const W_PROCESS_COUNT: usize = 6;
const W_FLAGS: usize = 7;
const W_PACKAGE: usize = 8;
const W_DIE: usize = 9;
const W_CCD_COUNT: usize = 10;
const W_CORE_COUNT: usize = 11;
const W_CCD_INDEX: usize = 12;
const W_CCD_TEMP: usize = W_CCD_INDEX + MAX_CCDS;
const W_CORES: usize = W_CCD_TEMP + MAX_CCDS;
const W_PROCESSES: usize = W_CORES + MAX_CORES;
/// pid, disk read, disk write, sent, received (rates as f32 bits).
const ENTRY_WORDS: usize = 5;
const TOTAL_WORDS: usize = W_PROCESSES + MAX_PROCESSES * ENTRY_WORDS;
const TOTAL_BYTES: usize = TOTAL_WORDS * 4;
const FLAG_TEMPS: u32 = 1;
// The sequence must be 8-byte aligned, and the whole section stays small.
const _: () = assert!(W_SEQUENCE.is_multiple_of(2) && TOTAL_BYTES < 256 * 1024);

/// One process's rates, in bytes per second.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct LiveProcess {
    pub pid: u32,
    pub disk_read: f32,
    pub disk_write: f32,
    pub sent: f32,
    pub received: f32,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct LiveSnapshot {
    pub updated_unix_ms: u64,
    pub temps: Option<CpuTemps>,
    pub processes: Vec<LiveProcess>,
}

/// A mapped view of the section, unmapped on drop.
#[derive(Debug)]
struct View {
    base: *mut u32,
}

impl Drop for View {
    fn drop(&mut self) {
        // SAFETY: `base` is the address MapViewOfFile returned, unmapped once.
        unsafe { UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: self.base.cast() }) };
    }
}

impl View {
    fn map(mapping: &OwnedHandle, access: u32) -> Result<Self> {
        // SAFETY: `mapping` is a live section handle; mapping its whole size.
        let view = unsafe { MapViewOfFile(mapping.as_raw_handle(), access, 0, 0, 0) };
        if view.Value.is_null() {
            return Err(Error::last_win32("MapViewOfFile"));
        }
        let view = Self { base: view.Value.cast() };
        // A section made by something else could be smaller than ours.
        let mut info = MEMORY_BASIC_INFORMATION::default();
        // SAFETY: `info` is a valid out buffer of the size passed.
        let filled = unsafe { VirtualQuery(view.base.cast(), &mut info, size_of::<MEMORY_BASIC_INFORMATION>()) };
        if filled == 0 || info.RegionSize < TOTAL_BYTES {
            return Err(Error::Other { message: "the live-data section is too small" });
        }
        Ok(view)
    }

    fn word(&self, index: usize) -> &AtomicU32 {
        debug_assert!(index < TOTAL_WORDS);
        let index = index.min(TOTAL_WORDS - 1);
        // SAFETY: the view is at least TOTAL_BYTES long (checked in `map`), so the word is inside.
        let at = unsafe { self.base.add(index) };
        // SAFETY: page-aligned base, so every word is 4-byte aligned; all access to the shared
        // memory goes through atomics, and the view outlives the reference.
        unsafe { AtomicU32::from_ptr(at) }
    }

    fn sequence(&self) -> &AtomicU64 {
        // SAFETY: as in `word`; W_SEQUENCE is even, so the u64 is 8-byte aligned.
        let at = unsafe { self.base.add(W_SEQUENCE) };
        // SAFETY: as in `word`.
        unsafe { AtomicU64::from_ptr(at.cast()) }
    }
}

/// The service's side: creates the section and writes to it.
#[derive(Debug)]
pub struct LivePublisher {
    view: View,
    _mapping: OwnedHandle,
}

impl LivePublisher {
    /// Creates the section. Needs `SeCreateGlobalPrivilege` (services have it).
    pub fn create() -> Result<Self> {
        Self::create_named(SECTION_NAME, SECTION_SDDL)
    }

    fn create_named(section: &str, sddl: &str) -> Result<Self> {
        let descriptor = SecurityDescriptor::from_sddl(sddl)?;
        let attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: 0,
        };
        let name = wide(section);
        // SAFETY: a pagefile-backed section of TOTAL_BYTES with a live descriptor and a
        // NUL-terminated name.
        let raw = unsafe {
            CreateFileMappingW(INVALID_HANDLE_VALUE, &attributes, PAGE_READWRITE, 0, TOTAL_BYTES as u32, name.as_ptr())
        };
        if raw.is_null() {
            return Err(Error::last_win32("CreateFileMappingW"));
        }
        // SAFETY: no preconditions; reads the error the successful call set.
        let existed = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
        // SAFETY: `raw` is a section handle we own.
        let mapping = unsafe { OwnedHandle::from_raw_handle(raw) };
        // Left by an earlier run of the service (a window still had it open) is fine; one
        // anybody else made is not ours to write.
        if existed && !owned_by_system(&mapping) {
            return Err(Error::Other { message: "another program owns the live-data section" });
        }
        let view = View::map(&mapping, FILE_MAP_WRITE | FILE_MAP_READ)?;
        view.word(W_MAGIC).store(MAGIC, Ordering::Relaxed);
        view.word(W_VERSION).store(VERSION, Ordering::Relaxed);
        Ok(Self { view, _mapping: mapping })
    }

    pub fn publish(&self, snapshot: &LiveSnapshot) {
        let view = &self.view;
        let sequence = view.sequence().load(Ordering::Relaxed);
        // Odd: a reader that sees this, or sees it change, discards what it read.
        view.sequence().store(sequence | 1, Ordering::Relaxed);
        fence(Ordering::Release);
        let set = |index: usize, value: u32| view.word(index).store(value, Ordering::Relaxed);
        set(W_UPDATED, snapshot.updated_unix_ms as u32);
        set(W_UPDATED + 1, (snapshot.updated_unix_ms >> 32) as u32);
        let temps = snapshot.temps.as_ref();
        set(W_FLAGS, if temps.is_some() { FLAG_TEMPS } else { 0 });
        let celsius = |value: Option<f32>| value.unwrap_or(f32::NAN).to_bits();
        set(W_PACKAGE, celsius(temps.and_then(|t| t.package_c)));
        set(W_DIE, celsius(temps.and_then(|t| t.die_c)));
        let ccds = temps.map_or(&[][..], |t| &t.ccds[..]);
        let ccds = &ccds[..ccds.len().min(MAX_CCDS)];
        set(W_CCD_COUNT, ccds.len() as u32);
        for (i, &(index, value)) in ccds.iter().enumerate() {
            set(W_CCD_INDEX + i, index);
            set(W_CCD_TEMP + i, value.to_bits());
        }
        let cores = temps.map_or(&[][..], |t| &t.cores[..]);
        let cores = &cores[..cores.len().min(MAX_CORES)];
        set(W_CORE_COUNT, cores.len() as u32);
        for (i, value) in cores.iter().enumerate() {
            set(W_CORES + i, value.to_bits());
        }
        let processes = &snapshot.processes[..snapshot.processes.len().min(MAX_PROCESSES)];
        set(W_PROCESS_COUNT, processes.len() as u32);
        for (i, p) in processes.iter().enumerate() {
            let at = W_PROCESSES + i * ENTRY_WORDS;
            set(at, p.pid);
            set(at + 1, p.disk_read.to_bits());
            set(at + 2, p.disk_write.to_bits());
            set(at + 3, p.sent.to_bits());
            set(at + 4, p.received.to_bits());
        }
        view.sequence().store((sequence | 1).wrapping_add(1), Ordering::Release);
    }
}

/// Whether a kernel object belongs to LocalSystem.
fn owned_by_system(object: &OwnedHandle) -> bool {
    let mut owner: PSID = std::ptr::null_mut();
    let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: `object` is live and the out pointers valid; only the owner is asked for.
    let status = unsafe {
        GetSecurityInfo(
            object.as_raw_handle(),
            SE_KERNEL_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    // SAFETY: `owner` points into `descriptor`, still allocated.
    let system = status == 0 && !owner.is_null() && unsafe { IsWellKnownSid(owner, WinLocalSystemSid) } != 0;
    // SAFETY: frees the descriptor GetSecurityInfo allocated (null is allowed).
    unsafe { LocalFree(descriptor as HLOCAL) };
    system
}

/// A window's side: reads what the service publishes.
#[derive(Debug)]
pub struct LiveReader {
    view: View,
    _mapping: OwnedHandle,
}

impl LiveReader {
    /// Opens the section; fails when the service isn't running.
    pub fn open() -> Result<Self> {
        Self::open_named(SECTION_NAME, true)
    }

    fn open_named(section: &str, require_system: bool) -> Result<Self> {
        let name = wide(section);
        // SAFETY: the name is NUL-terminated; read access only.
        let raw = unsafe { OpenFileMappingW(FILE_MAP_READ, 0, name.as_ptr()) };
        if raw.is_null() {
            return Err(Error::last_win32("OpenFileMappingW"));
        }
        // SAFETY: `raw` is a section handle we own.
        let mapping = unsafe { OwnedHandle::from_raw_handle(raw) };
        // Only trust a section the service (LocalSystem) made.
        if require_system && !owned_by_system(&mapping) {
            return Err(Error::Other { message: "the live-data section isn't the service's" });
        }
        let view = View::map(&mapping, FILE_MAP_READ)?;
        Ok(Self { view, _mapping: mapping })
    }

    /// A consistent copy of the latest figures, or `None` if the service never wrote any or
    /// kept writing while this tried to read.
    pub fn read(&self) -> Option<LiveSnapshot> {
        let view = &self.view;
        if view.word(W_MAGIC).load(Ordering::Relaxed) != MAGIC
            || view.word(W_VERSION).load(Ordering::Relaxed) != VERSION
        {
            return None;
        }
        for _ in 0..8 {
            let before = view.sequence().load(Ordering::Acquire);
            if before % 2 == 1 {
                std::thread::yield_now();
                continue;
            }
            let words: Vec<u32> = (0..TOTAL_WORDS).map(|i| view.word(i).load(Ordering::Relaxed)).collect();
            fence(Ordering::Acquire);
            if before != 0 && view.sequence().load(Ordering::Relaxed) == before {
                return Some(parse(&words));
            }
        }
        None
    }
}

/// Decodes a copy of the section, keeping only sane values.
pub(crate) fn parse(words: &[u32]) -> LiveSnapshot {
    let at = |index: usize| words.get(index).copied().unwrap_or(0);
    let celsius =
        |index: usize| Some(f32::from_bits(at(index))).filter(|t| t.is_finite() && (-50.0..=200.0).contains(t));
    let rate = |index: usize| Some(f32::from_bits(at(index))).filter(|r| r.is_finite() && *r >= 0.0).unwrap_or(0.0);
    let temps = (at(W_FLAGS) & FLAG_TEMPS != 0).then(|| CpuTemps {
        package_c: celsius(W_PACKAGE),
        die_c: celsius(W_DIE),
        ccds: (0..(at(W_CCD_COUNT) as usize).min(MAX_CCDS))
            .filter_map(|i| Some((at(W_CCD_INDEX + i), celsius(W_CCD_TEMP + i)?)))
            .collect(),
        cores: (0..(at(W_CORE_COUNT) as usize).min(MAX_CORES)).filter_map(|i| celsius(W_CORES + i)).collect(),
    });
    let processes = (0..(at(W_PROCESS_COUNT) as usize).min(MAX_PROCESSES))
        .map(|i| {
            let base = W_PROCESSES + i * ENTRY_WORDS;
            LiveProcess {
                pid: at(base),
                disk_read: rate(base + 1),
                disk_write: rate(base + 2),
                sent: rate(base + 3),
                received: rate(base + 4),
            }
        })
        .collect();
    LiveSnapshot { updated_unix_ms: u64::from(at(W_UPDATED)) | (u64::from(at(W_UPDATED + 1)) << 32), temps, processes }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_sanitizes_a_snapshot() {
        let mut words = vec![0u32; TOTAL_WORDS];
        words[W_UPDATED] = 5;
        words[W_UPDATED + 1] = 1;
        words[W_FLAGS] = FLAG_TEMPS;
        words[W_PACKAGE] = 61.5f32.to_bits();
        words[W_DIE] = f32::NAN.to_bits();
        words[W_CCD_COUNT] = 2;
        words[W_CCD_INDEX] = 0;
        words[W_CCD_TEMP] = 55.0f32.to_bits();
        words[W_CCD_INDEX + 1] = 2;
        words[W_CCD_TEMP + 1] = 9999.0f32.to_bits();
        words[W_CORE_COUNT] = u32::MAX;
        words[W_PROCESS_COUNT] = 2;
        let first = W_PROCESSES;
        words[first] = 42;
        words[first + 1] = 1024.0f32.to_bits();
        words[first + 3] = (-1.0f32).to_bits();
        words[first + ENTRY_WORDS] = 7;
        let snapshot = parse(&words);
        assert_eq!(snapshot.updated_unix_ms, (1 << 32) | 5);
        let temps = snapshot.temps.expect("temps");
        assert_eq!((temps.package_c, temps.die_c), (Some(61.5), None));
        assert_eq!(temps.ccds, [(0, 55.0)], "an absurd reading is dropped");
        assert!(temps.cores.iter().all(|t| *t == 0.0) && temps.cores.len() == MAX_CORES, "count clamped");
        assert_eq!(snapshot.processes.len(), 2);
        assert_eq!(snapshot.processes[0], LiveProcess { pid: 42, disk_read: 1024.0, ..LiveProcess::default() });
        assert_eq!(snapshot.processes[1].pid, 7);
        // Garbage and short copies parse without panicking.
        assert_eq!(parse(&[]).processes.len(), 0);
        let mut huge = vec![u32::MAX; 64];
        huge[W_FLAGS] = 0;
        assert!(parse(&huge).temps.is_none());
    }

    #[test]
    fn a_reader_sees_what_was_published() {
        // A session-local name needs no privilege, unlike the service's global one.
        let name = format!(r"Local\Ferrotop.LiveTest.{}", std::process::id());
        let publisher = LivePublisher::create_named(&name, TEST_SECTION_SDDL).expect("create");
        let reader = LiveReader::open_named(&name, false).expect("open");
        assert_eq!(reader.read(), None, "nothing published yet");
        let snapshot = LiveSnapshot {
            updated_unix_ms: 1_700_000_000_123,
            temps: Some(CpuTemps {
                package_c: Some(48.5),
                die_c: None,
                ccds: vec![(1, 44.0)],
                cores: vec![40.0, 41.5],
            }),
            processes: vec![
                LiveProcess { pid: 4, disk_read: 10.0, disk_write: 20.0, sent: 0.0, received: 0.0 },
                LiveProcess { pid: 99, disk_read: 0.0, disk_write: 0.0, sent: 1500.0, received: 64.0 },
            ],
        };
        publisher.publish(&snapshot);
        assert_eq!(reader.read(), Some(snapshot.clone()));
        let shorter = LiveSnapshot { temps: None, processes: vec![snapshot.processes[1]], ..snapshot };
        publisher.publish(&shorter);
        assert_eq!(reader.read(), Some(shorter));
        // The service's section is LocalSystem's; this test's isn't, so the real check refuses it.
        assert!(LiveReader::open_named(&name, true).is_err());
    }

    #[test]
    fn opening_without_the_service_fails_cleanly() {
        // Unless the service is installed and running here, there's nothing to open.
        if let Ok(reader) = LiveReader::open() {
            let _ = reader.read();
        }
    }
}
