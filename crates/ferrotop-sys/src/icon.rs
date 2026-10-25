//! Application icons for the process list, extracted from executables as RGBA pixels.

use std::ffi::c_void;

use windows_sys::Win32::Graphics::Gdi::{
    BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC, DeleteObject,
    GetDIBits, GetObjectW, HBITMAP, HDC, RGBQUAD,
};
use windows_sys::Win32::UI::Shell::ExtractIconExW;
use windows_sys::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, HICON, ICONINFO};

use crate::buffer::wide;
use crate::{Error, Result};

/// Straight-alpha RGBA pixels, row by row from the top.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IconPixels {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

struct Icon(HICON);
impl Drop for Icon {
    fn drop(&mut self) {
        // SAFETY: the icon came from ExtractIconExW and is destroyed exactly once.
        unsafe { DestroyIcon(self.0) };
    }
}

struct Bitmap(HBITMAP);
impl Drop for Bitmap {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the bitmap came from GetIconInfo, which hands ownership to the caller.
            unsafe { DeleteObject(self.0) };
        }
    }
}

struct MemoryDc(HDC);
impl Drop for MemoryDc {
    fn drop(&mut self) {
        // SAFETY: the DC came from CreateCompatibleDC and is deleted exactly once.
        unsafe { DeleteDC(self.0) };
    }
}

/// The large (usually 32x32) icon of an executable or DLL.
pub fn file_icon(path: &str) -> Result<IconPixels> {
    let path = wide(path);
    let mut large: HICON = std::ptr::null_mut();
    // SAFETY: `path` is NUL-terminated and `large` is a valid out-pointer for one icon.
    let extracted = unsafe { ExtractIconExW(path.as_ptr(), 0, &mut large, std::ptr::null_mut(), 1) };
    if extracted == 0 || large.is_null() {
        return Err(Error::Malformed { call: "ExtractIconExW" });
    }
    let icon = Icon(large);

    let mut info =
        ICONINFO { fIcon: 0, xHotspot: 0, yHotspot: 0, hbmMask: std::ptr::null_mut(), hbmColor: std::ptr::null_mut() };
    // SAFETY: `icon` is a live icon and `info` is a writable ICONINFO.
    if unsafe { GetIconInfo(icon.0, &mut info) } == 0 {
        return Err(Error::last_win32("GetIconInfo"));
    }
    let (mask, color) = (Bitmap(info.hbmMask), Bitmap(info.hbmColor));
    if color.0.is_null() {
        // A monochrome icon; rare enough not to bother converting.
        return Err(Error::Malformed { call: "GetIconInfo" });
    }

    let mut bitmap = BITMAP {
        bmType: 0,
        bmWidth: 0,
        bmHeight: 0,
        bmWidthBytes: 0,
        bmPlanes: 0,
        bmBitsPixel: 0,
        bmBits: std::ptr::null_mut(),
    };
    // SAFETY: `color` is a live bitmap and `bitmap` is a writable BITMAP of the size passed.
    if unsafe { GetObjectW(color.0, size_of::<BITMAP>() as i32, (&raw mut bitmap).cast::<c_void>()) } == 0 {
        return Err(Error::Malformed { call: "GetObjectW" });
    }
    let (width, height) = (bitmap.bmWidth, bitmap.bmHeight);
    if !(1..=256).contains(&width) || !(1..=256).contains(&height) {
        return Err(Error::Malformed { call: "GetObjectW" });
    }

    // SAFETY: a null DC means "compatible with the screen"; no other preconditions.
    let dc = MemoryDc(unsafe { CreateCompatibleDC(std::ptr::null_mut()) });
    if dc.0.is_null() {
        return Err(Error::last_win32("CreateCompatibleDC"));
    }
    let mut bgra = read_bits(&dc, &color, width, height)?;

    // Icons without an alpha channel use the AND mask for transparency (black = opaque).
    if bgra.as_chunks::<4>().0.iter().all(|pixel| pixel[3] == 0) {
        let mask_bits = read_bits(&dc, &mask, width, height)?;
        for (pixel, mask) in bgra.as_chunks_mut::<4>().0.iter_mut().zip(mask_bits.as_chunks::<4>().0) {
            pixel[3] = if mask[0] == 0 { 255 } else { 0 };
        }
    }
    for pixel in bgra.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2); // BGRA → RGBA
    }
    Ok(IconPixels { width: width as u32, height: height as u32, rgba: bgra })
}

/// A bitmap's pixels as top-down 32-bit BGRA.
fn read_bits(dc: &MemoryDc, bitmap: &Bitmap, width: i32, height: i32) -> Result<Vec<u8>> {
    if bitmap.0.is_null() {
        return Err(Error::Malformed { call: "GetDIBits" });
    }
    let mut header = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            biHeight: -height, // negative: top-down rows
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB,
            biSizeImage: 0,
            biXPelsPerMeter: 0,
            biYPelsPerMeter: 0,
            biClrUsed: 0,
            biClrImportant: 0,
        },
        bmiColors: [RGBQUAD { rgbBlue: 0, rgbGreen: 0, rgbRed: 0, rgbReserved: 0 }],
    };
    let mut pixels = vec![0u8; width as usize * height as usize * 4];
    // SAFETY: `dc` and `bitmap` are live and the bitmap isn't selected into any DC; `pixels`
    // holds exactly width x height 32-bit pixels, which is what the header requests.
    let lines =
        unsafe { GetDIBits(dc.0, bitmap.0, 0, height as u32, pixels.as_mut_ptr().cast(), &mut header, DIB_RGB_COLORS) };
    if lines == 0 { Err(Error::Malformed { call: "GetDIBits" }) } else { Ok(pixels) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_an_icon_with_some_opaque_pixels() {
        let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
        let icon = file_icon(&format!(r"{root}\explorer.exe")).expect("explorer icon");
        assert_eq!(icon.rgba.len(), (icon.width * icon.height * 4) as usize);
        assert!(icon.rgba.as_chunks::<4>().0.iter().any(|p| p[3] == 255));
        assert!(icon.rgba.as_chunks::<4>().0.iter().any(|p| p[3] == 0), "corners should be transparent");
    }

    #[test]
    fn missing_files_are_errors_not_panics() {
        assert!(file_icon(r"C:\definitely\not\here.exe").is_err());
    }
}
