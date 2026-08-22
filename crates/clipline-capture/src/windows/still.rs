//! One-shot screenshot grabs: build a short-lived WGC session, pull exactly
//! one frame with a timeout, read it back to CPU, and tear everything down.
//! The Drop impl on [WgcCapture] closes the session on every path, including
//! timeouts, so a still never leaks a capture session.

use std::sync::OnceLock;
use std::time::Duration;

use windows::Win32::Foundation::POINT;
use windows::Win32::UI::HiDpi::GetDpiForMonitor;
use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

use crate::still::{BgraImage, PlacedRect, StillError};
use crate::traits::CaptureError;
use crate::windows::display::{display_handle_by_id, enumerate_displays};
use crate::windows::nv12::read_bgra;
use crate::windows::window::{window_frame_bounds, window_from_raw_handle};
use crate::windows::wgc::WgcCapture;

/// How long a still waits for WGC's first frame. WGC only delivers on screen
/// updates; a static desktop still gets one initial frame from StartCapture,
/// so anything past this means the target went away.
const STILL_GRAB_TIMEOUT: Duration = Duration::from_secs(2);

/// One shared D3D11 device for every still grab. Device creation costs
/// tens of milliseconds per screenshot; textures stay on one device within
/// a single grab, which is the only cross-use constraint that matters here.
fn shared_still_device() -> Result<
    &'static windows::Win32::Graphics::Direct3D11::ID3D11Device,
    CaptureError,
> {
    static DEVICE: OnceLock<windows::Win32::Graphics::Direct3D11::ID3D11Device> = OnceLock::new();
    if let Some(device) = DEVICE.get() {
        return Ok(device);
    }
    let (device, _) = crate::windows::d3d11::create_device()
        .map_err(|e| CaptureError::Init(format!("create D3D11 device: {e}")))?;
    let _ = DEVICE.set(device);
    DEVICE
        .get()
        .ok_or_else(|| CaptureError::Init("shared still device unavailable".into()))
}

#[derive(Debug, thiserror::Error)]
pub enum StillGrabError {
    #[error(transparent)]
    Capture(#[from] CaptureError),
    #[error(transparent)]
    Still(#[from] StillError),
    #[error("WinRT call failed: {0}")]
    WinRt(#[from] windows_core::Error),
}

/// Grab one full-monitor frame, cursor excluded.
pub fn grab_monitor(monitor_id: &str) -> Result<BgraImage, StillGrabError> {
    grab_monitor_with_timeout(monitor_id, STILL_GRAB_TIMEOUT)
}

pub fn grab_monitor_with_timeout(
    monitor_id: &str,
    timeout: Duration,
) -> Result<BgraImage, StillGrabError> {
    let display = display_handle_by_id(Some(monitor_id))?;
    let device = shared_still_device()?;
    let mut capture = WgcCapture::for_monitor_on_without_cursor(
        device.clone(),
        display.handle,
        WgcCapture::new_clock()?,
    )?;
    pull_readback(&mut capture, device, timeout)
}

/// Grab one whole-window frame (DWM extended frame bounds, title bar
/// included — the Alt+PrintScreen convention), cursor excluded.
pub fn grab_window(raw_hwnd: isize) -> Result<BgraImage, StillGrabError> {
    grab_window_with_timeout(raw_hwnd, STILL_GRAB_TIMEOUT)
}

pub fn grab_window_with_timeout(raw_hwnd: isize, timeout: Duration) -> Result<BgraImage, StillGrabError> {
    let hwnd = window_from_raw_handle(raw_hwnd)
        .ok_or_else(|| CaptureError::Init("window is gone or not visible".into()))?;
    let device = shared_still_device()?;
    let mut capture = WgcCapture::for_window_on_without_cursor(
        device.clone(),
        hwnd,
        WgcCapture::new_clock()?,
    )?;
    pull_readback(&mut capture, device, timeout)
}

/// The monitor containing the cursor, as a placed rect in virtual-desktop
/// coordinates. Entire-screen mode captures this monitor, not the union.
pub fn monitor_at_cursor() -> Option<PlacedRect> {
    cursor_display().map(|display| PlacedRect {
            x: display.x,
            y: display.y,
            width: display.width,
            height: display.height,
        })
}

/// Device id of the monitor containing the cursor, ready for grab_monitor.
/// None when the cursor sits outside every display.
pub fn cursor_monitor_id() -> Option<String> {
    cursor_display().map(|display| display.id)
}

fn cursor_display() -> Option<crate::windows::display::DisplayInfo> {
    let point = cursor_point()?;
    enumerate_displays()
        .ok()?
        .into_iter()
        .find(|display| {
            let right = display.x + display.width as i32;
            let bottom = display.y + display.height as i32;
            point.x >= display.x && point.x < right && point.y >= display.y && point.y < bottom
        })
}

/// DPI scale (96 = 100%) of the monitor containing the cursor. Region-mode
/// UI needs this to map physical frozen-frame pixels to screen points.
pub fn dpi_at_cursor() -> Option<u32> {
    let monitor = monitor_from_point(cursor_point()?)?;
    // SAFETY: monitor is a live HMONITOR; info is a properly-sized struct.
    let mut dpi_x = 0u32;
    let mut dpi_y = 0u32;
    // SAFETY: monitor is a live HMONITOR from MonitorFromPoint; out-params
    // are valid u32 slots.
    unsafe {
        GetDpiForMonitor(
            monitor,
            windows::Win32::UI::HiDpi::MDT_EFFECTIVE_DPI,
            &mut dpi_x,
            &mut dpi_y,
        )
        .ok()?;
    }
    Some(dpi_x.max(dpi_y))
}

fn pull_readback(
    capture: &mut WgcCapture,
    device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
    timeout: Duration,
) -> Result<BgraImage, StillGrabError> {
    let frame = capture
        .next_frame_timeout(timeout)?
        .ok_or(CaptureError::Init("capture ended before the first frame".into()))?;
    let crate::traits::FrameData::Gpu(texture) = frame.data else {
        return Err(CaptureError::Init("unexpected CPU frame from WGC".into()).into());
    };
    let readback = read_bgra(device, &texture)?;
    Ok(BgraImage::from_readback(
        readback.width,
        readback.height,
        readback.stride,
        &readback.bytes,
    )?)
}

fn cursor_point() -> Option<POINT> {
    let mut point = POINT::default();
    // SAFETY: plain output-pointer query.
    let ok = unsafe { GetCursorPos(&mut point) };
    ok.ok().map(|_| point)
}

fn monitor_from_point(point: POINT) -> Option<windows::Win32::Graphics::Gdi::HMONITOR> {
    use windows::Win32::Graphics::Gdi::{MonitorFromPoint, MONITOR_DEFAULTTONEAREST};
    // SAFETY: plain query with a value POINT.
    let monitor = unsafe { MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST) };
    (!monitor.is_invalid()).then_some(monitor)
}

// Region mode consumes the bounds helper through the app layer; keep it
// linked from here so the re-export stays honest until then.
#[allow(unused_imports)]
use window_frame_bounds as _window_frame_bounds_reexport;

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Foundation::HWND;
    use std::time::Instant;

    fn skip_on_ci(reason: &str) -> bool {
        if std::env::var_os("CI").is_some() {
            eprintln!("SKIP: {reason}");
            return true;
        }
        false
    }

    #[test]
    fn monitor_grab_matches_the_monitors_pixel_size() {
        if skip_on_ci("WGC device test needs a real interactive desktop") {
            return;
        }
        let displays = enumerate_displays().expect("enumerate displays");
        let primary = displays.iter().find(|d| d.is_primary).expect("primary");
        let image = grab_monitor(&primary.id).expect("grab");
        assert_eq!(image.width(), primary.width);
        assert_eq!(image.height(), primary.height);
        assert_eq!(image.bytes().len(), (primary.width as usize * primary.height as usize * 4));
    }

    #[test]
    fn window_grab_matches_dwm_extended_frame_bounds() {
        if skip_on_ci("WGC device test needs a real interactive desktop") {
            return;
        }
        let hwnd = (unsafe { visible_window_for_test() }).expect("create test window");
        if hwnd.is_invalid() {
            eprintln!("SKIP: could not create a visible test window");
            return;
        };
        let bounds = window_frame_bounds(hwnd).expect("frame bounds");
        let result = grab_window(hwnd.0 as isize);
        // SAFETY: window created by this test on this thread.
        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::DestroyWindow(hwnd);
        }
        let image = result.expect("grab");
        assert_eq!(image.width(), bounds.width);
        assert_eq!(image.height(), bounds.height);
    }

    #[test]
    fn invalid_hwnd_is_a_clean_error_not_a_hang() {
        let error = grab_window(0xdead_beef).unwrap_err();
        assert!(!error.to_string().is_empty());
        assert!(!matches!(error, StillGrabError::Capture(CaptureError::Timeout(_))));
    }

    #[test]
    fn grab_bounded_wait_never_blocks_forever() {
        if skip_on_ci("WGC device test needs a real interactive desktop") {
            return;
        }
        let displays = enumerate_displays().expect("enumerate displays");
        let primary = displays.first().expect("a display");
        let started = Instant::now();
        let result = grab_monitor_with_timeout(&primary.id, Duration::from_millis(50));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "a timed-out grab must return promptly"
        );
        // Either outcome is fine: a live desktop delivers a frame inside
        // 50 ms; a quiet one hits the timeout. Both prove the wait is bound.
        match result {
            Ok(_) | Err(StillGrabError::Capture(CaptureError::Timeout(_))) => {}
            Err(other) => panic!("unexpected grab error: {other}"),
        }
    }

    /// A small visible top-level window using the preexisting STATIC class.
    unsafe fn visible_window_for_test() -> Option<HWND> {
        use windows::core::PCWSTR;
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, CW_USEDEFAULT, WINDOW_EX_STYLE, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
        };
        let class: Vec<u16> = "STATIC\0".encode_utf16().collect();
        let title: Vec<u16> = "clipline-still-test\0".encode_utf16().collect();
        // SAFETY: class/title are null-terminated; the window is destroyed
        // by the caller on the same thread.
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                PCWSTR(class.as_ptr()),
                PCWSTR(title.as_ptr()),
                WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                320,
                240,
                None,
                None,
                None,
                None,
            )
        }
        .ok()?;
        Some(HWND(hwnd.0))
    }
}
