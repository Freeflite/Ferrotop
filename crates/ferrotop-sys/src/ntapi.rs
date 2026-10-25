//! Raw NT definitions that `windows-sys` doesn't cover, ported from phnt.

use std::ffi::c_void;

/// `UNICODE_STRING`. `length` is in bytes, not characters.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct UnicodeString {
    pub length: u16,
    pub maximum_length: u16,
    pub buffer: *const u16,
}

/// `SYSTEM_PROCESS_INFORMATION` (phnt `ntexapi.h`), without the trailing thread array.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct SystemProcessInformation {
    pub next_entry_offset: u32,
    pub number_of_threads: u32,
    pub working_set_private_size: u64,
    pub hard_fault_count: u32,
    pub number_of_threads_high_watermark: u32,
    pub cycle_time: u64,
    pub create_time: i64,
    pub user_time: i64,
    pub kernel_time: i64,
    pub image_name: UnicodeString,
    pub base_priority: i32,
    pub unique_process_id: usize,
    pub inherited_from_unique_process_id: usize,
    pub handle_count: u32,
    pub session_id: u32,
    pub unique_process_key: usize,
    pub peak_virtual_size: usize,
    pub virtual_size: usize,
    pub page_fault_count: u32,
    pub peak_working_set_size: usize,
    pub working_set_size: usize,
    pub quota_peak_paged_pool_usage: usize,
    pub quota_paged_pool_usage: usize,
    pub quota_peak_non_paged_pool_usage: usize,
    pub quota_non_paged_pool_usage: usize,
    pub pagefile_usage: usize,
    pub peak_pagefile_usage: usize,
    pub private_page_count: usize,
    pub read_operation_count: i64,
    pub write_operation_count: i64,
    pub other_operation_count: i64,
    pub read_transfer_count: i64,
    pub write_transfer_count: i64,
    pub other_transfer_count: i64,
}

/// `SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION`. Times are in 100 ns units and
/// `kernel_time` includes `idle_time`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct SystemProcessorPerformanceInformation {
    pub idle_time: i64,
    pub kernel_time: i64,
    pub user_time: i64,
    pub dpc_time: i64,
    pub interrupt_time: i64,
    pub interrupt_count: u32,
}

// Pin the layouts to the offsets the kernel uses so a typo can't silently misread memory.
#[cfg(target_pointer_width = "64")]
const _: () = {
    use std::mem::{offset_of, size_of};
    assert!(size_of::<UnicodeString>() == 16);
    assert!(size_of::<SystemProcessInformation>() == 0x100);
    assert!(offset_of!(SystemProcessInformation, cycle_time) == 0x18);
    assert!(offset_of!(SystemProcessInformation, image_name) == 0x38);
    assert!(offset_of!(SystemProcessInformation, unique_process_id) == 0x50);
    assert!(offset_of!(SystemProcessInformation, handle_count) == 0x60);
    assert!(offset_of!(SystemProcessInformation, page_fault_count) == 0x80);
    assert!(offset_of!(SystemProcessInformation, pagefile_usage) == 0xB8);
    assert!(offset_of!(SystemProcessInformation, other_transfer_count) == 0xF8);
    assert!(size_of::<SystemProcessorPerformanceInformation>() == 48);
};

/// `SystemProcessorPerformanceInformation` information class.
pub(crate) const SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION: i32 = 8;
/// `SystemProcessorIdleCycleTimeInformation`: one `u64` idle cycle count per processor.
pub(crate) const SYSTEM_PROCESSOR_IDLE_CYCLE_TIME_INFORMATION: i32 = 83;
/// `SystemProcessorCycleTimeInformation`: one `u64` DPC/interrupt cycle count per processor.
pub(crate) const SYSTEM_PROCESSOR_CYCLE_TIME_INFORMATION: i32 = 108;

/// `SID_AND_ATTRIBUTES`, the payload of `TOKEN_USER`. `sid` points back into the same buffer.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct SidAndAttributes {
    pub sid: *const u8,
    pub attributes: u32,
}

/// `ENUM_SERVICE_STATUS_PROCESSW`. Both name pointers point back into the same buffer.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct EnumServiceStatusProcess {
    pub service_name: *const u16,
    pub display_name: *const u16,
    pub service_type: u32,
    pub current_state: u32,
    pub controls_accepted: u32,
    pub win32_exit_code: u32,
    pub service_specific_exit_code: u32,
    pub check_point: u32,
    pub wait_hint: u32,
    pub process_id: u32,
    pub service_flags: u32,
}

#[cfg(target_pointer_width = "64")]
const _: () = {
    use std::mem::{offset_of, size_of};
    assert!(size_of::<SidAndAttributes>() == 16);
    assert!(size_of::<EnumServiceStatusProcess>() == 56);
    assert!(offset_of!(EnumServiceStatusProcess, process_id) == 44);
};

// Not exported by windows-sys. Takes a processor group as input for per-group classes.
windows_link::link!("ntdll.dll" "system" fn NtQuerySystemInformationEx(
    system_information_class: i32,
    input_buffer: *const c_void,
    input_buffer_length: u32,
    system_information: *mut c_void,
    system_information_length: u32,
    return_length: *mut u32
) -> i32);

/// `CLIENT_ID`.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct ClientId {
    pub unique_process: usize,
    pub unique_thread: usize,
}

/// `SYSTEM_THREAD_INFORMATION`, the array that follows each `SYSTEM_PROCESS_INFORMATION`.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct SystemThreadInformation {
    pub kernel_time: i64,
    pub user_time: i64,
    pub create_time: i64,
    pub wait_time: u32,
    pub start_address: usize,
    pub client_id: ClientId,
    pub priority: i32,
    pub base_priority: i32,
    pub context_switches: u32,
    pub thread_state: u32,
    pub wait_reason: u32,
}

/// `PROCESS_BASIC_INFORMATION`.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct ProcessBasicInformation {
    pub exit_status: i32,
    pub peb_base_address: usize,
    pub affinity_mask: usize,
    pub base_priority: i32,
    pub unique_process_id: usize,
    pub inherited_from_unique_process_id: usize,
}

/// `PROCESS_HANDLE_TABLE_ENTRY_INFO`, the entries of `PROCESS_HANDLE_SNAPSHOT_INFORMATION`
/// (which starts with two `usize` fields: the handle count and a reserved word).
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct ProcessHandleTableEntryInfo {
    pub handle_value: usize,
    pub handle_count: usize,
    pub pointer_count: usize,
    pub granted_access: u32,
    pub object_type_index: u32,
    pub handle_attributes: u32,
    pub reserved: u32,
}

/// `OBJECT_TYPE_INFORMATION`, one entry of `ObjectTypesInformation`; its name follows it.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct ObjectTypeInformation {
    pub type_name: UnicodeString,
    pub counts: [u32; 13],
    pub generic_mapping: [u32; 4],
    pub valid_access_mask: u32,
    pub security_required: u8,
    pub maintain_handle_count: u8,
    pub type_index: u8,
    pub reserved_byte: u8,
    pub pool_type: u32,
    pub default_paged_pool_charge: u32,
    pub default_non_paged_pool_charge: u32,
}

/// `QUERY_SERVICE_CONFIGW`. The string pointers point back into the same buffer.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct QueryServiceConfig {
    pub service_type: u32,
    pub start_type: u32,
    pub error_control: u32,
    pub binary_path_name: *const u16,
    pub load_order_group: *const u16,
    pub tag_id: u32,
    pub dependencies: *const u16,
    pub service_start_name: *const u16,
    pub display_name: *const u16,
}

#[cfg(target_pointer_width = "64")]
const _: () = {
    use std::mem::{offset_of, size_of};
    assert!(size_of::<SystemThreadInformation>() == 0x50);
    assert!(offset_of!(SystemThreadInformation, start_address) == 0x20);
    assert!(offset_of!(SystemThreadInformation, client_id) == 0x28);
    assert!(offset_of!(SystemThreadInformation, wait_reason) == 0x48);
    assert!(size_of::<ProcessBasicInformation>() == 48);
    assert!(offset_of!(ProcessBasicInformation, peb_base_address) == 8);
    assert!(size_of::<ProcessHandleTableEntryInfo>() == 40);
    assert!(size_of::<ObjectTypeInformation>() == 0x68);
    assert!(offset_of!(ObjectTypeInformation, type_index) == 0x5A);
    assert!(size_of::<QueryServiceConfig>() == 64);
    assert!(offset_of!(QueryServiceConfig, service_start_name) == 48);
};

/// `ObjectNameInformation` / `ObjectTypesInformation` classes for `NtQueryObject`.
pub(crate) const OBJECT_NAME_INFORMATION: i32 = 1;
pub(crate) const OBJECT_TYPES_INFORMATION: i32 = 3;

// Not exported by windows-sys.
windows_link::link!("ntdll.dll" "system" fn NtSuspendProcess(process: *mut c_void) -> i32);
windows_link::link!("ntdll.dll" "system" fn NtResumeProcess(process: *mut c_void) -> i32);
