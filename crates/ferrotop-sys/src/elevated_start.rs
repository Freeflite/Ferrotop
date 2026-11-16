//! Starting Ferrotop as administrator without a UAC prompt, through a scheduled task.
//!
//! Windows only skips the prompt for programs Task Scheduler starts "with highest privileges",
//! so turning this on (which does need administrator approval, once) registers such a task for
//! the current user. From then on, a Ferrotop started normally starts the task and quits.
//!
//! The task only ever starts the installed copy in Program Files, which users can't change, with
//! a fixed command line: any program can start the task, so it must not be able to choose what
//! runs or with which arguments.

use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;
use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

use crate::service_host;
use crate::{Error, Result};

const FOLDER: &str = r"\Ferrotop";
/// A value under `HKCU\Software\Ferrotop` asking the elevated copy to start in the tray (a start
/// at sign-in); only honored for a minute, then removed.
const TRAY_KEY: &str = r"Software\Ferrotop";
const TRAY_VALUE: &str = "StartInTray";

/// The task's name: one per user, as each user's task starts Ferrotop for them.
pub fn task_name() -> Result<String> {
    Ok(format!(r"{FOLDER}\Start as administrator ({})", crate::token::current_token()?.user))
}

fn schtasks() -> Result<PathBuf> {
    let mut buffer = [0u16; 260];
    // SAFETY: `buffer` is writable for the length passed.
    let len = unsafe { GetSystemDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) } as usize;
    if len == 0 || len >= buffer.len() {
        return Err(Error::last_win32("GetSystemDirectoryW"));
    }
    Ok(PathBuf::from(String::from_utf16_lossy(&buffer[..len])).join("schtasks.exe"))
}

/// Runs Windows' `schtasks.exe` (from System32, never a copy elsewhere) with no window.
fn run(arguments: &[&str]) -> Result<bool> {
    let status = Command::new(schtasks()?)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .map_err(|_| Error::Other { message: "couldn't run schtasks.exe" })?;
    Ok(status.success())
}

/// Whether the current user's task is registered.
pub fn is_enabled() -> bool {
    task_name().is_ok_and(|name| run(&["/Query", "/TN", &name]).unwrap_or(false))
}

/// Starts the installed copy as administrator through the task (no prompt). Fails if the task
/// isn't there.
pub fn start() -> Result<()> {
    if run(&["/Run", "/TN", &task_name()?])? {
        Ok(())
    } else {
        Err(Error::Other { message: "the scheduled task to start Ferrotop as administrator is missing" })
    }
}

/// The task, as Task Scheduler's XML: started on demand, as this user with highest privileges,
/// at normal priority, never stopped for running too long or on battery.
fn task_xml(exe: &Path, user: &str) -> String {
    let escape = |text: &str| text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>Starts Ferrotop as administrator without a prompt. Made by Ferrotop's "Always run as administrator" setting; turn that off to remove it.</Description>
  </RegistrationInfo>
  <Principals>
    <Principal id="Author">
      <UserId>{user}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>HighestAvailable</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>false</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>5</Priority>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{}</Command>
    </Exec>
  </Actions>
</Task>
"#,
        escape(&exe.to_string_lossy())
    )
}

/// Registers the task for the current user, to start the installed copy. Needs administrator
/// rights.
pub fn enable() -> Result<()> {
    let exe = service_host::service_exe_path()?;
    if !exe.exists() {
        return Err(Error::Other { message: "install Ferrotop first" });
    }
    let user = crate::token::current_token()?.user.to_string();
    // The definition goes in Ferrotop's Program Files folder, which users can't write to, so
    // nothing can change it between writing and registering.
    let folder = exe.parent().ok_or(Error::Malformed { call: "service_exe_path" })?;
    let definition = folder.join("start-as-administrator.xml");
    let mut bytes = vec![0xff, 0xfe];
    bytes.extend(task_xml(&exe, &user).encode_utf16().flat_map(u16::to_le_bytes));
    std::fs::write(&definition, bytes).map_err(|_| Error::Other { message: "couldn't write the task definition" })?;
    let registered = run(&["/Create", "/TN", &task_name()?, "/XML", &definition.to_string_lossy(), "/F"]);
    let _ = std::fs::remove_file(&definition);
    if registered? { Ok(()) } else { Err(Error::Other { message: "Task Scheduler didn't accept the task" }) }
}

/// Removes the current user's task (if it's there), then the Program Files copy if nothing else
/// uses it. Needs administrator rights.
pub fn disable() -> Result<()> {
    remove_task()?;
    service_host::remove_copy_if_unused();
    Ok(())
}

/// Removes the current user's task, if it's there. Needs administrator rights.
pub fn remove_task() -> Result<()> {
    if is_enabled() && !run(&["/Delete", "/TN", &task_name()?, "/F"])? {
        return Err(Error::Other { message: "Task Scheduler didn't remove the task" });
    }
    Ok(())
}

/// Asks the copy the task is about to start to begin in the tray (a start at sign-in).
pub fn request_tray() {
    let now = crate::system::unix_now().to_string();
    let _ = crate::system::set_user_value(TRAY_KEY, TRAY_VALUE, &now);
}

/// Whether this copy should begin in the tray: asked within the last minute. Clears the request.
pub fn take_tray_request() -> bool {
    let Some(asked) = crate::system::user_value(TRAY_KEY, TRAY_VALUE) else { return false };
    let _ = crate::system::delete_user_value(TRAY_KEY, TRAY_VALUE);
    asked.parse::<u64>().is_ok_and(|asked| crate::system::unix_now().saturating_sub(asked) <= 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_task_starts_only_the_installed_copy_as_this_user() {
        let xml = task_xml(Path::new(r"C:\Program Files\Ferrotop\ferrotop.exe"), "S-1-5-21-1-2-3-1001");
        assert!(xml.contains(r"<Command>C:\Program Files\Ferrotop\ferrotop.exe</Command>"));
        assert!(!xml.contains("<Arguments>"), "a fixed command line");
        assert!(xml.contains("<RunLevel>HighestAvailable</RunLevel>"));
        assert!(xml.contains("<UserId>S-1-5-21-1-2-3-1001</UserId>"));
        assert!(xml.contains("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>"));
        let odd = task_xml(Path::new(r"C:\A&B<C>\ferrotop.exe"), "S-1-5-18");
        assert!(odd.contains(r"<Command>C:\A&amp;B&lt;C&gt;\ferrotop.exe</Command>"));
    }

    #[test]
    fn the_name_is_per_user() {
        let name = task_name().expect("name");
        assert!(name.starts_with(r"\Ferrotop\Start as administrator (S-1-"), "{name}");
    }
}
