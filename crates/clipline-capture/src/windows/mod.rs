//! Windows platform layer. All COM/WinRT `unsafe` lives in this module
//! tree; everything exported is a safe wrapper honoring the platform
//! traits' contracts (see `crate::mock` for the reference behavior).

pub mod d3d11;
pub mod display;
pub mod dxgi_dup;
pub mod mft;
pub mod mft_probe;
pub mod nv12;
pub mod wasapi;
pub mod wgc;
pub mod window;

pub use dxgi_dup::DxgiDuplicationCapture;
pub use mft::{MftConfig, MftH264Encoder, SoftwareMftH264Encoder};
pub use wasapi::{WasapiLoopback, WasapiMicrophoneMonitor, WasapiMicrophoneMonitorInfo};
pub use wgc::WgcCapture;
pub use window::{
    enumerate_capturable_windows, enumerate_capturable_windows_with_checkpoint,
    find_window_by_title, validate_capturable_window_catalog, window_from_raw_handle,
    CapturableWindow,
};

/// Re-export so downstream crates (the app) can name the shared D3D11 device
/// type without taking their own pinned `windows` dependency.
pub use windows::Win32::Graphics::Direct3D11::ID3D11Device;

/// The capture-clock origin: QPC now, in the 100 ns units shared by WGC
/// `SystemRelativeTime` and WASAPI QPC positions (ddoc §6).
pub fn qpc_now_ticks_100ns() -> windows::core::Result<i64> {
    use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
    let (mut counter, mut freq) = (0i64, 0i64);
    // SAFETY: out-pointers are valid; these calls cannot fail on XP+.
    unsafe {
        QueryPerformanceCounter(&mut counter)?;
        QueryPerformanceFrequency(&mut freq)?;
    }
    Ok(crate::clock::qpc_to_ticks_100ns(counter, freq))
}

#[cfg(test)]
mod tests {
    use crate::traits::{Frame, FrameData};

    /// A GPU frame must round-trip through the platform-neutral `Frame`
    /// struct (Debug + Clone are derived; windows-rs COM wrappers provide
    /// both). WARP renders headless, so this runs on the CI runner too.
    #[test]
    fn gpu_frame_data_wraps_a_d3d11_texture() {
        let (device, _context) =
            super::d3d11::create_device_for_tests().expect("WARP D3D11 device");
        let texture = super::d3d11::create_bgra_texture(&device, 16, 16).expect("16x16 texture");
        let frame = Frame {
            pts_s: 0.25,
            data: FrameData::Gpu(texture),
        };
        let cloned = frame.clone();
        let FrameData::Gpu(tex) = cloned.data else {
            panic!("expected Gpu variant");
        };
        let (w, h) = super::d3d11::texture_size(&tex);
        assert_eq!((w, h), (16, 16));
        assert!(!format!("{frame:?}").is_empty());
    }
}
