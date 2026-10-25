//! Machine facts for the Performance view: CPU model and topology, memory modules (from the
//! SMBIOS firmware table), and the kernel's memory counters.

use std::time::Duration;

use windows_sys::Win32::System::ProcessStatus::{GetPerformanceInfo, PERFORMANCE_INFORMATION};
use windows_sys::Win32::System::SystemInformation::{
    GetLogicalProcessorInformationEx, GetPhysicallyInstalledSystemMemory, GetSystemFirmwareTable, GetTickCount64,
    RelationAll,
};
use windows_sys::Win32::System::Threading::{GetActiveProcessorCount, GetActiveProcessorGroupCount};

use crate::buffer::AlignedBuffer;
use crate::{Error, Result};

pub(crate) const CPU_KEY: &str = r"HARDWARE\DESCRIPTION\System\CentralProcessor\0";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CpuInfo {
    pub name: String,
    pub base_mhz: Option<u32>,
    pub sockets: u32,
    pub cores: u32,
    pub logical: u32,
    /// Total cache per level, in bytes.
    pub l1_cache: u64,
    pub l2_cache: u64,
    pub l3_cache: u64,
    /// For each physical core, its logical processors as indexes into
    /// [`crate::system::processor_times`] (e.g. `[[0, 1], [2, 3], ...]` with SMT).
    pub core_threads: Vec<Vec<usize>>,
    /// Logical processors sharing each L3 cache. On AMD Zen that's a CCX (on Zen 3 and later,
    /// one CCD); on most Intel desktop chips, the whole package.
    pub l3_groups: Vec<Vec<usize>>,
    /// The CCD each L3 group belongs to, as an opaque key, where the CPU says (see [`l3_ccds`]);
    /// otherwise empty.
    pub l3_ccds: Vec<u32>,
}

pub fn cpu_info() -> CpuInfo {
    let mut info = CpuInfo {
        name: crate::system::registry_string(CPU_KEY, "ProcessorNameString").unwrap_or_default().trim().to_owned(),
        base_mhz: crate::system::registry_dword(CPU_KEY, "~MHz"),
        ..CpuInfo::default()
    };
    if let Ok(records) = logical_processor_records() {
        parse_topology(&records, &group_bases(), &mut info);
    }
    info.l3_ccds = l3_ccds(&info.l3_groups);
    info
}

/// The CCD holding each L3 group (CCX) on AMD family 17h (Zen, Zen+ and Zen 2), whose CCDs (dies,
/// on Zen and Zen+) hold up to two CCXs; empty for other CPUs. Windows doesn't report CCDs, but
/// these chips number every logical processor's APIC ID the same way: thread in bit 0, core in
/// bits 1–2, CCX in bit 3, then the CCD and socket. So a CCX's CCD is the APIC ID of one of its
/// threads, read with CPUID while pinned to it, shifted right by 4. From Zen 3 on a CCD is
/// usually a single CCX, which the L3 groups already show.
#[cfg(target_arch = "x86_64")]
fn l3_ccds(l3_groups: &[Vec<usize>]) -> Vec<u32> {
    use std::arch::x86_64::{__cpuid, __get_cpuid_max};

    use crate::cpu_temp::{self, Vendor};

    const EXTENDED_FEATURES: u32 = 0x8000_0001;
    const TOPOLOGY_EXTENSIONS: u32 = 1 << 22;
    const EXTENDED_APIC_ID: u32 = 0x8000_001E;
    let Some(id) = cpu_temp::cpu_id() else { return Vec::new() };
    if id.vendor != Vendor::Amd || id.family != 0x17 || __get_cpuid_max(0x8000_0000).0 < EXTENDED_APIC_ID {
        return Vec::new();
    }
    // Without topology extensions, leaf 0x8000_001E is reserved.
    if __cpuid(EXTENDED_FEATURES).ecx & TOPOLOGY_EXTENSIONS == 0 {
        return Vec::new();
    }
    let bases = group_bases();
    let Some(cpus) =
        l3_groups.iter().map(|threads| cpu_temp::group_mask(&bases, *threads.first()?)).collect::<Option<Vec<_>>>()
    else {
        return Vec::new();
    };
    let apic_ids = cpu_temp::pinned_reads(&cpus, || Some(__cpuid(EXTENDED_APIC_ID).eax));
    apic_ids.into_iter().map(|apic| apic.map(|id| id >> 4)).collect::<Option<Vec<_>>>().unwrap_or_default()
}

#[cfg(not(target_arch = "x86_64"))]
fn l3_ccds(_: &[Vec<usize>]) -> Vec<u32> {
    Vec::new()
}

/// Where each processor group's processors start in `processor_times()` order, which lists
/// every group's active processors in turn.
pub(crate) fn group_bases() -> Vec<usize> {
    // SAFETY: no preconditions.
    let groups = unsafe { GetActiveProcessorGroupCount() };
    let mut bases = Vec::with_capacity(usize::from(groups));
    let mut next = 0;
    for group in 0..groups {
        bases.push(next);
        // SAFETY: no preconditions; returns 0 for a group that doesn't exist.
        next += unsafe { GetActiveProcessorCount(group) } as usize;
    }
    bases
}

pub(crate) fn logical_processor_records() -> Result<Vec<u8>> {
    let mut len = 0u32;
    // SAFETY: a null buffer with zero length asks for the required size.
    unsafe { GetLogicalProcessorInformationEx(RelationAll, std::ptr::null_mut(), &mut len) };
    let mut buffer = AlignedBuffer::new(len as usize);
    let mut len = buffer.capacity_u32();
    // SAFETY: the buffer is writable for `len` bytes and `len` is a valid in/out pointer.
    if unsafe { GetLogicalProcessorInformationEx(RelationAll, buffer.as_mut_ptr().cast(), &mut len) } == 0 {
        return Err(Error::last_win32("GetLogicalProcessorInformationEx"));
    }
    Ok(buffer.bytes(len as usize).to_vec())
}

/// Walks `SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX` records (`Relationship`, `Size`, payload).
/// `group_bases[g]` is the index of processor group `g`'s first logical processor.
pub(crate) fn parse_topology(records: &[u8], group_bases: &[usize], info: &mut CpuInfo) {
    const CORE: u32 = 0;
    const CACHE: u32 = 2;
    const PACKAGE: u32 = 3;
    let u16_at = |offset: usize| records.get(offset..offset + 2).map(|b| u16::from_le_bytes([b[0], b[1]]));
    let u32_at = |offset: usize| records.get(offset..offset + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    let u64_at =
        |offset: usize| records.get(offset..offset + 8).map(|b| u64::from_le_bytes(b.try_into().unwrap_or_default()));
    // GROUP_AFFINITY entries (Mask u64, Group u16, padding; 16 bytes each) to logical processors.
    let affinity_threads = |start: usize, entries: usize| {
        let mut threads = Vec::new();
        for entry in 0..entries {
            let at = start + entry * 16;
            let (Some(mask), Some(group)) = (u64_at(at), u16_at(at + 8)) else { break };
            let Some(&base) = group_bases.get(usize::from(group)) else { continue };
            threads.extend((0..64).filter(|bit| mask & (1u64 << bit) != 0).map(|bit| base + bit));
        }
        threads
    };
    let mut offset = 0;
    while let (Some(relationship), Some(size)) = (u32_at(offset), u32_at(offset + 4)) {
        match relationship {
            CORE => {
                info.cores += 1;
                // PROCESSOR_RELATIONSHIP: GroupCount (u16) at +30, GROUP_AFFINITY entries from +32.
                info.core_threads.push(affinity_threads(offset + 32, usize::from(u16_at(offset + 30).unwrap_or(0))));
            }
            PACKAGE => info.sockets += 1,
            CACHE => {
                // CACHE_RELATIONSHIP: Level (u8) at +8, CacheSize (u32) at +12, GroupCount (u16;
                // zero on older Windows, meaning one) at +38, GROUP_AFFINITY entries from +40.
                if let (Some(&level), Some(bytes)) = (records.get(offset + 8), u32_at(offset + 12)) {
                    match level {
                        1 => info.l1_cache += u64::from(bytes),
                        2 => info.l2_cache += u64::from(bytes),
                        3 => {
                            info.l3_cache += u64::from(bytes);
                            let entries = usize::from(u16_at(offset + 38).unwrap_or(0).max(1));
                            info.l3_groups.push(affinity_threads(offset + 40, entries));
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        if size == 0 {
            break;
        }
        offset += size as usize;
    }
    info.logical = std::thread::available_parallelism().map_or(0, |n| n.get() as u32);
}

/// The kernel's memory counters, in bytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MemoryCounters {
    pub physical_total: u64,
    pub physical_available: u64,
    pub commit_total: u64,
    pub commit_limit: u64,
    pub system_cache: u64,
    pub paged_pool: u64,
    pub nonpaged_pool: u64,
}

pub fn memory_counters() -> Result<MemoryCounters> {
    let mut info = PERFORMANCE_INFORMATION {
        cb: size_of::<PERFORMANCE_INFORMATION>() as u32,
        CommitTotal: 0,
        CommitLimit: 0,
        CommitPeak: 0,
        PhysicalTotal: 0,
        PhysicalAvailable: 0,
        SystemCache: 0,
        KernelTotal: 0,
        KernelPaged: 0,
        KernelNonpaged: 0,
        PageSize: 0,
        HandleCount: 0,
        ProcessCount: 0,
        ThreadCount: 0,
    };
    // SAFETY: `info` is a writable PERFORMANCE_INFORMATION of the size passed.
    if unsafe { GetPerformanceInfo(&mut info, info.cb) } == 0 {
        return Err(Error::last_win32("GetPerformanceInfo"));
    }
    let pages = |count: usize| count as u64 * info.PageSize as u64;
    Ok(MemoryCounters {
        physical_total: pages(info.PhysicalTotal),
        physical_available: pages(info.PhysicalAvailable),
        commit_total: pages(info.CommitTotal),
        commit_limit: pages(info.CommitLimit),
        system_cache: pages(info.SystemCache),
        paged_pool: pages(info.KernelPaged),
        nonpaged_pool: pages(info.KernelNonpaged),
    })
}

/// RAM installed in the machine, including what firmware and devices reserve.
pub fn installed_memory() -> Option<u64> {
    let mut kilobytes = 0u64;
    // SAFETY: `kilobytes` is a valid out-pointer.
    (unsafe { GetPhysicallyInstalledSystemMemory(&mut kilobytes) } != 0).then_some(kilobytes * 1024)
}

pub fn uptime() -> Duration {
    // SAFETY: no preconditions.
    Duration::from_millis(unsafe { GetTickCount64() })
}

/// One populated or empty memory slot (SMBIOS type 17).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemorySlot {
    pub locator: String,
    /// Bytes; 0 for an empty slot.
    pub size: u64,
    /// Rated speed, in MT/s.
    pub speed: Option<u16>,
    /// Speed the module is actually running at, in MT/s.
    pub configured_speed: Option<u16>,
    pub form_factor: Option<&'static str>,
    /// e.g. `DDR4`.
    pub memory_type: Option<&'static str>,
}

/// Memory slots from the SMBIOS table.
pub fn memory_slots() -> Result<Vec<MemorySlot>> {
    let provider = u32::from_be_bytes(*b"RSMB");
    // SAFETY: a null buffer with zero size asks for the table size.
    let size = unsafe { GetSystemFirmwareTable(provider, 0, std::ptr::null_mut(), 0) };
    if size == 0 {
        return Err(Error::last_win32("GetSystemFirmwareTable"));
    }
    let mut table = vec![0u8; size as usize];
    // SAFETY: `table` is writable for `size` bytes.
    let written = unsafe { GetSystemFirmwareTable(provider, 0, table.as_mut_ptr(), size) };
    table.truncate((written as usize).min(table.len()));
    Ok(parse_memory_slots(&table))
}

/// Parses `RawSMBIOSData` (8-byte header, then structures) for type 17 Memory Device entries.
pub(crate) fn parse_memory_slots(raw: &[u8]) -> Vec<MemorySlot> {
    let Some(length) = raw.get(4..8).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize) else {
        return Vec::new();
    };
    let Some(data) = raw.get(8..8 + length.min(raw.len().saturating_sub(8))) else { return Vec::new() };
    let mut slots = Vec::new();
    let mut offset = 0;
    while let (Some(&kind), Some(&header_len)) = (data.get(offset), data.get(offset + 1)) {
        let header_len = usize::from(header_len);
        if kind == 127 || header_len < 4 {
            break;
        }
        let Some(formatted) = data.get(offset..offset + header_len) else { break };
        // Strings follow the formatted area, each NUL-terminated, ending with an extra NUL.
        let strings_start = offset + header_len;
        let Some(strings_len) = data.get(strings_start..).and_then(|tail| tail.windows(2).position(|w| w == [0, 0]))
        else {
            break;
        };
        let strings: Vec<&[u8]> = data[strings_start..strings_start + strings_len].split(|&b| b == 0).collect();
        if kind == 17 {
            let byte = |at: usize| formatted.get(at).copied();
            let word = |at: usize| formatted.get(at..at + 2).map(|b| u16::from_le_bytes([b[0], b[1]]));
            let dword = |at: usize| formatted.get(at..at + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
            let size = match word(0x0C) {
                None | Some(0) | Some(0xFFFF) => 0,
                Some(0x7FFF) => u64::from(dword(0x1C).unwrap_or(0) & 0x7FFF_FFFF) << 20,
                Some(kb) if kb & 0x8000 != 0 => u64::from(kb & 0x7FFF) << 10,
                Some(mb) => u64::from(mb) << 20,
            };
            let locator = byte(0x10)
                .filter(|&i| i > 0)
                .and_then(|i| strings.get(usize::from(i) - 1))
                .map(|s| String::from_utf8_lossy(s).trim().to_owned())
                .unwrap_or_default();
            let speed = |at: usize| word(at).filter(|&s| s != 0 && s != 0xFFFF);
            slots.push(MemorySlot {
                locator,
                size,
                speed: speed(0x15),
                configured_speed: speed(0x20),
                form_factor: byte(0x0E).and_then(form_factor),
                memory_type: byte(0x12).and_then(memory_type),
            });
        }
        offset = strings_start + strings_len + 2;
    }
    slots
}

fn memory_type(code: u8) -> Option<&'static str> {
    Some(match code {
        0x18 => "DDR3",
        0x1A => "DDR4",
        0x1B => "LPDDR",
        0x1C => "LPDDR2",
        0x1D => "LPDDR3",
        0x1E => "LPDDR4",
        0x22 => "DDR5",
        0x23 => "LPDDR5",
        _ => return None,
    })
}

fn form_factor(code: u8) -> Option<&'static str> {
    Some(match code {
        0x09 => "DIMM",
        0x0D => "SODIMM",
        0x0F => "FB-DIMM",
        0x0B => "Row of chips",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_cpu_and_memory_facts() {
        let cpu = cpu_info();
        assert!(!cpu.name.is_empty() && cpu.cores > 0 && cpu.sockets > 0, "{cpu:?}");
        // Every logical processor belongs to exactly one core.
        let mut threads: Vec<usize> = cpu.core_threads.iter().flatten().copied().collect();
        threads.sort_unstable();
        assert_eq!(cpu.core_threads.len(), cpu.cores as usize);
        assert_eq!(threads, (0..cpu.logical as usize).collect::<Vec<_>>());
        // L3 groups cover every logical processor too.
        let mut shared: Vec<usize> = cpu.l3_groups.iter().flatten().copied().collect();
        shared.sort_unstable();
        assert_eq!(shared, (0..cpu.logical as usize).collect::<Vec<_>>(), "{:?}", cpu.l3_groups);
        // CCDs, where the CPU reports them, are one per L3 group.
        assert!(cpu.l3_ccds.is_empty() || cpu.l3_ccds.len() == cpu.l3_groups.len(), "{:?}", cpu.l3_ccds);
        let memory = memory_counters().expect("memory counters");
        assert!(memory.physical_total > 0 && memory.commit_total <= memory.commit_limit);
        assert!(installed_memory().unwrap_or(0) >= memory.physical_total);
        assert!(uptime() > Duration::ZERO);
    }

    #[test]
    fn parses_an_smbios_memory_device() {
        // Header: version bytes, then table length.
        let mut table = vec![0, 3, 4, 0];
        let mut data = vec![17u8, 0x22, 0x40, 0x00];
        data.resize(0x22, 0);
        // 64 GB doesn't fit the 15-bit MB field, so it uses the extended size (in MB).
        data[0x0C..0x0E].copy_from_slice(&0x7FFFu16.to_le_bytes());
        data[0x1C..0x20].copy_from_slice(&65536u32.to_le_bytes());
        data[0x0E] = 0x09; // DIMM
        data[0x10] = 1; // locator: string 1
        data[0x12] = 0x22; // DDR5
        data[0x15..0x17].copy_from_slice(&5600u16.to_le_bytes());
        data[0x20..0x22].copy_from_slice(&4800u16.to_le_bytes());
        data.extend_from_slice(b"DIMM_A1\0\0");
        data.extend_from_slice(&[127, 4, 0, 0, 0, 0]);
        table.extend_from_slice(&(data.len() as u32).to_le_bytes());
        table.extend_from_slice(&data);
        let slots = parse_memory_slots(&table);
        assert_eq!(
            slots,
            [MemorySlot {
                locator: "DIMM_A1".into(),
                size: 64 << 30,
                speed: Some(5600),
                configured_speed: Some(4800),
                form_factor: Some("DIMM"),
                memory_type: Some("DDR5"),
            }]
        );
        assert!(parse_memory_slots(&[1, 2, 3]).is_empty(), "truncated table is ignored");
    }
}
