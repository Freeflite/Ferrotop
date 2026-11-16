//! The Settings pane's installation part: installing Ferrotop from its own exe (or updating or
//! removing it), and always running it as administrator without a prompt. Each change is made
//! by an elevated copy of Ferrotop; the UAC prompt is the confirmation.

use ferrotop_sys::install::{self, Installed};
use ferrotop_sys::service_host;

use crate::actions::Pending;
use crate::{App, ERROR_CANCELLED, MainWindow, background};

/// `TOKEN_ELEVATION_TYPE`: a standard user, or an administrator with UAC off.
const ELEVATION_DEFAULT: u32 = 1;

/// The Installation section's figures.
struct Status {
    text: String,
    button: &'static str,
    button_enabled: bool,
    installed: bool,
    elevate_enabled: bool,
    hint: String,
}

fn status(installed: Option<&Installed>) -> Status {
    let own = std::env::current_exe().ok();
    let version = env!("CARGO_PKG_VERSION");
    let current = installed.is_some_and(|i| own.as_deref().is_some_and(|own| install::is_installed_build(own, i)));
    let (text, button, button_enabled) = match installed {
        None => (
            "Not installed. Installing copies Ferrotop to Program Files, where only administrators can change it, \
             and adds it to the Start menu and to Settings → Apps."
                .to_owned(),
            "Install",
            true,
        ),
        Some(i) if current => {
            let folder = i.exe.parent().map_or_else(String::new, |f| f.display().to_string());
            (format!("Installed in {folder} (version {version})."), "Installed", false)
        }
        Some(i) => {
            let theirs = i.version.as_deref().unwrap_or("unknown");
            let text = if theirs == version {
                format!("Installed: another build of version {version}. Update to install this one.")
            } else {
                format!("Installed: version {theirs}. This is version {version}; update to install it.")
            };
            (text, "Update to this version", true)
        }
    };
    let elevation = ferrotop_sys::token::current_elevation_type();
    let (elevate_enabled, mut hint) = match (installed, elevation) {
        (None, _) => {
            (false, "Install first: only the installed copy can start as administrator without a prompt.".to_owned())
        }
        (Some(_), Some(ELEVATION_DEFAULT)) if ferrotop_sys::system::is_elevated() => (
            false,
            "Your account always runs programs as administrator (UAC is off), so Ferrotop already does.".to_owned(),
        ),
        (Some(_), Some(ELEVATION_DEFAULT)) => (false, "Your account isn't an administrator.".to_owned()),
        (Some(_), _) => (
            true,
            "Ferrotop starts as administrator without asking, through a scheduled task for your account that \
             starts only the installed copy. Turning it on asks once."
                .to_owned(),
        ),
    };
    if let Some(i) = installed
        && ferrotop_sys::system::runs_as_admin_by_compatibility(&i.exe)
    {
        hint.push_str(
            " The installed copy is set to \u{201c}Run this program as an administrator\u{201d} in its Properties, \
             which stops Task Manager from opening it; turning this on clears that.",
        );
    }
    Status { text, button, button_enabled, installed: installed.is_some(), elevate_enabled, hint }
}

impl App {
    /// The Settings pane is opening: show how things stand, and check the scheduled task.
    pub(crate) fn open_settings(&mut self, ui: &MainWindow) {
        self.refresh_setup(ui);
        background(ferrotop_sys::elevated_start::is_enabled, |app, ui, enabled| {
            if app.settings.always_elevated != enabled {
                app.settings.always_elevated = enabled;
                app.save_settings(ui);
            }
            ui.set_always_elevated(enabled);
        });
    }

    fn refresh_setup(&mut self, ui: &MainWindow) {
        let status = status(install::installed().as_ref());
        ui.set_install_status(status.text.into());
        ui.set_install_button(status.button.into());
        ui.set_install_button_enabled(status.button_enabled);
        ui.set_installed(status.installed);
        ui.set_always_elevated_enabled(status.elevate_enabled);
        ui.set_always_elevated_hint(status.hint.into());
        ui.set_always_elevated(self.settings.always_elevated);
        ui.set_replace_task_manager(ferrotop_sys::task_manager::is_replaced());
    }

    /// Runs this Ferrotop elevated with `arguments`, then `done` with whether it worked (and a
    /// message saying so already shown, using `what` for failures).
    fn run_setup(
        &mut self,
        ui: &MainWindow,
        arguments: &'static str,
        what: &'static str,
        done: impl FnOnce(&mut App, &MainWindow, bool) + Send + 'static,
    ) {
        ui.set_setup_busy(true);
        self.show_message(ui, "Asking Windows for administrator approval…".into());
        background(
            move || service_host::run_self_elevated(arguments),
            move |app, ui, result| {
                ui.set_setup_busy(false);
                let failure = match result {
                    Ok(0) => None,
                    Ok(1) => Some(what.to_owned()),
                    Ok(code) => Some(format!("{what}: {}", std::io::Error::from_raw_os_error(code as i32))),
                    Err(ferrotop_sys::Error::Win32 { code: ERROR_CANCELLED, .. }) => Some("Cancelled".to_owned()),
                    Err(error) => Some(format!("{what}: {error}")),
                };
                if let Some(message) = &failure {
                    app.show_message(ui, message.clone());
                }
                done(app, ui, failure.is_none());
                app.refresh_setup(ui);
            },
        );
    }

    pub(crate) fn install_clicked(&mut self, ui: &MainWindow) {
        self.run_setup(ui, "--install", "Couldn't install Ferrotop", |app, ui, ok| {
            if !ok {
                return;
            }
            // Starting at sign-in moves to the installed copy.
            if ferrotop_sys::system::sign_in_command().is_some()
                && let Some(installed) = install::installed()
            {
                let _ = ferrotop_sys::system::set_start_at_sign_in_for(true, &installed.exe);
            }
            app.show_message(ui, "Installed: Ferrotop is in the Start menu and in Settings → Apps".into());
        });
    }

    pub(crate) fn uninstall_clicked(&mut self, ui: &MainWindow) {
        self.confirm(
            ui,
            "Uninstall Ferrotop?".into(),
            "This removes Ferrotop from Program Files, the Start menu and Settings → Apps, along with the history \
             service, the no-prompt administrator start and Task Manager's replacement. Your settings and recorded \
             history are kept."
                .into(),
            "Uninstall",
            Pending::Uninstall,
        );
    }

    /// The confirmation dialog's Uninstall.
    pub(crate) fn uninstall_confirmed(&mut self, ui: &MainWindow) {
        self.run_setup(ui, "--uninstall-quiet", "Couldn't uninstall Ferrotop", |app, ui, ok| {
            if !ok {
                return;
            }
            app.settings.always_elevated = false;
            app.save_settings(ui);
            ui.set_start_at_sign_in(ferrotop_sys::system::sign_in_command().is_some());
            app.show_message(ui, "Uninstalled. Anything still running is removed when Windows restarts".into());
        });
    }

    pub(crate) fn always_elevated_toggled(&mut self, ui: &MainWindow, on: bool) {
        let (arguments, what) = if on {
            ("--enable-elevated-start", "Couldn't set up starting as administrator")
        } else {
            ("--disable-elevated-start", "Couldn't turn off starting as administrator")
        };
        self.run_setup(ui, arguments, what, move |app, ui, ok| {
            if !ok {
                ui.set_always_elevated(!on);
                return;
            }
            app.settings.always_elevated = on;
            app.save_settings(ui);
            if on {
                // "Run as administrator" in the copy's Properties would stop Task Manager opening it.
                if let Some(installed) = install::installed() {
                    let _ = ferrotop_sys::system::clear_run_as_admin_compatibility(&installed.exe);
                }
                let message = if ferrotop_sys::system::is_elevated() {
                    "Ferrotop will start as administrator without asking"
                } else {
                    "Ferrotop will start as administrator without asking, from its next start"
                };
                app.show_message(ui, message.into());
            } else {
                app.show_message(ui, "Ferrotop will start without administrator rights".into());
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_installed_offers_install_and_no_elevation() {
        let status = status(None);
        assert_eq!(status.button, "Install");
        assert!(status.button_enabled && !status.installed && !status.elevate_enabled);
        assert!(status.hint.starts_with("Install first"));
    }

    #[test]
    fn another_build_offers_an_update() {
        let installed =
            Installed { exe: std::path::PathBuf::from(r"C:\nowhere\ferrotop.exe"), version: Some("0.1.0".into()) };
        let status = status(Some(&installed));
        assert_eq!(status.button, "Update to this version");
        assert!(status.text.contains("version 0.1.0"), "{}", status.text);
        assert!(status.installed);
    }
}
