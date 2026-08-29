//! Extract a Windows executable's embedded icon as a PNG `data:` URL so the UI
//! can show real per-game icons. Uses only documented shell + GDI calls on the
//! file path the user pointed us at — no injection, no game memory. Every GDI
//! handle is released before returning.

use std::mem::size_of;
use std::path::{Component, Path, PathBuf, Prefix};

use base64::Engine as _;
use windows_sys::Win32::Graphics::Gdi::{
    DeleteObject, GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO, BITMAPINFOHEADER,
    BI_RGB, DIB_RGB_COLORS,
};
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL;
use windows_sys::Win32::UI::Shell::{SHGetFileInfoW, SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON};
use windows_sys::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, HICON, ICONINFO};

/// The shell's "large" icon is 32x32 — plenty for a list badge.
pub fn extract_exe_icon_png(exe_path: &str) -> Option<Vec<u8>> {
    let path = validated_local_exe_path(Path::new(exe_path.trim()))?;
    let wide = crate::windows::wide_null(path.as_os_str());

    unsafe {
        let mut info: SHFILEINFOW = std::mem::zeroed();
        let ok = SHGetFileInfoW(
            wide.as_ptr(),
            FILE_ATTRIBUTE_NORMAL,
            &mut info,
            size_of::<SHFILEINFOW>() as u32,
            SHGFI_ICON | SHGFI_LARGEICON,
        );
        if ok == 0 || info.hIcon.is_null() {
            return None;
        }
        let png = icon_to_png(info.hIcon);
        DestroyIcon(info.hIcon);
        png
    }
}

fn validated_local_exe_path(path: &Path) -> Option<PathBuf> {
    if path.as_os_str().is_empty()
        || path.as_os_str().to_string_lossy().contains('\0')
        || !path.is_absolute()
        || !has_local_disk_prefix(path, false)
        || !path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("exe"))
    {
        return None;
    }

    let canonical = path.canonicalize().ok()?;
    if !has_local_disk_prefix(&canonical, true) || !std::fs::metadata(&canonical).ok()?.is_file() {
        return None;
    }
    Some(path.to_path_buf())
}

fn has_local_disk_prefix(path: &Path, allow_verbatim: bool) -> bool {
    matches!(
        path.components().next(),
        Some(Component::Prefix(prefix))
            if matches!(prefix.kind(), Prefix::Disk(_))
                || (allow_verbatim && matches!(prefix.kind(), Prefix::VerbatimDisk(_)))
    )
}

/// Wrap PNG bytes as a `data:` URL the webview can use directly in `<img src>`.
pub fn png_data_url(png: &[u8]) -> String {
    let b64 = base64::engine::general_purpose::STANDARD.encode(png);
    format!("data:image/png;base64,{b64}")
}

/// Convenience: extract an exe's icon straight to a `data:` URL.
pub fn extract_exe_icon_data_url(exe_path: &str) -> Option<String> {
    extract_exe_icon_png(exe_path).map(|png| png_data_url(&png))
}

unsafe fn icon_to_png(hicon: HICON) -> Option<Vec<u8>> {
    let mut icon_info: ICONINFO = std::mem::zeroed();
    if GetIconInfo(hicon, &mut icon_info) == 0 {
        return None;
    }
    let hbm_color = icon_info.hbmColor;
    let hbm_mask = icon_info.hbmMask;

    // Pull the color bitmap as top-down 32-bit BGRA, then free both bitmaps.
    let result = color_bitmap_to_png(hbm_color);
    if !hbm_color.is_null() {
        DeleteObject(hbm_color as _);
    }
    if !hbm_mask.is_null() {
        DeleteObject(hbm_mask as _);
    }
    result
}

unsafe fn color_bitmap_to_png(
    hbm_color: windows_sys::Win32::Graphics::Gdi::HBITMAP,
) -> Option<Vec<u8>> {
    if hbm_color.is_null() {
        return None;
    }
    let mut bm: BITMAP = std::mem::zeroed();
    if GetObjectW(
        hbm_color as _,
        size_of::<BITMAP>() as i32,
        (&mut bm as *mut BITMAP).cast(),
    ) == 0
    {
        return None;
    }
    let (width, height) = (bm.bmWidth, bm.bmHeight);
    if width <= 0 || height <= 0 || width > 1024 || height > 1024 {
        return None;
    }

    let mut bmi: BITMAPINFO = std::mem::zeroed();
    bmi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
    bmi.bmiHeader.biWidth = width;
    bmi.bmiHeader.biHeight = -height; // negative => top-down rows
    bmi.bmiHeader.biPlanes = 1;
    bmi.bmiHeader.biBitCount = 32;
    bmi.bmiHeader.biCompression = BI_RGB;

    let mut buf = vec![0u8; (width * height) as usize * 4];
    let dc = GetDC(std::ptr::null_mut());
    if dc.is_null() {
        return None;
    }
    let lines = GetDIBits(
        dc,
        hbm_color,
        0,
        height as u32,
        buf.as_mut_ptr().cast(),
        &mut bmi,
        DIB_RGB_COLORS,
    );
    ReleaseDC(std::ptr::null_mut(), dc);
    if lines == 0 {
        return None;
    }

    // GetDIBits hands back BGRA. Icons without a real alpha channel come back
    // fully transparent; treat those as opaque so they don't vanish.
    let has_alpha = buf.as_chunks::<4>().0.iter().any(|px| px[3] != 0);
    for px in buf.as_chunks_mut::<4>().0 {
        px.swap(0, 2); // BGRA -> RGBA
        if !has_alpha {
            px[3] = 255;
        }
    }

    encode_rgba_png(width as u32, height as u32, &buf)
}

pub fn encode_rgba_png(width: u32, height: u32, rgba: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().ok()?;
        writer.write_image_data(rgba).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn executable_icon_paths_are_existing_local_exe_files() {
        let current_exe = std::env::current_exe().unwrap();
        assert_eq!(validated_local_exe_path(&current_exe), Some(current_exe));

        assert!(validated_local_exe_path(Path::new("relative.exe")).is_none());
        assert!(validated_local_exe_path(Path::new(r"\\server\share\game.exe")).is_none());
        assert!(validated_local_exe_path(Path::new(r"\\?\C:\Games\game.exe")).is_none());
        assert!(validated_local_exe_path(Path::new(r"\\.\C:\Games\game.exe")).is_none());
        assert!(validated_local_exe_path(Path::new(r"C:\Games\notes.txt")).is_none());
        assert!(validated_local_exe_path(Path::new(r"C:\missing\game.exe")).is_none());
        assert!(validated_local_exe_path(&std::env::temp_dir()).is_none());
    }
}
