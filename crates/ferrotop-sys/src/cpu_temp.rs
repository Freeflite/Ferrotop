//! CPU temperatures through PawnIO: AMD Zen's SMN thermal registers and Intel's thermal MSRs.
//!
//! Neither vendor exposes these to user mode; both need ring 0, which PawnIO provides when the
//! user has installed it (see [`crate::pawnio`]). The register layouts follow the Linux
//! `k10temp` (AMD) and `coretemp` (Intel) drivers.

use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

use windows_sys::Win32::Foundation::{WAIT_ABANDONED, WAIT_OBJECT_0};
use windows_sys::Win32::System::SystemInformation::GROUP_AFFINITY;
use windows_sys::Win32::System::Threading::{
    CreateMutexW, GetCurrentThread, OpenMutexW, ReleaseMutex, SetThreadGroupAffinity, WaitForSingleObject,
};

use crate::Error;
use crate::buffer::wide;
use crate::pawnio::{self, PawnIo};

/// AMD's reported-temperature control register (Tctl) in SMN space.
const ZEN_TEMP_CTRL: u32 = 0x0005_9800;
const MSR_IA32_THERM_STATUS: u64 = 0x19C;
const MSR_IA32_TEMPERATURE_TARGET: u64 = 0x1A2;
const MSR_IA32_PACKAGE_THERM_STATUS: u64 = 0x1B1;
const ERROR_NOT_SUPPORTED: u32 = 50;
/// How long to wait for another monitoring tool to finish with the PCI index/data registers.
const PCI_LOCK_TIMEOUT_MS: u32 = 25;

/// One reading of the CPU's temperature sensors, in degrees Celsius.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CpuTemps {
    /// AMD Tctl (the control temperature fans follow) or the Intel package temperature.
    pub package_c: Option<f32>,
    /// AMD Tdie, on the few older parts where Tctl carries a fixed offset.
    pub die_c: Option<f32>,
    /// AMD per-CCD temperatures as `(CCD index, °C)`.
    pub ccds: Vec<(u32, f32)>,
    /// Intel per-core temperatures, in core order.
    pub cores: Vec<f32>,
}

/// Why PawnIO temperatures aren't available.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unavailable {
    /// The PawnIO driver isn't installed.
    NotInstalled,
    /// PawnIO is installed, but only Administrators may use it.
    NeedsAdmin,
    /// The CPU isn't one the bundled modules support.
    Unsupported,
    Failed(Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Vendor {
    Amd,
    Intel,
    Other,
}

/// The CPU's vendor and display family/model, as Windows reports them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpuId {
    pub vendor: Vendor,
    pub family: u32,
    pub model: u32,
}

pub fn cpu_id() -> Option<CpuId> {
    let key = crate::hardware::CPU_KEY;
    let vendor = crate::system::registry_string(key, "VendorIdentifier")?;
    let identifier = crate::system::registry_string(key, "Identifier")?;
    parse_identifier(&vendor, &identifier)
}

/// `AuthenticAMD`, `AMD64 Family 25 Model 33 Stepping 2` → AMD, family 0x19, model 0x21.
fn parse_identifier(vendor: &str, identifier: &str) -> Option<CpuId> {
    let vendor = match vendor.trim() {
        "AuthenticAMD" => Vendor::Amd,
        "GenuineIntel" => Vendor::Intel,
        _ => Vendor::Other,
    };
    let words: Vec<&str> = identifier.split_whitespace().collect();
    let after = |label: &str| words.iter().position(|w| *w == label).and_then(|i| words.get(i + 1)?.parse().ok());
    Some(CpuId { vendor, family: after("Family")?, model: after("Model")? })
}

/// Where AMD's per-CCD temperature registers sit, as (offset from Tctl, how many to probe).
fn amd_ccd_layout(family: u32, model: u32) -> Option<(u32, u32)> {
    match (family, model) {
        (0x17, 0x01 | 0x08 | 0x11 | 0x18) => Some((0x154, 4)),
        (0x17, 0x31 | 0x47 | 0x60 | 0x68 | 0x71) => Some((0x154, 8)),
        (0x17, 0xa0..=0xaf) => Some((0x300, 8)),
        (0x19, 0x00..=0x01 | 0x08 | 0x21 | 0x50..=0x5f) => Some((0x154, 8)),
        (0x19, 0x40..=0x4f) => Some((0x300, 8)),
        (0x19, 0x60..=0x7f) => Some((0x308, 8)),
        (0x19, 0x10..=0x1f | 0xa0..=0xaf) => Some((0x300, 12)),
        (0x1a, 0x00..=0x1f) => Some((0x1f0, 16)),
        (0x1a, 0x40..=0x4f) => Some((0x308, 8)),
        _ => None,
    }
}

/// The few first-generation parts whose Tctl reads higher than the die (Tdie = Tctl − offset).
fn amd_tdie_offset(family: u32, name: &str) -> Option<f32> {
    const TABLE: [(&str, f32); 6] = [
        ("AMD Ryzen 5 1600X", 20.0),
        ("AMD Ryzen 7 1700X", 20.0),
        ("AMD Ryzen 7 1800X", 20.0),
        ("AMD Ryzen 7 2700X", 10.0),
        ("AMD Ryzen Threadripper 19", 27.0),
        ("AMD Ryzen Threadripper 29", 27.0),
    ];
    (family == 0x17).then(|| TABLE.iter().find(|(prefix, _)| name.contains(prefix)).map(|(_, offset)| *offset))?
}

/// Tctl: bits 31:21 in eighths of a degree, minus 49 °C when the extended range is selected.
fn decode_tctl(register: u32) -> f32 {
    let mut celsius = (register >> 21) as f32 * 0.125;
    if register & (1 << 19) != 0 || register & 0x3_0000 == 0x3_0000 {
        celsius -= 49.0;
    }
    celsius
}

/// A CCD register: bits 10:0 in eighths of a degree from −49 °C, valid when bit 11 is set.
/// Absent CCDs read 0 or all ones.
fn decode_ccd(register: u32) -> Option<f32> {
    (register != u32::MAX && register & (1 << 11) != 0).then_some((register & 0x7FF) as f32 * 0.125 - 49.0)
}

/// TjMax from `MSR_TEMPERATURE_TARGET` bits 23:16; 100 °C if it reads as nonsense.
fn decode_tjmax(target: u64) -> f32 {
    let tjmax = (target >> 16) & 0xFF;
    if (50..=150).contains(&tjmax) { tjmax as f32 } else { 100.0 }
}

/// An Intel thermal status MSR: degrees below TjMax in bits 22:16, valid when bit 31 is set.
fn decode_intel(status: u64, tjmax: f32) -> Option<f32> {
    (status & (1 << 31) != 0).then_some(tjmax - ((status >> 16) & 0x7F) as f32)
}

/// A named mutex shared with other monitoring tools.
#[derive(Debug)]
struct SharedMutex(OwnedHandle);

impl SharedMutex {
    fn open(name: &str) -> Option<Self> {
        let name = wide(name);
        // SAFETY: `name` is NUL-terminated and null attributes are allowed. This opens the mutex
        // if another tool already created it.
        let mut raw = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
        if raw.is_null() {
            const SYNCHRONIZE: u32 = 0x0010_0000;
            const MUTEX_MODIFY_STATE: u32 = 0x0001;
            // SAFETY: as above; this asks for just enough access to wait on and release it.
            raw = unsafe { OpenMutexW(SYNCHRONIZE | MUTEX_MODIFY_STATE, 0, name.as_ptr()) };
        }
        // SAFETY: a non-null result is a mutex handle we own.
        (!raw.is_null()).then(|| Self(unsafe { OwnedHandle::from_raw_handle(raw) }))
    }

    fn lock(&self, timeout_ms: u32) -> Option<SharedMutexGuard<'_>> {
        // SAFETY: the handle is a live mutex.
        let wait = unsafe { WaitForSingleObject(self.0.as_raw_handle(), timeout_ms) };
        // An abandoned mutex is still ours; its previous owner just exited without releasing it.
        matches!(wait, WAIT_OBJECT_0 | WAIT_ABANDONED).then_some(SharedMutexGuard(self))
    }
}

#[derive(Debug)]
struct SharedMutexGuard<'a>(&'a SharedMutex);

impl Drop for SharedMutexGuard<'_> {
    fn drop(&mut self) {
        // SAFETY: this thread owns the mutex (the wait succeeded) and releases it once.
        unsafe { ReleaseMutex(self.0.0.as_raw_handle()) };
    }
}

#[derive(Debug)]
enum Sensor {
    Amd {
        tdie_offset: Option<f32>,
        ccd_base: u32,
        ccd_limit: u32,
        /// CCDs that report a temperature; found on the first read that gets the PCI lock.
        ccds: Option<Vec<u32>>,
        /// `Global\Access_PCI`, which tools touching PCI configuration space share so their SMN
        /// index/data accesses don't interleave.
        pci: Option<SharedMutex>,
    },
    Intel {
        tjmax: f32,
        /// One `(group, mask)` per physical core, selecting its first logical processor.
        cores: Vec<(u16, usize)>,
    },
}

/// CPU temperature sensors read through PawnIO.
#[derive(Debug)]
pub struct CpuThermal {
    device: PawnIo,
    sensor: Sensor,
}

impl CpuThermal {
    /// Opens PawnIO and loads the module for this CPU. `name` is the CPU brand string and
    /// `core_threads` the logical processors of each core (from [`crate::hardware::cpu_info`]).
    pub fn open(name: &str, core_threads: &[Vec<usize>]) -> Result<Self, Unavailable> {
        let id = cpu_id().ok_or(Unavailable::Unsupported)?;
        let module = match id.vendor {
            Vendor::Amd if (0x17..=0x1a).contains(&id.family) => pawnio::AMD_FAMILY_17_MODULE,
            Vendor::Intel => pawnio::INTEL_MSR_MODULE,
            _ => return Err(Unavailable::Unsupported),
        };
        let device = PawnIo::open().map_err(|error| {
            if pawnio::is_not_installed(&error) {
                Unavailable::NotInstalled
            } else if error.is_access_denied() {
                Unavailable::NeedsAdmin
            } else {
                Unavailable::Failed(error)
            }
        })?;
        device.load(module).map_err(|error| match error {
            Error::Win32 { code: ERROR_NOT_SUPPORTED, .. } => Unavailable::Unsupported,
            error => Unavailable::Failed(error),
        })?;

        let sensor = match id.vendor {
            Vendor::Amd => {
                let (ccd_base, ccd_limit) = amd_ccd_layout(id.family, id.model).unwrap_or((0, 0));
                Sensor::Amd {
                    tdie_offset: amd_tdie_offset(id.family, name),
                    ccd_base,
                    ccd_limit,
                    ccds: None,
                    pci: SharedMutex::open(r"Global\Access_PCI"),
                }
            }
            _ => {
                let mut target = [0u64];
                let tjmax = match device.execute("ioctl_read_msr", &[MSR_IA32_TEMPERATURE_TARGET], &mut target) {
                    Ok(1) => decode_tjmax(target[0]),
                    _ => 100.0,
                };
                let bases = crate::hardware::group_bases();
                let cores = core_threads.iter().filter_map(|threads| group_mask(&bases, *threads.first()?)).collect();
                Sensor::Intel { tjmax, cores }
            }
        };
        Ok(Self { device, sensor })
    }

    /// Reads every sensor; `None` if the driver stopped answering or the PCI lock was busy.
    pub fn read(&mut self) -> Option<CpuTemps> {
        let device = &self.device;
        let read = |function: &str, input: u64| {
            let mut out = [0u64];
            match device.execute(function, &[input], &mut out) {
                Ok(1) => Some(out[0]),
                _ => None,
            }
        };
        match &mut self.sensor {
            Sensor::Amd { tdie_offset, ccd_base, ccd_limit, ccds, pci } => {
                // Without the shared mutex (another tool made it inaccessible) read anyway: the
                // reads are idempotent, and a rare torn read only skews one sample.
                let _guard = match pci {
                    Some(mutex) => Some(mutex.lock(PCI_LOCK_TIMEOUT_MS)?),
                    None => None,
                };
                let smn = |address: u32| read("ioctl_read_smn", u64::from(address)).map(|v| v as u32);
                let tctl = decode_tctl(smn(ZEN_TEMP_CTRL)?);
                let ccd_address = |ccd: u32| ZEN_TEMP_CTRL + *ccd_base + ccd * 4;
                if ccds.is_none() && *ccd_base != 0 {
                    *ccds = Some(
                        (0..*ccd_limit).filter(|&ccd| smn(ccd_address(ccd)).and_then(decode_ccd).is_some()).collect(),
                    );
                }
                let ccds = ccds
                    .iter()
                    .flatten()
                    .filter_map(|&ccd| Some((ccd, smn(ccd_address(ccd)).and_then(decode_ccd)?)))
                    .collect();
                Some(CpuTemps {
                    package_c: Some(tctl),
                    die_c: tdie_offset.map(|offset| tctl - offset),
                    ccds,
                    cores: Vec::new(),
                })
            }
            Sensor::Intel { tjmax, cores } => {
                let tjmax = *tjmax;
                let package =
                    read("ioctl_read_msr", MSR_IA32_PACKAGE_THERM_STATUS).and_then(|s| decode_intel(s, tjmax));
                let cores: Vec<f32> = pinned_reads(cores, || {
                    read("ioctl_read_msr", MSR_IA32_THERM_STATUS).and_then(|s| decode_intel(s, tjmax))
                })
                .into_iter()
                .flatten()
                .collect();
                (package.is_some() || !cores.is_empty()).then(|| CpuTemps {
                    package_c: package.or_else(|| cores.iter().copied().reduce(f32::max)),
                    cores,
                    ..CpuTemps::default()
                })
            }
        }
    }
}

/// The processor group and affinity mask for flat logical-processor index `index`.
pub(crate) fn group_mask(bases: &[usize], index: usize) -> Option<(u16, usize)> {
    let group = bases.iter().rposition(|&base| base <= index)?;
    let bit = index - bases[group];
    (bit < usize::BITS as usize).then(|| (group as u16, 1usize << bit))
}

/// Runs `read` once on each of `cpus` by pinning this thread to it (MSRs and CPUID answer for
/// the processor they run on), then restores the thread's affinity. `None` for a processor the
/// thread couldn't be pinned to or whose read failed.
pub(crate) fn pinned_reads<T>(cpus: &[(u16, usize)], read: impl Fn() -> Option<T>) -> Vec<Option<T>> {
    let affinity = |group: u16, mask: usize| GROUP_AFFINITY { Mask: mask, Group: group, Reserved: [0; 3] };
    let mut original = affinity(0, 0);
    let mut values = Vec::with_capacity(cpus.len());
    let mut first = true;
    // SAFETY: no preconditions; returns a pseudo-handle for the calling thread.
    let this_thread = unsafe { GetCurrentThread() };
    for &(group, mask) in cpus {
        let mut previous = affinity(0, 0);
        // SAFETY: `this_thread` is the calling thread's pseudo-handle; both affinity pointers are valid.
        let ok = unsafe { SetThreadGroupAffinity(this_thread, &affinity(group, mask), &mut previous) };
        if ok == 0 {
            values.push(None);
            continue;
        }
        if first {
            original = previous;
            first = false;
        }
        values.push(read());
    }
    if !first {
        // SAFETY: as above; this puts back the affinity the thread had before.
        unsafe { SetThreadGroupAffinity(this_thread, &original, std::ptr::null_mut()) };
    }
    values
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_registry_identifier() {
        let id = parse_identifier("AuthenticAMD", "AMD64 Family 25 Model 33 Stepping 2").unwrap();
        assert_eq!(id, CpuId { vendor: Vendor::Amd, family: 0x19, model: 0x21 });
        let id = parse_identifier("GenuineIntel", "Intel64 Family 6 Model 183 Stepping 1").unwrap();
        assert_eq!((id.vendor, id.family, id.model), (Vendor::Intel, 6, 183));
        assert!(parse_identifier("AuthenticAMD", "garbage").is_none());
    }

    #[test]
    fn decodes_tctl_with_and_without_the_extended_range() {
        // 60.0 °C = 480 eighths.
        assert_eq!(decode_tctl(480 << 21), 60.0);
        assert_eq!(decode_tctl((480 << 21) | (1 << 19)), 11.0);
        assert_eq!(decode_tctl((480 << 21) | 0x3_0000), 11.0);
    }

    #[test]
    fn decodes_ccd_registers_and_rejects_absent_ones() {
        // 45.5 °C = (45.5 + 49) * 8 = 756 eighths.
        assert_eq!(decode_ccd((1 << 11) | 756), Some(45.5));
        assert_eq!(decode_ccd(756), None, "valid bit clear");
        assert_eq!(decode_ccd(0), None);
        assert_eq!(decode_ccd(u32::MAX), None, "PCI error response");
    }

    #[test]
    fn ccd_layouts_follow_the_model() {
        assert_eq!(amd_ccd_layout(0x17, 0x71), Some((0x154, 8)), "Zen 2 desktop");
        assert_eq!(amd_ccd_layout(0x19, 0x61), Some((0x308, 8)), "Zen 4 desktop");
        assert_eq!(amd_ccd_layout(0x1a, 0x44), Some((0x308, 8)), "Zen 5 desktop");
        assert_eq!(amd_ccd_layout(0x17, 0x99), None);
    }

    #[test]
    fn only_old_parts_have_a_tdie_offset() {
        assert_eq!(amd_tdie_offset(0x17, "AMD Ryzen Threadripper 1950X 16-Core Processor"), Some(27.0));
        assert_eq!(amd_tdie_offset(0x17, "AMD Ryzen 7 3700X 8-Core Processor"), None);
        assert_eq!(amd_tdie_offset(0x19, "AMD Ryzen 7 1800X"), None, "family mismatch");
    }

    #[test]
    fn decodes_intel_thermal_status() {
        assert_eq!(decode_tjmax(100 << 16), 100.0);
        assert_eq!(decode_tjmax(0), 100.0, "nonsense falls back");
        assert_eq!(decode_intel((1 << 31) | (35 << 16), 100.0), Some(65.0));
        assert_eq!(decode_intel(35 << 16, 100.0), None, "reading not valid");
    }

    #[test]
    fn maps_flat_processor_indexes_to_groups() {
        let bases = [0, 64];
        assert_eq!(group_mask(&bases, 0), Some((0, 1)));
        assert_eq!(group_mask(&bases, 63), Some((0, 1 << 63)));
        assert_eq!(group_mask(&bases, 65), Some((1, 2)));
        assert_eq!(group_mask(&[], 3), None);
    }

    #[test]
    fn reports_why_it_is_unavailable_without_panicking() {
        // On machines without PawnIO this is NotInstalled; elsewhere it may open. Either way it
        // must not panic, and a reading (if any) must be plausible.
        let info = crate::hardware::cpu_info();
        match CpuThermal::open(&info.name, &info.core_threads) {
            Ok(mut thermal) => {
                if let Some(temps) = thermal.read() {
                    let t = temps.package_c.unwrap();
                    assert!((-20.0..150.0).contains(&t), "{temps:?}");
                }
            }
            Err(reason) => assert_ne!(reason, Unavailable::Failed(Error::ProcessGone)),
        }
    }
}
