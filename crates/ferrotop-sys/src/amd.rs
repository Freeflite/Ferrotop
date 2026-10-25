//! AMD GPU sensors (edge, hotspot and memory temperatures, fan, power, clocks) through AMD's
//! Display Library, which the Radeon driver installs in System32. Struct layouts and sensor
//! numbers follow AMD's ADL SDK headers (`adl_structures.h`, `adl_defines.h`).
//!
//! The library is only ever loaded from System32, so a planted DLL elsewhere on the search
//! path can't be picked up. On machines without an AMD driver, [`Adl::open`] returns `None`.

use std::ffi::c_void;

use windows_sys::Win32::Foundation::{FARPROC, FreeLibrary, HMODULE};
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW};
use windows_sys::Win32::System::Memory::{GetProcessHeap, HeapAlloc};

use crate::buffer::wide;

const ADL_OK: i32 = 0;
const ADL_MAX_PATH: usize = 256;
const ADL_PMLOG_MAX_SENSORS: usize = 256;
/// ADL reports AMD's vendor ID as the decimal number 1002 (not PCI's 0x1002), as AMD's samples check.
const AMD_VENDOR_ID: i32 = 1002;

// `ADL_PMLOG_SENSORS` indexes.
const CLK_GFXCLK: usize = 1;
const CLK_MEMCLK: usize = 2;
const TEMPERATURE_EDGE: usize = 8;
const TEMPERATURE_MEM: usize = 9;
const FAN_RPM: usize = 14;
const ASIC_POWER: usize = 23;
const TEMPERATURE_HOTSPOT: usize = 27;
const TEMPERATURE_HOTSPOT_GCD: usize = 50;
const BOARD_POWER: usize = 73;

/// `AdapterInfo` (Windows layout).
#[repr(C)]
#[derive(Clone, Copy)]
struct AdapterInfo {
    size: i32,
    adapter_index: i32,
    udid: [u8; ADL_MAX_PATH],
    bus_number: i32,
    device_number: i32,
    function_number: i32,
    vendor_id: i32,
    adapter_name: [u8; ADL_MAX_PATH],
    display_name: [u8; ADL_MAX_PATH],
    present: i32,
    exist: i32,
    driver_path: [u8; ADL_MAX_PATH],
    driver_path_ext: [u8; ADL_MAX_PATH],
    pnp_string: [u8; ADL_MAX_PATH],
    os_display_index: i32,
}

/// `ADLSingleSensorData`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SensorData {
    supported: i32,
    value: i32,
}

/// `ADLPMLogDataOutput`.
#[repr(C)]
#[derive(Clone, Copy)]
struct PmLogOutput {
    size: i32,
    sensors: [SensorData; ADL_PMLOG_MAX_SENSORS],
}

const _: () = {
    assert!(size_of::<AdapterInfo>() == 1572);
    assert!(size_of::<PmLogOutput>() == 4 + 8 * ADL_PMLOG_MAX_SENSORS);
};

type MallocCallback = unsafe extern "system" fn(i32) -> *mut c_void;
type ControlCreate = unsafe extern "C" fn(MallocCallback, i32, *mut *mut c_void) -> i32;
type ControlDestroy = unsafe extern "C" fn(*mut c_void) -> i32;
type NumberOfAdapters = unsafe extern "C" fn(*mut c_void, *mut i32) -> i32;
type AdapterInfoGet = unsafe extern "C" fn(*mut c_void, *mut AdapterInfo, i32) -> i32;
type PmLogGet = unsafe extern "C" fn(*mut c_void, i32, *mut PmLogOutput) -> i32;

/// ADL allocates with this callback; it's only used for buffers ADL returns to callers, which
/// Ferrotop's calls don't request.
unsafe extern "system" fn adl_malloc(size: i32) -> *mut c_void {
    // SAFETY: no preconditions; returns the process heap, which lives as long as the process.
    let heap = unsafe { GetProcessHeap() };
    // SAFETY: `heap` is the process heap and HeapAlloc accepts any size (returning null on
    // failure, which ADL handles).
    unsafe { HeapAlloc(heap, 0, size.max(0) as usize) }
}

pub use crate::gpu::VendorSensors as AmdSensors;

/// An initialized ADL context. Destroyed and unloaded on drop.
pub struct Adl {
    library: HMODULE,
    context: *mut c_void,
    destroy: ControlDestroy,
    query: PmLogGet,
    /// `(bus, device, function)` → ADL adapter index, for present AMD adapters.
    adapters: Vec<((u32, u32, u32), i32)>,
}

impl std::fmt::Debug for Adl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Adl").field("adapters", &self.adapters).finish_non_exhaustive()
    }
}

/// Looks up an export. Callers transmute it to the signature the ADL SDK declares.
fn export(library: HMODULE, name: &std::ffi::CStr) -> FARPROC {
    // SAFETY: `library` is a loaded module and `name` is NUL-terminated.
    unsafe { GetProcAddress(library, name.as_ptr().cast()) }
}

impl Adl {
    pub fn open() -> Option<Self> {
        let name = wide("atiadlxx.dll");
        // SAFETY: `name` is NUL-terminated; restricting the search to System32 means only the
        // driver-installed copy can load.
        let library = unsafe { LoadLibraryExW(name.as_ptr(), std::ptr::null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32) };
        if library.is_null() {
            return None;
        }
        let adl = Self::init(library);
        if adl.is_none() {
            // SAFETY: `library` was loaded above and isn't used after this.
            unsafe { FreeLibrary(library) };
        }
        adl
    }

    fn init(library: HMODULE) -> Option<Self> {
        let (create, destroy, count, info, query) = (
            export(library, c"ADL2_Main_Control_Create")?,
            export(library, c"ADL2_Main_Control_Destroy")?,
            export(library, c"ADL2_Adapter_NumberOfAdapters_Get")?,
            export(library, c"ADL2_Adapter_AdapterInfo_Get")?,
            export(library, c"ADL2_New_QueryPMLogData_Get")?,
        );
        // SAFETY: the export has exactly this signature in the ADL SDK headers (adl_sdk.h).
        let create: ControlCreate = unsafe { std::mem::transmute(create) };
        // SAFETY: the export has exactly this signature in the ADL SDK headers.
        let destroy: ControlDestroy = unsafe { std::mem::transmute(destroy) };
        // SAFETY: the export has exactly this signature in the ADL SDK headers.
        let count: NumberOfAdapters = unsafe { std::mem::transmute(count) };
        // SAFETY: the export has exactly this signature in the ADL SDK headers.
        let info: AdapterInfoGet = unsafe { std::mem::transmute(info) };
        // SAFETY: the export has exactly this signature in the ADL SDK headers.
        let query: PmLogGet = unsafe { std::mem::transmute(query) };

        let mut context = std::ptr::null_mut();
        // SAFETY: `adl_malloc` matches ADL's callback type, and `context` is a valid out-pointer.
        if unsafe { create(adl_malloc, 1, &mut context) } != ADL_OK || context.is_null() {
            return None;
        }
        let mut adl = Adl { library, context, destroy, query, adapters: Vec::new() };

        let mut total = 0i32;
        // SAFETY: `context` is live and `total` is a valid out-pointer.
        if unsafe { count(adl.context, &mut total) } != ADL_OK || total <= 0 {
            return Some(adl);
        }
        let empty = AdapterInfo {
            size: size_of::<AdapterInfo>() as i32,
            adapter_index: 0,
            udid: [0; ADL_MAX_PATH],
            bus_number: 0,
            device_number: 0,
            function_number: 0,
            vendor_id: 0,
            adapter_name: [0; ADL_MAX_PATH],
            display_name: [0; ADL_MAX_PATH],
            present: 0,
            exist: 0,
            driver_path: [0; ADL_MAX_PATH],
            driver_path_ext: [0; ADL_MAX_PATH],
            pnp_string: [0; ADL_MAX_PATH],
            os_display_index: 0,
        };
        let mut infos = vec![empty; total as usize];
        let bytes = i32::try_from(infos.len() * size_of::<AdapterInfo>()).ok()?;
        // SAFETY: `context` is live and `infos` holds `total` writable AdapterInfo entries,
        // whose total size is passed.
        if unsafe { info(adl.context, infos.as_mut_ptr(), bytes) } != ADL_OK {
            return Some(adl);
        }
        // ADL lists one entry per display output; keep one per physical adapter.
        for entry in infos.iter().filter(|e| e.vendor_id == AMD_VENDOR_ID && e.present != 0) {
            let location = (entry.bus_number as u32, entry.device_number as u32, entry.function_number as u32);
            if !adl.adapters.iter().any(|(existing, _)| *existing == location) {
                adl.adapters.push((location, entry.adapter_index));
            }
        }
        Some(adl)
    }

    /// Sensors for the AMD adapter at PCI `location`, or `None` if it isn't an AMD adapter
    /// ADL knows about.
    pub fn sensors(&self, location: (u32, u32, u32)) -> Option<AmdSensors> {
        let &(_, index) = self.adapters.iter().find(|(at, _)| *at == location)?;
        let mut output = PmLogOutput {
            size: size_of::<PmLogOutput>() as i32,
            sensors: [SensorData::default(); ADL_PMLOG_MAX_SENSORS],
        };
        // SAFETY: `context` is live and `output` is a writable ADLPMLogDataOutput.
        if unsafe { (self.query)(self.context, index, &mut output) } != ADL_OK {
            return None;
        }
        // A supported sensor's value; zero is real for the fan (zero-RPM idle) but means "no
        // reading" for temperatures, clocks and power.
        let read = |sensor: usize| {
            let data = output.sensors[sensor];
            (data.supported != 0 && data.value >= 0).then_some(data.value)
        };
        let positive = |sensor| read(sensor).filter(|&v| v > 0);
        let celsius = |sensor| positive(sensor).map(|v| v as f32);
        Some(AmdSensors {
            edge_c: celsius(TEMPERATURE_EDGE),
            hotspot_c: celsius(TEMPERATURE_HOTSPOT).or_else(|| celsius(TEMPERATURE_HOTSPOT_GCD)),
            memory_c: celsius(TEMPERATURE_MEM),
            fan_rpm: read(FAN_RPM).map(|v| v as u32),
            fan_percent: None,
            power_w: positive(BOARD_POWER).or_else(|| positive(ASIC_POWER)).map(|v| v as u32),
            core_clock_mhz: positive(CLK_GFXCLK).map(|v| v as u32),
            memory_clock_mhz: positive(CLK_MEMCLK).map(|v| v as u32),
        })
    }
}

impl Drop for Adl {
    fn drop(&mut self) {
        // SAFETY: `context` came from ADL2_Main_Control_Create and is destroyed exactly once.
        unsafe { (self.destroy)(self.context) };
        // SAFETY: nothing from the library is used after this.
        unsafe { FreeLibrary(self.library) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_amd_sensors_when_an_amd_gpu_is_present() {
        let Some(adl) = Adl::open() else { return };
        let Ok(adapters) = crate::gpu::Adapters::open() else { return };
        for info in adapters.infos() {
            let Some(location) = info.location else { continue };
            if let Some(sensors) = adl.sensors(location) {
                let edge = sensors.edge_c.expect("edge temperature");
                assert!((10.0..110.0).contains(&edge), "{sensors:?}");
            }
        }
    }
}
