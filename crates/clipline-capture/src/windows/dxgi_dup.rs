//! DXGI Desktop Duplication capture (issue #42): a borderless display/region
//! engine for Windows 10, where WGC's `SetIsBorderRequired(false)` is ignored
//! and the yellow privacy border remains. Display/region only — window capture
//! stays on WGC (Desktop Duplication cannot target a single window).
//!
//! Mirrors `WgcCapture`'s contracts: a caller-provided device (shared with the
//! encoder — textures don't cross devices) and clock (shared with audio — one
//! QPC timebase, ddoc §6), and GPU BGRA frames (ddoc §3: pixels stay on the
//! GPU). Unlike WGC's free-threaded frame pool + callback, duplication is a
//! pull model on the calling thread: `AcquireNextFrame` blocks up to a timeout,
//! we copy the read-only (recycled) desktop texture into a fresh BGRA texture
//! and `ReleaseFrame`. The mouse cursor is NOT composited in v1 — it is often
//! already in the desktop image (only a hardware-overlay cursor is absent; see
//! the issue #42 follow-up).

use std::time::{Duration, Instant};

use windows::Win32::Foundation::E_ACCESSDENIED;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_MODE_ROTATION, DXGI_MODE_ROTATION_IDENTITY,
    DXGI_MODE_ROTATION_UNSPECIFIED,
};
use windows::Win32::Graphics::Dxgi::{
    DXGI_ERROR_ACCESS_DENIED, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_DEVICE_REMOVED,
    DXGI_ERROR_NOT_CURRENTLY_AVAILABLE, DXGI_ERROR_NOT_FOUND, DXGI_ERROR_SESSION_DISCONNECTED,
    DXGI_ERROR_UNSUPPORTED, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTPUT_DESC,
    IDXGIAdapter, IDXGIDevice, IDXGIOutput, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
};
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows::Win32::System::Performance::QueryPerformanceFrequency;
use windows::core::{HRESULT, Interface};

use crate::clock::{RelativeClock, qpc_to_ticks_100ns};
use crate::diagnostics::{CaptureDiagnostic, DiagnosticRateLimiter, emit_diagnostic};
use crate::traits::{CaptureEngine, CaptureError, Frame, FrameData};
use crate::windows::d3d11;
use crate::windows::nv12::CropRect;

/// Default `next_frame` wait, matching `WgcCapture`: duplication only delivers
/// on screen updates, so an idle desktop can legitimately go quiet.
const DEFAULT_FRAME_TIMEOUT: Duration = Duration::from_secs(5);
/// Cap a single `AcquireNextFrame` wait so a long first-frame budget still
/// polls (and access-loss recreation is retried within the budget).
const MAX_ACQUIRE_WAIT: Duration = Duration::from_millis(1000);
/// Back-off after an immediate, recoverable miss (access-loss recreate,
/// pointer-only/no-new-content frame, or a skipped frame). `AcquireNextFrame`
/// returns instantly in these cases, so without a wait the recorder would spin
/// and race the replay timeline ahead of wall clock during a UAC prompt / lock.
const RETRY_BACKOFF: Duration = Duration::from_millis(10);

#[derive(Clone, Copy)]
enum CopyMode {
    Full,
    Region(CropRect),
}

/// Result of one `AcquireNextFrame` attempt.
enum AcquireOutcome {
    /// A frame to emit.
    Frame(Frame),
    /// `AcquireNextFrame` waited the full slice with no new frame — real time
    /// was consumed, so the caller only needs to re-check the deadline.
    WaitedOut,
    /// An immediate, recoverable miss (access loss recreated, no new desktop
    /// content, or a skipped frame). The caller must back off, not spin.
    RetryImmediately,
}

pub struct DxgiDuplicationCapture {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    /// Kept so the duplication can be recreated on access loss without
    /// re-enumerating the adapter.
    output: IDXGIOutput1,
    dupl: Option<IDXGIOutputDuplication>,
    needs_seed: bool,
    reopen_diagnostics: DiagnosticRateLimiter,
    copy_mode: CopyMode,
    clock: RelativeClock,
    qpc_freq: i64,
    /// A frame is acquired but not yet released — exactly one `ReleaseFrame`
    /// must pair each successful `AcquireNextFrame` before the next acquire.
    frame_held: bool,
    /// Source-side monotonic floor for pts ticks.
    last_ticks_100ns: i64,
}

impl DxgiDuplicationCapture {
    /// Duplicate the primary monitor on a caller-provided device and clock.
    pub fn primary_monitor_on(
        device: ID3D11Device,
        clock: RelativeClock,
    ) -> Result<Self, CaptureError> {
        let display = crate::windows::display::display_handle_by_id(None)?;
        Self::for_monitor_on(device, display.handle, clock)
    }

    /// Duplicate a specific monitor on a caller-provided device and clock.
    pub fn for_monitor_on(
        device: ID3D11Device,
        monitor: HMONITOR,
        clock: RelativeClock,
    ) -> Result<Self, CaptureError> {
        Self::new(device, monitor, clock, CopyMode::Full)
    }

    /// Duplicate a monitor but emit only a fixed sub-region of it.
    pub fn for_monitor_region_on(
        device: ID3D11Device,
        monitor: HMONITOR,
        clock: RelativeClock,
        crop: CropRect,
    ) -> Result<Self, CaptureError> {
        Self::new(device, monitor, clock, CopyMode::Region(crop))
    }

    fn new(
        device: ID3D11Device,
        monitor: HMONITOR,
        clock: RelativeClock,
        copy_mode: CopyMode,
    ) -> Result<Self, CaptureError> {
        if monitor.is_invalid() {
            return Err(CaptureError::Init("invalid monitor handle".into()));
        }
        let init = |e: windows::core::Error| CaptureError::Init(e.to_string());
        crate::windows::d3d11::ensure_multithread_protected(&device).map_err(init)?;
        // SAFETY: trivial getter on a valid device.
        let context = unsafe { device.GetImmediateContext() }.map_err(init)?;
        let output = find_output_for_monitor(&device, monitor)?;
        let dupl = duplicate_output(&output, &device)?;
        let mut qpc_freq = 0i64;
        // SAFETY: out-pointer is valid; the call cannot fail on XP+.
        unsafe { QueryPerformanceFrequency(&mut qpc_freq).map_err(init)? };
        Ok(Self {
            device,
            context,
            output,
            dupl: Some(dupl),
            needs_seed: true,
            reopen_diagnostics: DiagnosticRateLimiter::new(Duration::from_secs(5)),
            copy_mode,
            clock,
            qpc_freq: qpc_freq.max(1),
            frame_held: false,
            last_ticks_100ns: i64::MIN,
        })
    }

    /// The capture-start clock — share it with audio sources so all pts live on
    /// one timeline (ddoc §6).
    pub fn clock(&self) -> RelativeClock {
        self.clock
    }

    /// Pull a frame, waiting up to `timeout`. A timeout maps to
    /// `CaptureError::Timeout` so the caller's cadencer reuses the last frame.
    ///
    /// The budget is tracked against a real wall-clock deadline, not by
    /// subtracting the requested slice: access-loss and no-new-content misses
    /// return from `AcquireNextFrame` instantly, so a naive subtraction would
    /// let the recorder spin and advance the replay timeline far faster than
    /// real time during a UAC prompt / lock. Immediate misses back off instead.
    pub fn next_frame_timeout(&mut self, timeout: Duration) -> Result<Option<Frame>, CaptureError> {
        let start = Instant::now();
        loop {
            let Some(remaining) = timeout.checked_sub(start.elapsed()) else {
                return Err(CaptureError::Timeout(timeout));
            };
            if remaining.is_zero() {
                return Err(CaptureError::Timeout(timeout));
            }
            match self.try_acquire(remaining.min(MAX_ACQUIRE_WAIT))? {
                AcquireOutcome::Frame(frame) => return Ok(Some(frame)),
                // AcquireNextFrame already consumed real time; just loop and
                // re-check the deadline.
                AcquireOutcome::WaitedOut => {}
                // Returned immediately — sleep so we honor wall-clock cadence.
                AcquireOutcome::RetryImmediately => {
                    std::thread::sleep(remaining.min(RETRY_BACKOFF));
                }
            }
        }
    }

    /// One acquire→copy→release cycle.
    fn try_acquire(&mut self, wait: Duration) -> Result<AcquireOutcome, CaptureError> {
        // Defensive: never hold two frames across an acquire.
        self.release_held_frame();

        if self.dupl.is_none() {
            self.recreate_duplication();
            return Ok(AcquireOutcome::RetryImmediately);
        }
        let timeout_ms = wait.as_millis().min(u32::MAX as u128) as u32;
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        // SAFETY: dupl is live; out-params are valid for the call's duration.
        let acquired = unsafe {
            self.dupl
                .as_ref()
                .expect("duplication opened above")
                .AcquireNextFrame(timeout_ms, &mut info, &mut resource)
        };
        match acquired {
            Ok(()) => self.frame_held = true,
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(AcquireOutcome::WaitedOut),
            Err(e) if is_access_lost(e.code()) => {
                // Mode change, UAC/secure desktop, session lock. Recreate the
                // duplication and let the caller retry — never drop the engine.
                self.recreate_duplication();
                return Ok(AcquireOutcome::RetryImmediately);
            }
            Err(e) if e.code() == DXGI_ERROR_DEVICE_REMOVED => {
                return Err(CaptureError::DeviceLost(format!(
                    "DXGI device removed: {e}"
                )));
            }
            Err(e) => return Err(CaptureError::DeviceLost(format!("AcquireNextFrame: {e}"))),
        }

        // A frame is held — release on every path below.
        let outcome = self.copy_frame(&info, resource.as_ref());
        self.release_held_frame();
        outcome
    }

    fn copy_frame(
        &mut self,
        info: &DXGI_OUTDUPL_FRAME_INFO,
        resource: Option<&IDXGIResource>,
    ) -> Result<AcquireOutcome, CaptureError> {
        let dev = |e: windows::core::Error| CaptureError::DeviceLost(e.to_string());
        let Some(resource) = resource else {
            return Ok(AcquireOutcome::RetryImmediately);
        };
        // SAFETY: the desktop resource is an ID3D11Texture2D on the shared device.
        let source: ID3D11Texture2D = resource.cast().map_err(dev)?;
        let desc = d3d11::texture_desc(&source);
        // The duplicated desktop is documented as always B8G8R8A8_UNORM; bail to
        // reuse-last on the theoretical mismatch rather than risk a copy error.
        if desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM {
            return Ok(AcquireOutcome::RetryImmediately);
        }
        let (source_w, source_h) = (desc.Width, desc.Height);
        let crop = match self.copy_mode {
            CopyMode::Full => CropRect {
                x: 0,
                y: 0,
                width: source_w,
                height: source_h,
            },
            CopyMode::Region(crop) => crop,
        };
        // Validate before skipping pointer-only updates. A mode change must not
        // turn an invalid fixed region into an endless stream of old frames.
        crate::capture_geometry::validate_fixed_capture_region(
            source_w,
            source_h,
            crop.x,
            crop.y,
            crop.width,
            crop.height,
        )?;
        // Pointer-only updates can reuse the last frame once geometry is known
        // to remain valid. Always take a seed frame when capture starts.
        if info.LastPresentTime == 0 && info.AccumulatedFrames == 0 && self.has_emitted() {
            return Ok(AcquireOutcome::RetryImmediately);
        }
        let copy =
            d3d11::create_bgra_texture(&self.device, crop.width, crop.height).map_err(dev)?;
        d3d11::copy_texture_region(
            &self.context,
            &copy,
            &source,
            crop.x,
            crop.y,
            crop.width,
            crop.height,
        );
        let ticks = self.timestamp(info);
        self.needs_seed = false;
        Ok(AcquireOutcome::Frame(Frame {
            pts_s: self.clock.pts_s(ticks),
            data: FrameData::Gpu(copy),
        }))
    }

    /// Whether this duplication has emitted its seed. Reopening clears this
    /// state independently of the monotonic timestamp floor.
    fn has_emitted(&self) -> bool {
        !self.needs_seed
    }

    /// `LastPresentTime` is a raw QPC counter; convert to the shared 100 ns
    /// timebase. A zero value is only reached here for the seed frame (later
    /// no-content frames are skipped in `copy_frame`), so stamp it "now".
    /// Monotonicity is enforced at the source.
    fn timestamp(&mut self, info: &DXGI_OUTDUPL_FRAME_INFO) -> i64 {
        let ticks = if info.LastPresentTime > 0 {
            qpc_to_ticks_100ns(info.LastPresentTime, self.qpc_freq)
        } else {
            crate::windows::qpc_now_ticks_100ns().unwrap_or(self.last_ticks_100ns.max(0))
        };
        let ticks = ticks.max(self.last_ticks_100ns);
        self.last_ticks_100ns = ticks;
        ticks
    }

    /// Re-run `DuplicateOutput` on the cached output after access loss. A
    /// failure here (e.g. the secure desktop is still up) leaves no duplication;
    /// the caller retries on a later tick once the OS permits it.
    fn recreate_duplication(&mut self) {
        self.release_held_frame();
        self.needs_seed = true;
        // DXGI requires releasing the invalidated duplication BEFORE requesting
        // its replacement after access loss.
        // A failed reopen leaves None so later attempts never reuse stale state.
        let reopened = crate::reopen_resource::reopen_resource(&mut self.dupl, || {
            // SAFETY: same live output and device used at initialization; the
            // helper has already released the previous duplication interface.
            unsafe { self.output.DuplicateOutput(&self.device) }
        });
        if let Err(error) = reopened {
            if let Some(suppressed_since_last) = self.reopen_diagnostics.observe(Instant::now()) {
                emit_diagnostic(CaptureDiagnostic::DxgiReopenFailed {
                    hresult: error.code().0,
                    suppressed_since_last,
                });
            }
        }
    }

    fn release_held_frame(&mut self) {
        if self.frame_held {
            // SAFETY: a frame was acquired and not yet released.
            if let Some(dupl) = &self.dupl {
                let _ = unsafe { dupl.ReleaseFrame() };
            }
            self.frame_held = false;
        }
    }
}

impl CaptureEngine for DxgiDuplicationCapture {
    fn next_frame(&mut self) -> Result<Option<Frame>, CaptureError> {
        self.next_frame_timeout(DEFAULT_FRAME_TIMEOUT)
    }
}

impl Drop for DxgiDuplicationCapture {
    fn drop(&mut self) {
        self.release_held_frame();
    }
}

/// Enumerate the capture device's own adapter outputs and match the target
/// monitor. If the monitor is not among them, the encoder/capture device lives
/// on a different GPU (multi-GPU/hybrid) and `DuplicateOutput` would fail — we
/// catch it here and return `Init` so the app can fall back to WGC.
fn find_output_for_monitor(
    device: &ID3D11Device,
    monitor: HMONITOR,
) -> Result<IDXGIOutput1, CaptureError> {
    let init = CaptureError::Init;
    // SAFETY: every D3D11 device implements IDXGIDevice.
    let dxgi_device: IDXGIDevice = device
        .cast()
        .map_err(|e| init(format!("not a DXGI device: {e}")))?;
    // SAFETY: dxgi_device is live.
    let adapter: IDXGIAdapter =
        unsafe { dxgi_device.GetAdapter() }.map_err(|e| init(format!("GetAdapter: {e}")))?;

    let mut index = 0u32;
    loop {
        // SAFETY: adapter is live; NOT_FOUND signals past the last output.
        let output: IDXGIOutput = match unsafe { adapter.EnumOutputs(index) } {
            Ok(output) => output,
            Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => {
                return Err(init(
                    "target monitor is not on the capture device's GPU (multi-GPU/hybrid)".into(),
                ));
            }
            Err(e) => return Err(init(format!("EnumOutputs: {e}"))),
        };
        // SAFETY: output is live.
        let desc: DXGI_OUTPUT_DESC =
            unsafe { output.GetDesc() }.map_err(|e| init(format!("GetDesc: {e}")))?;
        if desc.Monitor == monitor && desc.AttachedToDesktop.as_bool() {
            if !is_identity_rotation(desc.Rotation) {
                // AcquireNextFrame hands back an un-rotated surface with the
                // image rotated within it; v1 doesn't re-rotate, so fall back.
                return Err(init(
                    "rotated displays are not supported by the Desktop Duplication backend".into(),
                ));
            }
            // SAFETY: IDXGIOutput1 is available on Windows 8+.
            return output
                .cast::<IDXGIOutput1>()
                .map_err(|e| init(format!("IDXGIOutput1 unavailable: {e}")));
        }
        index += 1;
    }
}

fn duplicate_output(
    output: &IDXGIOutput1,
    device: &ID3D11Device,
) -> Result<IDXGIOutputDuplication, CaptureError> {
    // SAFETY: output + device are live; DuplicateOutput takes Param<IUnknown>,
    // satisfied by &ID3D11Device.
    unsafe { output.DuplicateOutput(device) }.map_err(|e| {
        let detail = match e.code() {
            DXGI_ERROR_UNSUPPORTED => "unsupported on this output (adapter mismatch?)",
            DXGI_ERROR_NOT_CURRENTLY_AVAILABLE => "unavailable (too many active duplications)",
            E_ACCESSDENIED => "access denied",
            _ => "failed",
        };
        CaptureError::Init(format!("DuplicateOutput {detail}: {e}"))
    })
}

fn is_identity_rotation(rotation: DXGI_MODE_ROTATION) -> bool {
    rotation == DXGI_MODE_ROTATION_IDENTITY || rotation == DXGI_MODE_ROTATION_UNSPECIFIED
}

fn is_access_lost(code: HRESULT) -> bool {
    code == DXGI_ERROR_ACCESS_LOST
        || code == DXGI_ERROR_ACCESS_DENIED
        || code == DXGI_ERROR_SESSION_DISCONNECTED
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::windows::display::display_handle_by_id;
    use std::time::Duration;

    /// An invalid monitor handle is rejected before any device work, so this
    /// runs anywhere (WARP device, headless CI).
    #[test]
    fn invalid_monitor_handle_is_rejected() {
        let (device, _ctx) = crate::windows::d3d11::create_device_for_tests().expect("device");
        let clock = RelativeClock::new(0);
        let result = DxgiDuplicationCapture::for_monitor_on(device, HMONITOR::default(), clock);
        assert!(
            matches!(result, Err(CaptureError::Init(_))),
            "invalid handle must fail with Init"
        );
    }

    /// Real duplication against the primary monitor. Self-skips on CI and when
    /// init fails (no hardware outputs / WARP) — the skip-if-absent pattern the
    /// WGC device tests use.
    #[test]
    fn captures_monotonic_gpu_frames_from_primary_monitor() {
        if std::env::var_os("CI").is_some() {
            eprintln!("SKIP: duplication device test needs a real interactive desktop");
            return;
        }
        let (device, _ctx) = match crate::windows::d3d11::create_device() {
            Ok(pair) => pair,
            Err(e) => {
                eprintln!("SKIP: no hardware device: {e}");
                return;
            }
        };
        let clock = RelativeClock::new(crate::windows::qpc_now_ticks_100ns().expect("qpc"));
        let mut cap = match DxgiDuplicationCapture::primary_monitor_on(device.clone(), clock) {
            Ok(cap) => cap,
            Err(e) => {
                eprintln!("SKIP: duplication unavailable: {e}");
                return;
            }
        };
        let mut last_pts = -1.0;
        for _ in 0..3 {
            let frame = match cap.next_frame_timeout(Duration::from_secs(5)) {
                Ok(Some(frame)) => frame,
                Ok(None) => panic!("duplication should not end"),
                Err(CaptureError::Timeout(_)) => {
                    eprintln!("SKIP: idle desktop produced no frame");
                    return;
                }
                Err(e) => panic!("unexpected error: {e}"),
            };
            assert!(frame.pts_s >= last_pts, "pts must be monotonic");
            assert!(frame.pts_s < 60.0, "pts is relative to capture start");
            last_pts = frame.pts_s;
            let FrameData::Gpu(tex) = &frame.data else {
                panic!("duplication frames are GPU textures");
            };
            // The texture must live on the device we provided.
            // SAFETY: trivial getter on a valid texture.
            let owner = unsafe { tex.GetDevice() }.expect("owner device");
            assert_eq!(owner.as_raw(), device.as_raw());
            let (w, h) = crate::windows::d3d11::texture_size(tex);
            assert!(w > 0 && h > 0);
        }
    }

    /// A region capture emits exactly the requested crop size.
    #[test]
    fn region_capture_crops_to_requested_size() {
        if std::env::var_os("CI").is_some() {
            eprintln!("SKIP: duplication device test needs a real interactive desktop");
            return;
        }
        let (device, _ctx) = match crate::windows::d3d11::create_device() {
            Ok(pair) => pair,
            Err(e) => {
                eprintln!("SKIP: no hardware device: {e}");
                return;
            }
        };
        let display = match display_handle_by_id(None) {
            Ok(display) => display,
            Err(e) => {
                eprintln!("SKIP: no display: {e}");
                return;
            }
        };
        let crop = CropRect {
            x: 0,
            y: 0,
            width: 100,
            height: 80,
        };
        let clock = RelativeClock::new(crate::windows::qpc_now_ticks_100ns().expect("qpc"));
        let mut cap = match DxgiDuplicationCapture::for_monitor_region_on(
            device,
            display.handle,
            clock,
            crop,
        ) {
            Ok(cap) => cap,
            Err(e) => {
                eprintln!("SKIP: duplication unavailable: {e}");
                return;
            }
        };
        match cap.next_frame_timeout(Duration::from_secs(5)) {
            Ok(Some(frame)) => {
                let FrameData::Gpu(tex) = &frame.data else {
                    panic!("gpu frame");
                };
                assert_eq!(crate::windows::d3d11::texture_size(tex), (100, 80));
            }
            Ok(None) => panic!("duplication should not end"),
            Err(CaptureError::Timeout(_)) => eprintln!("SKIP: idle desktop produced no frame"),
            Err(e) => panic!("unexpected error: {e}"),
        }
    }
}
