//! Acting on processes: suspend and resume, priority, affinity, dump files, plus starting new
//! programs and process-wide hardening.
//!
//! Every action on an existing process takes the creation time the user saw and goes through
//! [`open_verified`], so a recycled PID can never redirect it to a different process.

use std::os::windows::io::{AsRawHandle, FromRawHandle};

use windows_sys::Wdk::System::Threading::{NtQueryInformationProcess, ProcessBreakOnTermination};
use windows_sys::Win32::Foundation::{FreeLibrary, GENERIC_WRITE, HANDLE, HMODULE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{CREATE_NEW, CreateFileW, FILE_ATTRIBUTE_NORMAL};
use windows_sys::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW, SetDefaultDllDirectories,
};
use windows_sys::Win32::System::Threading::{
    GetPriorityClass, GetProcessAffinityMask, PROCESS_DUP_HANDLE, PROCESS_QUERY_INFORMATION,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_INFORMATION, PROCESS_SUSPEND_RESUME, PROCESS_VM_READ,
    ProcessExtensionPointDisablePolicy, ProcessImageLoadPolicy, SetPriorityClass, SetProcessAffinityMask,
    SetProcessMitigationPolicy,
};
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

use crate::buffer::wide;
use crate::ntapi::{NtResumeProcess, NtSuspendProcess};
use crate::process::{open_process, open_verified};
use crate::{Error, Result};

/// Win32 priority classes, lowest to highest.
pub const PRIORITY_CLASSES: [(u32, &str); 6] = [
    (0x40, "Idle"),
    (0x4000, "Below normal"),
    (0x20, "Normal"),
    (0x8000, "Above normal"),
    (0x80, "High"),
    (0x100, "Realtime"),
];

/// Suspends every thread of the process.
pub fn suspend(pid: u32, create_time: i64) -> Result<()> {
    let process = open_verified(pid, create_time, PROCESS_SUSPEND_RESUME)?;
    // SAFETY: `process` is a live handle opened with PROCESS_SUSPEND_RESUME.
    let status = unsafe { NtSuspendProcess(process.as_raw_handle()) };
    if status < 0 { Err(Error::Nt { call: "NtSuspendProcess", status }) } else { Ok(()) }
}

/// Resumes every thread of the process.
pub fn resume(pid: u32, create_time: i64) -> Result<()> {
    let process = open_verified(pid, create_time, PROCESS_SUSPEND_RESUME)?;
    // SAFETY: `process` is a live handle opened with PROCESS_SUSPEND_RESUME.
    let status = unsafe { NtResumeProcess(process.as_raw_handle()) };
    if status < 0 { Err(Error::Nt { call: "NtResumeProcess", status }) } else { Ok(()) }
}

/// The process's priority class (one of [`PRIORITY_CLASSES`]).
pub fn priority_class(pid: u32) -> Result<u32> {
    let process = open_process(pid, PROCESS_QUERY_LIMITED_INFORMATION)?;
    // SAFETY: `process` is a live handle with query access.
    match unsafe { GetPriorityClass(process.as_raw_handle()) } {
        0 => Err(Error::last_win32("GetPriorityClass")),
        class => Ok(class),
    }
}

pub fn set_priority_class(pid: u32, create_time: i64, class: u32) -> Result<()> {
    if !PRIORITY_CLASSES.iter().any(|(c, _)| *c == class) {
        return Err(Error::Malformed { call: "SetPriorityClass" });
    }
    let process = open_verified(pid, create_time, PROCESS_SET_INFORMATION)?;
    // SAFETY: `process` is live with PROCESS_SET_INFORMATION and `class` is a valid class.
    if unsafe { SetPriorityClass(process.as_raw_handle(), class) } == 0 {
        return Err(Error::last_win32("SetPriorityClass"));
    }
    Ok(())
}

/// `(process mask, system mask)`: which logical processors (of the process's group) it may run on.
pub fn affinity(pid: u32) -> Result<(u64, u64)> {
    let process = open_process(pid, PROCESS_QUERY_LIMITED_INFORMATION)?;
    let (mut mine, mut system) = (0usize, 0usize);
    // SAFETY: `process` is live with query access; both out-pointers are valid.
    if unsafe { GetProcessAffinityMask(process.as_raw_handle(), &mut mine, &mut system) } == 0 {
        return Err(Error::last_win32("GetProcessAffinityMask"));
    }
    Ok((mine as u64, system as u64))
}

pub fn set_affinity(pid: u32, create_time: i64, mask: u64) -> Result<()> {
    let mask = usize::try_from(mask).map_err(|_| Error::Malformed { call: "SetProcessAffinityMask" })?;
    if mask == 0 {
        return Err(Error::Malformed { call: "SetProcessAffinityMask" });
    }
    let process = open_verified(pid, create_time, PROCESS_SET_INFORMATION)?;
    // SAFETY: `process` is live with PROCESS_SET_INFORMATION; the mask is non-zero.
    if unsafe { SetProcessAffinityMask(process.as_raw_handle(), mask) } == 0 {
        return Err(Error::last_win32("SetProcessAffinityMask"));
    }
    Ok(())
}

/// Whether Windows marks the process critical: ending (or, for some, suspending) it bugchecks
/// the machine. `None` when that can't be read.
pub fn is_critical(pid: u32) -> Option<bool> {
    let process = open_process(pid, PROCESS_QUERY_LIMITED_INFORMATION)
        .or_else(|_| open_process(pid, PROCESS_QUERY_INFORMATION))
        .ok()?;
    let mut flag = 0u32;
    // SAFETY: `process` is live and `flag` is a writable u32 of the size passed.
    let status = unsafe {
        NtQueryInformationProcess(
            process.as_raw_handle(),
            ProcessBreakOnTermination,
            (&raw mut flag).cast(),
            size_of::<u32>() as u32,
            std::ptr::null_mut(),
        )
    };
    (status >= 0).then_some(flag != 0)
}

/// `MiniDumpWriteDump`, resolved at run time from System32's dbghelp.dll.
type MiniDumpWriteDump =
    unsafe extern "system" fn(HANDLE, u32, HANDLE, u32, *const u8, *const u8, *const u8) -> windows_sys::core::BOOL;

/// A full-memory dump with handles, threads and unloaded modules: what Task Manager writes.
const DUMP_TYPE: u32 = 0x2 | 0x4 | 0x20 | 0x800 | 0x1000;

struct Library(HMODULE);
impl Drop for Library {
    fn drop(&mut self) {
        // SAFETY: the module came from LoadLibraryExW and is freed exactly once.
        unsafe { FreeLibrary(self.0) };
    }
}

/// Loads a DLL from System32 only, never from the application or current directory.
fn system_library(name: &str) -> Result<Library> {
    let name = wide(name);
    // SAFETY: `name` is NUL-terminated; the reserved file handle must be null.
    let module = unsafe { LoadLibraryExW(name.as_ptr(), std::ptr::null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32) };
    if module.is_null() {
        return Err(Error::last_win32("LoadLibraryExW"));
    }
    Ok(Library(module))
}

/// Writes a full dump of the process to `path`, which must not exist yet.
pub fn create_dump(pid: u32, create_time: i64, path: &str) -> Result<()> {
    if pid == std::process::id() {
        // Dumping yourself from the inside can deadlock on loader and heap locks.
        return Err(Error::Malformed { call: "MiniDumpWriteDump(self)" });
    }
    let process = open_verified(pid, create_time, PROCESS_QUERY_INFORMATION | PROCESS_VM_READ | PROCESS_DUP_HANDLE)?;
    let dbghelp = system_library("dbghelp.dll")?;
    // SAFETY: `dbghelp` is a loaded module and the name is a NUL-terminated ASCII string.
    let address = unsafe { GetProcAddress(dbghelp.0, c"MiniDumpWriteDump".as_ptr().cast()) }
        .ok_or_else(|| Error::last_win32("GetProcAddress"))?;
    // SAFETY: dbghelp exports MiniDumpWriteDump with exactly this documented signature.
    let write_dump: MiniDumpWriteDump = unsafe { std::mem::transmute(address) };

    let wide_path = wide(path);
    // SAFETY: `wide_path` is NUL-terminated; CREATE_NEW refuses to overwrite an existing file.
    let raw = unsafe {
        CreateFileW(
            wide_path.as_ptr(),
            GENERIC_WRITE,
            0,
            std::ptr::null(),
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };
    if raw == INVALID_HANDLE_VALUE {
        return Err(Error::last_win32("CreateFileW(dump)"));
    }
    // SAFETY: `raw` is a file handle we just created, and nothing else owns it.
    let file = unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(raw) };
    // SAFETY: the process and file handles are live with the access MiniDumpWriteDump needs, and
    // the optional parameters are null. `dbghelp` stays loaded for the call.
    let ok = unsafe {
        write_dump(
            process.as_raw_handle(),
            pid,
            file.as_raw_handle(),
            DUMP_TYPE,
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    if ok == 0 {
        let error = Error::last_win32("MiniDumpWriteDump");
        drop(file);
        let _ = std::fs::remove_file(path);
        return Err(error);
    }
    Ok(())
}

/// Splits a Run-box style command into the program and its arguments: a quoted program runs to
/// its closing quote, otherwise to the first space.
pub fn split_command(command: &str) -> (String, String) {
    let command = command.trim();
    if let Some(rest) = command.strip_prefix('"') {
        return match rest.split_once('"') {
            Some((program, args)) => (program.to_owned(), args.trim().to_owned()),
            None => (rest.to_owned(), String::new()),
        };
    }
    match command.split_once(char::is_whitespace) {
        Some((program, args)) => (program.to_owned(), args.trim().to_owned()),
        None => (command.to_owned(), String::new()),
    }
}

fn shell_execute(verb: &str, file: &str, parameters: &str) -> Result<()> {
    let (verb, file, parameters) = (wide(verb), wide(file), wide(parameters));
    // SAFETY: all strings are NUL-terminated; a null window and directory are allowed.
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            parameters.as_ptr(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    // ShellExecute returns a value greater than 32 on success, else an error code.
    if result as usize <= 32 {
        return Err(Error::Win32 { call: "ShellExecuteW", code: result as usize as u32 });
    }
    Ok(())
}

/// Starts a program like the Run box does, optionally asking for elevation (UAC). There's no
/// run-as-another-user: that would mean handling someone's password.
pub fn run(command: &str, elevated: bool) -> Result<()> {
    let (program, arguments) = split_command(command);
    if program.is_empty() {
        return Err(Error::Malformed { call: "ShellExecuteW" });
    }
    shell_execute(if elevated { "runas" } else { "open" }, &program, &arguments)
}

/// Opens Explorer with `path` selected.
pub fn open_file_location(path: &str) -> Result<()> {
    // Windows paths can't contain quotes, so anything with one isn't a real path.
    if path.is_empty() || path.contains('"') {
        return Err(Error::Malformed { call: "open_file_location" });
    }
    shell_execute("open", "explorer.exe", &format!("/select,\"{path}\""))
}

/// Opens a folder in Explorer.
pub fn open_folder(path: &str) -> Result<()> {
    if path.is_empty() || path.contains('"') {
        return Err(Error::Malformed { call: "open_folder" });
    }
    shell_execute("open", "explorer.exe", &format!("\"{path}\""))
}

/// Process-wide hardening, applied once at startup:
/// - later `LoadLibrary` calls search System32 (and explicitly added folders) only, never the
///   application or current directory, so a planted DLL next to the exe can't be picked up;
/// - images may not load from remote (UNC/WebDAV) shares or low-integrity files, and System32
///   copies are preferred;
/// - legacy injection points (AppInit DLLs, Winsock LSPs, global window hooks, legacy IMEs) are
///   disabled.
///
/// Returns how many of the three policies applied (older Windows versions may reject some).
pub fn harden_process() -> usize {
    const IMAGE_LOAD_NO_REMOTE: u32 = 1 << 0;
    const IMAGE_LOAD_NO_LOW_LABEL: u32 = 1 << 1;
    const IMAGE_LOAD_PREFER_SYSTEM32: u32 = 1 << 2;
    const DISABLE_EXTENSION_POINTS: u32 = 1 << 0;
    let mut applied = 0;
    // SAFETY: no pointers; the flag is a documented LOAD_LIBRARY_SEARCH value.
    if unsafe { SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_SYSTEM32) } != 0 {
        applied += 1;
    }
    for (policy, flags) in [
        (ProcessImageLoadPolicy, IMAGE_LOAD_NO_REMOTE | IMAGE_LOAD_NO_LOW_LABEL | IMAGE_LOAD_PREFER_SYSTEM32),
        (ProcessExtensionPointDisablePolicy, DISABLE_EXTENSION_POINTS),
    ] {
        // SAFETY: each policy structure is a single DWORD of flags, passed by pointer with its size.
        if unsafe { SetProcessMitigationPolicy(policy, (&raw const flags).cast(), size_of::<u32>()) } != 0 {
            applied += 1;
        }
    }
    applied
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_commands_like_the_run_box() {
        assert_eq!(split_command("notepad"), ("notepad".into(), String::new()));
        assert_eq!(split_command("  notepad  a.txt b "), ("notepad".into(), "a.txt b".into()));
        assert_eq!(split_command(r#""C:\Program Files\x.exe" -v"#), (r"C:\Program Files\x.exe".into(), "-v".into()));
        assert_eq!(split_command(r#""unterminated"#), ("unterminated".into(), String::new()));
        assert_eq!(split_command(""), (String::new(), String::new()));
    }

    #[test]
    fn reads_own_priority_affinity_and_criticality() {
        let pid = std::process::id();
        assert!(PRIORITY_CLASSES.iter().any(|(class, _)| *class == priority_class(pid).unwrap()));
        let (mine, system) = affinity(pid).unwrap();
        assert!(mine != 0 && mine & !system == 0);
        assert_eq!(is_critical(pid), Some(false));
    }

    #[test]
    fn actions_refuse_a_recycled_pid() {
        let pid = std::process::id();
        assert_eq!(suspend(pid, 1), Err(Error::ProcessGone));
        assert_eq!(set_priority_class(pid, 1, 0x20), Err(Error::ProcessGone));
        assert_eq!(set_affinity(pid, 1, 1), Err(Error::ProcessGone));
        assert!(set_priority_class(pid, 1, 0x1234).is_err(), "unknown class rejected first");
    }

    #[test]
    fn dumps_another_process_to_a_new_file_only() {
        let mut child = std::process::Command::new("cmd")
            .args(["/c", "pause"])
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let pid = child.id();
        let created = crate::process::enumerate().unwrap().into_iter().find(|p| p.pid == pid).unwrap().create_time;
        let path = std::env::temp_dir().join(format!("ferrotop-dump-test-{pid}.dmp"));
        let path = path.to_str().unwrap();
        let _ = std::fs::remove_file(path);
        let result = create_dump(pid, created, path);
        let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        let again = create_dump(pid, created, path);
        let _ = std::fs::remove_file(path);
        let _ = child.kill();
        let _ = child.wait();
        result.expect("dump written");
        assert!(size > 100_000, "a full dump of cmd.exe is large; got {size}");
        assert!(again.is_err(), "never overwrites");
    }
}
