//! Top-level windows and the processes that own them, and picking a window with the mouse.

use std::cell::RefCell;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, TRUE, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, EnumWindows, GA_ROOT, GetAncestor, GetClassNameW, GetWindowRect, GetWindowThreadProcessId,
    HC_ACTION, HHOOK, InternalGetWindowText, IsIconic, IsWindowVisible, IsZoomed, MSLLHOOKSTRUCT, SW_RESTORE,
    SetForegroundWindow, SetWindowsHookExW, ShowWindowAsync, UnhookWindowsHookEx, WH_MOUSE_LL, WM_LBUTTONDOWN,
    WM_LBUTTONUP, WM_RBUTTONDOWN, WM_RBUTTONUP, WindowFromPoint,
};
use windows_sys::core::BOOL;

use crate::{Error, Result};

/// Longest window title or class name read, in UTF-16 units.
const MAX_TEXT: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowInfo {
    /// The window handle's value.
    pub handle: usize,
    pub pid: u32,
    pub tid: u32,
    pub title: String,
    pub class: String,
    pub visible: bool,
    pub minimized: bool,
    pub maximized: bool,
    /// Left, top, right and bottom, in screen pixels.
    pub rect: Option<(i32, i32, i32, i32)>,
}

impl WindowInfo {
    pub fn state(&self) -> &'static str {
        match (self.visible, self.minimized, self.maximized) {
            (false, _, _) => "Hidden",
            (true, true, _) => "Minimized",
            (true, false, true) => "Maximized",
            (true, false, false) => "Visible",
        }
    }
}

/// Every top-level window on this desktop, frontmost first.
pub fn top_level_windows() -> Result<Vec<WindowInfo>> {
    let mut handles: Vec<HWND> = Vec::new();
    let target = (&raw mut handles).expose_provenance() as LPARAM;
    // SAFETY: `collect` only pushes onto the Vec that `target` points at, which outlives this
    // call; EnumWindows calls it synchronously and never after returning.
    if unsafe { EnumWindows(Some(collect), target) } == 0 {
        return Err(Error::last_win32("EnumWindows"));
    }
    // A window can close between the two steps; it's then left out.
    Ok(handles.into_iter().filter_map(window_info).collect())
}

/// The top-level windows that process `pid` owns.
pub fn windows_of(pid: u32) -> Result<Vec<WindowInfo>> {
    Ok(top_level_windows()?.into_iter().filter(|w| w.pid == pid).collect())
}

/// `EnumWindows` callback: appends each handle to the `Vec<HWND>` that `target` points at.
unsafe extern "system" fn collect(hwnd: HWND, target: LPARAM) -> BOOL {
    let handles: *mut Vec<HWND> = std::ptr::with_exposed_provenance_mut(target as usize);
    // SAFETY: `target` is the address of the live, otherwise unborrowed Vec in
    // `top_level_windows`, exposed there.
    unsafe { (*handles).push(hwnd) };
    TRUE
}

/// The top-level window containing screen point (`x`, `y`).
pub fn window_at(x: i32, y: i32) -> Option<WindowInfo> {
    // SAFETY: no preconditions; returns null where there's no window.
    let hwnd = unsafe { WindowFromPoint(POINT { x, y }) };
    if hwnd.is_null() {
        return None;
    }
    // SAFETY: as above; a stale handle gives null.
    let root = unsafe { GetAncestor(hwnd, GA_ROOT) };
    window_info(if root.is_null() { hwnd } else { root })
}

fn window_info(hwnd: HWND) -> Option<WindowInfo> {
    let mut pid = 0u32;
    // SAFETY: `pid` is a valid out pointer; a handle to a window that has closed returns 0.
    let tid = unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
    if tid == 0 {
        return None;
    }
    // InternalGetWindowText reads the stored caption without sending WM_GETTEXT, so a hung
    // window can't stall us (GetWindowText would send it to windows of this process).
    let title = read_text(|buffer, len| {
        // SAFETY: `buffer` is writable for `len` UTF-16 units.
        unsafe { InternalGetWindowText(hwnd, buffer, len) }
    });
    let class = read_text(|buffer, len| {
        // SAFETY: as above.
        unsafe { GetClassNameW(hwnd, buffer, len) }
    });
    // SAFETY: plain queries on a window handle; a stale one reads as false.
    let visible = unsafe { IsWindowVisible(hwnd) } != 0;
    // SAFETY: as above.
    let minimized = unsafe { IsIconic(hwnd) } != 0;
    // SAFETY: as above.
    let maximized = unsafe { IsZoomed(hwnd) } != 0;
    let mut rect = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    // SAFETY: `rect` is a valid out pointer.
    let rect =
        (unsafe { GetWindowRect(hwnd, &mut rect) } != 0).then_some((rect.left, rect.top, rect.right, rect.bottom));
    Some(WindowInfo { handle: hwnd.addr(), pid, tid, title, class, visible, minimized, maximized, rect })
}

/// Restores window `handle` if it's minimized, and brings it to the front.
pub fn bring_to_front(handle: usize) -> Result<()> {
    // A window handle is an opaque value, not a pointer to memory.
    let hwnd: HWND = std::ptr::without_provenance_mut(handle);
    // SAFETY: a plain query; a stale handle reads as false.
    if unsafe { IsIconic(hwnd) } != 0 {
        // SAFETY: ShowWindowAsync only posts the request, so a hung window can't stall us.
        unsafe { ShowWindowAsync(hwnd, SW_RESTORE) };
    }
    // SAFETY: as above; Windows allows it while this process is in the foreground.
    if unsafe { SetForegroundWindow(hwnd) } == 0 {
        return Err(Error::last_win32("SetForegroundWindow"));
    }
    Ok(())
}

/// Reads a string through a Win32 call that fills a UTF-16 buffer and returns its length.
fn read_text(call: impl FnOnce(*mut u16, i32) -> i32) -> String {
    let mut buffer = [0u16; MAX_TEXT];
    let len = usize::try_from(call(buffer.as_mut_ptr(), MAX_TEXT as i32)).unwrap_or(0).min(MAX_TEXT);
    String::from_utf16_lossy(&buffer[..len])
}

/// What to do when the user picks (`Some`) or cancels (`None`).
type PickDone = Box<dyn FnOnce(Option<WindowInfo>)>;

/// An active pick: the mouse hook and what to call; then, after the click, the release that
/// must be swallowed too.
enum Pick {
    Waiting(HHOOK, PickDone),
    Releasing(HHOOK),
}

thread_local! {
    static PICK: RefCell<Option<Pick>> = const { RefCell::new(None) };
}

/// Waits for the user's next click anywhere on screen and reports the top-level window under
/// it to `done`, or `None` for a right-click, which cancels. The click is swallowed, so the
/// window under it never receives it.
///
/// Uses a low-level mouse hook, which runs on this thread: call it on a thread that pumps
/// window messages (the UI thread). `done` runs there too, from inside the hook, so it should
/// only post work rather than do it.
pub fn pick_window(done: impl FnOnce(Option<WindowInfo>) + 'static) -> Result<()> {
    cancel_pick();
    // SAFETY: a null name asks for this executable's own module handle.
    let module = unsafe { GetModuleHandleW(std::ptr::null()) };
    // SAFETY: `pick_hook` is a valid hook procedure for the life of the process; a low-level
    // hook loads no DLL into other processes.
    let hook = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(pick_hook), module, 0) };
    if hook.is_null() {
        return Err(Error::last_win32("SetWindowsHookExW"));
    }
    PICK.with_borrow_mut(|pick| *pick = Some(Pick::Waiting(hook, Box::new(done))));
    Ok(())
}

/// Stops waiting for a pick, if one is in progress, without calling its `done`.
pub fn cancel_pick() {
    if let Some(Pick::Waiting(hook, _) | Pick::Releasing(hook)) = PICK.with_borrow_mut(Option::take) {
        unhook(hook);
    }
}

pub fn is_picking() -> bool {
    PICK.with_borrow(Option::is_some)
}

fn unhook(hook: HHOOK) {
    // SAFETY: `hook` came from SetWindowsHookExW and is unhooked once (it left PICK first).
    unsafe { UnhookWindowsHookEx(hook) };
}

/// Swallows the first button press (and its release), reporting the window under it.
unsafe extern "system" fn pick_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let message = wparam as u32;
        if matches!(message, WM_LBUTTONDOWN | WM_RBUTTONDOWN) {
            let point: *const MSLLHOOKSTRUCT = std::ptr::with_exposed_provenance(lparam as usize);
            // SAFETY: for WH_MOUSE_LL with HC_ACTION, `lparam` points at an MSLLHOOKSTRUCT that
            // is valid for the duration of this call.
            let point = unsafe { (*point).pt };
            let waiting = PICK.with_borrow_mut(|pick| match pick.take() {
                Some(Pick::Waiting(hook, done)) => {
                    *pick = Some(Pick::Releasing(hook));
                    Some(done)
                }
                other => {
                    *pick = other;
                    None
                }
            });
            if let Some(done) = waiting {
                done((message == WM_LBUTTONDOWN).then(|| window_at(point.x, point.y)).flatten());
                return 1;
            }
        } else if matches!(message, WM_LBUTTONUP | WM_RBUTTONUP)
            && let Some(Pick::Releasing(hook)) = PICK.with_borrow_mut(Option::take)
        {
            unhook(hook);
            return 1;
        }
    }
    // SAFETY: passes the event on unchanged, as every hook must when it doesn't consume it.
    unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_windows_with_their_owners() {
        let windows = top_level_windows().expect("windows");
        // Every desktop has windows (the shell's, or at least hidden helper windows).
        assert!(!windows.is_empty());
        assert!(windows.iter().all(|w| w.pid != 0 || w.tid != 0));
        for window in windows.iter().take(20) {
            assert!(window.title.encode_utf16().count() <= MAX_TEXT && !window.state().is_empty());
        }
        let first = &windows[0];
        assert!(windows_of(first.pid).expect("windows of a process").iter().all(|w| w.pid == first.pid));
    }

    #[test]
    fn cancelling_without_a_pick_is_harmless() {
        assert!(!is_picking());
        cancel_pick();
        assert!(!is_picking());
    }
}
