//! A process's open handles: object type, granted access and, where it's safe to ask, the name.
//!
//! Names come from duplicating each handle into Ferrotop (with no access rights) and asking the
//! kernel for the object's name. Asking for a *file* object's name can block forever when another
//! thread is stuck in synchronous I/O on it (the classic named-pipe hang), so file names are
//! queried on a separate thread with a timeout. If one hangs, Ferrotop gives up on file names for
//! that listing and the stuck thread finishes whenever the I/O does.

use std::collections::HashMap;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::sync::mpsc;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use windows_sys::Wdk::Foundation::NtQueryObject;
use windows_sys::Wdk::System::Threading::{NtQueryInformationProcess, ProcessHandleInformation};
use windows_sys::Win32::Foundation::{DuplicateHandle, HANDLE};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetProcessId, GetProcessIdOfThread, GetThreadId, PROCESS_DUP_HANDLE, PROCESS_QUERY_INFORMATION,
    PROCESS_QUERY_LIMITED_INFORMATION, THREAD_QUERY_LIMITED_INFORMATION,
};

use crate::buffer::{AlignedBuffer, is_size_status, read_pod, unicode_string_in};
use crate::memory::{dos_devices, dos_path};
use crate::ntapi::{
    OBJECT_NAME_INFORMATION, OBJECT_TYPES_INFORMATION, ObjectTypeInformation, ProcessHandleTableEntryInfo,
    UnicodeString,
};
use crate::process::open_process;
use crate::{Error, Result};

/// How long one file-name query may take before Ferrotop assumes it's hung.
const FILE_NAME_TIMEOUT: Duration = Duration::from_millis(250);
/// Queries per listing; the System process alone can hold hundreds of thousands of handles.
const MAX_NAMED: usize = 100_000;
const MAX_SNAPSHOT: usize = 64 << 20;
/// Access masks System Informer found on handles whose name queries hang (synchronous pipes).
const HANG_PRONE_FILE_ACCESS: [u32; 6] = [0x0012_019F, 0x0012_0189, 0x0012_008D, 0x0010_0000, 0x0016_019F, 0x001A_019F];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandleTarget {
    Process(u32),
    Thread { tid: u32, pid: u32 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandleInfo {
    pub handle: u64,
    pub type_name: String,
    pub name: Option<String>,
    /// For process and thread handles: which one.
    pub target: Option<HandleTarget>,
    pub granted_access: u32,
    /// `OBJ_INHERIT` (2), `OBJ_PROTECT_CLOSE` (1).
    pub attributes: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RawHandle {
    pub value: usize,
    pub type_index: u32,
    pub granted_access: u32,
    pub attributes: u32,
}

/// The handles `pid` holds. Names are best effort: they need `PROCESS_DUP_HANDLE` on the process.
pub fn handles(pid: u32) -> Result<Vec<HandleInfo>> {
    let (process, can_duplicate) = match open_process(pid, PROCESS_QUERY_INFORMATION | PROCESS_DUP_HANDLE) {
        Ok(process) => (process, true),
        Err(_) => (open_process(pid, PROCESS_QUERY_INFORMATION)?, false),
    };
    let raw = snapshot(&process)?;
    let types = object_types();
    let devices = dos_devices();
    let mut handles: Vec<HandleInfo> = raw
        .iter()
        .map(|h| HandleInfo {
            handle: h.value as u64,
            type_name: types.get(&h.type_index).cloned().unwrap_or_else(|| format!("Type {}", h.type_index)),
            name: None,
            target: None,
            granted_access: h.granted_access,
            attributes: h.attributes,
        })
        .collect();
    if !can_duplicate {
        return Ok(handles);
    }

    let mut files = FileNamer::new();
    for (info, raw) in handles.iter_mut().zip(&raw).take(MAX_NAMED) {
        match info.type_name.as_str() {
            "Process" => {
                if let Some(dup) = duplicate(&process, raw.value, PROCESS_QUERY_LIMITED_INFORMATION) {
                    // SAFETY: `dup` is a live process handle with query access.
                    let target = unsafe { GetProcessId(dup.as_raw_handle()) };
                    info.target = (target != 0).then_some(HandleTarget::Process(target));
                }
            }
            "Thread" => {
                if let Some(dup) = duplicate(&process, raw.value, THREAD_QUERY_LIMITED_INFORMATION) {
                    // SAFETY: `dup` is a live thread handle with query access.
                    let tid = unsafe { GetThreadId(dup.as_raw_handle()) };
                    // SAFETY: as above.
                    let owner = unsafe { GetProcessIdOfThread(dup.as_raw_handle()) };
                    info.target = (tid != 0).then_some(HandleTarget::Thread { tid, pid: owner });
                }
            }
            "File" => {
                if !HANG_PRONE_FILE_ACCESS.contains(&raw.granted_access)
                    && let Some(dup) = duplicate(&process, raw.value, 0)
                {
                    info.name = files.name(dup).map(|name| dos_path(&name, &devices));
                }
            }
            _ => {
                if let Some(dup) = duplicate(&process, raw.value, 0) {
                    info.name = object_name(&dup).filter(|name| !name.is_empty()).map(|name| pretty_key_name(&name));
                }
            }
        }
    }
    Ok(handles)
}

/// `NtQueryInformationProcess(ProcessHandleInformation)`, grown until it fits.
fn snapshot(process: &OwnedHandle) -> Result<Vec<RawHandle>> {
    const CALL: &str = "NtQueryInformationProcess(ProcessHandleInformation)";
    let mut buffer = AlignedBuffer::new(64 * 1024);
    for _ in 0..8 {
        let mut needed = 0u32;
        // SAFETY: `process` is live; the pointer/length pair describes memory owned by `buffer`;
        // `needed` is a valid out-pointer.
        let status = unsafe {
            NtQueryInformationProcess(
                process.as_raw_handle(),
                ProcessHandleInformation,
                buffer.as_mut_ptr(),
                buffer.capacity_u32(),
                &mut needed,
            )
        };
        if status >= 0 {
            return parse_snapshot(buffer.bytes(buffer.capacity())).ok_or(Error::Malformed { call: CALL });
        }
        if !is_size_status(status) {
            return Err(Error::Nt { call: CALL, status });
        }
        let wanted = (needed as usize).max(buffer.capacity() * 2) + 4096;
        if wanted > MAX_SNAPSHOT {
            return Err(Error::Malformed { call: CALL });
        }
        buffer = AlignedBuffer::new(wanted);
    }
    Err(Error::Malformed { call: CALL })
}

/// `PROCESS_HANDLE_SNAPSHOT_INFORMATION`: a count, a reserved word, then the entries.
pub(crate) fn parse_snapshot(bytes: &[u8]) -> Option<Vec<RawHandle>> {
    let count: usize = read_pod(bytes, 0)?;
    let first = 2 * size_of::<usize>();
    let fits = bytes.len().saturating_sub(first) / size_of::<ProcessHandleTableEntryInfo>();
    if count > fits {
        return None;
    }
    (0..count)
        .map(|i| {
            let entry: ProcessHandleTableEntryInfo =
                read_pod(bytes, first + i * size_of::<ProcessHandleTableEntryInfo>())?;
            Some(RawHandle {
                value: entry.handle_value,
                type_index: entry.object_type_index,
                granted_access: entry.granted_access,
                attributes: entry.handle_attributes,
            })
        })
        .collect()
}

/// Object type names by type index, read once (types only appear when drivers register them).
fn object_types() -> HashMap<u32, String> {
    static TYPES: OnceLock<Mutex<HashMap<u32, String>>> = OnceLock::new();
    let cache = TYPES.get_or_init(|| Mutex::new(HashMap::new()));
    let mut types = cache.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if types.is_empty()
        && let Some(fresh) = query_object_types()
    {
        *types = fresh;
    }
    types.clone()
}

fn query_object_types() -> Option<HashMap<u32, String>> {
    let mut buffer = AlignedBuffer::new(32 * 1024);
    for _ in 0..6 {
        let mut needed = 0u32;
        // SAFETY: a null handle with this class asks about all types; the pointer/length pair
        // describes memory owned by `buffer`; `needed` is a valid out-pointer.
        let status = unsafe {
            NtQueryObject(
                std::ptr::null_mut(),
                OBJECT_TYPES_INFORMATION,
                buffer.as_mut_ptr(),
                buffer.capacity_u32(),
                &mut needed,
            )
        };
        if status >= 0 {
            return parse_object_types(buffer.bytes(buffer.capacity()));
        }
        if !is_size_status(status) {
            return None;
        }
        buffer = AlignedBuffer::new((needed as usize).max(buffer.capacity() * 2).min(1 << 20));
    }
    None
}

/// `OBJECT_TYPES_INFORMATION`: a count, then pointer-aligned `OBJECT_TYPE_INFORMATION` entries,
/// each followed by its name.
pub(crate) fn parse_object_types(bytes: &[u8]) -> Option<HashMap<u32, String>> {
    let count: u32 = read_pod(bytes, 0)?;
    let align = |n: usize| n.checked_add(size_of::<usize>() - 1).map(|n| n & !(size_of::<usize>() - 1));
    let mut offset = align(size_of::<u32>())?;
    let mut types = HashMap::new();
    for _ in 0..count.min(1024) {
        let entry: ObjectTypeInformation = read_pod(bytes, offset)?;
        let name = unicode_string_in(bytes, &entry.type_name)?;
        types.insert(u32::from(entry.type_index), name);
        offset = offset
            .checked_add(size_of::<ObjectTypeInformation>())?
            .checked_add(align(usize::from(entry.type_name.maximum_length))?)?;
    }
    Some(types)
}

fn duplicate(process: &OwnedHandle, value: usize, access: u32) -> Option<OwnedHandle> {
    let mut duplicate: HANDLE = std::ptr::null_mut();
    // SAFETY: no preconditions; returns our own process's pseudo-handle.
    let this_process = unsafe { GetCurrentProcess() };
    // SAFETY: `process` is live with PROCESS_DUP_HANDLE; `value` is only interpreted inside that
    // process; the target is our own pseudo-handle; `duplicate` is a valid out-pointer.
    let ok = unsafe {
        DuplicateHandle(process.as_raw_handle(), value as HANDLE, this_process, &mut duplicate, access, 0, 0)
    };
    // SAFETY: on success `duplicate` is a new handle in our process that nothing else owns.
    (ok != 0 && !duplicate.is_null()).then(|| unsafe { OwnedHandle::from_raw_handle(duplicate) })
}

/// The object's name, e.g. `\BaseNamedObjects\Foo` or `\REGISTRY\MACHINE\SOFTWARE`.
fn object_name(handle: &OwnedHandle) -> Option<String> {
    let mut buffer = AlignedBuffer::new(1024);
    for _ in 0..4 {
        let mut needed = 0u32;
        // SAFETY: `handle` is live; the pointer/length pair describes memory owned by `buffer`;
        // `needed` is a valid out-pointer.
        let status = unsafe {
            NtQueryObject(
                handle.as_raw_handle(),
                OBJECT_NAME_INFORMATION,
                buffer.as_mut_ptr(),
                buffer.capacity_u32(),
                &mut needed,
            )
        };
        if status >= 0 {
            let bytes = buffer.bytes(buffer.capacity());
            let name: UnicodeString = read_pod(bytes, 0)?;
            return unicode_string_in(bytes, &name);
        }
        if !is_size_status(status) || needed as usize > 64 * 1024 {
            return None;
        }
        buffer = AlignedBuffer::new(needed as usize + 64);
    }
    None
}

/// Queries file names on a worker thread so a hung query can't hang Ferrotop.
struct FileNamer {
    jobs: Option<mpsc::Sender<OwnedHandle>>,
    results: Option<mpsc::Receiver<Option<String>>>,
}

impl FileNamer {
    fn new() -> Self {
        Self { jobs: None, results: None }
    }

    fn name(&mut self, file: OwnedHandle) -> Option<String> {
        if self.jobs.is_none() {
            let (jobs, inbox) = mpsc::channel::<OwnedHandle>();
            let (outbox, results) = mpsc::channel();
            std::thread::Builder::new()
                .name("ferrotop-file-names".into())
                .spawn(move || {
                    for file in inbox {
                        if outbox.send(object_name(&file)).is_err() {
                            break;
                        }
                    }
                })
                .ok()?;
            (self.jobs, self.results) = (Some(jobs), Some(results));
        }
        self.jobs.as_ref()?.send(file).ok()?;
        match self.results.as_ref()?.recv_timeout(FILE_NAME_TIMEOUT) {
            Ok(name) => name,
            Err(_) => {
                // Hung (or the worker died). Abandon this worker; stop asking for file names.
                self.jobs = None;
                self.results = None;
                self.give_up();
                None
            }
        }
    }

    fn give_up(&mut self) {
        // A sender that's always closed makes every later `name` call fail fast.
        let (jobs, _) = mpsc::channel();
        self.jobs = Some(jobs);
    }
}

/// `\REGISTRY\MACHINE\SOFTWARE` → `HKLM\SOFTWARE`; other names unchanged.
pub fn pretty_key_name(name: &str) -> String {
    const ROOTS: [(&str, &str); 2] = [(r"\REGISTRY\MACHINE", "HKLM"), (r"\REGISTRY\USER", "HKU")];
    for (prefix, short) in ROOTS {
        if let Some(rest) = name.strip_prefix(prefix)
            && (rest.is_empty() || rest.starts_with('\\'))
        {
            return format!("{short}{rest}");
        }
    }
    name.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_own_handles_with_types_and_names() {
        let me = std::process::id();
        // Hold a named event and a file so they show up.
        let path = std::env::temp_dir().join(format!("ferrotop-handles-{me}.tmp"));
        let file = std::fs::File::create(&path).unwrap();
        let handles = handles(me).expect("handles");
        drop(file);
        let _ = std::fs::remove_file(&path);
        assert!(handles.len() > 5);
        assert!(handles.iter().any(|h| h.type_name == "File"));
        let file_name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            handles.iter().any(|h| h.name.as_deref().is_some_and(|n| n.ends_with(&file_name) && n.contains(":\\"))),
            "own temp file listed by DOS path"
        );
        assert!(handles.iter().any(|h| h.type_name == "Key" && h.name.as_deref().is_some_and(|n| n.starts_with("HK"))));
    }

    #[test]
    fn shortens_registry_roots() {
        assert_eq!(pretty_key_name(r"\REGISTRY\MACHINE\SOFTWARE\x"), r"HKLM\SOFTWARE\x");
        assert_eq!(pretty_key_name(r"\REGISTRY\USER"), "HKU");
        assert_eq!(pretty_key_name(r"\REGISTRY\MACHINEX"), r"\REGISTRY\MACHINEX");
    }

    #[test]
    fn rejects_counts_larger_than_the_buffer() {
        let mut bytes = vec![0u8; 16 + 40];
        bytes[0] = 2; // claims two entries, holds one
        assert_eq!(parse_snapshot(&bytes), None);
        bytes[0] = 1;
        assert_eq!(parse_snapshot(&bytes).map(|h| h.len()), Some(1));
    }

    #[test]
    fn parsers_never_panic_on_arbitrary_input() {
        let mut snapshot = vec![0u8; 16 + 80];
        snapshot[0] = 2;
        crate::fuzz::mutate(&snapshot, 5_000, |bytes| {
            let _ = parse_snapshot(bytes);
        });
        let mut types = vec![0u8; 8 + 0x68 + 16];
        types[0] = 1;
        crate::fuzz::mutate(&types, 5_000, |bytes| {
            let _ = parse_object_types(bytes);
        });
    }
}
