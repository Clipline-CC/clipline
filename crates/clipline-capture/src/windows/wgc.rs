//! Windows.Graphics.Capture engine (ddoc §3/§8): DWM-level capture, no
//! injection. Free-threaded frame pool; the FrameArrived handler copies
//! each frame's texture (pool surfaces are recycled) and queues it; the
//! pull-model `next_frame` drains the queue.

use std::collections::VecDeque;
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use windows::core::{Interface, Result as WinResult};
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{
    Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::{E_FAIL, HWND, RPC_E_CHANGED_MODE};
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};

use crate::clock::RelativeClock;
use crate::traits::{CaptureEngine, CaptureError, Frame, FrameData};
use crate::windows::d3d11;
use crate::windows::nv12::CropRect;
use crate::windows::window::{window_client_crop_state, WindowClientCrop};

/// Default `next_frame` wait. WGC only delivers on screen updates, so an
/// idle desktop can legitimately go quiet; recorders that need cadence
/// repeat the previous frame (encoder-side concern, milestone 4).
const DEFAULT_FRAME_TIMEOUT: Duration = Duration::from_secs(5);
const FRAME_QUEUE_CAPACITY: usize = 1;

#[derive(Clone)]
enum FrameCopyMode {
    Full,
    StaticRegion(CropRect),
    WindowClient {
        hwnd: isize,
        cache: Arc<Mutex<Option<ClientCropCache>>>,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct FrameGeometry {
    content_width: i32,
    content_height: i32,
    source_width: u32,
    source_height: u32,
}

#[derive(Clone, Copy)]
struct ClientCropCache {
    geometry: FrameGeometry,
    crop: CropRect,
}

struct FramePoolState {
    size: SizeInt32,
}

struct QueuedFrame {
    texture: ID3D11Texture2D,
    ticks_100ns: i64,
}

pub struct WgcCapture {
    item: GraphicsCaptureItem,
    session: GraphicsCaptureSession,
    frame_pool: Direct3D11CaptureFramePool,
    frame_arrived_token: i64,
    closed_token: i64,
    queue: FrameSender,
    rx: FrameReceiver,
    clock: RelativeClock,
}

impl WgcCapture {
    /// Capture the primary monitor on a freshly created device and clock.
    pub fn primary_monitor() -> Result<Self, CaptureError> {
        let (device, _) = d3d11::create_device().map_err(|e| CaptureError::Init(e.to_string()))?;
        Self::primary_monitor_on(device, Self::new_clock()?)
    }

    /// A capture clock anchored at "now" — create one and share it across
    /// every engine of a recording (ddoc §6: one QPC timebase).
    pub fn new_clock() -> Result<RelativeClock, CaptureError> {
        let origin =
            crate::windows::qpc_now_ticks_100ns().map_err(|e| CaptureError::Init(e.to_string()))?;
        Ok(RelativeClock::new(origin))
    }

    /// Capture the primary monitor on a caller-provided device and clock
    /// (one device must be shared with the encoder — textures don't cross
    /// devices; one clock must be shared with audio — ddoc §6).
    pub fn primary_monitor_on(
        device: ID3D11Device,
        clock: RelativeClock,
    ) -> Result<Self, CaptureError> {
        let display = crate::windows::display::display_handle_by_id(None)?;
        Self::for_monitor_on(device, display.handle, clock)
    }

    /// Capture a specific monitor on a caller-provided device and clock.
    pub fn for_monitor_on(
        device: ID3D11Device,
        monitor: HMONITOR,
        clock: RelativeClock,
    ) -> Result<Self, CaptureError> {
        if monitor.is_invalid() {
            return Err(CaptureError::Init("invalid monitor handle".into()));
        }
        let item = create_item(|interop| unsafe { interop.CreateForMonitor(monitor) })?;
        Self::new(item, device, clock, FrameCopyMode::Full)
    }

    pub fn for_monitor_region_on(
        device: ID3D11Device,
        monitor: HMONITOR,
        clock: RelativeClock,
        crop: CropRect,
    ) -> Result<Self, CaptureError> {
        if monitor.is_invalid() {
            return Err(CaptureError::Init("invalid monitor handle".into()));
        }
        let item = create_item(|interop| unsafe { interop.CreateForMonitor(monitor) })?;
        Self::new(item, device, clock, FrameCopyMode::StaticRegion(crop))
    }

    /// Capture one window (must be visible; ddoc §3: per-window preferred,
    /// borderless fullscreen recommended for games).
    pub fn for_window(hwnd: HWND) -> Result<Self, CaptureError> {
        let (device, _) = d3d11::create_device().map_err(|e| CaptureError::Init(e.to_string()))?;
        Self::for_window_on(device, hwnd, Self::new_clock()?)
    }

    /// Window capture on a caller-provided device and clock.
    pub fn for_window_on(
        device: ID3D11Device,
        hwnd: HWND,
        clock: RelativeClock,
    ) -> Result<Self, CaptureError> {
        if hwnd.is_invalid() {
            return Err(CaptureError::Init("invalid window handle".into()));
        }
        let item = create_item(|interop| unsafe { interop.CreateForWindow(hwnd) })?;
        Self::new(item, device, clock, FrameCopyMode::Full)
    }

    /// Window capture that returns only the current client area. This keeps
    /// windowed games from encoding the title bar and tracks resize geometry.
    pub fn for_window_client_on(
        device: ID3D11Device,
        hwnd: HWND,
        clock: RelativeClock,
    ) -> Result<Self, CaptureError> {
        if hwnd.is_invalid() {
            return Err(CaptureError::Init("invalid window handle".into()));
        }
        let item = create_item(|interop| unsafe { interop.CreateForWindow(hwnd) })?;
        Self::new(
            item,
            device,
            clock,
            FrameCopyMode::WindowClient {
                hwnd: hwnd.0 as isize,
                cache: Arc::new(Mutex::new(None)),
            },
        )
    }

    fn new(
        item: GraphicsCaptureItem,
        device: ID3D11Device,
        clock: RelativeClock,
        copy_mode: FrameCopyMode,
    ) -> Result<Self, CaptureError> {
        init_winrt()?;
        let init = |e: windows::core::Error| CaptureError::Init(e.to_string());

        d3d11::ensure_multithread_protected(&device).map_err(init)?;
        // SAFETY: trivial getter on a valid device.
        let context = unsafe { device.GetImmediateContext() }.map_err(init)?;
        let winrt_device = winrt_device(&device).map_err(init)?;

        let size = item.Size().map_err(init)?;
        let frame_pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &winrt_device,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            2,
            size,
        )
        .map_err(init)?;
        let session = frame_pool.CreateCaptureSession(&item).map_err(init)?;
        // Best-effort: needs Win10 20348+/Win11 (ddoc Caveats) — older
        // builds show the yellow border.
        let _ = session.SetIsBorderRequired(false);

        let (tx, rx) = bounded_frame_channel(FRAME_QUEUE_CAPACITY);
        let state = Arc::new(Mutex::new(FramePoolState { size }));
        let frame_arrived_token = frame_pool
            .FrameArrived(&TypedEventHandler::new(on_frame_arrived(
                device,
                context,
                state,
                tx.clone(),
                copy_mode,
            )))
            .map_err(init)?;
        let closed_queue = tx.clone();
        let closed_token = item
            .Closed(&TypedEventHandler::new(move |_, _| {
                closed_queue.close();
                Ok(())
            }))
            .map_err(init)?;
        session.StartCapture().map_err(init)?;

        Ok(Self {
            item,
            session,
            frame_pool,
            frame_arrived_token,
            closed_token,
            queue: tx,
            rx,
            clock,
        })
    }

    /// The capture-start clock — share it with audio sources so all pts
    /// live on one timeline (ddoc §6).
    pub fn clock(&self) -> RelativeClock {
        self.clock
    }

    /// `next_frame` with an explicit wait bound.
    pub fn next_frame_timeout(&mut self, timeout: Duration) -> Result<Option<Frame>, CaptureError> {
        match self.rx.recv_timeout(timeout) {
            Ok(q) => Ok(Some(Frame {
                pts_s: self.clock.pts_s(q.ticks_100ns),
                data: FrameData::Gpu(q.texture),
            })),
            Err(RecvTimeoutError::Disconnected) => Ok(None), // session closed
            Err(RecvTimeoutError::Timeout) => Err(CaptureError::Timeout(timeout)),
        }
    }
}

impl CaptureEngine for WgcCapture {
    fn next_frame(&mut self) -> Result<Option<Frame>, CaptureError> {
        self.next_frame_timeout(DEFAULT_FRAME_TIMEOUT)
    }
}

impl Drop for WgcCapture {
    fn drop(&mut self) {
        self.queue.close();
        let _ = self.frame_pool.RemoveFrameArrived(self.frame_arrived_token);
        let _ = self.item.RemoveClosed(self.closed_token);
        let _ = self.session.Close();
        let _ = self.frame_pool.Close();
    }
}

/// FrameArrived handler: copy the (recycled) pool surface into a fresh
/// texture and queue it. Only this handler thread touches the immediate
/// context, respecting D3D11's single-threaded context rule.
fn on_frame_arrived(
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    state: Arc<Mutex<FramePoolState>>,
    tx: FrameSender,
    copy_mode: FrameCopyMode,
) -> impl Fn(
    windows::core::Ref<'_, Direct3D11CaptureFramePool>,
    windows::core::Ref<'_, windows::core::IInspectable>,
) -> WinResult<()> {
    move |pool, _| {
        let Some(pool) = pool.as_ref() else {
            return Ok(());
        };
        let Ok(frame) = pool.TryGetNextFrame() else {
            return Ok(());
        };
        let ticks_100ns = frame.SystemRelativeTime()?.Duration;
        let content_size = frame.ContentSize()?;
        if !size_has_area(content_size) {
            recreate_frame_pool_if_needed(pool, &device, &state, content_size)?;
            return Ok(());
        }
        let access: IDirect3DDxgiInterfaceAccess = frame.Surface()?.cast()?;
        // SAFETY: the surface is a live IDirect3D surface backed by an
        // ID3D11Texture2D; GetInterface AddRefs it.
        let source: ID3D11Texture2D = unsafe { access.GetInterface()? };
        let (source_w, source_h) = d3d11::texture_size(&source);
        if content_size_exceeds_source(content_size, source_w, source_h) {
            recreate_frame_pool_if_needed(pool, &device, &state, content_size)?;
            return Ok(());
        }
        let Some(crop) = copy_rect_for_frame(&copy_mode, content_size, source_w, source_h) else {
            recreate_frame_pool_if_needed(pool, &device, &state, content_size)?;
            return Ok(());
        };
        let copy = d3d11::create_bgra_texture(&device, crop.width, crop.height)?;
        d3d11::copy_texture_region(
            &context,
            &copy,
            &source,
            crop.x,
            crop.y,
            crop.width,
            crop.height,
        );
        recreate_frame_pool_if_needed(pool, &device, &state, content_size)?;
        tx.send_drop_oldest(QueuedFrame {
            texture: copy,
            ticks_100ns,
        });
        Ok(())
    }
}

fn recreate_frame_pool_if_needed(
    pool: &Direct3D11CaptureFramePool,
    device: &ID3D11Device,
    state: &Arc<Mutex<FramePoolState>>,
    size: SizeInt32,
) -> WinResult<()> {
    let mut state = state
        .lock()
        .map_err(|_| windows::core::Error::new(E_FAIL, "frame pool state lock poisoned"))?;
    if state.size == size || !size_has_area(size) {
        return Ok(());
    }
    let winrt_device = winrt_device(device)?;
    pool.Recreate(
        &winrt_device,
        DirectXPixelFormat::B8G8R8A8UIntNormalized,
        2,
        size,
    )?;
    state.size = size;
    Ok(())
}

fn copy_rect_for_frame(
    mode: &FrameCopyMode,
    content_size: SizeInt32,
    source_w: u32,
    source_h: u32,
) -> Option<CropRect> {
    match mode {
        FrameCopyMode::Full => content_crop(content_size, source_w, source_h),
        FrameCopyMode::StaticRegion(crop) => crop.in_frame(source_w, source_h),
        FrameCopyMode::WindowClient { hwnd, cache } => {
            let geometry = FrameGeometry {
                content_width: content_size.Width,
                content_height: content_size.Height,
                source_width: source_w,
                source_height: source_h,
            };
            if let Ok(guard) = cache.lock() {
                if let Some(cached) = *guard {
                    if cached.geometry == geometry {
                        return Some(cached.crop);
                    }
                }
            }

            let hwnd = HWND(*hwnd as *mut core::ffi::c_void);
            let crop = match window_client_crop_state(hwnd)? {
                WindowClientCrop::Client(crop) => crop.in_frame(source_w, source_h)?,
                WindowClientCrop::FullFrame => content_crop(content_size, source_w, source_h)?,
            };
            if let Ok(mut guard) = cache.lock() {
                *guard = Some(ClientCropCache { geometry, crop });
            }
            Some(crop)
        }
    }
}

fn content_crop(size: SizeInt32, source_w: u32, source_h: u32) -> Option<CropRect> {
    let width = u32::try_from(size.Width).ok()?;
    let height = u32::try_from(size.Height).ok()?;
    CropRect {
        x: 0,
        y: 0,
        width,
        height,
    }
    .in_frame(source_w, source_h)
}

fn content_size_exceeds_source(size: SizeInt32, source_w: u32, source_h: u32) -> bool {
    let Ok(width) = u32::try_from(size.Width) else {
        return false;
    };
    let Ok(height) = u32::try_from(size.Height) else {
        return false;
    };
    width > source_w || height > source_h
}

fn size_has_area(size: SizeInt32) -> bool {
    size.Width >= 2 && size.Height >= 2
}

#[derive(Clone)]
struct FrameSender {
    inner: Arc<FrameQueueInner>,
}

struct FrameReceiver {
    inner: Arc<FrameQueueInner>,
}

struct FrameQueueInner {
    state: Mutex<FrameQueueState>,
    ready: Condvar,
    capacity: usize,
}

struct FrameQueueState {
    queue: VecDeque<QueuedFrame>,
    closed: bool,
}

fn bounded_frame_channel(capacity: usize) -> (FrameSender, FrameReceiver) {
    let inner = Arc::new(FrameQueueInner {
        state: Mutex::new(FrameQueueState {
            queue: VecDeque::new(),
            closed: false,
        }),
        ready: Condvar::new(),
        capacity: capacity.max(1),
    });
    (
        FrameSender {
            inner: inner.clone(),
        },
        FrameReceiver { inner },
    )
}

impl FrameSender {
    fn send_drop_oldest(&self, frame: QueuedFrame) {
        let Ok(mut state) = self.inner.state.lock() else {
            return;
        };
        if state.closed {
            return;
        }
        if state.queue.len() >= self.inner.capacity {
            state.queue.pop_front();
        }
        state.queue.push_back(frame);
        self.inner.ready.notify_one();
    }

    fn close(&self) {
        let Ok(mut state) = self.inner.state.lock() else {
            self.inner.ready.notify_all();
            return;
        };
        state.closed = true;
        state.queue.clear();
        self.inner.ready.notify_all();
    }
}

impl Drop for FrameSender {
    fn drop(&mut self) {
        self.inner.ready.notify_all();
    }
}

impl FrameReceiver {
    fn recv_timeout(&self, timeout: Duration) -> Result<QueuedFrame, RecvTimeoutError> {
        let deadline = Instant::now() + timeout;
        let mut state = self
            .inner
            .state
            .lock()
            .map_err(|_| RecvTimeoutError::Disconnected)?;
        loop {
            if state.closed {
                return Err(RecvTimeoutError::Disconnected);
            }
            if let Some(frame) = state.queue.pop_front() {
                return Ok(frame);
            }
            if Arc::strong_count(&self.inner) == 1 {
                return Err(RecvTimeoutError::Disconnected);
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(RecvTimeoutError::Timeout);
            }
            let remaining = deadline.saturating_duration_since(now);
            let (next_state, result) = self
                .inner
                .ready
                .wait_timeout(state, remaining)
                .map_err(|_| RecvTimeoutError::Disconnected)?;
            state = next_state;
            if result.timed_out() && state.queue.is_empty() {
                return if state.closed || Arc::strong_count(&self.inner) == 1 {
                    Err(RecvTimeoutError::Disconnected)
                } else {
                    Err(RecvTimeoutError::Timeout)
                };
            }
        }
    }
}

fn create_item(
    create: impl FnOnce(&IGraphicsCaptureItemInterop) -> WinResult<GraphicsCaptureItem>,
) -> Result<GraphicsCaptureItem, CaptureError> {
    init_winrt()?;
    match GraphicsCaptureSession::IsSupported() {
        Ok(true) => {}
        Ok(false) => {
            return Err(CaptureError::Init(
                "WGC not supported in this session".into(),
            ))
        }
        Err(e) => return Err(CaptureError::Init(format!("WGC support query: {e}"))),
    }
    let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
        .map_err(|e| CaptureError::Init(format!("WGC interop factory: {e}")))?;
    create(&interop).map_err(|e| CaptureError::Init(format!("create capture item: {e}")))
}

/// Wrap the DXGI device for WinRT consumption.
fn winrt_device(device: &ID3D11Device) -> WinResult<IDirect3DDevice> {
    let dxgi: IDXGIDevice = device.cast()?;
    // SAFETY: dxgi is a valid device; the call returns an IInspectable we cast.
    let inspectable = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi)? };
    inspectable.cast()
}

/// Idempotent WinRT init; an already-initialized thread (RPC_E_CHANGED_MODE
/// under an STA host) is fine — WGC's free-threaded pool works either way.
fn init_winrt() -> Result<(), CaptureError> {
    // SAFETY: RoInitialize is safe to call repeatedly per thread.
    match unsafe { RoInitialize(RO_INIT_MULTITHREADED) } {
        Ok(()) => Ok(()),
        Err(e) if e.code() == RPC_E_CHANGED_MODE => Ok(()),
        Err(e) => Err(CaptureError::Init(format!("RoInitialize: {e}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::FrameData;
    use std::time::Duration;

    /// Real WGC against the primary monitor. Self-skips on CI and when
    /// capture init fails — the skip-if-absent pattern the ffprobe e2e
    /// tests use. The CI gate is unconditional: Windows Server runners
    /// report `IsSupported() == true` and expose a virtual display, then
    /// access-violate inside the capture component (observed on
    /// windows-2025); WGC verification is manual on a real desktop
    /// (see the handoff/plan).
    #[test]
    fn captures_monotonic_gpu_frames_from_primary_monitor() {
        if std::env::var_os("CI").is_some() {
            eprintln!("SKIP: WGC device test needs a real interactive desktop");
            return;
        }
        let mut cap = match WgcCapture::primary_monitor() {
            Ok(cap) => cap,
            Err(e) => {
                eprintln!("SKIP: WGC unavailable: {e}");
                return;
            }
        };
        let mut last_pts = -1.0;
        for _ in 0..3 {
            let frame = cap
                .next_frame_timeout(Duration::from_secs(5))
                .expect("frame within 5s on a live desktop")
                .expect("session still open");
            assert!(frame.pts_s >= last_pts, "pts must be monotonic");
            assert!(frame.pts_s < 60.0, "pts is relative to capture start");
            last_pts = frame.pts_s;
            let FrameData::Gpu(tex) = &frame.data else {
                panic!("WGC frames are GPU textures");
            };
            let (w, h) = crate::windows::d3d11::texture_size(tex);
            assert!(w > 0 && h > 0);
        }
    }

    #[test]
    fn capture_runs_on_a_caller_provided_device() {
        if std::env::var_os("CI").is_some() {
            eprintln!("SKIP: WGC device test needs a real interactive desktop");
            return;
        }
        let (device, _ctx) = crate::windows::d3d11::create_device().expect("device");
        let multithread: windows::Win32::Graphics::Direct3D10::ID3D10Multithread =
            device.cast().expect("ID3D10Multithread");
        let _ = unsafe { multithread.SetMultithreadProtected(false) };
        assert!(!unsafe { multithread.GetMultithreadProtected() }.as_bool());
        let clock = WgcCapture::new_clock().expect("clock");
        let mut cap = match WgcCapture::primary_monitor_on(device.clone(), clock) {
            Ok(cap) => cap,
            Err(e) => {
                eprintln!("SKIP: WGC unavailable: {e}");
                return;
            }
        };
        assert!(unsafe { multithread.GetMultithreadProtected() }.as_bool());
        let frame = cap
            .next_frame_timeout(Duration::from_secs(5))
            .expect("frame")
            .expect("session open");
        // The provided clock is the frame timebase: pts is near zero.
        assert!(
            frame.pts_s >= 0.0 && frame.pts_s < 5.0,
            "pts {}",
            frame.pts_s
        );
        let FrameData::Gpu(tex) = &frame.data else {
            panic!("gpu frame")
        };
        // The texture must live on the device we provided.
        use windows::core::Interface;
        // SAFETY: trivial getter on a valid texture.
        let owner = unsafe { tex.GetDevice() }.expect("owner device");
        assert_eq!(owner.as_raw(), device.as_raw());
    }

    #[test]
    fn bounded_frame_queue_drops_oldest_frame() {
        let (device, _) = crate::windows::d3d11::create_device_for_tests().expect("device");
        let tex = crate::windows::d3d11::create_bgra_texture(&device, 2, 2).expect("texture");
        let (tx, rx) = bounded_frame_channel(2);
        for ticks_100ns in [1, 2, 3] {
            tx.send_drop_oldest(QueuedFrame {
                texture: tex.clone(),
                ticks_100ns,
            });
        }

        assert_eq!(
            rx.recv_timeout(Duration::from_millis(1))
                .expect("second frame")
                .ticks_100ns,
            2
        );
        assert_eq!(
            rx.recv_timeout(Duration::from_millis(1))
                .expect("third frame")
                .ticks_100ns,
            3
        );
        assert!(matches!(
            rx.recv_timeout(Duration::from_millis(1)),
            Err(RecvTimeoutError::Timeout)
        ));
    }

    #[test]
    fn production_frame_queue_keeps_only_the_latest_frame() {
        let (device, _) = crate::windows::d3d11::create_device_for_tests().expect("device");
        let texture = crate::windows::d3d11::create_bgra_texture(&device, 2, 2).expect("texture");
        let (tx, rx) = bounded_frame_channel(FRAME_QUEUE_CAPACITY);
        for ticks_100ns in [1, 2] {
            tx.send_drop_oldest(QueuedFrame {
                texture: texture.clone(),
                ticks_100ns,
            });
        }

        assert_eq!(
            rx.recv_timeout(Duration::from_millis(1))
                .expect("latest frame")
                .ticks_100ns,
            2
        );
        assert!(matches!(
            rx.recv_timeout(Duration::from_millis(1)),
            Err(RecvTimeoutError::Timeout)
        ));
    }

    #[test]
    fn bounded_frame_queue_disconnects_when_sender_drops() {
        let (tx, rx) = bounded_frame_channel(2);
        drop(tx);

        assert!(matches!(
            rx.recv_timeout(Duration::from_millis(50)),
            Err(RecvTimeoutError::Disconnected)
        ));
    }

    #[test]
    fn explicit_target_close_disconnects_with_callback_senders_still_alive() {
        let (device, _) = crate::windows::d3d11::create_device_for_tests().expect("device");
        let texture = crate::windows::d3d11::create_bgra_texture(&device, 2, 2).expect("texture");
        let (tx, rx) = bounded_frame_channel(2);
        let callback_sender = tx.clone();
        tx.send_drop_oldest(QueuedFrame {
            texture: texture.clone(),
            ticks_100ns: 1,
        });

        tx.close();
        callback_sender.send_drop_oldest(QueuedFrame {
            texture,
            ticks_100ns: 2,
        });

        assert!(matches!(
            rx.recv_timeout(Duration::from_millis(50)),
            Err(RecvTimeoutError::Disconnected)
        ));
        drop(callback_sender);
    }

    #[test]
    fn explicit_target_close_wakes_a_blocked_receiver() {
        let (tx, rx) = bounded_frame_channel(2);
        let waiter = std::thread::spawn(move || rx.recv_timeout(Duration::from_secs(5)));
        std::thread::sleep(Duration::from_millis(10));

        tx.close();

        assert!(matches!(
            waiter.join().expect("receiver thread"),
            Err(RecvTimeoutError::Disconnected)
        ));
    }

    /// The milestone 4 exit test: video + audio on ONE clock, recorded by
    /// the real pipeline, validated by the tolerance-based avsync checks —
    /// the mock-pinned GOP discipline reproduced on real hardware.
    #[test]
    fn real_engines_on_one_clock_produce_a_synced_timeline() {
        if std::env::var_os("CI").is_some() {
            eprintln!("SKIP: device test needs desktop + audio endpoint");
            return;
        }
        let (device, _ctx) = crate::windows::d3d11::create_device().expect("device");
        let clock = WgcCapture::new_clock().expect("clock");
        let cap = match WgcCapture::primary_monitor_on(device.clone(), clock) {
            Ok(cap) => cap,
            Err(e) => {
                eprintln!("SKIP: WGC unavailable: {e}");
                return;
            }
        };
        let audio = match crate::windows::WasapiLoopback::start(clock) {
            Ok(a) => a,
            Err(e) => {
                eprintln!("SKIP: loopback unavailable: {e}");
                return;
            }
        };
        let cfg = crate::windows::MftConfig {
            width: 640,
            height: 360,
            fps: 60,
            bitrate_bps: 2_000_000,
            encoder_backend: None,
        };
        // Pull one frame to learn the capture size, then hand the engine on.
        let mut cap = cap;
        let first = cap
            .next_frame_timeout(std::time::Duration::from_secs(5))
            .expect("frame")
            .expect("open");
        let crate::traits::FrameData::Gpu(tex) = &first.data else {
            panic!("gpu")
        };
        let (in_w, in_h) = crate::windows::d3d11::texture_size(tex);
        let enc = match crate::windows::MftH264Encoder::new(&device, in_w, in_h, cfg) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("SKIP: no hardware encoder: {e}");
                return;
            }
        };
        let mut rec = crate::pipeline::Recorder::new(
            crate::mock::LimitedCapture::new(cap, 60),
            enc,
            usize::MAX,
        )
        .with_audio(Box::new(audio));
        rec.run_to_end().expect("record");
        let segs: Vec<&clipline_buffer::Segment> = rec.ring().unwrap().segments().collect();
        let report =
            crate::avsync::validate_timeline(&segs, &crate::avsync::SyncTolerances::default())
                .expect("real-clock timeline within tolerances");
        eprintln!("sync report: {report:?}");
        assert!(
            report.video_duration_s > 0.5,
            "recorded something substantial"
        );
    }
}
