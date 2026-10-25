//! Performance Data Helper (PDH) counters: the system-wide counters Task Manager reads for disks,
//! GPU engines and GPU memory.

use windows_sys::Win32::System::Performance::{
    PDH_CSTATUS_NEW_DATA, PDH_CSTATUS_VALID_DATA, PDH_FMT_DOUBLE, PDH_HCOUNTER, PDH_HQUERY, PDH_MORE_DATA,
    PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterArrayW, PdhOpenQueryW,
};

use crate::buffer::{AlignedBuffer, Pod, read_pod, wide, wide_cstr_in};
use crate::{Error, Result};

/// Don't clamp percentages at 100 (Windows does this itself where it matters).
const PDH_FMT_NOCAP100: u32 = 0x0000_8000;

/// `PDH_FMT_COUNTERVALUE_ITEM_W` read as a double. `name` points back into the same buffer.
#[repr(C)]
#[derive(Clone, Copy)]
struct CounterItem {
    name: *const u16,
    status: u32,
    value: f64,
}

#[cfg(target_pointer_width = "64")]
const _: () = assert!(std::mem::size_of::<CounterItem>() == 24);

// SAFETY: a raw pointer, an integer and a float; every bit pattern is valid.
unsafe impl Pod for CounterItem {}

/// Index of a counter added to a [`Query`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CounterId(usize);

/// An open PDH query. Counters are sampled together by [`Query::collect`].
#[derive(Debug)]
pub struct Query {
    handle: PDH_HQUERY,
    counters: Vec<PDH_HCOUNTER>,
}

fn check(status: u32, call: &'static str) -> Result<()> {
    if status == 0 { Ok(()) } else { Err(Error::HResult { call, hresult: status as i32 }) }
}

impl Query {
    pub fn open() -> Result<Self> {
        let mut handle = std::ptr::null_mut();
        // SAFETY: a null data source means live data, and `handle` is a valid out-pointer.
        check(unsafe { PdhOpenQueryW(std::ptr::null(), 0, &mut handle) }, "PdhOpenQueryW")?;
        Ok(Self { handle, counters: Vec::new() })
    }

    /// Adds a counter by its English path. A `*` instance wildcard is re-expanded on every
    /// collection, so new instances (processes, disks) show up automatically.
    pub fn add(&mut self, path: &str) -> Result<CounterId> {
        let path = wide(path);
        let mut counter = std::ptr::null_mut();
        // SAFETY: the query is open, `path` is NUL-terminated, and `counter` is a valid out-pointer.
        let status = unsafe { PdhAddEnglishCounterW(self.handle, path.as_ptr(), 0, &mut counter) };
        check(status, "PdhAddEnglishCounterW")?;
        self.counters.push(counter);
        Ok(CounterId(self.counters.len() - 1))
    }

    /// Samples every counter in the query. Rate counters need two collections before they have data.
    pub fn collect(&mut self) -> Result<()> {
        // SAFETY: the query handle is open.
        check(unsafe { PdhCollectQueryData(self.handle) }, "PdhCollectQueryData")
    }

    /// `(instance, value)` for every instance of a counter that has valid data.
    pub fn values(&self, id: CounterId) -> Vec<(String, f64)> {
        let Some(&counter) = self.counters.get(id.0) else { return Vec::new() };
        let format = PDH_FMT_DOUBLE | PDH_FMT_NOCAP100;
        for _ in 0..3 {
            let (mut size, mut count) = (0u32, 0u32);
            // SAFETY: the counter belongs to our open query; a null buffer with zero size asks
            // PDH for the size it needs, and both out-pointers are valid.
            let status =
                unsafe { PdhGetFormattedCounterArrayW(counter, format, &mut size, &mut count, std::ptr::null_mut()) };
            if status != PDH_MORE_DATA {
                return Vec::new();
            }
            let mut buffer = AlignedBuffer::new(size as usize);
            let mut capacity = buffer.capacity_u32();
            // SAFETY: as above, with a buffer writable for `capacity` bytes.
            let status = unsafe {
                PdhGetFormattedCounterArrayW(counter, format, &mut capacity, &mut count, buffer.as_mut_ptr().cast())
            };
            if status == PDH_MORE_DATA {
                continue; // instances appeared between the two calls
            }
            if status != 0 {
                return Vec::new();
            }
            let bytes = buffer.bytes(buffer.capacity());
            return (0..count as usize)
                .filter_map(|i| {
                    let item: CounterItem = read_pod(bytes, i * size_of::<CounterItem>())?;
                    let valid = matches!(item.status, PDH_CSTATUS_VALID_DATA | PDH_CSTATUS_NEW_DATA);
                    if !valid || !item.value.is_finite() {
                        return None;
                    }
                    Some((wide_cstr_in(bytes, item.name)?, item.value))
                })
                .collect();
        }
        Vec::new()
    }
}

impl Drop for Query {
    fn drop(&mut self) {
        // SAFETY: the handle came from PdhOpenQueryW and is closed exactly once, here.
        unsafe { PdhCloseQuery(self.handle) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_processor_counters() {
        let mut query = Query::open().expect("open");
        let total = query.add(r"\Processor Information(*)\% Processor Time").expect("add");
        query.collect().expect("first collect");
        std::thread::sleep(std::time::Duration::from_millis(200));
        query.collect().expect("second collect");
        let values = query.values(total);
        assert!(values.iter().any(|(name, _)| name == "_Total"), "{values:?}");
    }
}
