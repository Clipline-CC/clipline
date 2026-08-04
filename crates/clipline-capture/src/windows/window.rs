//! Visible top-level window enumeration and lookup. `FindWindowW` is exact
//! match only, so Clipline enumerates windows and matches the metadata it
//! needs for capture and custom-game detection.

use std::mem::size_of;
use std::path::Path;

use windows::core::{BOOL, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, POINT, RECT};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClientRect, GetWindowRect, GetWindowTextW, GetWindowThreadProcessId, IsWindow,
    IsWindowVisible,
};

use crate::windows::nv12::CropRect;

pub const MAX_PROBE_WINDOWS: usize = 256;
pub const MAX_WINDOW_TEXT_BYTES: usize = 64 * 1024;
pub const MAX_WINDOW_CATALOG_BYTES: usize = 4 * 1024 * 1024;

struct Search {
    needle_lower: String,
    found: Option<HWND>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CapturableWindow {
    pub handle: isize,
    pub title: String,
    pub process_id: u32,
    pub exe_name: String,
    pub exe_path: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WindowClientCrop {
    Client(CropRect),
    FullFrame,
}

pub fn find_window_by_title(needle: &str) -> Option<HWND> {
    let mut search = Search {
        needle_lower: needle.to_lowercase(),
        found: None,
    };
    // SAFETY: the callback only runs during this call; lparam points at
    // `search`, which outlives it. EnumWindows returns Err when the
    // callback stops enumeration early — our found case, not an error.
    unsafe {
        let _ = EnumWindows(Some(enum_proc), LPARAM(&mut search as *mut Search as isize));
    }
    search.found
}

pub fn window_from_raw_handle(raw: isize) -> Option<HWND> {
    if raw == 0 {
        return None;
    }
    let hwnd = HWND(raw as *mut core::ffi::c_void);
    // SAFETY: `hwnd` is a borrowed OS handle. We only validate it with
    // read-only window-manager queries before passing it to WGC.
    unsafe {
        if IsWindow(Some(hwnd)).as_bool() && IsWindowVisible(hwnd).as_bool() {
            Some(hwnd)
        } else {
            None
        }
    }
}

pub fn enumerate_capturable_windows() -> Vec<CapturableWindow> {
    enumerate_capturable_windows_with_checkpoint(|| Ok(())).unwrap_or_default()
}

pub fn enumerate_capturable_windows_with_checkpoint(
    checkpoint: impl FnOnce() -> Result<(), String>,
) -> Result<Vec<CapturableWindow>, String> {
    checkpoint()?;
    let mut windows = Vec::new();
    windows
        .try_reserve_exact(MAX_PROBE_WINDOWS)
        .map_err(|_| "reserve bounded capturable-window catalog".to_string())?;
    let mut enumeration = WindowEnumeration {
        windows,
        utf8_bytes: 0,
        error: None,
    };
    // SAFETY: the callback only runs during this call; lparam points at
    // `enumeration`, which outlives it.
    let result = unsafe {
        EnumWindows(
            Some(enum_capturable_proc),
            LPARAM(&mut enumeration as *mut WindowEnumeration as isize),
        )
    };
    if let Some(error) = enumeration.error {
        return Err(error);
    }
    result.map_err(|error| format!("EnumWindows failed: {error}"))?;
    validate_capturable_window_catalog(&enumeration.windows)?;
    Ok(enumeration.windows)
}

pub fn validate_capturable_window_catalog(windows: &[CapturableWindow]) -> Result<(), String> {
    if windows.len() > MAX_PROBE_WINDOWS {
        return Err(format!(
            "capturable window count {} exceeds {MAX_PROBE_WINDOWS}",
            windows.len()
        ));
    }
    let mut aggregate = 0usize;
    for window in windows {
        for value in [
            window.title.as_str(),
            window.exe_name.as_str(),
            window.exe_path.as_deref().unwrap_or_default(),
        ] {
            if value.len() > MAX_WINDOW_TEXT_BYTES {
                return Err(format!(
                    "capturable window text exceeds {MAX_WINDOW_TEXT_BYTES} UTF-8 bytes"
                ));
            }
            aggregate = aggregate
                .checked_add(value.len())
                .ok_or_else(|| "capturable window byte count overflowed".to_string())?;
        }
    }
    if aggregate > MAX_WINDOW_CATALOG_BYTES {
        return Err(format!(
            "capturable window catalog exceeds {MAX_WINDOW_CATALOG_BYTES} UTF-8 bytes"
        ));
    }
    Ok(())
}

struct WindowEnumeration {
    windows: Vec<CapturableWindow>,
    utf8_bytes: usize,
    error: Option<String>,
}

pub(super) fn window_client_crop_state(hwnd: HWND) -> Option<WindowClientCrop> {
    // SAFETY: `hwnd` is a borrowed OS handle. The calls below are read-only
    // window-manager queries used to describe the visible client area.
    unsafe {
        if !IsWindow(Some(hwnd)).as_bool() || !IsWindowVisible(hwnd).as_bool() {
            return None;
        }
        let frame = window_frame_rect(hwnd)?;
        let mut client = RECT::default();
        GetClientRect(hwnd, &mut client).ok()?;
        let client_width = client.right.checked_sub(client.left)?;
        let client_height = client.bottom.checked_sub(client.top)?;
        let mut client_origin = POINT {
            x: client.left,
            y: client.top,
        };
        if !ClientToScreen(hwnd, &mut client_origin).as_bool() {
            return None;
        }
        client_crop_from_rects(frame, client_origin, client_width, client_height)
    }
}

unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    // SAFETY: lparam is the `Search` pointer passed by find_window_by_title
    // on this same thread, alive for the whole enumeration.
    let search = unsafe { &mut *(lparam.0 as *mut Search) };
    // SAFETY: hwnd comes from the enumeration; these are read-only queries.
    unsafe {
        if !IsWindowVisible(hwnd).as_bool() {
            return BOOL(1);
        }
        let mut buf = [0u16; 512];
        let len = GetWindowTextW(hwnd, &mut buf);
        if len == 0 {
            return BOOL(1);
        }
        let title = String::from_utf16_lossy(&buf[..len as usize]).to_lowercase();
        if title.contains(&search.needle_lower) {
            search.found = Some(hwnd);
            return BOOL(0); // stop enumeration
        }
    }
    BOOL(1)
}

unsafe extern "system" fn enum_capturable_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    // SAFETY: lparam is the accumulator passed by enumerate_capturable_windows
    // on this same thread, alive for the whole enumeration.
    let enumeration = unsafe { &mut *(lparam.0 as *mut WindowEnumeration) };
    // SAFETY: hwnd comes from the enumeration; these are read-only queries.
    unsafe {
        if !IsWindowVisible(hwnd).as_bool() {
            return BOOL(1);
        }
        let Some(title) = window_title(hwnd) else {
            return BOOL(1);
        };
        if title.trim().is_empty() {
            return BOOL(1);
        }
        if enumeration.windows.len() == MAX_PROBE_WINDOWS {
            enumeration.error = Some(format!(
                "capturable window count exceeds {MAX_PROBE_WINDOWS}"
            ));
            return BOOL(0);
        }
        let mut process_id = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut process_id));
        let exe_path = process_path(process_id);
        let exe_name = exe_path
            .as_deref()
            .and_then(exe_name_from_path)
            .unwrap_or_default();
        if [
            title.as_str(),
            exe_name.as_str(),
            exe_path.as_deref().unwrap_or_default(),
        ]
        .into_iter()
        .any(|value| value.len() > MAX_WINDOW_TEXT_BYTES)
        {
            enumeration.error = Some(format!(
                "capturable window text exceeds {MAX_WINDOW_TEXT_BYTES} UTF-8 bytes"
            ));
            return BOOL(0);
        }
        let Some(utf8_bytes) = enumeration
            .utf8_bytes
            .checked_add(title.len())
            .and_then(|bytes| bytes.checked_add(exe_name.len()))
            .and_then(|bytes| bytes.checked_add(exe_path.as_deref().map_or(0, str::len)))
        else {
            enumeration.error = Some("capturable window byte count overflowed".into());
            return BOOL(0);
        };
        if utf8_bytes > MAX_WINDOW_CATALOG_BYTES {
            enumeration.error = Some(format!(
                "capturable window catalog exceeds {MAX_WINDOW_CATALOG_BYTES} UTF-8 bytes"
            ));
            return BOOL(0);
        }
        enumeration.utf8_bytes = utf8_bytes;
        enumeration.windows.push(CapturableWindow {
            handle: hwnd.0 as isize,
            title,
            process_id,
            exe_name,
            exe_path,
        });
    }
    BOOL(1)
}

unsafe fn window_title(hwnd: HWND) -> Option<String> {
    let mut buf = [0u16; 1024];
    let len = unsafe { GetWindowTextW(hwnd, &mut buf) };
    if len == 0 {
        return None;
    }
    Some(String::from_utf16_lossy(&buf[..len as usize]))
}

unsafe fn process_path(process_id: u32) -> Option<String> {
    if process_id == 0 {
        return None;
    }
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id).ok()? };
    let mut buf = vec![0u16; 32_768];
    let mut len = buf.len() as u32;
    let result = unsafe {
        QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
    };
    // SAFETY: handle came from OpenProcess and is no longer used afterwards.
    let _ = unsafe { CloseHandle(handle) };
    result.ok()?;
    Some(String::from_utf16_lossy(&buf[..len as usize]))
}

fn exe_name_from_path(path: &str) -> Option<String> {
    Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.to_string())
}

unsafe fn window_frame_rect(hwnd: HWND) -> Option<RECT> {
    let mut frame = RECT::default();
    let dwm_frame = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut frame as *mut _ as *mut _,
            size_of::<RECT>() as u32,
        )
    };
    if dwm_frame.is_ok() && rect_has_area(frame) {
        return Some(frame);
    }

    let mut frame = RECT::default();
    unsafe { GetWindowRect(hwnd, &mut frame).ok()? };
    rect_has_area(frame).then_some(frame)
}

fn rect_has_area(rect: RECT) -> bool {
    rect.right > rect.left && rect.bottom > rect.top
}

fn client_crop_from_rects(
    frame: RECT,
    client_origin: POINT,
    client_width: i32,
    client_height: i32,
) -> Option<WindowClientCrop> {
    let frame_width = frame.right.checked_sub(frame.left)?;
    let frame_height = frame.bottom.checked_sub(frame.top)?;
    let x = client_origin.x.checked_sub(frame.left)?;
    let y = client_origin.y.checked_sub(frame.top)?;
    let crop =
        CropRect::from_i32_in_frame(x, y, client_width, client_height, frame_width, frame_height)?;
    if crop.is_full_frame(
        u32::try_from(frame_width).ok()?,
        u32::try_from(frame_height).ok()?,
    ) {
        return Some(WindowClientCrop::FullFrame);
    }
    Some(WindowClientCrop::Client(crop))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_match_returns_none() {
        assert!(find_window_by_title("no window is named this 5f2c9a").is_none());
    }

    #[test]
    fn invalid_raw_window_handle_returns_none() {
        assert!(window_from_raw_handle(0).is_none());
    }

    #[test]
    fn enumeration_path_does_not_crash() {
        // Result depends on the session (CI may have no titled windows);
        // this just exercises EnumWindows + the callback end to end.
        let _ = find_window_by_title("");
        let _ = enumerate_capturable_windows();
    }

    #[test]
    fn probe_catalog_rejects_count_and_utf8_overflow() {
        let window = CapturableWindow {
            handle: 1,
            title: "game".into(),
            process_id: 2,
            exe_name: "game.exe".into(),
            exe_path: Some(r"C:\Games\game.exe".into()),
        };
        assert!(
            validate_capturable_window_catalog(&vec![window.clone(); MAX_PROBE_WINDOWS]).is_ok()
        );
        assert!(
            validate_capturable_window_catalog(&vec![window.clone(); MAX_PROBE_WINDOWS + 1])
                .is_err()
        );
        let mut huge = window;
        huge.title = "é".repeat(MAX_WINDOW_TEXT_BYTES);
        assert!(validate_capturable_window_catalog(&[huge]).is_err());
    }

    #[test]
    fn client_crop_skips_undecorated_full_frame() {
        let frame = RECT {
            left: 100,
            top: 200,
            right: 900,
            bottom: 700,
        };
        let origin = POINT { x: 100, y: 200 };

        assert_eq!(
            client_crop_from_rects(frame, origin, 800, 500),
            Some(WindowClientCrop::FullFrame)
        );
    }

    #[test]
    fn client_crop_removes_window_chrome() {
        let frame = RECT {
            left: 100,
            top: 200,
            right: 900,
            bottom: 700,
        };
        let origin = POINT { x: 108, y: 231 };

        assert_eq!(
            client_crop_from_rects(frame, origin, 784, 461),
            Some(WindowClientCrop::Client(CropRect {
                x: 8,
                y: 31,
                width: 784,
                height: 461,
            }))
        );
    }

    #[test]
    fn client_crop_rejects_rects_outside_frame() {
        let frame = RECT {
            left: 100,
            top: 200,
            right: 900,
            bottom: 700,
        };
        let origin = POINT { x: 108, y: 231 };

        assert_eq!(client_crop_from_rects(frame, origin, 900, 461), None);
    }
}
