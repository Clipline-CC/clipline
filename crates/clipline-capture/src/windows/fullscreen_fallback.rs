//! WGC for a selected game window, Desktop Duplication while that exact window
//! fills its monitor and is foreground. The desktop source is dropped as soon
//! as either guard changes. Its frames are checked again after acquisition.

use std::sync::{
    atomic::{AtomicU8, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11Texture2D, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE,
    D3D11_SUBRESOURCE_DATA, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Gdi::{
    ClientToScreen, GetMonitorInfoW, MonitorFromWindow, HMONITOR, MONITORINFO,
    MONITOR_DEFAULTTONULL,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetClientRect, GetForegroundWindow, GetWindowThreadProcessId, IsIconic, IsWindow,
    IsWindowVisible,
};

use crate::fallback_policy::{accept_frame, choose, Observation, Source};
use crate::{CaptureError, Frame, FrameData, RelativeClock};

use super::{d3d11, qpc_now_ticks_100ns, DxgiDuplicationCapture, WgcCapture};

const POLL_INTERVAL: Duration = Duration::from_millis(16);
const RETRY_INTERVAL: Duration = Duration::from_millis(200);
const RETRY_BUDGET: Duration = Duration::from_secs(5);

/// Cloneable live status for the recorder's support diagnostics.
#[derive(Clone, Default)]
pub struct FullscreenFallbackStatus(Arc<AtomicU8>);

impl FullscreenFallbackStatus {
    pub fn label(&self) -> &'static str {
        match self.0.load(Ordering::Relaxed) {
            1 => "windows_graphics_capture",
            2 => "desktop_duplication",
            _ => "fullscreen_transition_black",
        }
    }

    fn set(&self, source: Source) {
        let value = match source {
            Source::Waiting => 0,
            Source::Window => 1,
            Source::Display => 2,
        };
        if self.0.swap(value, Ordering::Relaxed) != value {
            tracing::info!(event = "capture_source_changed", source = self.label());
        }
    }
}

#[derive(Clone, Copy)]
struct Snapshot {
    observation: Observation,
    monitor: Option<HMONITOR>,
}

impl Snapshot {
    fn unavailable() -> Self {
        Self {
            observation: Observation {
                available: false,
                foreground: false,
                covers_monitor: false,
            },
            monitor: None,
        }
    }
}

enum ActiveSource {
    Window(WgcCapture),
    Display(HMONITOR, DxgiDuplicationCapture),
}

impl ActiveSource {
    fn matches(&self, source: Source, monitor: Option<HMONITOR>) -> bool {
        match (self, source) {
            (Self::Window(_), Source::Window) => true,
            (Self::Display(active_monitor, _), Source::Display) => Some(*active_monitor) == monitor,
            _ => false,
        }
    }

    fn next_frame_timeout(&mut self, timeout: Duration) -> Result<Option<Frame>, CaptureError> {
        match self {
            Self::Window(cap) => cap.next_frame_timeout(timeout),
            Self::Display(_, cap) => cap.next_frame_timeout(timeout),
        }
    }
}

pub struct FullscreenFallbackCapture {
    hwnd: HWND,
    process_id: u32,
    device: ID3D11Device,
    clock: RelativeClock,
    active: Option<ActiveSource>,
    black: Option<FrameData>,
    status: FullscreenFallbackStatus,
    retry_at: Instant,
    failures_since: Option<Instant>,
}

impl FullscreenFallbackCapture {
    pub fn for_window_on(
        device: ID3D11Device,
        hwnd: HWND,
        clock: RelativeClock,
    ) -> Result<Self, CaptureError> {
        if hwnd.is_invalid() {
            return Err(CaptureError::Init("invalid game window handle".into()));
        }
        let mut process_id = 0;
        // SAFETY: borrowed HWND; the read-only query only writes process_id.
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut process_id)) };
        if process_id == 0 {
            return Err(CaptureError::Init(
                "game window is no longer available".into(),
            ));
        }
        Ok(Self {
            hwnd,
            process_id,
            device,
            clock,
            active: None,
            black: None,
            status: FullscreenFallbackStatus::default(),
            retry_at: Instant::now(),
            failures_since: None,
        })
    }

    pub fn status(&self) -> FullscreenFallbackStatus {
        self.status.clone()
    }

    /// Returns short timeouts so a mode transition is observed within one
    /// video cadence. The app retries these while waiting for the first frame.
    pub fn next_frame_timeout(&mut self, timeout: Duration) -> Result<Option<Frame>, CaptureError> {
        let Some(before) = self.observe() else {
            self.active = None;
            return Ok(None);
        };
        let source = choose(before.observation);
        if self
            .active
            .as_ref()
            .is_some_and(|active| !active.matches(source, before.monitor))
        {
            // Closing the WGC session before opening DD is what removes the
            // Windows 10 border when a game enters fullscreen.
            self.active = None;
            self.failures_since = None;
            self.status.set(Source::Waiting);
            return self.waiting(timeout);
        }
        if source == Source::Waiting || Instant::now() < self.retry_at {
            return self.waiting(timeout);
        }
        if self.active.is_none() {
            let opened = match source {
                Source::Window => {
                    WgcCapture::for_window_client_on(self.device.clone(), self.hwnd, self.clock)
                        .map(ActiveSource::Window)
                }
                Source::Display => DxgiDuplicationCapture::for_monitor_on(
                    self.device.clone(),
                    before.monitor.expect("display source requires a monitor"),
                    self.clock,
                )
                .map(|cap| ActiveSource::Display(before.monitor.unwrap(), cap)),
                Source::Waiting => unreachable!(),
            };
            match opened {
                Ok(active) => self.active = Some(active),
                Err(error) => return self.recover(error, timeout),
            }
        }

        let result = self
            .active
            .as_mut()
            .expect("capture opened above")
            .next_frame_timeout(timeout.min(POLL_INTERVAL));
        let Some(after) = self.observe() else {
            self.active = None;
            return Ok(None);
        };
        if !accept_frame(
            source,
            before.observation,
            after.observation,
            before.monitor == after.monitor,
        ) {
            self.active = None;
            self.status.set(Source::Waiting);
            return self.waiting(Duration::ZERO);
        }
        match result {
            Ok(Some(frame)) => {
                if self.black.is_none() {
                    self.black = Some(FrameData::Gpu(black_texture_like(&self.device, &frame)?));
                }
                self.failures_since = None;
                self.status.set(source);
                Ok(Some(frame))
            }
            Err(CaptureError::Timeout(_)) => Err(CaptureError::Timeout(timeout)),
            Ok(None) => {
                self.active = None;
                self.recover(
                    CaptureError::SourceChanged("capture session closed".into()),
                    timeout,
                )
            }
            Err(error) => {
                self.active = None;
                self.recover(error, timeout)
            }
        }
    }

    fn recover(
        &mut self,
        error: CaptureError,
        timeout: Duration,
    ) -> Result<Option<Frame>, CaptureError> {
        let since = *self.failures_since.get_or_insert_with(Instant::now);
        if since.elapsed() >= RETRY_BUDGET {
            return Err(error);
        }
        self.retry_at = Instant::now() + RETRY_INTERVAL;
        tracing::warn!(event = "capture_source_retry", error = %error);
        self.waiting(timeout)
    }

    fn waiting(&mut self, timeout: Duration) -> Result<Option<Frame>, CaptureError> {
        self.status.set(Source::Waiting);
        std::thread::sleep(timeout.min(POLL_INTERVAL));
        let Some(data) = self.black.clone() else {
            return Err(CaptureError::Timeout(timeout));
        };
        let ticks = qpc_now_ticks_100ns().map_err(|e| CaptureError::DeviceLost(e.to_string()))?;
        Ok(Some(Frame {
            pts_s: self.clock.pts_s(ticks),
            data,
        }))
    }

    fn observe(&self) -> Option<Snapshot> {
        // SAFETY: all calls below are read-only window/monitor queries on a
        // borrowed HWND. A recycled HWND is rejected by its process id.
        unsafe {
            if !IsWindow(Some(self.hwnd)).as_bool() {
                return None;
            }
            let mut process_id = 0;
            GetWindowThreadProcessId(self.hwnd, Some(&mut process_id));
            if process_id != self.process_id {
                return None;
            }
            if !IsWindowVisible(self.hwnd).as_bool() || IsIconic(self.hwnd).as_bool() {
                return Some(Snapshot::unavailable());
            }
            let monitor = MonitorFromWindow(self.hwnd, MONITOR_DEFAULTTONULL);
            if monitor.is_invalid() {
                return Some(Snapshot::unavailable());
            }
            let mut client = RECT::default();
            if GetClientRect(self.hwnd, &mut client).is_err() {
                return Some(Snapshot::unavailable());
            }
            let mut origin = POINT {
                x: client.left,
                y: client.top,
            };
            if !ClientToScreen(self.hwnd, &mut origin).as_bool() {
                return Some(Snapshot::unavailable());
            }
            let mut info = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            if !GetMonitorInfoW(monitor, &mut info).as_bool() {
                return Some(Snapshot::unavailable());
            }
            let width = client.right.saturating_sub(client.left);
            let height = client.bottom.saturating_sub(client.top);
            let covers_monitor = origin.x == info.rcMonitor.left
                && origin.y == info.rcMonitor.top
                && origin.x.saturating_add(width) == info.rcMonitor.right
                && origin.y.saturating_add(height) == info.rcMonitor.bottom;
            Some(Snapshot {
                observation: Observation {
                    available: width > 0 && height > 0,
                    foreground: GetForegroundWindow() == self.hwnd,
                    covers_monitor,
                },
                monitor: Some(monitor),
            })
        }
    }
}

fn black_texture_like(
    device: &ID3D11Device,
    frame: &Frame,
) -> Result<ID3D11Texture2D, CaptureError> {
    let FrameData::Gpu(texture) = &frame.data else {
        return Err(CaptureError::Init("fallback expected a GPU frame".into()));
    };
    let (width, height) = d3d11::texture_size(texture);
    let pitch = width
        .checked_mul(4)
        .ok_or_else(|| CaptureError::Init("black frame width overflow".into()))?;
    let length = usize::try_from(pitch)
        .ok()
        .and_then(|pitch| pitch.checked_mul(height as usize))
        .ok_or_else(|| CaptureError::Init("black frame size overflow".into()))?;
    let mut pixels = vec![0u8; length];
    for pixel in pixels.as_chunks_mut::<4>().0 {
        pixel[3] = 255;
    }
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
        ..Default::default()
    };
    let data = D3D11_SUBRESOURCE_DATA {
        pSysMem: pixels.as_ptr().cast(),
        SysMemPitch: pitch,
        SysMemSlicePitch: 0,
    };
    let mut black = None;
    // SAFETY: the bounded BGRA buffer outlives synchronous CreateTexture2D;
    // the returned texture owns a copy of its pixels.
    unsafe {
        device
            .CreateTexture2D(&desc, Some(&data), Some(&mut black))
            .map_err(|e| CaptureError::Init(e.to_string()))?;
    }
    black.ok_or_else(|| CaptureError::Init("black texture was not created".into()))
}
