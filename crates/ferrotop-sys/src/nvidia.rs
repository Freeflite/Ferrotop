//! NVIDIA GPU sensors (temperature, hottest-point temperature, fan, power, clocks) through NVML,
//! the monitoring library the NVIDIA driver installs. Prototypes and layouts follow `nvml.h`.
//!
//! Best effort: there's no NVIDIA GPU on the machine this was written on, so it's untested on
//! real hardware. Every call is defensive; a failure just leaves that reading empty.
//!
//! The library is loaded from System32 (where current drivers put it) or the driver's NVSMI
//! folder, never from the DLL search path.

use std::ffi::c_void;

use windows_sys::Win32::Foundation::{FARPROC, FreeLibrary, HMODULE};
use windows_sys::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};

use crate::buffer::wide;
use crate::gpu::VendorSensors;

const NVML_SUCCESS: i32 = 0;
const NVML_TEMPERATURE_GPU: i32 = 0;
/// "Hottest part of the GPU die" (newer drivers only).
const NVML_TEMPERATURE_GPU_MAX: i32 = 1;
const NVML_CLOCK_GRAPHICS: i32 = 0;
const NVML_CLOCK_MEM: i32 = 2;

/// `nvmlPciInfo_t` (the layout `nvmlDeviceGetPciInfo_v3` fills).
#[repr(C)]
#[derive(Clone, Copy)]
struct PciInfo {
    bus_id_legacy: [u8; 16],
    domain: u32,
    bus: u32,
    device: u32,
    pci_device_id: u32,
    pci_sub_system_id: u32,
    bus_id: [u8; 32],
}

const _: () = assert!(size_of::<PciInfo>() == 68);

type Device = *mut c_void;
type Init = unsafe extern "C" fn() -> i32;
type Shutdown = unsafe extern "C" fn() -> i32;
type Count = unsafe extern "C" fn(*mut u32) -> i32;
type HandleByIndex = unsafe extern "C" fn(u32, *mut Device) -> i32;
type PciInfoGet = unsafe extern "C" fn(Device, *mut PciInfo) -> i32;
type Temperature = unsafe extern "C" fn(Device, i32, *mut u32) -> i32;
type FanSpeed = unsafe extern "C" fn(Device, *mut u32) -> i32;
type PowerUsage = unsafe extern "C" fn(Device, *mut u32) -> i32;
type ClockInfo = unsafe extern "C" fn(Device, i32, *mut u32) -> i32;

/// An initialized NVML session. Shut down and unloaded on drop.
pub struct Nvml {
    library: HMODULE,
    shutdown: Shutdown,
    temperature: Temperature,
    fan_speed: FanSpeed,
    power_usage: PowerUsage,
    clock_info: ClockInfo,
    /// `(bus, device)` → NVML device handle.
    devices: Vec<((u32, u32), Device)>,
}

impl std::fmt::Debug for Nvml {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let locations: Vec<_> = self.devices.iter().map(|(at, _)| at).collect();
        f.debug_struct("Nvml").field("devices", &locations).finish_non_exhaustive()
    }
}

fn export(library: HMODULE, name: &std::ffi::CStr) -> FARPROC {
    // SAFETY: `library` is a loaded module and `name` is NUL-terminated.
    unsafe { GetProcAddress(library, name.as_ptr().cast()) }
}

fn load() -> Option<HMODULE> {
    let system = wide("nvml.dll");
    // SAFETY: `system` is NUL-terminated; only System32 is searched.
    let library = unsafe { LoadLibraryExW(system.as_ptr(), std::ptr::null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32) };
    if !library.is_null() {
        return Some(library);
    }
    // Older drivers keep it in the NVSMI folder, which only administrators can write to.
    let program_files = std::env::var("ProgramW6432").or_else(|_| std::env::var("ProgramFiles")).ok()?;
    let path = wide(&format!(r"{program_files}\NVIDIA Corporation\NVSMI\nvml.dll"));
    // SAFETY: `path` is a NUL-terminated absolute path; its dependencies resolve from its own
    // folder and System32 only.
    let library = unsafe {
        LoadLibraryExW(
            path.as_ptr(),
            std::ptr::null_mut(),
            LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32,
        )
    };
    (!library.is_null()).then_some(library)
}

impl Nvml {
    pub fn open() -> Option<Self> {
        let library = load()?;
        let nvml = Self::init(library);
        if nvml.is_none() {
            // SAFETY: `library` was loaded above and isn't used after this.
            unsafe { FreeLibrary(library) };
        }
        nvml
    }

    fn init(library: HMODULE) -> Option<Self> {
        let exports = (
            export(library, c"nvmlInit_v2")?,
            export(library, c"nvmlShutdown")?,
            export(library, c"nvmlDeviceGetCount_v2")?,
            export(library, c"nvmlDeviceGetHandleByIndex_v2")?,
            export(library, c"nvmlDeviceGetPciInfo_v3")?,
            export(library, c"nvmlDeviceGetTemperature")?,
            export(library, c"nvmlDeviceGetFanSpeed")?,
            export(library, c"nvmlDeviceGetPowerUsage")?,
            export(library, c"nvmlDeviceGetClockInfo")?,
        );
        // SAFETY: the export has exactly this signature in nvml.h.
        let init: Init = unsafe { std::mem::transmute(exports.0) };
        // SAFETY: the export has exactly this signature in nvml.h.
        let shutdown: Shutdown = unsafe { std::mem::transmute(exports.1) };
        // SAFETY: the export has exactly this signature in nvml.h.
        let count: Count = unsafe { std::mem::transmute(exports.2) };
        // SAFETY: the export has exactly this signature in nvml.h.
        let handle_by_index: HandleByIndex = unsafe { std::mem::transmute(exports.3) };
        // SAFETY: the export has exactly this signature in nvml.h.
        let pci_info: PciInfoGet = unsafe { std::mem::transmute(exports.4) };
        // SAFETY: the export has exactly this signature in nvml.h.
        let temperature: Temperature = unsafe { std::mem::transmute(exports.5) };
        // SAFETY: the export has exactly this signature in nvml.h.
        let fan_speed: FanSpeed = unsafe { std::mem::transmute(exports.6) };
        // SAFETY: the export has exactly this signature in nvml.h.
        let power_usage: PowerUsage = unsafe { std::mem::transmute(exports.7) };
        // SAFETY: the export has exactly this signature in nvml.h.
        let clock_info: ClockInfo = unsafe { std::mem::transmute(exports.8) };

        // SAFETY: no preconditions.
        if unsafe { init() } != NVML_SUCCESS {
            return None;
        }
        let mut nvml = Nvml { library, shutdown, temperature, fan_speed, power_usage, clock_info, devices: Vec::new() };
        let mut total = 0u32;
        // SAFETY: NVML is initialized and `total` is a valid out-pointer.
        if unsafe { count(&mut total) } != NVML_SUCCESS {
            return Some(nvml);
        }
        for index in 0..total {
            let mut device = std::ptr::null_mut();
            // SAFETY: `index` is below the reported count and `device` is a valid out-pointer.
            if unsafe { handle_by_index(index, &mut device) } != NVML_SUCCESS || device.is_null() {
                continue;
            }
            let mut pci = PciInfo {
                bus_id_legacy: [0; 16],
                domain: 0,
                bus: 0,
                device: 0,
                pci_device_id: 0,
                pci_sub_system_id: 0,
                bus_id: [0; 32],
            };
            // SAFETY: `device` came from NVML and `pci` is a writable nvmlPciInfo_t.
            if unsafe { pci_info(device, &mut pci) } == NVML_SUCCESS {
                nvml.devices.push(((pci.bus, pci.device), device));
            }
        }
        Some(nvml)
    }

    /// Sensors for the NVIDIA GPU at PCI `location`, or `None` if NVML doesn't know it.
    pub fn sensors(&self, location: (u32, u32, u32)) -> Option<VendorSensors> {
        let &(_, device) = self.devices.iter().find(|((bus, slot), _)| (*bus, *slot) == (location.0, location.1))?;
        Some(VendorSensors {
            edge_c: self.temperature(device, NVML_TEMPERATURE_GPU).map(|t| t as f32),
            hotspot_c: self.temperature(device, NVML_TEMPERATURE_GPU_MAX).map(|t| t as f32),
            memory_c: None,
            fan_rpm: None,
            fan_percent: self.fan_percent(device),
            power_w: self.power_milliwatts(device).map(|mw| mw / 1000),
            core_clock_mhz: self.clock(device, NVML_CLOCK_GRAPHICS).filter(|&mhz| mhz > 0),
            memory_clock_mhz: self.clock(device, NVML_CLOCK_MEM).filter(|&mhz| mhz > 0),
        })
    }

    fn temperature(&self, device: Device, sensor: i32) -> Option<u32> {
        let mut value = 0u32;
        // SAFETY: `device` is a live NVML handle and `value` is a valid out-pointer.
        (unsafe { (self.temperature)(device, sensor, &mut value) } == NVML_SUCCESS).then_some(value)
    }

    fn clock(&self, device: Device, kind: i32) -> Option<u32> {
        let mut value = 0u32;
        // SAFETY: `device` is a live NVML handle and `value` is a valid out-pointer.
        (unsafe { (self.clock_info)(device, kind, &mut value) } == NVML_SUCCESS).then_some(value)
    }

    fn fan_percent(&self, device: Device) -> Option<u32> {
        let mut value = 0u32;
        // SAFETY: `device` is a live NVML handle and `value` is a valid out-pointer.
        (unsafe { (self.fan_speed)(device, &mut value) } == NVML_SUCCESS).then_some(value)
    }

    fn power_milliwatts(&self, device: Device) -> Option<u32> {
        let mut value = 0u32;
        // SAFETY: `device` is a live NVML handle and `value` is a valid out-pointer.
        (unsafe { (self.power_usage)(device, &mut value) } == NVML_SUCCESS).then_some(value)
    }
}

impl Drop for Nvml {
    fn drop(&mut self) {
        // SAFETY: NVML was initialized in `init` and is shut down exactly once.
        unsafe { (self.shutdown)() };
        // SAFETY: nothing from the library is used after this.
        unsafe { FreeLibrary(self.library) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_only_when_nvidia_is_installed() {
        // On machines without an NVIDIA driver this must quietly return None.
        if let Some(nvml) = Nvml::open() {
            for ((bus, device), _) in &nvml.devices {
                let _ = nvml.sensors((*bus, *device, 0));
            }
        }
    }
}
