//! Primary-display screenshot for the side panel's Capture Screen button.
//!
//! Captures the primary monitor at its physical resolution, crops the top and
//! bottom 10%, saves a PNG to the user's Downloads folder and copies the image to
//! the clipboard. The image stays on this machine: it is never sent to Chrome
//! (Native Messaging caps host messages at 1 MB) or to the network.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use windows::core::w;
use windows::Win32::Foundation::{GlobalFree, HANDLE};
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, GetDIBits,
    ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, CAPTUREBLT, DIB_RGB_COLORS,
    ROP_CODE, SRCCOPY,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::UI::HiDpi::{SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::UI::Shell::{FOLDERID_Downloads, SHGetKnownFolderPath, KNOWN_FOLDER_FLAG};
use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};

use crate::log_warn;

/// Fraction of the screen height removed from the top and from the bottom.
const CROP_FRACTION: f64 = 0.10;
/// Standard clipboard format (winuser.h); defined here to avoid the large Ole bindings.
const CF_DIB: u32 = 8;

/// Top-down 32-bit BGRA pixels.
pub struct Screenshot {
    pub width: u32,
    pub height: u32,
    bgra: Vec<u8>,
}

pub struct CaptureOutcome {
    pub path: Option<PathBuf>,
    pub copied: bool,
    pub width: u32,
    pub height: u32,
}

/// Rows to skip at the top and rows to keep, for a screen `height` pixels tall.
fn crop_rows(height: u32) -> (u32, u32) {
    let crop = (height as f64 * CROP_FRACTION).round() as u32;
    (crop, height - 2 * crop)
}

/// Blocking. Run it on its own thread: it switches that thread to per-monitor DPI
/// awareness so the capture covers the whole screen at physical resolution instead
/// of a DPI-scaled part of it.
pub fn capture_and_save() -> Result<CaptureOutcome, String> {
    let shot = capture_primary_cropped()?;
    let png = encode_png(&shot)?;
    let path = save_png(&png)
        .map_err(|e| log_warn!("Could not save screenshot: {e}"))
        .ok();
    let copied = copy_to_clipboard(&shot, &png)
        .map_err(|e| log_warn!("Could not copy screenshot: {e}"))
        .is_ok();
    if path.is_none() && !copied {
        return Err("screenshot could not be saved or copied".into());
    }
    Ok(CaptureOutcome {
        path,
        copied,
        width: shot.width,
        height: shot.height,
    })
}

fn capture_primary_cropped() -> Result<Screenshot, String> {
    unsafe {
        SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let width = GetSystemMetrics(SM_CXSCREEN);
        let full_height = GetSystemMetrics(SM_CYSCREEN);
        if width <= 0 || full_height <= 0 {
            return Err(format!("invalid primary display size {width}x{full_height}"));
        }
        let (top, height) = crop_rows(full_height as u32);

        let screen = GetDC(None);
        if screen.is_invalid() {
            return Err("GetDC(screen) failed".into());
        }
        let memory = CreateCompatibleDC(Some(screen));
        let bitmap = CreateCompatibleBitmap(screen, width, height as i32);
        let previous = SelectObject(memory, bitmap.into());
        let blit = BitBlt(
            memory,
            0,
            0,
            width,
            height as i32,
            Some(screen),
            0,
            top as i32,
            ROP_CODE(SRCCOPY.0 | CAPTUREBLT.0),
        );
        SelectObject(memory, previous);

        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -(height as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bgra = vec![0u8; width as usize * height as usize * 4];
        let lines = GetDIBits(
            memory,
            bitmap,
            0,
            height,
            Some(bgra.as_mut_ptr().cast()),
            &mut info,
            DIB_RGB_COLORS,
        );

        let _ = DeleteObject(bitmap.into());
        let _ = DeleteDC(memory);
        ReleaseDC(None, screen);

        blit.map_err(|e| format!("BitBlt failed: {e}"))?;
        if lines != height as i32 {
            return Err(format!("GetDIBits returned {lines} of {height} rows"));
        }
        Ok(Screenshot {
            width: width as u32,
            height,
            bgra,
        })
    }
}

fn encode_png(shot: &Screenshot) -> Result<Vec<u8>, String> {
    let mut rgb = Vec::with_capacity(shot.width as usize * shot.height as usize * 3);
    for px in shot.bgra.chunks_exact(4) {
        rgb.extend_from_slice(&[px[2], px[1], px[0]]);
    }
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, shot.width, shot.height);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::Fast);
    let mut writer = encoder.write_header().map_err(|e| format!("PNG header: {e}"))?;
    writer
        .write_image_data(&rgb)
        .map_err(|e| format!("PNG encode: {e}"))?;
    writer.finish().map_err(|e| format!("PNG finish: {e}"))?;
    Ok(out)
}

fn downloads_dir() -> Result<PathBuf, String> {
    unsafe {
        let raw = SHGetKnownFolderPath(&FOLDERID_Downloads, KNOWN_FOLDER_FLAG(0), None)
            .map_err(|e| format!("Downloads folder: {e}"))?;
        let path = raw.to_string();
        CoTaskMemFree(Some(raw.0 as *const _));
        path.map(PathBuf::from)
            .map_err(|e| format!("Downloads folder path: {e}"))
    }
}

fn unique_path(dir: &Path, stem: &str) -> PathBuf {
    let mut path = dir.join(format!("{stem}.png"));
    let mut n = 2;
    while path.exists() {
        path = dir.join(format!("{stem} ({n}).png"));
        n += 1;
    }
    path
}

fn save_png(png: &[u8]) -> Result<PathBuf, String> {
    let dir = downloads_dir()?;
    let t = unsafe { GetLocalTime() };
    let stem = format!(
        "Screenshot {:04}-{:02}-{:02} {:02}{:02}{:02}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    );
    let path = unique_path(&dir, &stem);
    fs::write(&path, png).map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(path)
}

/// Packed DIB (BITMAPINFOHEADER + bottom-up 24-bit rows) for CF_DIB. 24-bit avoids
/// apps treating the zero alpha channel GDI returns as transparency.
fn build_dib(shot: &Screenshot) -> Vec<u8> {
    let (w, h) = (shot.width as usize, shot.height as usize);
    let stride = (w * 3 + 3) & !3;
    let mut out = Vec::with_capacity(40 + stride * h);
    out.extend_from_slice(&40u32.to_le_bytes()); // biSize
    out.extend_from_slice(&(w as i32).to_le_bytes()); // biWidth
    out.extend_from_slice(&(h as i32).to_le_bytes()); // biHeight > 0: bottom-up
    out.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
    out.extend_from_slice(&24u16.to_le_bytes()); // biBitCount
    out.extend_from_slice(&0u32.to_le_bytes()); // biCompression = BI_RGB
    out.extend_from_slice(&((stride * h) as u32).to_le_bytes()); // biSizeImage
    out.extend_from_slice(&[0u8; 16]); // resolution, colors used/important
    for y in (0..h).rev() {
        let row = &shot.bgra[y * w * 4..(y + 1) * w * 4];
        for px in row.chunks_exact(4) {
            out.extend_from_slice(&px[..3]);
        }
        out.resize(out.len() + (stride - w * 3), 0);
    }
    out
}

unsafe fn set_clipboard_bytes(format: u32, bytes: &[u8]) -> Result<(), String> {
    let memory = GlobalAlloc(GMEM_MOVEABLE, bytes.len()).map_err(|e| format!("GlobalAlloc: {e}"))?;
    let target = GlobalLock(memory);
    if target.is_null() {
        let _ = GlobalFree(Some(memory));
        return Err("GlobalLock failed".into());
    }
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), target.cast::<u8>(), bytes.len());
    let _ = GlobalUnlock(memory);
    if let Err(e) = SetClipboardData(format, Some(HANDLE(memory.0))) {
        let _ = GlobalFree(Some(memory));
        return Err(format!("SetClipboardData: {e}"));
    }
    Ok(())
}

/// Puts the image on the clipboard as CF_DIB (every app) and "PNG" (browsers, Office).
fn copy_to_clipboard(shot: &Screenshot, png: &[u8]) -> Result<(), String> {
    let dib = build_dib(shot);
    unsafe {
        let mut opened = OpenClipboard(None);
        for _ in 0..10 {
            if opened.is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
            opened = OpenClipboard(None);
        }
        opened.map_err(|e| format!("OpenClipboard: {e}"))?;

        let result = EmptyClipboard()
            .map_err(|e| format!("EmptyClipboard: {e}"))
            .and_then(|()| set_clipboard_bytes(CF_DIB, &dib))
            .map(|()| {
                let png_format = RegisterClipboardFormatW(w!("PNG"));
                if png_format != 0 {
                    if let Err(e) = set_clipboard_bytes(png_format, png) {
                        log_warn!("PNG clipboard format not set: {e}");
                    }
                }
            });
        let _ = CloseClipboard();
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crops_ten_percent_top_and_bottom() {
        assert_eq!(crop_rows(1080), (108, 864));
        assert_eq!(crop_rows(1440), (144, 1152));
        assert_eq!(crop_rows(2160), (216, 1728));
        assert_eq!(crop_rows(1050), (105, 840));
    }

    #[test]
    fn dib_is_bottom_up_24_bit_with_padded_rows() {
        // 2x2: top row red, green; bottom row blue, white (BGRA).
        let shot = Screenshot {
            width: 2,
            height: 2,
            bgra: vec![0, 0, 255, 0, 0, 255, 0, 0, 255, 0, 0, 0, 255, 255, 255, 0],
        };
        let dib = build_dib(&shot);
        assert_eq!(dib.len(), 40 + 8 * 2);
        assert_eq!(&dib[14..16], &24u16.to_le_bytes());
        assert_eq!(&dib[40..48], &[255, 0, 0, 255, 255, 255, 0, 0]); // bottom row first
        assert_eq!(&dib[48..56], &[0, 0, 255, 0, 255, 0, 0, 0]);
    }

    #[test]
    fn png_has_cropped_dimensions() {
        let shot = Screenshot {
            width: 3,
            height: 2,
            bgra: vec![10; 3 * 2 * 4],
        };
        let png = encode_png(&shot).unwrap();
        assert_eq!(&png[1..4], b"PNG");
        assert_eq!(&png[16..24], &[0, 0, 0, 3, 0, 0, 0, 2]);
    }
}
