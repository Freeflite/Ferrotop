//! GPU adapters through the kernel graphics interface (D3DKMT), as System Informer's
//! HardwareDevices plugin and Task Manager do: name, memory sizes, PCI location, temperature.

use windows_sys::Wdk::Graphics::Direct3D::{
    D3DKMT_ADAPTER_PERFDATA, D3DKMT_ADAPTERADDRESS, D3DKMT_ADAPTERINFO, D3DKMT_ADAPTERREGISTRYINFO,
    D3DKMT_CLOSEADAPTER, D3DKMT_ENUMADAPTERS2, D3DKMT_QUERYADAPTERINFO, D3DKMT_SEGMENTSIZEINFO, D3DKMTCloseAdapter,
    D3DKMTEnumAdapters2, D3DKMTQueryAdapterInfo, KMTQAITYPE_ADAPTERADDRESS, KMTQAITYPE_ADAPTERPERFDATA,
    KMTQAITYPE_ADAPTERREGISTRYINFO, KMTQAITYPE_GETSEGMENTSIZE, KMTQUERYADAPTERINFOTYPE,
};
use windows_sys::Win32::Foundation::LUID;

use crate::buffer::Pod;
use crate::{Error, Result};

// SAFETY: integer arrays only; every bit pattern is valid.
unsafe impl Pod for D3DKMT_ADAPTERREGISTRYINFO {}
// SAFETY: integers only; every bit pattern is valid.
unsafe impl Pod for D3DKMT_SEGMENTSIZEINFO {}
// SAFETY: integers only; every bit pattern is valid.
unsafe impl Pod for D3DKMT_ADAPTERADDRESS {}
// SAFETY: integers only; every bit pattern is valid.
unsafe impl Pod for D3DKMT_ADAPTER_PERFDATA {}

/// A GPU's fixed properties.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuInfo {
    /// The adapter LUID as `(high, low)`, which is how performance counters name it.
    pub luid: (i32, u32),
    pub name: String,
    pub dedicated_memory: u64,
    pub shared_memory: u64,
    /// PCI bus, device, function.
    pub location: Option<(u32, u32, u32)>,
    pub driver_version: Option<String>,
    pub driver_date: Option<String>,
}

/// Readings from a vendor's driver library (AMD ADL, NVIDIA NVML). Each is `None` when the
/// GPU or driver doesn't report it.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct VendorSensors {
    /// The main die temperature (AMD "edge", NVIDIA "GPU").
    pub edge_c: Option<f32>,
    /// Junction (hotspot) temperature, the hottest point on the die.
    pub hotspot_c: Option<f32>,
    pub memory_c: Option<f32>,
    pub fan_rpm: Option<u32>,
    /// Fan speed as a percentage of maximum (NVIDIA reports this instead of RPM).
    pub fan_percent: Option<u32>,
    pub power_w: Option<u32>,
    pub core_clock_mhz: Option<u32>,
    pub memory_clock_mhz: Option<u32>,
}

/// Live sensor readings, where the driver reports them.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct GpuSensors {
    pub temperature_c: Option<f32>,
    pub fan_rpm: Option<u32>,
}

/// Open handles to the system's hardware GPUs. Closed on drop.
#[derive(Debug)]
pub struct Adapters {
    adapters: Vec<(u32, GpuInfo)>,
}

impl Adapters {
    /// Every hardware render adapter, skipping Microsoft's software renderer.
    pub fn open() -> Result<Self> {
        let mut enumerate = D3DKMT_ENUMADAPTERS2 { NumAdapters: 0, pAdapters: std::ptr::null_mut() };
        // SAFETY: a null adapter array asks for the count only.
        let status = unsafe { D3DKMTEnumAdapters2(&mut enumerate) };
        if status < 0 {
            return Err(Error::Nt { call: "D3DKMTEnumAdapters2", status });
        }
        let empty = D3DKMT_ADAPTERINFO {
            hAdapter: 0,
            AdapterLuid: LUID { LowPart: 0, HighPart: 0 },
            NumOfSources: 0,
            bPrecisePresentRegionsPreferred: 0,
        };
        let mut infos = vec![empty; enumerate.NumAdapters as usize];
        enumerate.pAdapters = infos.as_mut_ptr();
        // SAFETY: `pAdapters` points at `NumAdapters` writable entries.
        let status = unsafe { D3DKMTEnumAdapters2(&mut enumerate) };
        if status < 0 {
            return Err(Error::Nt { call: "D3DKMTEnumAdapters2", status });
        }
        infos.truncate(enumerate.NumAdapters as usize);

        let mut adapters = Adapters { adapters: Vec::new() };
        for info in infos {
            let handle = info.hAdapter;
            let name = query::<D3DKMT_ADAPTERREGISTRYINFO>(handle, KMTQAITYPE_ADAPTERREGISTRYINFO)
                .map(|registry| utf16_field(&registry.AdapterString))
                .unwrap_or_default();
            if name.is_empty() || name == "Microsoft Basic Render Driver" {
                close(handle);
                continue;
            }
            let segments = query::<D3DKMT_SEGMENTSIZEINFO>(handle, KMTQAITYPE_GETSEGMENTSIZE).ok();
            let address = query::<D3DKMT_ADAPTERADDRESS>(handle, KMTQAITYPE_ADAPTERADDRESS).ok();
            let (driver_version, driver_date) = driver_details(&name);
            adapters.adapters.push((
                handle,
                GpuInfo {
                    luid: (info.AdapterLuid.HighPart, info.AdapterLuid.LowPart),
                    name,
                    dedicated_memory: segments.map_or(0, |s| s.DedicatedVideoMemorySize),
                    shared_memory: segments.map_or(0, |s| s.SharedSystemMemorySize),
                    location: address.map(|a| (a.BusNumber, a.DeviceNumber, a.FunctionNumber)),
                    driver_version,
                    driver_date,
                },
            ));
        }
        Ok(adapters)
    }

    pub fn infos(&self) -> impl Iterator<Item = &GpuInfo> {
        self.adapters.iter().map(|(_, info)| info)
    }

    /// Current temperature and fan speed for adapter `index`.
    pub fn sensors(&self, index: usize) -> GpuSensors {
        let Some(&(handle, _)) = self.adapters.get(index) else { return GpuSensors::default() };
        let mut perf = D3DKMT_ADAPTER_PERFDATA {
            PhysicalAdapterIndex: 0,
            MemoryFrequency: 0,
            MaxMemoryFrequency: 0,
            MaxMemoryFrequencyOC: 0,
            MemoryBandwidth: 0,
            PCIEBandwidth: 0,
            FanRPM: 0,
            Power: 0,
            Temperature: 0,
            PowerStateOverride: 0,
        };
        if query_into(handle, KMTQAITYPE_ADAPTERPERFDATA, &mut perf).is_err() {
            return GpuSensors::default();
        }
        GpuSensors {
            // Reported in tenths of a degree Celsius.
            temperature_c: (perf.Temperature > 0).then(|| perf.Temperature as f32 / 10.0),
            fan_rpm: (perf.FanRPM > 0).then_some(perf.FanRPM),
        }
    }
}

impl Drop for Adapters {
    fn drop(&mut self) {
        for &(handle, _) in &self.adapters {
            close(handle);
        }
    }
}

fn close(handle: u32) {
    let close = D3DKMT_CLOSEADAPTER { hAdapter: handle };
    // SAFETY: `handle` came from D3DKMTEnumAdapters2 and is closed exactly once.
    unsafe { D3DKMTCloseAdapter(&close) };
}

/// Queries adapter info of type `T`, starting from zeroed data.
fn query<T: Pod>(handle: u32, kind: KMTQUERYADAPTERINFOTYPE) -> Result<T> {
    // SAFETY: `T: Pod` means all-zero bytes are a valid `T`.
    let mut value: T = unsafe { std::mem::zeroed() };
    query_into(handle, kind, &mut value)?;
    Ok(value)
}

/// Queries adapter info into `value`, which may carry input fields.
fn query_into<T: Pod>(handle: u32, kind: KMTQUERYADAPTERINFOTYPE, value: &mut T) -> Result<()> {
    let mut request = D3DKMT_QUERYADAPTERINFO {
        hAdapter: handle,
        Type: kind,
        pPrivateDriverData: (value as *mut T).cast(),
        PrivateDriverDataSize: size_of::<T>() as u32,
    };
    // SAFETY: the request points at a live, writable `T` of the stated size (and `T: Pod`, so
    // whatever the kernel writes is valid); the kernel validates the adapter handle.
    let status = unsafe { D3DKMTQueryAdapterInfo(&mut request) };
    if status < 0 { Err(Error::Nt { call: "D3DKMTQueryAdapterInfo", status }) } else { Ok(()) }
}

fn utf16_field(field: &[u16]) -> String {
    let len = field.iter().position(|&c| c == 0).unwrap_or(field.len());
    String::from_utf16_lossy(&field[..len]).trim().to_owned()
}

/// Driver version and date from the display adapter's device-class registry key.
fn driver_details(name: &str) -> (Option<String>, Option<String>) {
    const DISPLAY_CLASS: &str = r"SYSTEM\CurrentControlSet\Control\Class\{4d36e968-e325-11ce-bfc1-08002be10318}";
    for index in 0..32 {
        let key = format!(r"{DISPLAY_CLASS}\{index:04}");
        match crate::system::registry_string(&key, "DriverDesc") {
            Some(description) if description == name => {
                return (
                    crate::system::registry_string(&key, "DriverVersion"),
                    crate::system::registry_string(&key, "DriverDate"),
                );
            }
            Some(_) => {}
            None if index > 4 => break,
            None => {}
        }
    }
    (None, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_hardware_adapters() {
        let adapters = Adapters::open().expect("enumerate adapters");
        for (index, info) in adapters.infos().enumerate() {
            assert!(!info.name.is_empty());
            let _ = adapters.sensors(index);
        }
    }
}
