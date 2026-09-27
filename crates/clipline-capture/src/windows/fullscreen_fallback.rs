//! WGC for a selected game window, Desktop Duplication while that window fills
//! its monitor and no other app's window stacked above it reaches that monitor.
//! The desktop source is dropped as soon as either guard changes. Its frames
//! are checked again after acquisition.

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
    MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTONULL,
};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::UI::WindowsAndMessaging::{
    GetClassNameW, GetClientRect, GetWindow, GetWindowDisplayAffinity, GetWindowLongPtrW,
    GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible, GWL_EXSTYLE, GW_HWNDPREV,
    WDA_NONE, WS_EX_LAYERED, WS_EX_TRANSPARENT,
};

use crate::fallback_policy::{
    accept_frame, choose, client_covers_monitor, first_cover, Observation, ScreenRect,
    StackCover, Source, WindowAbove,
};
use crate::{CaptureError, Frame, FrameData, RelativeClock};

use super::window::window_frame_rect;
use super::{qpc_now_ticks_100ns, DxgiDuplicationCapture, WgcCapture};

const POLL_INTERVAL: Duration = Duration::from_millis(16);
const RETRY_INTERVAL: Duration = Duration::from_millis(200);
const RETRY_BUDGET: Duration = Duration::from_secs(5);
/// Hidden windows count toward this too. Deeper stacks fail closed.
const MAX_WINDOWS_ABOVE: usize = 4096;

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
                covered: false,
                covers_monitor: false,
                display_supported: true,
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
    /// The game's monitor size when capture opened. Recording at this size
    /// and letterboxing the window lets a windowed-to-fullscreen switch keep
    /// full resolution instead of squeezing the monitor into the window size.
    canvas: (u32, u32),
    /// Canvas-sized, so transitions to and from the monitor don't rebuild the
    /// encoder's video processor, and ready before any source frame so a
    /// fullscreen game can start recording while another app covers it.
    black: FrameData,
    status: FullscreenFallbackStatus,
    retry_at: Instant,
    failures_since: Option<Instant>,
    unsupported_display: Option<HMONITOR>,
    last_cover: StackCover<isize>,
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
        let canvas = canvas_for_window(hwnd)?;
        let black = FrameData::Gpu(black_texture(&device, canvas)?);
        Ok(Self {
            hwnd,
            process_id,
            device,
            clock,
            active: None,
            canvas,
            black,
            status: FullscreenFallbackStatus::default(),
            retry_at: Instant::now(),
            failures_since: None,
            unsupported_display: None,
            last_cover: StackCover::Clear,
        })
    }

    pub fn status(&self) -> FullscreenFallbackStatus {
        self.status.clone()
    }

    /// The source size to configure the encoder with. Pair it with
    /// `VideoFit::Contain`, since window frames are smaller than this.
    pub fn canvas_size(&self) -> (u32, u32) {
        self.canvas
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
                Err(error) => return self.recover(error, source, before.monitor, timeout),
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
                self.failures_since = None;
                self.status.set(source);
                Ok(Some(frame))
            }
            Err(CaptureError::Timeout(_)) => Err(CaptureError::Timeout(timeout)),
            Ok(None) => {
                self.active = None;
                self.recover(
                    CaptureError::SourceChanged("capture session closed".into()),
                    source,
                    before.monitor,
                    timeout,
                )
            }
            Err(error) => {
                self.active = None;
                self.recover(error, source, before.monitor, timeout)
            }
        }
    }

    fn recover(
        &mut self,
        error: CaptureError,
        source: Source,
        monitor: Option<HMONITOR>,
        timeout: Duration,
    ) -> Result<Option<Frame>, CaptureError> {
        if source == Source::Display && matches!(error, CaptureError::Unsupported(_)) {
            // Rotated and cross-GPU monitors never duplicate. Keep recording
            // the game through WGC rather than ending the session. Transient
            // failures (mode switches, UAC) take the ordinary retry path.
            tracing::warn!(event = "desktop_duplication_unavailable", error = %error);
            self.unsupported_display = monitor;
            self.failures_since = None;
            self.retry_at = Instant::now();
            return self.waiting(timeout);
        }
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
        let ticks = qpc_now_ticks_100ns().map_err(|e| CaptureError::DeviceLost(e.to_string()))?;
        Ok(Some(Frame {
            pts_s: self.clock.pts_s(ticks),
            data: self.black.clone(),
        }))
    }

    /// Support bundles need to show which window blacked out a recording.
    fn log_cover_change(&mut self, cover: StackCover<HWND>) {
        let cover = match cover {
            StackCover::Clear => StackCover::Clear,
            StackCover::By(hwnd) => StackCover::By(hwnd.0 as isize),
            StackCover::Unknown => StackCover::Unknown,
        };
        if cover == self.last_cover {
            return;
        }
        self.last_cover = cover;
        match cover {
            StackCover::Clear => tracing::info!(event = "fullscreen_capture_uncovered"),
            StackCover::By(raw) => {
                let hwnd = HWND(raw as *mut _);
                let mut process_id = 0;
                // SAFETY: read-only query; the HWND came from the z-order walk.
                unsafe { GetWindowThreadProcessId(hwnd, Some(&mut process_id)) };
                tracing::info!(
                    event = "fullscreen_capture_covered",
                    class = %window_class(hwnd),
                    process_id,
                );
            }
            StackCover::Unknown => {
                tracing::warn!(event = "fullscreen_capture_covered", reason = "stack_too_deep")
            }
        }
    }

    fn observe(&mut self) -> Option<Snapshot> {
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
            let client_bounds = ScreenRect {
                left: origin.x,
                top: origin.y,
                right: origin.x.saturating_add(width),
                bottom: origin.y.saturating_add(height),
            };
            let monitor_rect = screen_rect(info.rcMonitor);
            let covers_monitor = client_covers_monitor(client_bounds, monitor_rect, monitor_bounds);
            // Only a fullscreen game can be duplicated, so only then does the
            // stack above it matter.
            let cover = if covers_monitor {
                first_cover(
                    windows_above(self.hwnd, self.process_id),
                    monitor_rect,
                    MAX_WINDOWS_ABOVE,
                )
            } else {
                StackCover::Clear
            };
            self.log_cover_change(cover);
            Some(Snapshot {
                observation: Observation {
                    available: width > 0 && height > 0,
                    covered: !cover.is_clear(),
                    covers_monitor,
                    display_supported: self.unsupported_display != Some(monitor),
                },
                monitor: Some(monitor),
            })
        }
    }
}

/// Top-level windows stacked above `hwnd`, nearest first. Windows the policy
/// already rules out (the game's own, hidden, minimized, cloaked) skip the
/// costlier style, affinity, class and bounds queries.
fn windows_above(hwnd: HWND, game_process: u32) -> impl Iterator<Item = (HWND, WindowAbove)> {
    // SAFETY: GetWindow only reads the z-order; stale handles end the walk.
    let mut next = unsafe { GetWindow(hwnd, GW_HWNDPREV) }.ok();
    std::iter::from_fn(move || {
        let current = next?;
        // SAFETY: as above.
        next = unsafe { GetWindow(current, GW_HWNDPREV) }.ok();
        Some((current, describe_window(current, game_process)))
    })
}

fn describe_window(hwnd: HWND, game_process: u32) -> WindowAbove {
    // SAFETY: read-only queries on a window handle from the z-order walk. A
    // window destroyed mid-walk only makes these calls fail.
    unsafe {
        let mut process_id = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut process_id));
        if process_id == game_process {
            return WindowAbove {
                same_process: true,
                ..WindowAbove::default()
            };
        }
        if !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() || is_cloaked(hwnd) {
            return WindowAbove::default();
        }
        let ex_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        let click_through = ex_style & WS_EX_LAYERED.0 != 0 && ex_style & WS_EX_TRANSPARENT.0 != 0;
        let mut affinity = WDA_NONE.0;
        let excluded_from_capture =
            GetWindowDisplayAffinity(hwnd, &mut affinity).is_ok() && affinity != WDA_NONE.0;
        let class = window_class(hwnd);
        WindowAbove {
            same_process: false,
            shown: true,
            click_through,
            excluded_from_capture,
            taskbar: class == "Shell_TrayWnd" || class == "Shell_SecondaryTrayWnd",
            bounds: window_frame_rect(hwnd).map(screen_rect),
        }
    }
}

/// Windows on other virtual desktops, suspended UWP apps and a hidden Game
/// Bar stay "visible" but are cloaked, so DWM does not draw them.
unsafe fn is_cloaked(hwnd: HWND) -> bool {
    let mut cloaked = 0u32;
    // SAFETY: DWMWA_CLOAKED writes one u32 into the provided buffer.
    let result = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            &mut cloaked as *mut u32 as *mut _,
            std::mem::size_of::<u32>() as u32,
        )
    };
    result.is_ok() && cloaked != 0
}

fn window_class(hwnd: HWND) -> String {
    let mut buffer = [0u16; 64];
    // SAFETY: GetClassNameW writes at most buffer.len() UTF-16 units.
    let len = unsafe { GetClassNameW(hwnd, &mut buffer) };
    String::from_utf16_lossy(&buffer[..usize::try_from(len).unwrap_or(0)])
}

fn monitor_bounds() -> Option<Vec<ScreenRect>> {
    let displays = super::display::complete_display_handles().ok()?;
    displays
        .into_iter()
        .map(|display| {
            let info = display.info;
            Some(ScreenRect {
                left: info.x,
                top: info.y,
                right: info.x.checked_add(i32::try_from(info.width).ok()?)?,
                bottom: info.y.checked_add(i32::try_from(info.height).ok()?)?,
            })
        })
        .collect()
}

fn screen_rect(rect: RECT) -> ScreenRect {
    ScreenRect {
        left: rect.left,
        top: rect.top,
        right: rect.right,
        bottom: rect.bottom,
    }
}

/// The game's monitor size in physical pixels (the process is per-monitor
/// DPI aware). Nearest, not null, so a minimized game still has a canvas.
fn canvas_for_window(hwnd: HWND) -> Result<(u32, u32), CaptureError> {
    // SAFETY: read-only monitor queries on a borrowed HWND.
    let rect = unsafe {
        let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !GetMonitorInfoW(monitor, &mut info).as_bool() {
            return Err(CaptureError::Init("game monitor is unavailable".into()));
        }
        info.rcMonitor
    };
    let width = u32::try_from(rect.right.saturating_sub(rect.left)).unwrap_or(0);
    let height = u32::try_from(rect.bottom.saturating_sub(rect.top)).unwrap_or(0);
    if width == 0 || height == 0 {
        return Err(CaptureError::Init("game monitor has no area".into()));
    }
    Ok((width, height))
}

fn black_texture(
    device: &ID3D11Device,
    (width, height): (u32, u32),
) -> Result<ID3D11Texture2D, CaptureError> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use windows::core::w;
    use windows::Win32::UI::WindowsAndMessaging::{
        FindWindowW, GetDesktopWindow, GW_CHILD, GW_HWNDLAST,
    };

    /// The shell's own taskbar reappears over an unfocused game and must not
    /// black out the recording. Needs an interactive desktop.
    #[test]
    fn the_taskbar_is_recognised_and_never_covers() {
        if std::env::var_os("CI").is_some() {
            return;
        }
        // SAFETY: FindWindowW only reads the window list.
        let Ok(tray) = (unsafe { FindWindowW(w!("Shell_TrayWnd"), None) }) else {
            eprintln!("SKIP: no taskbar on this desktop");
            return;
        };
        let window = describe_window(tray, 0);
        assert!(window.shown && window.taskbar, "{window:?}");
    }

    /// Walking from the bottom of the z-order visits every top-level window,
    /// hidden ones included. It must end well inside the limit and quickly
    /// enough to run twice per ~16 ms poll.
    #[test]
    fn a_full_z_order_walk_ends_inside_the_limit() {
        if std::env::var_os("CI").is_some() {
            return;
        }
        // SAFETY: read-only z-order queries.
        let bottom = unsafe { GetWindow(GetDesktopWindow(), GW_CHILD) }
            .and_then(|first| unsafe { GetWindow(first, GW_HWNDLAST) });
        let Ok(bottom) = bottom else {
            eprintln!("SKIP: no top-level windows");
            return;
        };
        let started = Instant::now();
        let count = windows_above(bottom, 0).count();
        let elapsed = started.elapsed();
        eprintln!("z-order walk: {count} windows in {elapsed:?}");
        assert!(count < MAX_WINDOWS_ABOVE, "{count} windows");
    }
}
