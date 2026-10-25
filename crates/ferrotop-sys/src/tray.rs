//! The notification-area (tray) icon, and finding an already-running Ferrotop.
//!
//! The icon talks to a hidden window owned by the UI thread; the window message loop Slint
//! already runs delivers its messages. It's an ordinary hidden window rather than a message-only
//! one so it also hears `TaskbarCreated`, and puts the icon back when Explorer restarts.

use std::cell::RefCell;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NIM_SETVERSION, NIN_SELECT,
    NOTIFYICON_VERSION_4, NOTIFYICONDATAW, NOTIFYICONDATAW_0, Shell_NotifyIconW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, ChangeWindowMessageFilterEx, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu,
    DestroyWindow, FindWindowW, GetCursorPos, GetSystemMetrics, HICON, IDI_APPLICATION, IMAGE_ICON, LR_DEFAULTCOLOR,
    LoadIconW, LoadImageW, MF_SEPARATOR, MF_STRING, MSGFLT_ALLOW, PostMessageW, RegisterClassExW,
    RegisterWindowMessageW, SM_CXSMICON, SM_CYSMICON, SetForegroundWindow, TPM_RETURNCMD, TPM_RIGHTBUTTON,
    TrackPopupMenu, WM_APP, WM_CONTEXTMENU, WM_LBUTTONDBLCLK, WM_NULL, WNDCLASSEXW, WS_EX_TOOLWINDOW, WS_POPUP,
};

use crate::buffer::wide;
use crate::{Error, Result};

const WINDOW_CLASS: &str = "Ferrotop.Tray";
/// The icon's callback message.
const WM_TRAY: u32 = WM_APP + 1;
/// `NIN_SELECT | NINF_KEY`: the icon chosen with the keyboard.
const NIN_KEYSELECT: u32 = NIN_SELECT | 1;
const MENU_SHOW: usize = 1;
const MENU_EXIT: usize = 2;

/// What the user did with the icon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayEvent {
    /// Clicked the icon, chose Show, or started Ferrotop again: bring the window back.
    Show,
    Exit,
}

/// What to call when the user does something with the icon.
type Handler = Box<dyn Fn(TrayEvent)>;

thread_local! {
    static HANDLER: RefCell<Option<Handler>> = const { RefCell::new(None) };
    /// The icon as last added, to put back after Explorer restarts.
    static ICON: RefCell<Option<NOTIFYICONDATAW>> = const { RefCell::new(None) };
}

/// The tray icon. Dropping it removes the icon.
#[derive(Debug)]
pub struct TrayIcon {
    window: usize,
}

impl TrayIcon {
    /// Adds the icon with tooltip `tooltip`. `on_event` runs on this thread, from the window
    /// procedure, so it should post work rather than do it. Call on the UI thread.
    pub fn add(tooltip: &str, on_event: impl Fn(TrayEvent) + 'static) -> Result<Self> {
        // SAFETY: a null name asks for this executable's own module handle.
        let module = unsafe { GetModuleHandleW(std::ptr::null()) };
        let class = wide(WINDOW_CLASS);
        let window_class = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(window_proc),
            hInstance: module,
            lpszClassName: class.as_ptr(),
            ..WNDCLASSEXW::default()
        };
        // SAFETY: the class structure and its name are valid for the call. Registering twice
        // fails harmlessly (the class already exists), so the result isn't checked.
        unsafe { RegisterClassExW(&window_class) };
        let title = wide("Ferrotop");
        // SAFETY: the class is registered and the strings are NUL-terminated; the window is never
        // shown.
        let window = unsafe {
            CreateWindowExW(
                WS_EX_TOOLWINDOW,
                class.as_ptr(),
                title.as_ptr(),
                WS_POPUP,
                0,
                0,
                0,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                module,
                std::ptr::null(),
            )
        };
        if window.is_null() {
            return Err(Error::last_win32("CreateWindowExW"));
        }
        // Another Ferrotop, possibly not elevated, may ask this one to show itself.
        // SAFETY: `window` is ours; no extended result is wanted.
        unsafe { ChangeWindowMessageFilterEx(window, show_message(), MSGFLT_ALLOW, std::ptr::null_mut()) };
        HANDLER.with_borrow_mut(|handler| *handler = Some(Box::new(on_event)));

        let mut data = NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: window,
            uID: 1,
            uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP,
            uCallbackMessage: WM_TRAY,
            hIcon: app_icon(),
            Anonymous: NOTIFYICONDATAW_0 { uVersion: NOTIFYICON_VERSION_4 },
            ..NOTIFYICONDATAW::default()
        };
        set_tip(&mut data, tooltip);
        let tray = Self { window: window.addr() };
        add_icon(&data)?;
        ICON.with_borrow_mut(|icon| *icon = Some(data));
        Ok(tray)
    }

    /// Changes the tooltip (Windows shows up to 127 characters).
    pub fn set_tooltip(&self, tooltip: &str) {
        ICON.with_borrow_mut(|icon| {
            if let Some(data) = icon.as_mut() {
                set_tip(data, tooltip);
                // SAFETY: `data` describes the icon this window added.
                unsafe { Shell_NotifyIconW(NIM_MODIFY, data) };
            }
        });
    }
}

impl Drop for TrayIcon {
    fn drop(&mut self) {
        if let Some(data) = ICON.with_borrow_mut(Option::take) {
            // SAFETY: removes the icon this window added.
            unsafe { Shell_NotifyIconW(NIM_DELETE, &data) };
        }
        HANDLER.with_borrow_mut(Option::take);
        // SAFETY: the window is ours and destroyed once.
        unsafe { DestroyWindow(std::ptr::without_provenance_mut(self.window)) };
    }
}

/// Whether another Ferrotop is already running in this session.
pub fn is_running() -> bool {
    let class = wide(WINDOW_CLASS);
    // SAFETY: the class name is NUL-terminated; a null title matches any.
    !unsafe { FindWindowW(class.as_ptr(), std::ptr::null()) }.is_null()
}

/// Asks an already-running Ferrotop to show its window. `true` if there was one to ask.
pub fn show_existing() -> bool {
    let class = wide(WINDOW_CLASS);
    // SAFETY: the class name is NUL-terminated; a null title matches any.
    let window = unsafe { FindWindowW(class.as_ptr(), std::ptr::null()) };
    // SAFETY: posting a registered message to a window handle has no memory-safety preconditions.
    !window.is_null() && unsafe { PostMessageW(window, show_message(), 0, 0) } != 0
}

/// The message another instance sends to say "show yourself".
fn show_message() -> u32 {
    let name = wide("Ferrotop.Show");
    // SAFETY: the name is NUL-terminated; the same name always gives the same message number.
    unsafe { RegisterWindowMessageW(name.as_ptr()) }
}

fn taskbar_created() -> u32 {
    let name = wide("TaskbarCreated");
    // SAFETY: as above.
    unsafe { RegisterWindowMessageW(name.as_ptr()) }
}

fn add_icon(data: &NOTIFYICONDATAW) -> Result<()> {
    // SAFETY: `data` is a fully initialized NOTIFYICONDATAW of the size it declares.
    if unsafe { Shell_NotifyIconW(NIM_ADD, data) } == 0 {
        return Err(Error::last_win32("Shell_NotifyIconW"));
    }
    // SAFETY: as above; opts in to the version 4 click messages.
    unsafe { Shell_NotifyIconW(NIM_SETVERSION, data) };
    Ok(())
}

fn set_tip(data: &mut NOTIFYICONDATAW, tooltip: &str) {
    data.szTip = [0; 128];
    for (slot, unit) in data.szTip.iter_mut().take(127).zip(tooltip.encode_utf16()) {
        *slot = unit;
    }
}

/// The app's own icon (resource 1) at the small-icon size, else Windows' generic one.
fn app_icon() -> HICON {
    // SAFETY: a null name asks for this executable's own module handle.
    let module = unsafe { GetModuleHandleW(std::ptr::null()) };
    // SAFETY: a plain query.
    let width = unsafe { GetSystemMetrics(SM_CXSMICON) };
    // SAFETY: as above.
    let height = unsafe { GetSystemMetrics(SM_CYSMICON) };
    // Resource 1, as MAKEINTRESOURCE does it.
    let name: *const u16 = std::ptr::without_provenance(1);
    // SAFETY: loads a resource of this module by ordinal; returns null when there isn't one (as
    // in tests).
    let icon = unsafe { LoadImageW(module, name, IMAGE_ICON, width, height, LR_DEFAULTCOLOR) };
    if icon.is_null() {
        // SAFETY: a stock icon; a null module selects the system's.
        unsafe { LoadIconW(std::ptr::null_mut(), IDI_APPLICATION) }
    } else {
        icon
    }
}

fn notify(event: TrayEvent) {
    HANDLER.with_borrow(|handler| {
        if let Some(handler) = handler {
            handler(event);
        }
    });
}

/// Shows the icon's menu at the cursor and returns the chosen item, if any.
fn show_menu(window: HWND) -> Option<usize> {
    // SAFETY: no preconditions.
    let menu = unsafe { CreatePopupMenu() };
    if menu.is_null() {
        return None;
    }
    let (show, exit) = (wide("Show Ferrotop"), wide("Exit"));
    // SAFETY: `menu` is ours and the labels are NUL-terminated for the calls.
    unsafe { AppendMenuW(menu, MF_STRING, MENU_SHOW, show.as_ptr()) };
    // SAFETY: as above.
    unsafe { AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null()) };
    // SAFETY: as above.
    unsafe { AppendMenuW(menu, MF_STRING, MENU_EXIT, exit.as_ptr()) };
    let mut cursor = POINT { x: 0, y: 0 };
    // SAFETY: `cursor` is a valid out pointer.
    unsafe { GetCursorPos(&mut cursor) };
    // The menu only closes when clicking elsewhere if its window is in the foreground.
    // SAFETY: `window` is ours.
    unsafe { SetForegroundWindow(window) };
    // SAFETY: `menu` and `window` are ours; TPM_RETURNCMD returns the chosen item instead of
    // posting it.
    let chosen = unsafe {
        TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON, cursor.x, cursor.y, 0, window, std::ptr::null())
    };
    // SAFETY: as documented for tray menus: lets the menu dismiss properly next time.
    unsafe { PostMessageW(window, WM_NULL, 0, 0) };
    // SAFETY: the menu is ours and destroyed once.
    unsafe { DestroyMenu(menu) };
    usize::try_from(chosen).ok().filter(|&item| item != 0)
}

unsafe extern "system" fn window_proc(window: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if message == WM_TRAY {
        // Version 4: the event is in the low word of `lparam`.
        match (lparam & 0xFFFF) as u32 {
            NIN_SELECT | NIN_KEYSELECT | WM_LBUTTONDBLCLK => notify(TrayEvent::Show),
            WM_CONTEXTMENU => match show_menu(window) {
                Some(MENU_SHOW) => notify(TrayEvent::Show),
                Some(MENU_EXIT) => notify(TrayEvent::Exit),
                _ => {}
            },
            _ => {}
        }
        return 0;
    }
    if message == show_message() {
        notify(TrayEvent::Show);
        return 0;
    }
    if message == taskbar_created() {
        ICON.with_borrow(|icon| {
            if let Some(data) = icon {
                let _ = add_icon(data);
            }
        });
        return 0;
    }
    // SAFETY: everything else gets the default handling.
    unsafe { DefWindowProcW(window, message, wparam, lparam) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tooltips_are_cut_to_fit() {
        let mut data = NOTIFYICONDATAW::default();
        set_tip(&mut data, &"x".repeat(300));
        assert_eq!(data.szTip.iter().filter(|&&unit| unit != 0).count(), 127);
        assert_eq!(data.szTip[127], 0);
        set_tip(&mut data, "CPU 5%");
        assert_eq!(String::from_utf16_lossy(&data.szTip[..6]), "CPU 5%");
        assert_eq!(data.szTip[6], 0);
    }

    #[test]
    fn registered_messages_are_stable() {
        assert_eq!(show_message(), show_message());
        assert_ne!(show_message(), 0);
    }
}
