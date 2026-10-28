//! Opening Ferrotop in place of Task Manager.
//!
//! Windows' Image File Execution Options let one program name another to start in its place: a
//! `Debugger` value under `taskmgr.exe` makes the taskbar's Task Manager, Ctrl+Shift+Esc and the
//! Ctrl+Alt+Del screen start that program instead, with Task Manager's path as its first
//! argument. Process Explorer and System Informer replace Task Manager the same way.
//!
//! The value points at Ferrotop's copy in Program Files, which only administrators can change:
//! a program users could replace would be started wherever Task Manager would have been.

use std::path::Path;

use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use windows_sys::Win32::System::Registry::{HKEY_LOCAL_MACHINE, REG_SZ, RegDeleteKeyValueW, RegSetKeyValueW};

use crate::buffer::wide;
use crate::service_host::{self, ServiceState};
use crate::{Error, Result};

const KEY: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\taskmgr.exe";
const VALUE: &str = "Debugger";

/// The command Windows starts instead of Task Manager, if any.
pub fn replacement() -> Option<String> {
    crate::system::string_value(HKEY_LOCAL_MACHINE, KEY, VALUE).filter(|command| !command.trim().is_empty())
}

/// Whether Task Manager opens Ferrotop (its copy in Program Files).
pub fn is_replaced() -> bool {
    let (Some(command), Ok(copy)) = (replacement(), service_host::service_exe_path()) else { return false };
    names_program(&command, &copy)
}

/// Whether a `Debugger` command starts the program at `exe` (quoted or not, any case).
fn names_program(command: &str, exe: &Path) -> bool {
    let command = command.trim();
    let program = match command.strip_prefix('"') {
        Some(rest) => rest.split('"').next().unwrap_or_default(),
        None => command,
    };
    program.eq_ignore_ascii_case(&exe.to_string_lossy())
}

/// Whether this process was started in Task Manager's place: its first argument is Task
/// Manager's path.
pub fn started_as_task_manager(first_argument: Option<&str>) -> bool {
    first_argument.is_some_and(|argument| {
        Path::new(argument).file_name().is_some_and(|name| name.eq_ignore_ascii_case("taskmgr.exe"))
    })
}

/// Makes Task Manager open Ferrotop: copies `exe` to Program Files (unless it's that copy) and
/// points Task Manager at it. Needs administrator rights.
pub fn replace(exe: &Path) -> Result<()> {
    // The history service runs from the same copy: update it properly (stop, copy, start), or it
    // would go on running the old program.
    if service_host::state() != ServiceState::NotInstalled {
        service_host::install(exe)?;
    }
    let copy = service_host::install_copy(exe)?;
    let command = wide(&format!("\"{}\"", copy.display()));
    let bytes = u32::try_from(command.len() * 2).map_err(|_| Error::Malformed { call: "Task Manager command" })?;
    let (key, value) = (wide(KEY), wide(VALUE));
    // SAFETY: the key and value names are NUL-terminated, and `command` is a NUL-terminated
    // REG_SZ of `bytes` bytes. The key is created if it isn't there.
    let status = unsafe {
        RegSetKeyValueW(HKEY_LOCAL_MACHINE, key.as_ptr(), value.as_ptr(), REG_SZ, command.as_ptr().cast(), bytes)
    };
    if status == ERROR_SUCCESS { Ok(()) } else { Err(Error::Win32 { call: "RegSetKeyValueW", code: status }) }
}

/// Puts Task Manager back, if Ferrotop replaced it (another program's replacement is left
/// alone), and removes the Program Files copy unless the history service still uses it. Needs
/// administrator rights.
pub fn restore() -> Result<()> {
    if is_replaced() {
        let (key, value) = (wide(KEY), wide(VALUE));
        // SAFETY: both strings are NUL-terminated.
        let status = unsafe { RegDeleteKeyValueW(HKEY_LOCAL_MACHINE, key.as_ptr(), value.as_ptr()) };
        if !matches!(status, ERROR_SUCCESS | ERROR_FILE_NOT_FOUND) {
            return Err(Error::Win32 { call: "RegDeleteKeyValueW", code: status });
        }
    }
    if service_host::state() == ServiceState::NotInstalled {
        // A copy that's running (Ferrotop opened as Task Manager) stays until it's next updated.
        service_host::remove_copy();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_its_own_command() {
        let exe = Path::new(r"C:\Program Files\Ferrotop\ferrotop.exe");
        assert!(names_program(r#""C:\Program Files\Ferrotop\ferrotop.exe""#, exe));
        assert!(names_program(r#"  "c:\program files\ferrotop\FERROTOP.EXE" /x"#, exe));
        assert!(names_program(r"C:\Program Files\Ferrotop\ferrotop.exe", exe));
        assert!(!names_program(r#""C:\Tools\procexp.exe""#, exe));
        assert!(!names_program("", exe));
    }

    #[test]
    fn knows_a_task_manager_launch() {
        assert!(started_as_task_manager(Some(r"C:\WINDOWS\system32\taskmgr.exe")));
        assert!(started_as_task_manager(Some("TASKMGR.EXE")));
        assert!(!started_as_task_manager(Some("--tray")));
        assert!(!started_as_task_manager(Some(r"C:\Tools\taskmgr.exe.bak")));
        assert!(!started_as_task_manager(None));
    }
}
