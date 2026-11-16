//! Installing Ferrotop from its own exe: a copy in Program Files, a Start menu shortcut, and an
//! entry in Settings → Apps (so it can be removed from there too). The same copy is what the
//! history service, Task Manager's replacement and the no-prompt administrator start run.

use std::path::{Path, PathBuf};

use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use windows_sys::Win32::System::Registry::{HKEY_LOCAL_MACHINE, REG_DWORD, REG_SZ, RegDeleteTreeW, RegSetKeyValueW};
use windows_sys::Win32::UI::Shell::FOLDERID_CommonPrograms;

use crate::buffer::wide;
use crate::service_host::{self, ServiceState};
use crate::{Error, Result};

const UNINSTALL_KEY: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\Ferrotop";
const REPOSITORY: &str = "https://github.com/Freeflite/Ferrotop";

/// The installed copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    pub exe: PathBuf,
    /// Its version, from its version resource.
    pub version: Option<String>,
}

/// The installed copy, if Ferrotop is installed (registered in Apps, with its file in place).
pub fn installed() -> Option<Installed> {
    crate::system::string_value(HKEY_LOCAL_MACHINE, UNINSTALL_KEY, "DisplayName")?;
    let exe = service_host::service_exe_path().ok().filter(|exe| exe.exists())?;
    let version = crate::version::version_info(&exe.to_string_lossy()).ok().and_then(|v| v.product_version);
    Some(Installed { exe, version })
}

/// Whether `exe` is the installed copy, or the very same build of it (byte for byte).
pub fn is_installed_build(exe: &Path, installed: &Installed) -> bool {
    if let (Ok(a), Ok(b)) = (std::fs::canonicalize(exe), std::fs::canonicalize(&installed.exe))
        && a == b
    {
        return true;
    }
    let size = |path: &Path| std::fs::metadata(path).map(|m| m.len()).ok();
    size(exe).is_some()
        && size(exe) == size(&installed.exe)
        && std::fs::read(exe).ok() == std::fs::read(&installed.exe).ok()
}

/// The all-users Start menu shortcut.
fn shortcut_path() -> Result<PathBuf> {
    Ok(service_host::known_folder(&FOLDERID_CommonPrograms)?.join("Ferrotop.lnk"))
}

fn set_string(name: &str, value: &str) -> Result<()> {
    let (key, name, data) = (wide(UNINSTALL_KEY), wide(name), wide(value));
    let bytes = u32::try_from(data.len() * 2).map_err(|_| Error::Malformed { call: "registry value" })?;
    // SAFETY: the key and value names are NUL-terminated and `data` is a NUL-terminated REG_SZ of
    // `bytes` bytes. The key is created if it isn't there.
    let status = unsafe {
        RegSetKeyValueW(HKEY_LOCAL_MACHINE, key.as_ptr(), name.as_ptr(), REG_SZ, data.as_ptr().cast(), bytes)
    };
    if status == ERROR_SUCCESS { Ok(()) } else { Err(Error::Win32 { call: "RegSetKeyValueW", code: status }) }
}

fn set_dword(name: &str, value: u32) -> Result<()> {
    let (key, name) = (wide(UNINSTALL_KEY), wide(name));
    // SAFETY: as above, with a 4-byte DWORD.
    let status = unsafe {
        RegSetKeyValueW(HKEY_LOCAL_MACHINE, key.as_ptr(), name.as_ptr(), REG_DWORD, (&raw const value).cast(), 4)
    };
    if status == ERROR_SUCCESS { Ok(()) } else { Err(Error::Win32 { call: "RegSetKeyValueW", code: status }) }
}

/// Installs (or updates to) `exe`: copies it to Program Files, adds the Start menu shortcut and
/// the Apps entry. An installed history service is updated too, as it runs the same copy. Needs
/// administrator rights.
pub fn install(exe: &Path) -> Result<PathBuf> {
    if service_host::state() == ServiceState::NotInstalled {
        service_host::install_copy(exe)?;
    } else {
        service_host::install(exe)?;
    }
    let copy = service_host::service_exe_path()?;
    crate::shortcut::create(&shortcut_path()?, &copy, "Ferrotop, a memory-safe process explorer")?;

    let folder = copy.parent().ok_or(Error::Malformed { call: "service_exe_path" })?;
    let size_kb = std::fs::metadata(&copy).map_or(0, |m| m.len() / 1024);
    set_string("DisplayName", "Ferrotop")?;
    set_string("DisplayVersion", env!("CARGO_PKG_VERSION"))?;
    set_string("DisplayIcon", &format!("{},0", copy.display()))?;
    set_string("InstallLocation", &folder.to_string_lossy())?;
    set_string("UninstallString", &format!("\"{}\" --uninstall", copy.display()))?;
    set_string("URLInfoAbout", REPOSITORY)?;
    set_dword("EstimatedSize", u32::try_from(size_kb).unwrap_or(u32::MAX))?;
    set_dword("NoModify", 1)?;
    set_dword("NoRepair", 1)?;
    Ok(copy)
}

/// Removes everything installing set up: the no-prompt administrator start, Task Manager's
/// replacement, the history service (its recorded history stays), the shortcut, the Apps entry,
/// starting at sign-in from the installed copy, and the copy itself (at the next restart if it's
/// running). Settings stay. Needs administrator rights.
pub fn uninstall() -> Result<()> {
    crate::elevated_start::remove_task()?;
    crate::task_manager::remove_replacement()?;
    service_host::remove_service()?;
    if let Ok(link) = shortcut_path() {
        let _ = std::fs::remove_file(link);
    }
    let key = wide(UNINSTALL_KEY);
    // SAFETY: the key name is NUL-terminated.
    let status = unsafe { RegDeleteTreeW(HKEY_LOCAL_MACHINE, key.as_ptr()) };
    if !matches!(status, ERROR_SUCCESS | ERROR_FILE_NOT_FOUND) {
        return Err(Error::Win32 { call: "RegDeleteTreeW", code: status });
    }
    let copy = service_host::service_exe_path()?;
    if crate::system::sign_in_command().is_some_and(|command| command_runs(&command, &copy)) {
        let _ = crate::system::set_start_at_sign_in(false);
    }
    service_host::remove_copy();
    Ok(())
}

/// Whether a command line starts the program at `exe`.
fn command_runs(command: &str, exe: &Path) -> bool {
    let command = command.trim();
    let program = match command.strip_prefix('"') {
        Some(rest) => rest.split('"').next().unwrap_or_default(),
        None => command.split(' ').next().unwrap_or_default(),
    };
    program.eq_ignore_ascii_case(&exe.to_string_lossy())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_commands_for_the_copy() {
        let exe = Path::new(r"C:\Program Files\Ferrotop\ferrotop.exe");
        assert!(command_runs(r#""C:\Program Files\Ferrotop\ferrotop.exe" --tray"#, exe));
        assert!(command_runs(r#""c:\program files\ferrotop\FERROTOP.exe""#, exe));
        assert!(!command_runs(r#""C:\Tools\ferrotop.exe" --tray"#, exe));
    }
}
