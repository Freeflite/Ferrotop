//! Window chrome: matching the native title bar to the app's theme.

use std::num::NonZeroIsize;

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::Graphics::Dwm::{
    DWMWA_BORDER_COLOR, DWMWA_CAPTION_COLOR, DWMWA_TEXT_COLOR, DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWINDOWATTRIBUTE,
    DwmSetWindowAttribute,
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
