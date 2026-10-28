//! Running Ferrotop as a Windows service (the history recorder), and installing and removing it.
//!
//! The service runs as LocalSystem, so it's built to take nothing from ordinary users: it reads
//! no requests, writes only to its own folder (which only SYSTEM and Administrators can write),
//! and keeps just the privileges it needs. Its program file is a copy in Program Files, which
//! users can't modify, never the copy the user launched (often somewhere writable, like Downloads).

use std::ffi::OsString;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::io::FromRawHandle;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    ERROR_ALREADY_EXISTS, ERROR_SERVICE_DOES_NOT_EXIST, ERROR_SERVICE_EXISTS, ERROR_SERVICE_NOT_ACTIVE, HLOCAL,
    LocalFree, NO_ERROR,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, GetNamedSecurityInfoW, SDDL_REVISION_1, SE_FILE_OBJECT,
};
use windows_sys::Win32::Security::{
    IsWellKnownSid, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES,
    WinBuiltinAdministratorsSid, WinLocalSystemSid,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateDirectoryW, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, GetFileAttributesW,
    INVALID_FILE_ATTRIBUTES, MoveFileExW,
};
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::System::Services::{
    ChangeServiceConfig2W, ChangeServiceConfigW, ControlService, CreateServiceW, DeleteService, QueryServiceStatus,
    RegisterServiceCtrlHandlerExW, SC_ACTION, SC_ACTION_RESTART, SC_MANAGER_CONNECT, SC_MANAGER_CREATE_SERVICE,
    SERVICE_ACCEPT_SHUTDOWN, SERVICE_ACCEPT_STOP, SERVICE_ALL_ACCESS, SERVICE_AUTO_START,
    SERVICE_CONFIG_DELAYED_AUTO_START_INFO, SERVICE_CONFIG_DESCRIPTION, SERVICE_CONFIG_FAILURE_ACTIONS,
    SERVICE_CONFIG_REQUIRED_PRIVILEGES_INFO, SERVICE_CONFIG_SERVICE_SID_INFO, SERVICE_CONTROL_INTERROGATE,
    SERVICE_CONTROL_SHUTDOWN, SERVICE_CONTROL_STOP, SERVICE_DELAYED_AUTO_START_INFO, SERVICE_DESCRIPTIONW,
    SERVICE_ERROR_NORMAL, SERVICE_FAILURE_ACTIONSW, SERVICE_QUERY_STATUS, SERVICE_REQUIRED_PRIVILEGES_INFOW,
    SERVICE_RUNNING, SERVICE_SID_INFO, SERVICE_SID_TYPE_UNRESTRICTED, SERVICE_STATUS, SERVICE_STOP,
    SERVICE_STOP_PENDING, SERVICE_STOPPED, SERVICE_TABLE_ENTRYW, SERVICE_WIN32_OWN_PROCESS, SetServiceStatus,
    StartServiceCtrlDispatcherW, StartServiceW,
};
use windows_sys::Win32::System::Threading::{GetExitCodeProcess, INFINITE, WaitForSingleObject};
use windows_sys::Win32::UI::Shell::{
    FOLDERID_ProgramData, FOLDERID_ProgramFiles, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
    SHGetKnownFolderPath, ShellExecuteExW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;
use windows_sys::core::GUID;

use crate::buffer::wide;
use crate::service::{ScManager, open_manager, open_service};
use crate::{Error, Result};

pub const SERVICE_NAME: &str = "Ferrotop";
const DISPLAY_NAME: &str = "Ferrotop history";
const DESCRIPTION: &str = "Records CPU, memory, disk, network and GPU history for Ferrotop. It writes only to its own \
                           folder and accepts no requests.";
/// The privileges the service keeps; Windows removes every other one from its token. Kept:
/// bypassing traverse checks (everything needs it), creating the `Global\` live-data section,
/// and system profiling (the kernel event trace).
const PRIVILEGES: &str = "SeChangeNotifyPrivilege\0SeCreateGlobalPrivilege\0SeSystemProfilePrivilege\0\0";
/// The data folder: owned by Administrators; full control for SYSTEM and Administrators, read
/// for users; protected, so nothing is inherited from ProgramData (which lets users add files).
const DATA_DIR_SDDL: &str = "O:BAG:SYD:PAI(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FRFX;;;BU)";
const ERROR_CALL_NOT_IMPLEMENTED: u32 = 120;
const SERVICE_NO_CHANGE: u32 = 0xFFFF_FFFF;

// ----- Running as the service -----

/// Lets the service's work loop wait while noticing a stop request.
#[derive(Debug)]
pub struct StopSignal(());

static STOPPING: Mutex<bool> = Mutex::new(false);
static STOP_WAKE: Condvar = Condvar::new();
/// The `SERVICE_STATUS_HANDLE`, exposed as an address so the handler can reach it.
static STATUS_HANDLE: AtomicUsize = AtomicUsize::new(0);
type ServiceMain = Box<dyn Fn(&StopSignal) + Send + Sync>;
static MAIN: OnceLock<ServiceMain> = OnceLock::new();

impl StopSignal {
    /// Waits up to `timeout`; `true` once the service has been asked to stop.
    pub fn wait(&self, timeout: Duration) -> bool {
        let Ok(stopping) = STOPPING.lock() else { return true };
        match STOP_WAKE.wait_timeout_while(stopping, timeout, |stopping| !*stopping) {
            Ok((stopping, _)) => *stopping,
            Err(_) => true,
        }
    }
}

fn request_stop() {
    if let Ok(mut stopping) = STOPPING.lock() {
        *stopping = true;
    }
    STOP_WAKE.notify_all();
}

/// Hands this process to the Service Control Manager and runs `main` as the service until it
/// returns (it should return soon after its `StopSignal` reports a stop). Only works when the
/// SCM started the process.
pub fn run_service(main: impl Fn(&StopSignal) + Send + Sync + 'static) -> Result<()> {
    let _ = MAIN.set(Box::new(main));
    let mut name = wide(SERVICE_NAME);
    let table = [
        SERVICE_TABLE_ENTRYW { lpServiceName: name.as_mut_ptr(), lpServiceProc: Some(service_main) },
        SERVICE_TABLE_ENTRYW::default(),
    ];
    // SAFETY: the table ends with an empty entry and its name outlives the call, which returns
    // only once the service has stopped.
    if unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) } == 0 {
        return Err(Error::last_win32("StartServiceCtrlDispatcherW"));
    }
    Ok(())
}

unsafe extern "system" fn service_main(_argc: u32, _argv: *mut windows_sys::core::PWSTR) {
    let name = wide(SERVICE_NAME);
    // SAFETY: the name is NUL-terminated; the handler needs no context.
    let handle = unsafe { RegisterServiceCtrlHandlerExW(name.as_ptr(), Some(control_handler), std::ptr::null()) };
    if handle.is_null() {
        return;
    }
    STATUS_HANDLE.store(handle.expose_provenance(), Ordering::SeqCst);
    report(SERVICE_RUNNING, 0);
    if let Some(main) = MAIN.get() {
        main(&StopSignal(()));
    }
    report(SERVICE_STOPPED, 0);
}

unsafe extern "system" fn control_handler(
    control: u32,
    _event: u32,
    _data: *mut std::ffi::c_void,
    _context: *mut std::ffi::c_void,
) -> u32 {
    match control {
        SERVICE_CONTROL_STOP | SERVICE_CONTROL_SHUTDOWN => {
            report(SERVICE_STOP_PENDING, 10_000);
            request_stop();
            NO_ERROR
        }
        SERVICE_CONTROL_INTERROGATE => NO_ERROR,
        _ => ERROR_CALL_NOT_IMPLEMENTED,
    }
}

fn report(state: u32, wait_hint_ms: u32) {
    let handle = STATUS_HANDLE.load(Ordering::SeqCst);
    if handle == 0 {
        return;
    }
    let status = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        dwControlsAccepted: if state == SERVICE_RUNNING { SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN } else { 0 },
        dwWin32ExitCode: NO_ERROR,
        dwServiceSpecificExitCode: 0,
        dwCheckPoint: 0,
        dwWaitHint: wait_hint_ms,
    };
    // SAFETY: `handle` is the status handle RegisterServiceCtrlHandlerExW returned, exposed in
    // `service_main`; it stays valid for the life of the service.
    unsafe { SetServiceStatus(std::ptr::with_exposed_provenance_mut(handle), &status) };
}

// ----- Folders -----

/// Where the service runs from: `%ProgramFiles%\Ferrotop\ferrotop.exe`.
pub fn service_exe_path() -> Result<PathBuf> {
    Ok(known_folder(&FOLDERID_ProgramFiles)?.join("Ferrotop").join("ferrotop.exe"))
}

/// Where the history is kept: `%ProgramData%\Ferrotop`.
pub fn data_dir() -> Result<PathBuf> {
    Ok(known_folder(&FOLDERID_ProgramData)?.join("Ferrotop"))
}

fn known_folder(id: &GUID) -> Result<PathBuf> {
    let mut path: *mut u16 = std::ptr::null_mut();
    // SAFETY: `id` is a valid folder GUID and `path` a valid out pointer; a null token means the
    // current user (these folders are the same for everyone).
    let result = unsafe { SHGetKnownFolderPath(id, 0, std::ptr::null_mut(), &mut path) };
    let folder = if result >= 0 && !path.is_null() {
        // SAFETY: on success the shell returns a NUL-terminated path.
        Ok(PathBuf::from(unsafe { from_wide_ptr(path) }))
    } else {
        Err(Error::HResult { call: "SHGetKnownFolderPath", hresult: result })
    };
    // SAFETY: frees the string the shell allocated (freeing null is allowed).
    unsafe { CoTaskMemFree(path.cast_const().cast()) };
    folder
}

/// Copies a NUL-terminated UTF-16 string.
///
/// # Safety
/// `ptr` must point at a readable, NUL-terminated UTF-16 string.
unsafe fn from_wide_ptr(ptr: *const u16) -> OsString {
    let mut len = 0usize;
    loop {
        // SAFETY: every unit up to the terminator is readable, so `len` is in bounds.
        let at = unsafe { ptr.add(len) };
        // SAFETY: as above.
        let unit = unsafe { at.read() };
        if unit == 0 {
            break;
        }
        len += 1;
    }
    // SAFETY: the `len` units before the terminator are readable.
    OsString::from_wide(unsafe { std::slice::from_raw_parts(ptr, len) })
}

fn wide_path(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
}

// ----- The trusted data folder -----

/// True when `path` is a real folder (not a link to somewhere else) owned by SYSTEM or
/// Administrators, which ordinary users can't do that to. The service only writes into a
/// folder that passes.
pub fn is_trusted_dir(path: &Path) -> bool {
    let name = wide_path(path);
    // SAFETY: the name is NUL-terminated.
    let attributes = unsafe { GetFileAttributesW(name.as_ptr()) };
    if attributes == INVALID_FILE_ATTRIBUTES
        || attributes & FILE_ATTRIBUTE_DIRECTORY == 0
        || attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
    {
        return false;
    }
    let mut owner: PSID = std::ptr::null_mut();
    let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: the name is NUL-terminated and the owner and descriptor out pointers are valid;
    // the other parts aren't asked for.
    let status = unsafe {
        GetNamedSecurityInfoW(
            name.as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    let trusted = status == NO_ERROR
        && !owner.is_null()
        // SAFETY: `owner` points into `descriptor`, which is still allocated.
        && (unsafe { IsWellKnownSid(owner, WinLocalSystemSid) } != 0
            // SAFETY: as above.
            || unsafe { IsWellKnownSid(owner, WinBuiltinAdministratorsSid) } != 0);
    // SAFETY: frees the descriptor GetNamedSecurityInfoW allocated (null is allowed).
    unsafe { LocalFree(descriptor as HLOCAL) };
    trusted
}

/// Creates the history folder writable only by SYSTEM and Administrators. ProgramData lets any
/// user create folders, so an existing folder that isn't trusted (made by a user, or a link
/// that could redirect the service's writes) is moved aside first. Needs administrator rights.
pub fn prepare_data_dir(path: &Path) -> Result<()> {
    for attempt in 0..3 {
        let name = wide_path(path);
        // SAFETY: the name is NUL-terminated.
        let exists = unsafe { GetFileAttributesW(name.as_ptr()) } != INVALID_FILE_ATTRIBUTES;
        if exists {
            if is_trusted_dir(path) {
                return Ok(());
            }
            let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
            let aside = wide_path(&path.with_file_name(format!("Ferrotop.untrusted-{stamp}-{attempt}")));
            // SAFETY: both names are NUL-terminated; moving a link moves the link, not its target.
            if unsafe { MoveFileExW(name.as_ptr(), aside.as_ptr(), 0) } == 0 {
                return Err(Error::last_win32("MoveFileExW"));
            }
            continue;
        }
        let descriptor = SecurityDescriptor::from_sddl(DATA_DIR_SDDL)?;
        let attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: 0,
        };
        // SAFETY: the name is NUL-terminated and `attributes` points at a live descriptor.
        if unsafe { CreateDirectoryW(name.as_ptr(), &attributes) } != 0 {
            return if is_trusted_dir(path) {
                Ok(())
            } else {
                Err(Error::Other { message: "couldn't make a history folder only administrators can change" })
            };
        }
        let error = Error::last_win32("CreateDirectoryW");
        // Someone created it in between: check it again.
        if !matches!(error, Error::Win32 { code: ERROR_ALREADY_EXISTS, .. }) {
            return Err(error);
        }
    }
    Err(Error::Other { message: "couldn't make a history folder only administrators can change" })
}

/// A security descriptor made from SDDL, freed on drop.
pub(crate) struct SecurityDescriptor(pub(crate) PSECURITY_DESCRIPTOR);

impl SecurityDescriptor {
    pub(crate) fn from_sddl(sddl: &str) -> Result<Self> {
        let text = wide(sddl);
        let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // SAFETY: the SDDL is NUL-terminated and `descriptor` is a valid out pointer; the size
        // isn't wanted.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                text.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(Error::last_win32("ConvertStringSecurityDescriptorToSecurityDescriptorW"));
        }
        Ok(Self(descriptor))
    }
}

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        // SAFETY: the descriptor was allocated by the conversion and is freed once.
        unsafe { LocalFree(self.0 as HLOCAL) };
    }
}

// ----- Installing and removing -----

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceState {
    NotInstalled,
    Stopped,
    Running,
    /// Starting, stopping or paused.
    Changing,
}

/// Whether the service is installed and running. Needs no special rights.
pub fn state() -> ServiceState {
    let Ok(manager) = open_manager(SC_MANAGER_CONNECT) else { return ServiceState::NotInstalled };
    let Ok(service) = open_service(&manager, SERVICE_NAME, SERVICE_QUERY_STATUS) else {
        return ServiceState::NotInstalled;
    };
    match query_state(&service) {
        Some(SERVICE_RUNNING) => ServiceState::Running,
        Some(SERVICE_STOPPED) => ServiceState::Stopped,
        Some(_) => ServiceState::Changing,
        None => ServiceState::NotInstalled,
    }
}

fn query_state(service: &ScManager) -> Option<u32> {
    let mut status = SERVICE_STATUS::default();
    // SAFETY: `service` is a live handle with query rights and `status` a valid out pointer.
    (unsafe { QueryServiceStatus(service.0, &mut status) } != 0).then_some(status.dwCurrentState)
}

/// Stops the service, if it's running, and waits up to 20 seconds for it to stop.
fn stop_and_wait(service: &ScManager) -> Result<()> {
    let mut status = SERVICE_STATUS::default();
    // SAFETY: `service` is live and `status` a valid out pointer.
    if unsafe { ControlService(service.0, SERVICE_CONTROL_STOP, &mut status) } == 0 {
        match Error::last_win32("ControlService") {
            Error::Win32 { code: ERROR_SERVICE_NOT_ACTIVE, .. } => return Ok(()),
            error => return Err(error),
        }
    }
    let deadline = Instant::now() + Duration::from_secs(20);
    while query_state(service) != Some(SERVICE_STOPPED) {
        if Instant::now() > deadline {
            return Err(Error::Other { message: "the service didn't stop" });
        }
        thread::sleep(Duration::from_millis(200));
    }
    Ok(())
}

/// Installs or updates the service from `exe` (this Ferrotop): copies it to Program Files,
/// prepares the data folder, registers the service with least privilege and starts it. Needs
/// administrator rights.
pub fn install(exe: &Path) -> Result<()> {
    let manager = open_manager(SC_MANAGER_CONNECT | SC_MANAGER_CREATE_SERVICE)?;
    let existing = open_service(&manager, SERVICE_NAME, SERVICE_ALL_ACCESS).ok();
    if let Some(service) = &existing {
        stop_and_wait(service)?;
    }

    let target = install_copy(exe)?;
    prepare_data_dir(&data_dir()?)?;

    let command = wide(&format!("\"{}\" --service", target.display()));
    let (name, display) = (wide(SERVICE_NAME), wide(DISPLAY_NAME));
    let service = match existing {
        Some(service) => {
            // SAFETY: `service` is live with full access; the strings are NUL-terminated and the
            // nulls mean "unchanged".
            let ok = unsafe {
                ChangeServiceConfigW(
                    service.0,
                    SERVICE_NO_CHANGE,
                    SERVICE_AUTO_START,
                    SERVICE_NO_CHANGE,
                    command.as_ptr(),
                    std::ptr::null(),
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    std::ptr::null(),
                    std::ptr::null(),
                    display.as_ptr(),
                )
            };
            if ok == 0 {
                return Err(Error::last_win32("ChangeServiceConfigW"));
            }
            service
        }
        None => {
            // SAFETY: `manager` has create rights; the strings are NUL-terminated; a null account
            // and password mean LocalSystem.
            let raw = unsafe {
                CreateServiceW(
                    manager.0,
                    name.as_ptr(),
                    display.as_ptr(),
                    SERVICE_ALL_ACCESS,
                    SERVICE_WIN32_OWN_PROCESS,
                    SERVICE_AUTO_START,
                    SERVICE_ERROR_NORMAL,
                    command.as_ptr(),
                    std::ptr::null(),
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    std::ptr::null(),
                    std::ptr::null(),
                )
            };
            if raw.is_null() {
                let error = Error::last_win32("CreateServiceW");
                if matches!(error, Error::Win32 { code: ERROR_SERVICE_EXISTS, .. }) {
                    return Err(Error::Other { message: "the service appeared while installing; try again" });
                }
                return Err(error);
            }
            ScManager(raw)
        }
    };
    configure(&service)?;
    // SAFETY: `service` is live with start rights; no arguments.
    if unsafe { StartServiceW(service.0, 0, std::ptr::null()) } == 0 {
        return Err(Error::last_win32("StartServiceW"));
    }
    Ok(())
}

/// Description, delayed start, least privilege, its own SID, and restart on failure.
fn configure(service: &ScManager) -> Result<()> {
    let mut description = wide(DESCRIPTION);
    let mut privileges: Vec<u16> = PRIVILEGES.encode_utf16().collect();
    let mut actions = [
        SC_ACTION { Type: SC_ACTION_RESTART, Delay: 60_000 },
        SC_ACTION { Type: SC_ACTION_RESTART, Delay: 60_000 },
        SC_ACTION { Type: SC_ACTION_RESTART, Delay: 300_000 },
    ];
    let description = SERVICE_DESCRIPTIONW { lpDescription: description.as_mut_ptr() };
    let delayed = SERVICE_DELAYED_AUTO_START_INFO { fDelayedAutostart: 1 };
    let privileges = SERVICE_REQUIRED_PRIVILEGES_INFOW { pmszRequiredPrivileges: privileges.as_mut_ptr() };
    let sid = SERVICE_SID_INFO { dwServiceSidType: SERVICE_SID_TYPE_UNRESTRICTED };
    let failure = SERVICE_FAILURE_ACTIONSW {
        dwResetPeriod: 86_400,
        lpRebootMsg: std::ptr::null_mut(),
        lpCommand: std::ptr::null_mut(),
        cActions: actions.len() as u32,
        lpsaActions: actions.as_mut_ptr(),
    };
    let settings: [(u32, *const std::ffi::c_void, &'static str); 5] = [
        (SERVICE_CONFIG_DESCRIPTION, (&raw const description).cast(), "description"),
        (SERVICE_CONFIG_DELAYED_AUTO_START_INFO, (&raw const delayed).cast(), "delayed start"),
        (SERVICE_CONFIG_REQUIRED_PRIVILEGES_INFO, (&raw const privileges).cast(), "required privileges"),
        (SERVICE_CONFIG_SERVICE_SID_INFO, (&raw const sid).cast(), "service SID"),
        (SERVICE_CONFIG_FAILURE_ACTIONS, (&raw const failure).cast(), "failure actions"),
    ];
    for (level, info, _what) in settings {
        // SAFETY: each `info` points at the live structure its level expects, and the strings
        // and arrays those point to outlive the call.
        if unsafe { ChangeServiceConfig2W(service.0, level, info) } == 0 {
            return Err(Error::last_win32("ChangeServiceConfig2W"));
        }
    }
    Ok(())
}

/// Copies `exe` to Program Files, where the service and Task Manager's replacement run from,
/// unless it's that copy already. Needs administrator rights.
pub fn install_copy(exe: &Path) -> Result<PathBuf> {
    let target = service_exe_path()?;
    let folder = target.parent().ok_or(Error::Malformed { call: "service_exe_path" })?;
    std::fs::create_dir_all(folder)
        .map_err(|_| Error::Other { message: "couldn't create the Ferrotop folder in Program Files" })?;
    remove_set_aside(folder);
    if same_file(exe, &target) {
        return Ok(target);
    }
    // A copy that's running (the old service letting go, or Ferrotop opened as Task Manager)
    // can't be overwritten, but it can be moved aside, to be deleted next time.
    if std::fs::copy(exe, &target).is_err() {
        if target.exists() {
            std::fs::rename(&target, set_aside_name(folder))
                .map_err(|_| Error::Other { message: "couldn't replace Ferrotop's copy in Program Files" })?;
        }
        std::fs::copy(exe, &target).map_err(|_| Error::Other { message: "couldn't copy Ferrotop to Program Files" })?;
    }
    Ok(target)
}

/// Deletes the copy in Program Files, and its folder once empty. A copy that's still running
/// is moved aside instead, to be deleted next time. Needs administrator rights.
pub fn remove_copy() {
    let Ok(target) = service_exe_path() else { return };
    let Some(folder) = target.parent() else { return };
    if target.exists() && std::fs::remove_file(&target).is_err() {
        let _ = std::fs::rename(&target, set_aside_name(folder));
    }
    remove_set_aside(folder);
    let _ = std::fs::remove_dir(folder);
}

fn set_aside_name(folder: &Path) -> PathBuf {
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    folder.join(format!("ferrotop-{stamp}.old"))
}

/// Deletes copies moved aside earlier (those still running stay).
fn remove_set_aside(folder: &Path) {
    let Ok(entries) = std::fs::read_dir(folder) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("old")) {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Stops and removes the service, and its copy in Program Files unless Task Manager opens it.
/// The recorded history stays (see [`remove_history`]). Needs administrator rights.
pub fn uninstall() -> Result<()> {
    let manager = open_manager(SC_MANAGER_CONNECT)?;
    match open_service(&manager, SERVICE_NAME, SERVICE_STOP | SERVICE_QUERY_STATUS | DELETE) {
        Ok(service) => {
            stop_and_wait(&service)?;
            // SAFETY: `service` is live with delete rights.
            if unsafe { DeleteService(service.0) } == 0 {
                return Err(Error::last_win32("DeleteService"));
            }
        }
        Err(Error::Win32 { code: ERROR_SERVICE_DOES_NOT_EXIST, .. }) => {}
        Err(error) => return Err(error),
    }
    // Task Manager would fail to open with its replacement gone.
    if !crate::task_manager::is_replaced() {
        remove_copy();
    }
    Ok(())
}

/// Deletes the recorded history. Needs administrator rights.
pub fn remove_history() -> Result<()> {
    let folder = data_dir()?;
    if folder.exists() {
        std::fs::remove_dir_all(&folder).map_err(|_| Error::Other { message: "couldn't remove the history folder" })?;
    }
    Ok(())
}

/// Runs this Ferrotop again, elevated through the UAC prompt and with no window, with
/// `arguments`; waits for it and returns its exit code. An error if the prompt was declined.
pub fn run_self_elevated(arguments: &str) -> Result<u32> {
    let exe = std::env::current_exe().map_err(|_| Error::Other { message: "couldn't find Ferrotop's own path" })?;
    let (verb, file, parameters) = (wide("runas"), wide_path(&exe), wide(arguments));
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC,
        lpVerb: verb.as_ptr(),
        lpFile: file.as_ptr(),
        lpParameters: parameters.as_ptr(),
        nShow: SW_HIDE,
        ..SHELLEXECUTEINFOW::default()
    };
    // SAFETY: `info` is fully initialized and its strings outlive the call.
    if unsafe { ShellExecuteExW(&mut info) } == 0 {
        return Err(Error::last_win32("ShellExecuteExW"));
    }
    if info.hProcess.is_null() {
        return Err(Error::Other { message: "the elevated helper didn't start" });
    }
    // SAFETY: the process handle ShellExecuteExW returned is ours to own and close.
    let process = unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(info.hProcess) };
    // SAFETY: `process` is a live process handle.
    unsafe { WaitForSingleObject(std::os::windows::io::AsRawHandle::as_raw_handle(&process), INFINITE) };
    let mut code = 0u32;
    // SAFETY: `process` is live and `code` a valid out pointer.
    if unsafe { GetExitCodeProcess(std::os::windows::io::AsRawHandle::as_raw_handle(&process), &mut code) } == 0 {
        return Err(Error::last_win32("GetExitCodeProcess"));
    }
    Ok(code)
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// `DELETE` standard access right.
const DELETE: u32 = 0x0001_0000;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_machine_folders() {
        let exe = service_exe_path().expect("program files");
        assert!(exe.ends_with(r"Ferrotop\ferrotop.exe"), "{}", exe.display());
        assert!(data_dir().expect("program data").ends_with("Ferrotop"));
    }

    #[test]
    fn trusts_only_folders_admins_own() {
        // ProgramData itself belongs to SYSTEM.
        assert!(is_trusted_dir(&known_folder(&FOLDERID_ProgramData).expect("program data")));
        // The temp folder belongs to this user, unless it was made elevated (as on CI), which
        // makes Administrators its owner.
        if !crate::system::is_elevated() {
            assert!(!is_trusted_dir(&std::env::temp_dir()));
        }
        assert!(!is_trusted_dir(Path::new(r"C:\definitely\not\here")));
        let file = std::env::current_exe().expect("exe");
        assert!(!is_trusted_dir(&file), "a file isn't a folder");
    }

    #[test]
    fn the_data_folder_sddl_parses() {
        assert!(SecurityDescriptor::from_sddl(DATA_DIR_SDDL).is_ok());
        assert!(SecurityDescriptor::from_sddl("not sddl").is_err());
    }

    #[test]
    fn reports_a_state_without_rights() {
        // Not installed on a developer machine or CI unless someone installed it.
        let _ = state();
    }

    #[test]
    fn stop_signal_times_out_without_a_stop() {
        assert!(!StopSignal(()).wait(Duration::from_millis(10)));
    }

    #[test]
    fn privileges_are_a_double_terminated_list() {
        assert!(PRIVILEGES.ends_with("\0\0"));
        assert_eq!(PRIVILEGES.trim_end_matches('\0').split('\0').count(), 3);
    }
}
