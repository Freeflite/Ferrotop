//! Window chrome: matching the native title bar to the app's theme, and noticing when Windows
//! needs the window repainted.

use std::num::NonZeroIsize;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicIsize, Ordering};

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::Graphics::Dwm::{
    DWMWA_BORDER_COLOR, DWMWA_CAPTION_COLOR, DWMWA_TEXT_COLOR, DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWINDOWATTRIBUTE,
    DwmSetWindowAttribute,
};
use windows_sys::Win32::Graphics::Gdi::GetUpdateRect;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallWindowProcW, GWLP_WNDPROC, GetWindowLongPtrW, SetWindowLongPtrW, WM_PAINT, WNDPROC,
};

use crate::{Error, Result};

/// An 8-bit-per-channel RGB color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    /// Win32 `COLORREF` layout: `0x00BBGGRR`.
    fn colorref(self) -> u32 {
        u32::from(self.0) | (u32::from(self.1) << 8) | (u32::from(self.2) << 16)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TitleBarStyle {
    /// Dark caption buttons and system menu.
    pub dark: bool,
    pub caption: Rgb,
    pub text: Rgb,
    pub border: Rgb,
}

/// Restyles a top-level window's title bar. `hwnd` is the window's handle value.
///
/// Dark mode works on Windows 10 20H1 and later; custom caption, text and border colors need
/// Windows 11, and are skipped quietly on older systems.
pub fn style_title_bar(hwnd: NonZeroIsize, style: &TitleBarStyle) -> Result<()> {
    // An HWND is an opaque handle value, not a pointer to memory we own.
    let hwnd: HWND = std::ptr::without_provenance_mut(hwnd.get() as usize);
    set_attribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, u32::from(style.dark))?;
    let _ = set_attribute(hwnd, DWMWA_CAPTION_COLOR, style.caption.colorref());
    let _ = set_attribute(hwnd, DWMWA_TEXT_COLOR, style.text.colorref());
    let _ = set_attribute(hwnd, DWMWA_BORDER_COLOR, style.border.colorref());
    Ok(())
}

/// Sets a 4-byte window attribute (every attribute used here is a BOOL or COLORREF).
fn set_attribute(hwnd: HWND, attribute: DWMWINDOWATTRIBUTE, value: u32) -> Result<()> {
    // SAFETY: `value` is a live u32 and its size is passed; DWM validates the window handle and
    // fails with an error code for one that isn't a valid top-level window.
    let result =
        unsafe { DwmSetWindowAttribute(hwnd, attribute as u32, (&raw const value).cast(), size_of::<u32>() as u32) };
    if result < 0 { Err(Error::HResult { call: "DwmSetWindowAttribute", hresult: result }) } else { Ok(()) }
}

/// The window procedure [`watch_repaints`] took over from, and what it calls.
static PREVIOUS_PROC: AtomicIsize = AtomicIsize::new(0);
static ON_EXPOSED: OnceLock<Box<dyn Fn() + Send + Sync>> = OnceLock::new();

/// Calls `exposed` whenever Windows asks for part of the window to be painted again (after it's
/// maximized or restored, for one), so the app can repaint all of it.
///
/// Slint's software renderer copies to the screen only what it changed itself, so areas Windows
/// lost (maximizing a window that's already that size drops them) would otherwise stay blank.
/// One window per process; later calls do nothing.
pub fn watch_repaints(hwnd: NonZeroIsize, exposed: impl Fn() + Send + Sync + 'static) -> Result<()> {
    if ON_EXPOSED.set(Box::new(exposed)).is_err() {
        return Ok(());
    }
    // An HWND is an opaque handle value, not a pointer to memory we own.
    let hwnd: HWND = std::ptr::without_provenance_mut(hwnd.get() as usize);
    // SAFETY: reads a window's procedure; fails (returns 0) for a window that isn't valid.
    let previous = unsafe { GetWindowLongPtrW(hwnd, GWLP_WNDPROC) };
    if previous == 0 {
        return Err(Error::last_win32("GetWindowLongPtrW"));
    }
    // Stored before the swap, so the new procedure always has one to pass messages on to.
    PREVIOUS_PROC.store(previous, Ordering::Release);
    let procedure: unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT = repaint_watcher;
    // SAFETY: replaces the procedure of a window this thread created with `repaint_watcher`, which
    // hands every message on to the previous procedure unchanged.
    if unsafe { SetWindowLongPtrW(hwnd, GWLP_WNDPROC, procedure as usize as isize) } == 0 {
        return Err(Error::last_win32("SetWindowLongPtrW"));
    }
    Ok(())
}

/// Notices `WM_PAINT`s that come with an update region (Windows wants pixels back); Slint's own
/// redraws come with none. Every message then goes to the window's own procedure.
unsafe extern "system" fn repaint_watcher(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if message == WM_PAINT {
        // SAFETY: with a null rectangle this only asks whether the window has an update region.
        let invalid = unsafe { GetUpdateRect(hwnd, std::ptr::null_mut(), 0) } != 0;
        if invalid && let Some(exposed) = ON_EXPOSED.get() {
            exposed();
        }
    }
    let previous = PREVIOUS_PROC.load(Ordering::Acquire);
    // SAFETY: `previous` is the window procedure GetWindowLongPtrW returned for this window (never
    // 0 once this procedure is installed); an `Option` of a function pointer has the same layout
    // as the pointer-sized integer it came as.
    let previous: WNDPROC = unsafe { std::mem::transmute::<isize, WNDPROC>(previous) };
    // SAFETY: passes the message, unchanged, to the procedure that handled it before.
    unsafe { CallWindowProcW(previous, hwnd, message, wparam, lparam) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colorref_is_bgr() {
        assert_eq!(Rgb(0x12, 0x34, 0x56).colorref(), 0x0056_3412);
    }

    #[test]
    fn rejects_a_bogus_window() {
        let style = TitleBarStyle { dark: true, caption: Rgb(0, 0, 0), text: Rgb(255, 255, 255), border: Rgb(0, 0, 0) };
        assert!(style_title_bar(NonZeroIsize::new(0x1234_5678).unwrap(), &style).is_err());
    }
}
