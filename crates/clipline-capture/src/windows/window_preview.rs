//! One-frame window previews for the custom-game picker. A preview session
//! starts only when the yellow capture border can be hidden; otherwise the
//! caller shows the app icon instead. Previews never restore or activate the
//! window.

use std::time::{Duration, Instant};

use windows::core::Interface;
use windows::Graphics::Capture::{
    Direct3D11CaptureFramePool, GraphicsCaptureAccess, GraphicsCaptureAccessKind,
    GraphicsCaptureSession,
};
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Security::Authorization::AppCapabilityAccess::AppCapabilityAccessStatus;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::Win32::Graphics::Dxgi::Common::DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020;
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1, IDXGIOutput6};
use windows::Win32::Graphics::Gdi::{MonitorFromWindow, MONITOR_DEFAULTTONEAREST};
use windows::Win32::System::WinRT::Direct3D11::IDirect3DDxgiInterfaceAccess;
use windows::Win32::UI::WindowsAndMessaging::IsIconic;

use super::nv12::read_bgra;
use super::wgc::{create_item, winrt_device};
use super::{d3d11, is_windows_11_or_later, window_process_id};
use crate::traits::CaptureError;
use crate::window_picker::{
    downscale_bgra_to_rgba, preview_region, start_without_border, BorderlessCapture, Thumbnail,
};

/// Grab one frame of the window `raw_hwnd`, scaled to fit the box, while it
/// still belongs to `process_id`. `Ok(None)` means "show the icon": the
/// window is minimized, on an HDR display, changed owner, produced no frame
/// within `timeout`, or the border could not be hidden. Call with one
/// preview at a time per device.
pub fn capture_window_preview(
    device: &ID3D11Device,
    raw_hwnd: isize,
    process_id: u32,
    max_width: u32,
    max_height: u32,
    timeout: Duration,
) -> Result<Option<Thumbnail>, CaptureError> {
    // Handles are recycled: confirm the owner around every step that could
    // otherwise show another process's window under this card.
    let still_listed = || window_process_id(raw_hwnd) == Some(process_id);
    if !still_listed() {
        return Ok(None);
    }
    let hwnd = HWND(raw_hwnd as *mut core::ffi::c_void);
    // SAFETY: read-only query on a handle validated just above.
    if unsafe { IsIconic(hwnd) }.as_bool() || window_on_hdr_display(hwnd) {
        return Ok(None);
    }

    let init = |e: windows::core::Error| CaptureError::Init(e.to_string());
    // SAFETY: CreateForWindow only reads the handle validated above.
    let item = create_item(|interop| unsafe { interop.CreateForWindow(hwnd) })?;
    if !still_listed() {
        return Ok(None);
    }
    d3d11::ensure_multithread_protected(device).map_err(init)?;
    let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
        &winrt_device(device).map_err(init)?,
        DirectXPixelFormat::B8G8R8A8UIntNormalized,
        1,
        item.Size().map_err(init)?,
    )
    .map_err(init)?;
    let session = pool.CreateCaptureSession(&item).map_err(init)?;

    let preview = (|| {
        let _ = session.SetIsCursorCaptureEnabled(false);
        if !start_without_border(&mut PreviewSession(&session)).map_err(CaptureError::Init)? {
            return Ok(None);
        }
        let deadline = Instant::now() + timeout;
        let frame = loop {
            if let Ok(frame) = pool.TryGetNextFrame() {
                break frame;
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        let content = frame.ContentSize().map_err(init)?;
        let access: IDirect3DDxgiInterfaceAccess =
            frame.Surface().map_err(init)?.cast().map_err(init)?;
        // SAFETY: the surface is a live IDirect3D surface backed by an
        // ID3D11Texture2D; GetInterface AddRefs it.
        let texture: ID3D11Texture2D = unsafe { access.GetInterface() }.map_err(init)?;
        let (surface_width, surface_height) = d3d11::texture_size(&texture);
        let Some((width, height)) =
            preview_region(content.Width, content.Height, surface_width, surface_height)
        else {
            return Ok(None);
        };
        let readback = read_bgra(device, &texture).map_err(init)?;
        let _ = frame.Close();
        Ok(downscale_bgra_to_rgba(
            &readback.bytes,
            width,
            height,
            readback.stride,
            max_width,
            max_height,
        ))
    })();
    let _ = session.Close();
    let _ = pool.Close();
    if !still_listed() {
        return Ok(None);
    }
    preview
}

struct PreviewSession<'a>(&'a GraphicsCaptureSession);

impl BorderlessCapture for PreviewSession<'_> {
    fn borderless_supported(&mut self) -> bool {
        is_windows_11_or_later()
    }

    /// `IsBorderRequired = false` is silently ignored until borderless access
    /// is granted, so it must be requested and come back `Allowed`.
    fn request_borderless_access(&mut self) -> bool {
        GraphicsCaptureAccess::RequestAccessAsync(GraphicsCaptureAccessKind::Borderless)
            .and_then(|request| request.join())
            .is_ok_and(|status| status == AppCapabilityAccessStatus::Allowed)
    }

    fn hide_border(&mut self) -> bool {
        self.0.SetIsBorderRequired(false).is_ok()
    }

    fn start(&mut self) -> Result<(), String> {
        self.0.StartCapture().map_err(|error| error.to_string())
    }
}

/// BGRA8 captures of HDR content look washed out, so those windows get an
/// icon. Unknown displays count as SDR.
fn window_on_hdr_display(hwnd: HWND) -> bool {
    // SAFETY: MonitorFromWindow only reads the handle; the DXGI calls below
    // are COM queries on objects owned by this function.
    unsafe {
        let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let Ok(factory) = CreateDXGIFactory1::<IDXGIFactory1>() else {
            return false;
        };
        for adapter_index in 0.. {
            let Ok(adapter) = factory.EnumAdapters1(adapter_index) else {
                return false;
            };
            for output_index in 0.. {
                let Ok(output) = adapter.EnumOutputs(output_index) else {
                    break;
                };
                if output.GetDesc().is_ok_and(|desc| desc.Monitor == monitor) {
                    return output.cast::<IDXGIOutput6>().is_ok_and(|output| {
                        output.GetDesc1().is_ok_and(|desc| {
                            desc.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020
                        })
                    });
                }
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::core::w;
    use windows::Win32::UI::WindowsAndMessaging::FindWindowW;

    #[test]
    fn previews_the_taskbar_and_refuses_a_stale_owner() {
        if std::env::var_os("CI").is_some() {
            eprintln!("skipping device test on CI");
            return;
        }
        // SAFETY: FindWindowW only reads the class name.
        let Ok(taskbar) = (unsafe { FindWindowW(w!("Shell_TrayWnd"), None) }) else {
            eprintln!("skipping: no taskbar window");
            return;
        };
        let raw = taskbar.0 as isize;
        let owner = window_process_id(raw).expect("taskbar has an owner");
        let (device, _) = d3d11::create_device().expect("D3D11 device");
        let timeout = Duration::from_millis(750);

        let stale = capture_window_preview(&device, raw, owner.wrapping_add(1), 320, 180, timeout);
        assert_eq!(stale.unwrap(), None, "a different owner must never be captured");

        let preview = capture_window_preview(&device, raw, owner, 320, 180, timeout).unwrap();
        if !is_windows_11_or_later() {
            assert_eq!(preview, None, "no border-free preview before Windows 11");
            return;
        }
        let Some(thumbnail) = preview else {
            eprintln!("no taskbar preview (HDR display or borderless access denied)");
            return;
        };
        assert!(thumbnail.width <= 320 && thumbnail.height <= 180);
        assert_eq!(
            thumbnail.rgba.len(),
            thumbnail.width as usize * thumbnail.height as usize * 4
        );
        assert!(thumbnail.rgba.chunks(4).all(|pixel| pixel[3] == u8::MAX));
    }
}
