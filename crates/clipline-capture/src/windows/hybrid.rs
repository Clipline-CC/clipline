//! Opt-in PrintWindow / display experiment. Shell fullscreen state is a global
//! heuristic: restrict display switching to one monitor and the exact foreground
//! target. Checks around acquisition reduce exposure; they are not atomic desktop
//! isolation. No WGC, injection, screen crop fallback, or elevation.
use super::print_window::{Target, Worker};
use super::{DxgiDuplicationCapture, display, qpc_now_ticks_100ns};
use crate::hybrid_policy::{Observation, Source, accept_frame, accept_window_packet, choose};
use crate::{CaptureError, Frame, FrameData, RelativeClock};
use std::sync::{
    Arc,
    atomic::{AtomicU8, Ordering},
};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Gdi::{HMONITOR, MONITOR_DEFAULTTONULL, MonitorFromWindow};
use windows::Win32::UI::Shell::{
    QUNS_ACCEPTS_NOTIFICATIONS, QUNS_APP, QUNS_BUSY, QUNS_NOT_PRESENT, QUNS_PRESENTATION_MODE,
    QUNS_QUIET_TIME, QUNS_RUNNING_D3D_FULL_SCREEN, SHQueryUserNotificationState,
};
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

fn error(e: impl std::fmt::Display) -> CaptureError {
    CaptureError::Init(e.to_string())
}

/// A cloneable, live source indicator for recorder status and support bundles.
#[derive(Clone, Default)]
pub struct HybridStatus(Arc<AtomicU8>);
impl HybridStatus {
    pub fn label(&self) -> &'static str {
        match self.0.load(Ordering::Relaxed) {
            1 => "experimental_print_window",
            2 => "experimental_fullscreen_display",
            _ => "experimental_waiting_black",
        }
    }
    fn set(&self, source: Source) {
        let previous = self.0.swap(
            match source {
                Source::Window => 1,
                Source::Display => 2,
                Source::Waiting => 0,
            },
            Ordering::Relaxed,
        );
        if previous != self.0.load(Ordering::Relaxed) {
            crate::diagnostics::emit_diagnostic(
                crate::diagnostics::CaptureDiagnostic::HybridSourceChanged {
                    source: self.label(),
                },
            );
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
struct Snapshot {
    observation: Observation,
    monitor: HMONITOR,
    client: RECT,
}

fn observe_topology(
    mut observation: Observation,
    monitor: HMONITOR,
    client: RECT,
    displays: Result<Vec<display::DisplayHandle>, CaptureError>,
) -> Observation {
    // Only topology errors are recoverable here. Identity/protection validation
    // happens before this function and must still propagate to the recorder.
    let displays = displays.unwrap_or_default();
    observation.available &=
        !monitor.is_invalid() && displays.iter().any(|display| display.handle == monitor);
    observation.single_monitor = displays.len() == 1;
    for display in displays {
        if display.handle == monitor {
            let info = display.info;
            observation.covers_monitor = client.left == info.x
                && client.top == info.y
                && client.right as i64 == info.x as i64 + info.width as i64
                && client.bottom as i64 == info.y as i64 + info.height as i64;
        }
    }
    observation
}

pub struct HybridCapture {
    target: Target,
    automatic_target: bool,
    target_ended: bool,
    device: ID3D11Device,
    clock: RelativeClock,
    worker: Option<Worker>,
    window_request: Option<Snapshot>,
    display: Option<(HMONITOR, DxgiDuplicationCapture)>,
    black: FrameData,
    status: HybridStatus,
    candidate: Option<(Source, Instant)>,
    retry_at: Instant,
    failures_since: Option<Instant>,
}

impl HybridCapture {
    pub fn for_window_on(
        device: ID3D11Device,
        hwnd: isize,
        expected_pid: Option<u32>,
        clock: RelativeClock,
    ) -> Result<Self, CaptureError> {
        let hwnd = HWND(hwnd as *mut _);
        let mut pid = 0;
        // SAFETY: borrowed window handle, read-only query.
        unsafe {
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
        }
        if pid == 0 {
            return Err(error("target window unavailable"));
        }
        if expected_pid.is_some_and(|expected| expected != pid) {
            return Err(error("detected game window changed process"));
        }
        let target = Target::new(hwnd.0 as isize, pid).map_err(error)?;
        // SAFETY: resolve this target's monitor, never substitute primary.
        let monitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONULL) };
        let displays = display::enumerate_complete_display_handles()?;
        let info = displays
            .into_iter()
            .find(|display| display.handle == monitor)
            .map(|display| display.info)
            .ok_or_else(|| error("target display unavailable"))?;
        let client = target.geometry().ok().map(|(_, client, _)| {
            (client.right as u32, client.bottom as u32)
        });
        let (width, height) = crate::hybrid_policy::initial_canvas(client, (info.width, info.height));
        let length = crate::print_protocol::frame_bytes(width, height).map_err(error)?;
        let black = FrameData::Gpu(upload(&device, width, height, &vec![0; length])?);
        Ok(Self {
            target,
            automatic_target: expected_pid.is_some(),
            target_ended: false,
            device,
            clock,
            worker: None,
            window_request: None,
            display: None,
            black,
            status: HybridStatus::default(),
            candidate: None,
            retry_at: Instant::now(),
            failures_since: None,
        })
    }

    pub fn status(&self) -> HybridStatus {
        self.status.clone()
    }

    /// The seed fixes output geometry to the initial validated client, avoiding
    /// monitor-shaped padding for window capture. Unavailable initial geometry
    /// retains the display fallback; existing conversion fits later input.
    pub fn seed(&self) -> Result<Frame, CaptureError> {
        self.frame(self.black.clone())
    }

    fn frame(&self, data: FrameData) -> Result<Frame, CaptureError> {
        Ok(Frame {
            pts_s: self.clock.pts_s(qpc_now_ticks_100ns().map_err(error)?),
            data,
        })
    }

    fn observe(&self) -> Result<Snapshot, CaptureError> {
        self.target.validate_identity().map_err(error)?;
        let geometry = self.target.geometry().ok();
        // SAFETY: read-only window/shell queries. Unknown shell states fail closed.
        let (monitor, foreground, fullscreen) = unsafe {
            let state = SHQueryUserNotificationState();
            let fullscreen = match state {
                Ok(s) if s == QUNS_RUNNING_D3D_FULL_SCREEN => Some(true),
                Ok(s)
                    if s == QUNS_BUSY
                        || s == QUNS_ACCEPTS_NOTIFICATIONS
                        || s == QUNS_QUIET_TIME
                        || s == QUNS_PRESENTATION_MODE
                        || s == QUNS_APP =>
                {
                    Some(false)
                }
                Ok(s) if s == QUNS_NOT_PRESENT => None,
                _ => None,
            };
            (
                MonitorFromWindow(self.target.hwnd, MONITOR_DEFAULTTONULL),
                GetForegroundWindow() == self.target.hwnd,
                fullscreen,
            )
        };
        let mut client = RECT::default();
        if let Some((_, size, origin)) = geometry {
            client = RECT {
                left: origin.x,
                top: origin.y,
                right: origin.x.saturating_add(size.right),
                bottom: origin.y.saturating_add(size.bottom),
            };
        }
        Ok(Snapshot {
            observation: observe_topology(
                Observation {
                    available: geometry.is_some(),
                    foreground,
                    covers_monitor: false,
                    single_monitor: false,
                    fullscreen,
                },
                monitor,
                client,
                display::enumerate_complete_display_handles(),
            ),
            monitor,
            client,
        })
    }

    fn waiting(&mut self, wait: Duration) -> Result<Option<Frame>, CaptureError> {
        self.status.set(Source::Waiting);
        std::thread::sleep(wait.min(Duration::from_millis(16)));
        Ok(Some(self.frame(self.black.clone())?))
    }

    pub fn next_frame_timeout(&mut self, timeout: Duration) -> Result<Option<Frame>, CaptureError> {
        if self.target_ended || self.target.ended() {
            return self.ended(timeout);
        }
        let before = match self.observe() {
            Ok(snapshot) => snapshot,
            Err(_) if self.target.ended() => return self.ended(timeout),
            Err(e) => return Err(e),
        };
        let source = choose(before.observation);
        if source == Source::Waiting {
            self.worker = None;
            self.window_request = None;
            self.display = None;
            self.candidate = None;
            return self.waiting(timeout);
        }
        if self.candidate.is_none_or(|(s, _)| s != source) {
            // Release before opening a replacement and discard in-flight worker
            // packets. Debounce entry; guard loss above is immediate.
            self.worker = None;
            self.window_request = None;
            self.display = None;
            self.candidate = Some((source, Instant::now()));
            self.failures_since = None;
            return self.waiting(timeout);
        }
        if self
            .candidate
            .is_some_and(|(_, since)| since.elapsed() < Duration::from_millis(150))
            || Instant::now() < self.retry_at
        {
            return self.waiting(timeout);
        }
        let result = match source {
            Source::Window => self.read_window(before, timeout),
            Source::Display => self.read_display(before.monitor, timeout),
            Source::Waiting => unreachable!(),
        };
        let after = match self.observe() {
            Ok(snapshot) => snapshot,
            Err(_) if self.target.ended() => return self.ended(timeout),
            Err(e) => return Err(e),
        };
        if !accept_frame(
            source,
            before.observation,
            after.observation,
            before.monitor == after.monitor && before.client == after.client,
        ) {
            self.worker = None;
            self.window_request = None;
            self.display = None;
            self.candidate = None;
            return self.waiting(Duration::ZERO);
        }
        match result {
            Ok(Some(frame)) => {
                self.failures_since = None;
                self.status.set(source);
                Ok(Some(frame))
            }
            Ok(None) => {
                // Keep only a previously validated texture via the cadencer.
                // No guard failure reaches this timeout path.
                Err(CaptureError::Timeout(timeout))
            }
            Err(e) => {
                self.worker = None;
                self.window_request = None;
                self.display = None;
                let since = *self.failures_since.get_or_insert_with(Instant::now);
                if since.elapsed() >= Duration::from_secs(5) {
                    return Err(e);
                }
                self.retry_at = Instant::now() + Duration::from_millis(200);
                self.waiting(Duration::ZERO)
            }
        }
    }

    fn ended(&mut self, timeout: Duration) -> Result<Option<Frame>, CaptureError> {
        self.target_ended = true;
        self.worker = None;
        self.window_request = None;
        self.display = None;
        if self.automatic_target {
            // Let game detection close this session and return to waiting for the
            // next game. A fatal service error would clear automatic-record intent.
            self.waiting(timeout)
        } else {
            Ok(None)
        }
    }

    fn read_window(
        &mut self,
        before: Snapshot,
        timeout: Duration,
    ) -> Result<Option<Frame>, CaptureError> {
        if self.window_request.is_some_and(|request| request != before) {
            self.worker = None;
            self.window_request = None;
        }
        if self.worker.is_none() {
            self.worker =
                Some(Worker::new(self.target.hwnd.0 as isize, self.target.pid).map_err(error)?);
        }
        let request = *self.window_request.get_or_insert(before);
        let Some(p) = self
            .worker
            .as_mut()
            .expect("worker opened")
            .sample(timeout)
            .map_err(error)?
        else {
            return Ok(None);
        };
        self.window_request = None;
        let now = qpc_now_ticks_100ns().map_err(error)?;
        let age = Duration::from_nanos(now.saturating_sub(p.begin).max(0) as u64 * 100);
        if !accept_window_packet(
            request.observation,
            before.observation,
            request.monitor == before.monitor && request.client == before.client,
            age,
        ) {
            return Err(error("late or changed PrintWindow request discarded"));
        }
        Ok(Some(Frame {
            pts_s: self.clock.pts_s(p.end),
            data: FrameData::Gpu(upload(&self.device, p.width, p.height, &p.pixels)?),
        }))
    }

    fn read_display(
        &mut self,
        monitor: HMONITOR,
        timeout: Duration,
    ) -> Result<Option<Frame>, CaptureError> {
        if self.display.as_ref().is_none_or(|(h, _)| *h != monitor) {
            self.display = None;
            self.display = Some((
                monitor,
                DxgiDuplicationCapture::for_monitor_on(self.device.clone(), monitor, self.clock)?,
            ));
        }
        match self
            .display
            .as_mut()
            .expect("display opened")
            .1
            .next_frame_timeout(timeout.min(Duration::from_millis(16)))
        {
            Ok(frame) => Ok(frame),
            Err(CaptureError::Timeout(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

fn upload(
    device: &ID3D11Device,
    width: u32,
    height: u32,
    pixels: &[u8],
) -> Result<ID3D11Texture2D, CaptureError> {
    if crate::print_protocol::frame_bytes(width, height).map_err(error)? != pixels.len() {
        return Err(error("invalid BGRA frame length"));
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
        SysMemPitch: width * 4,
        SysMemSlicePitch: 0,
    };
    let mut texture = None;
    // SAFETY: bounded contiguous BGRA input lives through synchronous upload;
    // a new texture never overwrites a frame retained by the encoder/cadencer.
    unsafe {
        device
            .CreateTexture2D(&desc, Some(&data), Some(&mut texture))
            .map_err(error)?;
    }
    texture.ok_or_else(|| error("texture missing"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_seed_texture_uses_client_size_instead_of_display_size() {
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, WINDOW_EX_STYLE, WS_POPUP, WS_VISIBLE,
        };
        // WARP handles the texture, but this integration test needs a desktop.
        // The neutral ultrawide/conversion regression runs on every CI OS.
        if std::env::var_os("CI").is_some()
            || display::enumerate_complete_display_handles().ok().is_none_or(|d| d.is_empty())
        {
            return;
        }
        struct TestWindow(HWND);
        impl Drop for TestWindow {
            fn drop(&mut self) {
                // SAFETY: test owns this window on its creating thread.
                unsafe { let _ = DestroyWindow(self.0); }
            }
        }
        // SAFETY: built-in window class; this test owns the noninteractive fixture
        // and destroys it before returning. No user window is modified.
        let window = TestWindow(unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(), windows::core::w!("STATIC"),
                windows::core::w!("Clipline canvas test"), WS_POPUP | WS_VISIBLE,
                0, 0, 192, 108, None, None, None, None,
            ).unwrap()
        });
        let (device, _) = super::super::d3d11::create_device_for_tests().unwrap();
        let capture = HybridCapture::for_window_on(
            device, window.0.0 as isize, Some(std::process::id()), RelativeClock::new(0),
        ).unwrap();
        let FrameData::Gpu(texture) = capture.seed().unwrap().data else {
            panic!("expected GPU seed");
        };
        assert_eq!(super::super::d3d11::texture_size(&texture), (192, 108));
    }

    fn topology_observation(fullscreen: bool) -> Observation {
        Observation {
            available: true,
            foreground: true,
            covers_monitor: false,
            single_monitor: false,
            fullscreen: Some(fullscreen),
        }
    }

    fn test_display() -> display::DisplayHandle {
        display::DisplayHandle {
            handle: HMONITOR(std::ptr::dangling_mut()),
            info: display::DisplayInfo {
                id: "test-target".into(),
                name: "test-target".into(),
                x: 0,
                y: 0,
                width: 1280,
                height: 720,
                is_primary: true,
            },
        }
    }

    #[test]
    fn topology_failure_waits_discards_acquired_pixels_and_recovers() {
        let monitor = test_display().handle;
        let client = RECT {
            left: 0,
            top: 0,
            right: 1280,
            bottom: 720,
        };
        for fullscreen in [false, true] {
            let input = topology_observation(fullscreen);
            let before = observe_topology(input, monitor, client, Ok(vec![test_display()]));
            let source = choose(before);
            assert_eq!(
                source,
                if fullscreen {
                    Source::Display
                } else {
                    Source::Window
                }
            );
            let unavailable =
                observe_topology(input, monitor, client, Err(error("monitor info failed")));
            assert_eq!(choose(unavailable), Source::Waiting);
            assert!(!accept_frame(source, before, unavailable, true));
            assert!(!accept_window_packet(
                before,
                unavailable,
                true,
                Duration::ZERO
            ));
            let recovered = observe_topology(input, monitor, client, Ok(vec![test_display()]));
            assert_eq!(choose(recovered), source);
        }
    }

    #[test]
    fn missing_target_monitor_never_substitutes_primary_or_captures_window() {
        for fullscreen in [false, true] {
            for monitor in [HMONITOR::default(), HMONITOR(2usize as *mut _)] {
                for displays in [vec![], vec![test_display()]] {
                    let observation = observe_topology(
                        topology_observation(fullscreen),
                        monitor,
                        RECT::default(),
                        Ok(displays),
                    );
                    assert_eq!(choose(observation), Source::Waiting);
                }
            }
        }
    }

    #[test]
    fn complete_multi_monitor_topology_keeps_window_capture_but_blocks_display() {
        let target = test_display();
        let mut secondary = test_display();
        secondary.handle = HMONITOR(2usize as *mut _);
        let client = RECT {
            left: 0,
            top: 0,
            right: 1280,
            bottom: 720,
        };
        for fullscreen in [false, true] {
            let observation = observe_topology(
                topology_observation(fullscreen),
                target.handle,
                client,
                Ok(vec![target.clone(), secondary.clone()]),
            );
            assert_eq!(
                choose(observation),
                if fullscreen {
                    Source::Waiting
                } else {
                    Source::Window
                }
            );
        }
    }
    #[test]
    fn owned_upload_preserves_bgra_and_black_without_mutating_previous_frame() {
        let (device, _) = super::super::d3d11::create_device_for_tests().unwrap();
        let colors = [
            0u8, 0, 255, 255, 255, 0, 0, 255, 0, 255, 0, 255, 255, 255, 255, 255,
        ];
        let colored = upload(&device, 2, 2, &colors).unwrap();
        let black = upload(&device, 2, 2, &[0; 16]).unwrap();
        assert_eq!(
            super::super::nv12::read_bgra(&device, &black)
                .unwrap()
                .bytes,
            [0; 16]
        );
        assert_eq!(
            super::super::nv12::read_bgra(&device, &colored)
                .unwrap()
                .bytes,
            colors
        );
        assert!(upload(&device, 2, 2, &[0; 15]).is_err());
    }
}
