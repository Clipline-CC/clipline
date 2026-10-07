//! Visible top-level window enumeration and lookup. `FindWindowW` is exact
//! match only, so Clipline enumerates windows and matches the metadata it
//! needs for capture and custom-game detection.

use std::collections::HashMap;
use std::mem::size_of;
use std::path::Path;

use windows::core::{BOOL, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, POINT, RECT};
use windows::Win32::Graphics::Dwm::{
    DwmGetWindowAttribute, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS,
};
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClientRect, GetWindow, GetWindowLongPtrW, GetWindowRect, GetWindowTextW,
    GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible, GWL_EXSTYLE, GW_OWNER,
    WS_EX_APPWINDOW, WS_EX_TOOLWINDOW,
};

use crate::window_picker::{has_pickable_size, is_pickable_window, WindowTraits};
use crate::windows::nv12::CropRect;

struct Search {
    needle_lower: String,
    found: Option<HWND>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturableWindow {
    pub handle: isize,
    pub title: String,
    pub process_id: u32,
    pub exe_name: String,
    pub exe_path: Option<String>,
}

#[derive(Default)]
struct WindowEnumeration {
    windows: Vec<CapturableWindow>,
    process_paths: HashMap<u32, Option<String>>,
    path_buffer: Vec<u16>,
}

impl WindowEnumeration {
    fn process_path(&mut self, process_id: u32) -> Option<String> {
        // ponytail: scan-local discovery metadata; cross-scan caching needs
        // live process-instance validation to handle exits and PID reuse.
        self.process_paths
            .entry(process_id)
            .or_insert_with(|| {
                // SAFETY: process_path owns/closes the query handle and bounds
                // Win32's output to this enumeration's reusable path buffer.
                unsafe { process_path(process_id, &mut self.path_buffer) }
            })
            .clone()
    }
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

/// A window the custom-game picker offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickableWindow {
    pub window: CapturableWindow,
    pub minimized: bool,
}

/// The capturable windows Alt-Tab would offer. Game detection keeps using the
/// unfiltered [`enumerate_capturable_windows`].
pub fn enumerate_pickable_windows() -> Vec<PickableWindow> {
    enumerate_capturable_windows()
        .into_iter()
        .filter_map(|window| {
            let hwnd = HWND(window.handle as *mut core::ffi::c_void);
            // SAFETY: `hwnd` came from this enumeration; window_traits only
            // runs read-only window-manager queries, which fail safely if the
            // window has since closed.
            let (traits, minimized) = unsafe { window_traits(hwnd) };
            is_pickable_window(&traits).then_some(PickableWindow { window, minimized })
        })
        .collect()
}

/// The process that owns `raw` right now, or `None` once it is gone. Window
/// handles are recycled, so callers holding an old handle confirm the owner.
pub fn window_process_id(raw: isize) -> Option<u32> {
    let hwnd = window_from_raw_handle(raw)?;
    let mut process_id = 0u32;
    // SAFETY: read-only query on a validated window handle.
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut process_id)) };
    (process_id != 0).then_some(process_id)
}

pub fn enumerate_capturable_windows() -> Vec<CapturableWindow> {
    let mut enumeration = WindowEnumeration::default();
    // SAFETY: the callback only runs during this call; lparam points at
    // `enumeration`, which outlives it.
    unsafe {
        let _ = EnumWindows(
            Some(enum_capturable_proc),
            LPARAM(&mut enumeration as *mut WindowEnumeration as isize),
        );
    }
    enumeration.windows
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
    // SAFETY: lparam is the enumeration passed by enumerate_capturable_windows
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
        let mut process_id = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut process_id));
        let exe_path = enumeration.process_path(process_id);
        let exe_name = exe_path
            .as_deref()
            .and_then(exe_name_from_path)
            .unwrap_or_default();
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

/// Picker traits plus whether the window is minimized.
unsafe fn window_traits(hwnd: HWND) -> (WindowTraits, bool) {
    let ex_style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) } as u32;
    let owner = unsafe { GetWindow(hwnd, GW_OWNER) }.ok().filter(|owner| !owner.is_invalid());
    let mut cloaked = 0u32;
    let cloaked = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            &mut cloaked as *mut u32 as *mut core::ffi::c_void,
            size_of::<u32>() as u32,
        )
    }
    .is_ok()
        && cloaked != 0;
    let minimized = unsafe { IsIconic(hwnd) }.as_bool();
    let mut rect = RECT::default();
    let has_area = unsafe { GetWindowRect(hwnd, &mut rect) }.is_ok()
        && has_pickable_size(rect.right - rect.left, rect.bottom - rect.top, minimized);
    let traits = WindowTraits {
        visible: unsafe { IsWindowVisible(hwnd) }.as_bool(),
        cloaked,
        has_title: true, // enumerate_capturable_windows only keeps titled windows
        has_area,
        tool_window: ex_style & WS_EX_TOOLWINDOW.0 != 0,
        app_window: ex_style & WS_EX_APPWINDOW.0 != 0,
        owned: owner.is_some(),
        owner_visible: owner.is_some_and(|owner| unsafe { IsWindowVisible(owner) }.as_bool()),
    };
    (traits, minimized)
}

unsafe fn window_title(hwnd: HWND) -> Option<String> {
    let mut buf = [0u16; 1024];
    let len = unsafe { GetWindowTextW(hwnd, &mut buf) };
    if len == 0 {
        return None;
    }
    Some(String::from_utf16_lossy(&buf[..len as usize]))
}

unsafe fn process_path(process_id: u32, buf: &mut Vec<u16>) -> Option<String> {
    if process_id == 0 {
        return None;
    }
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id).ok()? };
    buf.resize(32_768, 0);
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

pub(super) unsafe fn window_frame_rect(hwnd: HWND) -> Option<RECT> {
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
    fn process_queries_reuse_metadata_and_scratch_only_within_one_scan() {
        let pid = std::process::id();
        let mut scan = WindowEnumeration::default();
        let path = scan.process_path(pid).expect("query this live process");
        let scratch = scan.path_buffer.as_ptr();
        assert_eq!(scan.path_buffer.len(), 32_768);

        // A repeated PID must not query Win32 again: that would overwrite this
        // scratch buffer. The cache must also retain failed queries.
        scan.path_buffer.fill(0x2603);
        assert_eq!(scan.process_path(pid), Some(path.clone()));
        assert!(scan.path_buffer.iter().all(|value| *value == 0x2603));
        assert_eq!(scan.path_buffer.as_ptr(), scratch);
        scan.process_paths.insert(pid, None);
        assert_eq!(scan.process_path(pid), None);
        assert!(scan.path_buffer.iter().all(|value| *value == 0x2603));

        assert_eq!(scan.process_path(0), None);
        assert!(scan.process_paths.contains_key(&0));
        assert_eq!(scan.path_buffer.as_ptr(), scratch);

        // No stale success or failure survives a new enumeration.
        let mut next_scan = WindowEnumeration::default();
        assert_eq!(next_scan.process_path(pid), Some(path));
    }

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
