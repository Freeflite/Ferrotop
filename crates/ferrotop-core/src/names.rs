//! Human-readable names for the Windows constants the property and system views show.

/// `KTHREAD_STATE`.
pub fn thread_state(state: u32, wait_reason: u32) -> String {
    match state {
        0 => "Initialized".into(),
        1 => "Ready".into(),
        2 => "Running".into(),
        3 => "Standby".into(),
        4 => "Terminated".into(),
        5 => format!("Wait: {}", wait_reason_name(wait_reason)),
        6 => "Transition".into(),
        7 => "Deferred ready".into(),
        9 => "Waiting for swap-in".into(),
        other => format!("State {other}"),
    }
}

/// `KWAIT_REASON`, without the `Wr` prefix.
fn wait_reason_name(reason: u32) -> String {
    const NAMES: [&str; 43] = [
        "Executive",
        "FreePage",
        "PageIn",
        "PoolAllocation",
        "DelayExecution",
        "Suspended",
        "UserRequest",
        "Executive",
        "FreePage",
        "PageIn",
        "PoolAllocation",
        "DelayExecution",
        "Suspended",
        "UserRequest",
        "EventPair",
        "Queue",
        "LpcReceive",
        "LpcReply",
        "VirtualMemory",
        "PageOut",
        "Rendezvous",
        "KeyedEvent",
        "Terminated",
        "ProcessInSwap",
        "CpuRateControl",
        "CalloutStack",
        "Kernel",
        "Resource",
        "PushLock",
        "Mutex",
        "QuantumEnd",
        "DispatchInt",
        "Preempted",
        "YieldExecution",
        "FastMutex",
        "GuardedMutex",
        "Rundown",
        "AlertByThreadId",
        "DeferredPreempt",
        "PhysicalFault",
        "IoRing",
        "MdlCache",
        "Rcu",
    ];
    NAMES.get(reason as usize).map_or_else(|| format!("{reason}"), |name| (*name).to_owned())
}

/// A thread's base priority, named relative to its process's base priority as Windows does.
pub fn thread_priority(base: i32, process_base: i32) -> String {
    if base == 31 || (base == 15 && process_base < 16) {
        return "Time critical".into();
    }
    if base == 1 || (base == 16 && process_base >= 16) {
        return "Idle".into();
    }
    match base - process_base {
        0 => "Normal".into(),
        1 => "Above normal".into(),
        2 => "Highest".into(),
        -1 => "Below normal".into(),
        -2 => "Lowest".into(),
        delta => format!("{base} ({delta:+})"),
    }
}

/// `PAGE_*` protection: `RX`, `RW`, `RWX`, `WC`, with `+G` (guard) and `NC`/`WC` modifiers.
pub fn protection(protect: u32) -> String {
    if protect == 0 {
        return String::new();
    }
    let base = match protect & 0xFF {
        0x01 => "NA",
        0x02 => "R",
        0x04 => "RW",
        0x08 => "WC",
        0x10 => "X",
        0x20 => "RX",
        0x40 => "RWX",
        0x80 => "WCX",
        _ => "?",
    };
    let mut text = base.to_owned();
    if protect & 0x100 != 0 {
        text.push_str("+G");
    }
    if protect & 0x200 != 0 {
        text.push_str("+NC");
    }
    if protect & 0x400 != 0 {
        text.push_str("+WCM");
    }
    text
}

pub fn memory_state(state: u32) -> &'static str {
    match state {
        0x1000 => "Commit",
        0x2000 => "Reserve",
        0x10000 => "Free",
        _ => "",
    }
}

pub fn memory_type(kind: u32) -> &'static str {
    match kind {
        0x100_0000 => "Image",
        0x4_0000 => "Mapped",
        0x2_0000 => "Private",
        _ => "",
    }
}

pub fn service_state(state: u32) -> &'static str {
    match state {
        1 => "Stopped",
        2 => "Starting",
        3 => "Stopping",
        4 => "Running",
        5 => "Continuing",
        6 => "Pausing",
        7 => "Paused",
        _ => "Unknown",
    }
}

pub fn service_start(start_type: Option<u32>, delayed: bool) -> &'static str {
    match start_type {
        Some(0) => "Boot",
        Some(1) => "System",
        Some(2) if delayed => "Automatic (delayed)",
        Some(2) => "Automatic",
        Some(3) => "Manual",
        Some(4) => "Disabled",
        _ => "",
    }
}

pub fn service_kind(kind: u32) -> &'static str {
    match kind & 0x3F {
        0x1 => "Kernel driver",
        0x2 => "File system driver",
        0x10 => "Own process",
        0x20 => "Shared process",
        k if k & 0x3 != 0 => "Driver",
        _ => "Service",
    }
}

/// `MIB_TCP_STATE`.
pub fn tcp_state(state: u32) -> &'static str {
    match state {
        1 => "Closed",
        2 => "Listen",
        3 => "SYN sent",
        4 => "SYN received",
        5 => "Established",
        6 => "FIN wait 1",
        7 => "FIN wait 2",
        8 => "Close wait",
        9 => "Closing",
        10 => "Last ACK",
        11 => "Time wait",
        12 => "Delete TCB",
        _ => "",
    }
}

/// Mandatory label SID (`S-1-16-n`) → integrity level.
pub fn integrity(sid: &str) -> String {
    match sid.strip_prefix("S-1-16-").and_then(|rid| rid.parse::<u32>().ok()) {
        Some(0) => "Untrusted".into(),
        Some(0x1000) => "Low".into(),
        Some(0x2000) => "Medium".into(),
        Some(0x2100) => "Medium plus".into(),
        Some(0x3000) => "High".into(),
        Some(0x4000) => "System".into(),
        Some(0x5000) => "Protected process".into(),
        _ => sid.into(),
    }
}

pub fn elevation_type(kind: u32) -> &'static str {
    match kind {
        1 => "Default",
        2 => "Full",
        3 => "Limited",
        _ => "",
    }
}

/// `SE_GROUP_*` flags, most telling first.
pub fn group_flags(attributes: u32) -> String {
    let mut flags = Vec::new();
    // The mandatory label isn't enabled or disabled like other groups.
    if attributes & 0x20 != 0 {
        return if attributes & 0x40 != 0 { "Integrity (enabled)".into() } else { "Integrity".into() };
    }
    if attributes & 0x10 != 0 {
        flags.push("Deny only");
    } else if attributes & 0x4 != 0 {
        flags.push("Enabled");
    } else {
        flags.push("Disabled");
    }
    if attributes & 0x1 != 0 {
        flags.push("Mandatory");
    }
    if attributes & 0x8 != 0 {
        flags.push("Owner");
    }
    if attributes & 0xC000_0000 == 0xC000_0000 {
        flags.push("Logon ID");
    }
    if attributes & 0x2000_0000 != 0 {
        flags.push("Resource");
    }
    flags.join(", ")
}

/// `SE_PRIVILEGE_*` flags.
pub fn privilege_flags(attributes: u32) -> &'static str {
    match (attributes & 0x2 != 0, attributes & 0x1 != 0) {
        (true, true) => "Enabled (default)",
        (true, false) => "Enabled",
        (false, true) => "Disabled (default enabled)",
        (false, false) => "Disabled",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_common_values() {
        assert_eq!(thread_state(5, 6), "Wait: UserRequest");
        assert_eq!(thread_state(5, 13), "Wait: UserRequest");
        assert_eq!(thread_state(5, 999), "Wait: 999");
        assert_eq!(protection(0x20), "RX");
        assert_eq!(protection(0x104), "RW+G");
        assert_eq!(protection(0), "");
        assert_eq!(service_start(Some(2), true), "Automatic (delayed)");
        assert_eq!(integrity("S-1-16-12288"), "High");
        assert_eq!(integrity("S-1-5-18"), "S-1-5-18");
        assert_eq!(group_flags(0x7), "Enabled, Mandatory");
        assert_eq!(group_flags(0x10), "Deny only");
        assert_eq!(thread_priority(8, 8), "Normal");
        assert_eq!(thread_priority(10, 8), "Highest");
        assert_eq!(thread_priority(15, 8), "Time critical");
        assert_eq!(thread_priority(12, 8), "12 (+4)");
    }
}
