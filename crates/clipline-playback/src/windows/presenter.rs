use std::cell::Cell;
use std::ffi::c_void;
use std::marker::PhantomData;
use std::num::NonZeroIsize;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use thiserror::Error;
use windows::Win32::Foundation::{DXGI_STATUS_OCCLUDED, E_NOINTERFACE, HWND, RECT, S_OK};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11RenderTargetView, ID3D11Texture2D, ID3D11VideoContext,
    ID3D11VideoContext1, ID3D11VideoDevice, ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator,
    ID3D11VideoProcessorOutputView, D3D11_BIND_DECODER, D3D11_BIND_RENDER_TARGET,
    D3D11_BIND_SHADER_RESOURCE, D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_COLOR_SPACE,
    D3D11_VIDEO_PROCESSOR_CONTENT_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_STREAM,
    D3D11_VIDEO_USAGE_PLAYBACK_NORMAL, D3D11_VPIV_DIMENSION_TEXTURE2D,
    D3D11_VPOV_DIMENSION_TEXTURE2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_IGNORE, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
    DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12,
    DXGI_FORMAT_UNKNOWN, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    IDXGIDevice, IDXGIFactory2, IDXGISwapChain1, DXGI_ERROR_DEVICE_HUNG, DXGI_ERROR_DEVICE_REMOVED,
    DXGI_ERROR_DEVICE_RESET, DXGI_ERROR_DRIVER_INTERNAL_ERROR, DXGI_ERROR_WAS_STILL_DRAWING,
    DXGI_MWA_NO_ALT_ENTER, DXGI_MWA_NO_WINDOW_CHANGES, DXGI_PRESENT_DO_NOT_WAIT,
    DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FLAG,
    DXGI_SWAP_EFFECT_FLIP_DISCARD, DXGI_USAGE_RENDER_TARGET_OUTPUT,
};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, GetClientRect, GetWindowThreadProcessId, IsWindow,
    SetWindowPos, ShowWindow, SWP_NOACTIVATE, SWP_NOZORDER, SWP_SHOWWINDOW, SW_HIDE, WS_CHILD,
    WS_CLIPCHILDREN, WS_CLIPSIBLINGS, WS_EX_NOACTIVATE,
};
use windows_core::{w, Interface};

use crate::{
    BackendComponent, BackendError, BackendErrorKind, DecodedVideoFrame, FramePublisher,
    PhysicalVideoRect, PipelineToken, PresentationError, PresentationLifecycle, PresentationState,
    PresentationUpdate, PublicationReceipt, RecoveryDisposition,
};

use super::d3d11::{adapter_luid, ensure_multithread_protected};
use super::mft_decode::D3D11VideoSurface;

pub const PRESENTATION_SWAP_CHAIN_BUFFERS: u32 = 2;
pub const MAX_PRESENTATION_INPUT_SURFACES: usize = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresentOutcome {
    Presented,
    Occluded,
    Backpressured,
    DeviceLost,
    Failed,
}

pub fn classify_present_result(native_code: i32) -> PresentOutcome {
    match native_code {
        code if code == S_OK.0 => PresentOutcome::Presented,
        code if code == DXGI_STATUS_OCCLUDED.0 => PresentOutcome::Occluded,
        code if code == DXGI_ERROR_WAS_STILL_DRAWING.0 => PresentOutcome::Backpressured,
        code if [
            DXGI_ERROR_DEVICE_REMOVED.0,
            DXGI_ERROR_DEVICE_RESET.0,
            DXGI_ERROR_DEVICE_HUNG.0,
            DXGI_ERROR_DRIVER_INTERNAL_ERROR.0,
        ]
        .contains(&code) =>
        {
            PresentOutcome::DeviceLost
        }
        _ => PresentOutcome::Failed,
    }
}

#[derive(Debug, Clone, PartialEq, Error)]
pub enum VideoHostError {
    #[error("parent HWND must be non-zero")]
    NullParent,
    #[error("parent HWND is not a live window")]
    InvalidParent,
    #[error("parent HWND belongs to thread {expected}, not the current thread {actual}")]
    ParentThreadMismatch { expected: u32, actual: u32 },
    #[error("video-stage bounds exceed the Win32 coordinate range")]
    InvalidBounds,
    #[error("video host may only be accessed from its creating thread")]
    WrongThread,
    #[error("video host is closed")]
    Closed,
    #[error("video host already has an attached publisher target")]
    TargetAlreadyAttached,
    #[error("video publisher target must be dropped before the host closes")]
    PublisherStillAttached,
    #[error("video target state lock is poisoned")]
    StatePoisoned,
    #[error("video presentation state failed: {0}")]
    Presentation(#[from] PresentationError),
    #[error("Win32 video host operation failed with HRESULT 0x{native_code:08X}: {message}")]
    Native { native_code: u32, message: String },
}

#[derive(Debug)]
struct VideoTargetState {
    snapshot: Mutex<VideoTargetSnapshot>,
    publisher_attached: AtomicBool,
}

#[derive(Debug, Clone, Copy)]
struct VideoTargetSnapshot {
    revision: u64,
    presentable: bool,
    alive: bool,
}

/// Move-only playback-thread lease for the child window. Dropping it permits
/// the UI-thread host to destroy the HWND.
#[derive(Debug)]
pub struct WindowsVideoTarget {
    raw_hwnd: NonZeroIsize,
    state: Arc<VideoTargetState>,
    _not_sync: PhantomData<Cell<()>>,
}

impl Drop for WindowsVideoTarget {
    fn drop(&mut self) {
        self.state
            .publisher_attached
            .store(false, Ordering::Release);
    }
}

impl WindowsVideoTarget {
    pub const fn raw_handle(&self) -> NonZeroIsize {
        self.raw_hwnd
    }

    pub fn latest_revision(&self) -> u64 {
        self.snapshot().revision
    }

    pub fn is_presentable(&self) -> bool {
        let snapshot = self.snapshot();
        snapshot.alive && snapshot.presentable
    }

    pub fn is_alive(&self) -> bool {
        self.snapshot().alive
    }

    fn snapshot(&self) -> VideoTargetSnapshot {
        self.state
            .snapshot
            .lock()
            .map(|snapshot| *snapshot)
            .unwrap_or(VideoTargetSnapshot {
                revision: u64::MAX,
                presentable: false,
                alive: false,
            })
    }
}

/// UI-thread owner of Clipline's non-activating video child window.
///
/// The `Rc` marker intentionally makes this type neither `Send` nor `Sync`.
pub struct WindowsVideoHost {
    hwnd: Option<HWND>,
    owner_thread: u32,
    lifecycle: PresentationLifecycle,
    target_state: Arc<VideoTargetState>,
    _thread_affinity: PhantomData<Rc<()>>,
}

impl std::fmt::Debug for WindowsVideoHost {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WindowsVideoHost")
            .field("raw_handle", &self.raw_handle())
            .field("owner_thread", &self.owner_thread)
            .field("revision", &self.lifecycle.latest_revision())
            .field("presentable", &self.lifecycle.is_presentable())
            .finish()
    }
}

impl WindowsVideoHost {
    pub fn attach(parent_raw_hwnd: isize) -> Result<Self, VideoHostError> {
        let parent_raw = NonZeroIsize::new(parent_raw_hwnd).ok_or(VideoHostError::NullParent)?;
        let parent = hwnd(parent_raw);
        // SAFETY: `IsWindow` only validates the opaque value and does not
        // dereference caller memory.
        if !unsafe { IsWindow(Some(parent)) }.as_bool() {
            return Err(VideoHostError::InvalidParent);
        }
        // SAFETY: the live parent was validated above and the process-id
        // output is deliberately omitted.
        let expected = unsafe { GetWindowThreadProcessId(parent, None) };
        // SAFETY: this pure query has no preconditions.
        let actual = unsafe { GetCurrentThreadId() };
        if expected != actual {
            return Err(VideoHostError::ParentThreadMismatch { expected, actual });
        }

        // SAFETY: the built-in STATIC class is process-independent, the live
        // parent was validated above, and all optional pointer parameters are
        // null. The child starts hidden until valid geometry is committed.
        let child = unsafe {
            CreateWindowExW(
                WS_EX_NOACTIVATE,
                w!("STATIC"),
                w!(""),
                WS_CHILD | WS_CLIPSIBLINGS | WS_CLIPCHILDREN,
                0,
                0,
                1,
                1,
                Some(parent),
                None,
                None,
                None,
            )
        }
        .map_err(|error| native_error(error, "create video child window"))?;
        NonZeroIsize::new(child.0 as isize).ok_or_else(|| VideoHostError::Native {
            native_code: 0,
            message: "CreateWindowExW returned a null video child".into(),
        })?;
        let target_state = Arc::new(VideoTargetState {
            snapshot: Mutex::new(VideoTargetSnapshot {
                revision: 0,
                presentable: false,
                alive: true,
            }),
            publisher_attached: AtomicBool::new(false),
        });
        Ok(Self {
            hwnd: Some(child),
            // SAFETY: this pure query has no preconditions.
            owner_thread: unsafe { GetCurrentThreadId() },
            lifecycle: PresentationLifecycle::new(),
            target_state,
            _thread_affinity: PhantomData,
        })
    }

    pub fn take_target(&mut self) -> Result<WindowsVideoTarget, VideoHostError> {
        let hwnd = self.hwnd.ok_or(VideoHostError::Closed)?;
        self.target_state
            .publisher_attached
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| VideoHostError::TargetAlreadyAttached)?;
        let raw_hwnd = NonZeroIsize::new(hwnd.0 as isize).ok_or(VideoHostError::Closed)?;
        Ok(WindowsVideoTarget {
            raw_hwnd,
            state: Arc::clone(&self.target_state),
            _not_sync: PhantomData,
        })
    }

    pub fn update(
        &mut self,
        bounds: PhysicalVideoRect,
        state: PresentationState,
    ) -> Result<PresentationUpdate, VideoHostError> {
        self.require_owner_thread()?;
        let hwnd = self.hwnd.ok_or(VideoHostError::Closed)?;
        validate_video_bounds(bounds)?;

        let mut candidate = self.lifecycle.clone();
        let update = candidate.update(bounds, state)?;
        if matches!(update, PresentationUpdate::Unchanged { .. }) {
            return Ok(update);
        }

        let revision = match update {
            PresentationUpdate::Changed { revision, .. } => revision,
            PresentationUpdate::Unchanged { .. } => unreachable!(),
        };
        {
            let mut snapshot = self
                .target_state
                .snapshot
                .lock()
                .map_err(|_| VideoHostError::StatePoisoned)?;
            snapshot.revision = revision;
            snapshot.presentable = false;
        }

        if state == PresentationState::Visible && bounds.has_area() {
            // SAFETY: `hwnd` is owned by this host and dimensions were checked
            // against the Win32 signed coordinate range above.
            unsafe {
                SetWindowPos(
                    hwnd,
                    None,
                    bounds.x,
                    bounds.y,
                    bounds.width as i32,
                    bounds.height as i32,
                    SWP_NOACTIVATE | SWP_NOZORDER | SWP_SHOWWINDOW,
                )
            }
            .map_err(|error| native_error(error, "position video child window"))?;
        } else {
            // SAFETY: hiding an owned live window has no pointer preconditions.
            let _ = unsafe { ShowWindow(hwnd, SW_HIDE) };
        }

        self.lifecycle = candidate;
        let mut snapshot = self
            .target_state
            .snapshot
            .lock()
            .map_err(|_| VideoHostError::StatePoisoned)?;
        snapshot.presentable = self.lifecycle.is_presentable();
        Ok(update)
    }

    pub fn latest_revision(&self) -> u64 {
        self.lifecycle.latest_revision()
    }

    pub fn is_presentable(&self) -> bool {
        self.lifecycle.is_presentable()
    }

    pub fn raw_handle(&self) -> Option<NonZeroIsize> {
        self.hwnd
            .and_then(|hwnd| NonZeroIsize::new(hwnd.0 as isize))
    }

    pub fn close(&mut self) -> Result<(), VideoHostError> {
        self.require_owner_thread()?;
        let Some(hwnd) = self.hwnd.take() else {
            return Ok(());
        };
        if self.target_state.publisher_attached.load(Ordering::Acquire) {
            self.hwnd = Some(hwnd);
            return Err(VideoHostError::PublisherStillAttached);
        }
        let mut snapshot = self
            .target_state
            .snapshot
            .lock()
            .map_err(|_| VideoHostError::StatePoisoned)?;
        snapshot.presentable = false;
        snapshot.alive = false;
        drop(snapshot);
        // SAFETY: `hwnd` is the live child uniquely owned by this host.
        unsafe { DestroyWindow(hwnd) }
            .map_err(|error| native_error(error, "destroy video child window"))
    }

    fn require_owner_thread(&self) -> Result<(), VideoHostError> {
        // SAFETY: this pure query has no preconditions.
        if unsafe { GetCurrentThreadId() } != self.owner_thread {
            return Err(VideoHostError::WrongThread);
        }
        Ok(())
    }
}

impl Drop for WindowsVideoHost {
    fn drop(&mut self) {
        if self.target_state.publisher_attached.load(Ordering::Acquire) {
            if let Ok(mut snapshot) = self.target_state.snapshot.lock() {
                snapshot.presentable = false;
                snapshot.alive = false;
            }
            if let Some(hwnd) = self.hwnd.take() {
                // SAFETY: hiding the still-parent-owned child prevents further
                // display while the move-only publisher target drains. The OS
                // destroys it with its parent; destroying it here would race
                // the playback thread and permit HWND-reuse ABA.
                let _ = unsafe { ShowWindow(hwnd, SW_HIDE) };
            }
            return;
        }
        let _ = self.close();
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct D3D11PublisherTelemetry {
    pub swap_chain_creations: u64,
    pub swap_chain_resizes: u64,
    pub processor_reconfigurations: u64,
    pub presented_frames: u64,
    pub backpressured_frames: u64,
    pub occluded_frames: u64,
    pub device_losses: u64,
    pub input_compatibility_copies: u64,
    pub adapter_luid: Option<u64>,
    pub latest_geometry_revision: Option<u64>,
}

/// Playback-thread owner of the bounded D3D11 video-processor and swap chain.
pub struct WindowsD3D11Publisher {
    // Drop the GPU pipeline before releasing the target lease. Field order is
    // intentional and `close` makes the sequence explicit.
    pipeline: Option<D3DPresentationPipeline>,
    target: Option<WindowsVideoTarget>,
    active_token: Option<PipelineToken>,
    last_target_revision: Option<u64>,
    telemetry: D3D11PublisherTelemetry,
}

impl std::fmt::Debug for WindowsD3D11Publisher {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WindowsD3D11Publisher")
            .field("active_token", &self.active_token)
            .field("telemetry", &self.telemetry)
            .finish_non_exhaustive()
    }
}

impl WindowsD3D11Publisher {
    pub const fn new(target: WindowsVideoTarget) -> Self {
        Self {
            pipeline: None,
            target: Some(target),
            active_token: None,
            last_target_revision: None,
            telemetry: D3D11PublisherTelemetry {
                swap_chain_creations: 0,
                swap_chain_resizes: 0,
                processor_reconfigurations: 0,
                presented_frames: 0,
                backpressured_frames: 0,
                occluded_frames: 0,
                device_losses: 0,
                input_compatibility_copies: 0,
                adapter_luid: None,
                latest_geometry_revision: None,
            },
        }
    }

    pub const fn telemetry(&self) -> D3D11PublisherTelemetry {
        self.telemetry
    }

    pub fn close(&mut self) {
        self.pipeline = None;
        self.active_token = None;
        self.last_target_revision = None;
        self.target = None;
    }

    fn ensure_pipeline(
        &mut self,
        texture: &ID3D11Texture2D,
        hwnd: HWND,
        output_width: u32,
        output_height: u32,
        geometry_changed: bool,
    ) -> Result<&mut D3DPresentationPipeline, BackendError> {
        let mut texture_desc =
            windows::Win32::Graphics::Direct3D11::D3D11_TEXTURE2D_DESC::default();
        // SAFETY: `texture` is live for the duration of this publication.
        unsafe { texture.GetDesc(&mut texture_desc) };
        if texture_desc.Format != DXGI_FORMAT_NV12
            || texture_desc.Width == 0
            || texture_desc.Height == 0
            || texture_desc.Width > i32::MAX as u32
            || texture_desc.Height > i32::MAX as u32
        {
            return Err(publication_error(
                BackendErrorKind::CorruptInput,
                RecoveryDisposition::RetryPipeline,
                None,
                "frame publisher requires a bounded non-zero NV12 texture",
            ));
        }
        // SAFETY: a live D3D11 child always returns its owning device.
        let device = unsafe { texture.GetDevice() }
            .map_err(|error| publisher_windows_error(error, "query frame D3D11 device"))?;
        let device_identity = device.as_raw() as usize;

        if self.pipeline.is_none() {
            let pipeline = D3DPresentationPipeline::new(
                device,
                hwnd,
                texture_desc.Width,
                texture_desc.Height,
                output_width,
                output_height,
            )?;
            self.telemetry.swap_chain_creations =
                self.telemetry.swap_chain_creations.saturating_add(1);
            self.telemetry.processor_reconfigurations =
                self.telemetry.processor_reconfigurations.saturating_add(1);
            self.telemetry.adapter_luid = pipeline.adapter_luid;
            self.pipeline = Some(pipeline);
        }

        let pipeline = self.pipeline.as_mut().expect("pipeline is installed");
        if pipeline.device_identity != device_identity {
            return Err(publication_error(
                BackendErrorKind::DeviceLost,
                RecoveryDisposition::RecreateComponent,
                None,
                "decoded frame arrived from a different D3D11 device",
            ));
        }
        let source_changed = pipeline.source_width != texture_desc.Width
            || pipeline.source_height != texture_desc.Height;
        let output_changed =
            pipeline.output_width != output_width || pipeline.output_height != output_height;
        if output_changed && !geometry_changed {
            return Err(publication_error(
                BackendErrorKind::StaleWork,
                RecoveryDisposition::RetryPipeline,
                None,
                "video child size changed without a newer geometry revision",
            ));
        }
        if source_changed || output_changed {
            pipeline.reconfigure(
                texture_desc.Width,
                texture_desc.Height,
                output_width,
                output_height,
            )?;
            self.telemetry.processor_reconfigurations =
                self.telemetry.processor_reconfigurations.saturating_add(1);
            if output_changed {
                self.telemetry.swap_chain_resizes =
                    self.telemetry.swap_chain_resizes.saturating_add(1);
            }
        }
        Ok(pipeline)
    }

    fn blit_frame(
        &mut self,
        texture: &ID3D11Texture2D,
        hwnd: HWND,
        output_width: u32,
        output_height: u32,
        geometry_changed: bool,
    ) -> Result<(i32, u64), BackendError> {
        let pipeline =
            self.ensure_pipeline(texture, hwnd, output_width, output_height, geometry_changed)?;
        let before = pipeline.input_compatibility_copies;
        let present_code = pipeline.blit_and_present(texture)?;
        Ok((
            present_code,
            pipeline.input_compatibility_copies.saturating_sub(before),
        ))
    }
}

impl FramePublisher<D3D11VideoSurface> for WindowsD3D11Publisher {
    fn publish(
        &mut self,
        frame: DecodedVideoFrame<D3D11VideoSurface>,
    ) -> Result<PublicationReceipt, BackendError> {
        if let Some(expected) = self.active_token {
            if frame.token() != expected {
                return Err(publication_error(
                    BackendErrorKind::StaleWork,
                    RecoveryDisposition::RetryPipeline,
                    None,
                    format!(
                        "stale presentation token: expected {expected:?}, got {:?}",
                        frame.token()
                    ),
                ));
            }
        } else {
            self.active_token = Some(frame.token());
        }

        let target = self.target.as_ref().ok_or_else(|| {
            publication_error(
                BackendErrorKind::Unavailable,
                RecoveryDisposition::Fatal,
                None,
                "video publisher is closed",
            )
        })?;
        let raw_hwnd = target.raw_hwnd;
        let target_state = Arc::clone(&target.state);
        let snapshot = target_state.snapshot.lock().map_err(|_| {
            publication_error(
                BackendErrorKind::PublicationFailure,
                RecoveryDisposition::RecreateComponent,
                None,
                "video target state lock is poisoned",
            )
        })?;
        if !snapshot.alive {
            return Err(publication_error(
                BackendErrorKind::Unavailable,
                RecoveryDisposition::Fatal,
                None,
                "video host closed before its publisher",
            ));
        }
        if !snapshot.presentable {
            self.telemetry.occluded_frames = self.telemetry.occluded_frames.saturating_add(1);
            return Ok(PublicationReceipt::Occluded);
        }
        let geometry_changed =
            geometry_revision_changed(self.last_target_revision, snapshot.revision)?;

        let hwnd = hwnd(raw_hwnd);
        let (output_width, output_height) = child_client_size(hwnd)?;
        if output_width == 0 || output_height == 0 {
            self.telemetry.occluded_frames = self.telemetry.occluded_frames.saturating_add(1);
            return Ok(PublicationReceipt::Occluded);
        }

        let (present_code, compatibility_copies) = match self.blit_frame(
            frame.surface().texture(),
            hwnd,
            output_width,
            output_height,
            geometry_changed,
        ) {
            Ok(result) => result,
            Err(error) => {
                if error.kind == BackendErrorKind::DeviceLost {
                    self.telemetry.device_losses = self.telemetry.device_losses.saturating_add(1);
                }
                if error.recovery == RecoveryDisposition::RecreateComponent {
                    self.pipeline = None;
                }
                return Err(error);
            }
        };
        self.telemetry.input_compatibility_copies = self
            .telemetry
            .input_compatibility_copies
            .saturating_add(compatibility_copies);
        self.last_target_revision = Some(snapshot.revision);
        self.telemetry.latest_geometry_revision = Some(snapshot.revision);
        match classify_present_result(present_code) {
            PresentOutcome::Presented => {
                self.telemetry.presented_frames = self.telemetry.presented_frames.saturating_add(1);
                Ok(PublicationReceipt::Presented)
            }
            PresentOutcome::Backpressured => {
                self.telemetry.backpressured_frames =
                    self.telemetry.backpressured_frames.saturating_add(1);
                Ok(PublicationReceipt::Backpressured)
            }
            PresentOutcome::Occluded => {
                self.telemetry.occluded_frames = self.telemetry.occluded_frames.saturating_add(1);
                Ok(PublicationReceipt::Occluded)
            }
            PresentOutcome::DeviceLost => {
                self.telemetry.device_losses = self.telemetry.device_losses.saturating_add(1);
                self.pipeline = None;
                Err(publication_error(
                    BackendErrorKind::DeviceLost,
                    RecoveryDisposition::RecreateComponent,
                    Some(i64::from(present_code)),
                    "D3D11 presentation device was removed or reset",
                ))
            }
            PresentOutcome::Failed => Err(publication_error(
                BackendErrorKind::PublicationFailure,
                RecoveryDisposition::RecreateComponent,
                Some(i64::from(present_code)),
                "DXGI Present failed",
            )),
        }
    }

    fn clear(&mut self, token: PipelineToken) -> Result<(), BackendError> {
        self.active_token = Some(token);
        Ok(())
    }
}

impl Drop for WindowsD3D11Publisher {
    fn drop(&mut self) {
        self.close();
    }
}

struct D3DPresentationPipeline {
    device: ID3D11Device,
    device_identity: usize,
    context: ID3D11DeviceContext,
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    swap_chain: IDXGISwapChain1,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    output_view: Option<ID3D11VideoProcessorOutputView>,
    render_target: Option<ID3D11RenderTargetView>,
    compatibility_input: Option<ID3D11Texture2D>,
    input_compatibility_copies: u64,
    source_width: u32,
    source_height: u32,
    output_width: u32,
    output_height: u32,
    adapter_luid: Option<u64>,
}

impl D3DPresentationPipeline {
    fn new(
        device: ID3D11Device,
        hwnd: HWND,
        source_width: u32,
        source_height: u32,
        output_width: u32,
        output_height: u32,
    ) -> Result<Self, BackendError> {
        ensure_multithread_protected(&device)
            .map_err(|error| publisher_windows_error(error, "protect D3D11 presentation device"))?;
        let context = unsafe { device.GetImmediateContext() }
            .map_err(|error| publisher_windows_error(error, "query D3D11 immediate context"))?;
        let video_device: ID3D11VideoDevice = device.cast().map_err(|error| {
            publisher_windows_error(error, "query D3D11 video-processor device")
        })?;
        let video_context: ID3D11VideoContext = context.cast().map_err(|error| {
            publisher_windows_error(error, "query D3D11 video-processor context")
        })?;
        let dxgi_device: IDXGIDevice = device
            .cast()
            .map_err(|error| publisher_windows_error(error, "query DXGI presentation device"))?;
        // SAFETY: these are owned interface queries from the live device.
        let adapter = unsafe { dxgi_device.GetAdapter() }
            .map_err(|error| publisher_windows_error(error, "query presentation adapter"))?;
        let factory: IDXGIFactory2 = unsafe { adapter.GetParent() }
            .map_err(|error| publisher_windows_error(error, "query DXGI factory"))?;
        let swap_desc = swap_chain_desc(output_width, output_height)?;
        // SAFETY: descriptor and child HWND are live and no fullscreen or
        // output restriction is requested.
        let swap_chain =
            unsafe { factory.CreateSwapChainForHwnd(&device, hwnd, &swap_desc, None, None) }
                .map_err(|error| {
                    publisher_windows_error(error, "create two-buffer video swap chain")
                })?;
        // SAFETY: associates this child with the factory while leaving all
        // window ownership and transitions to Slint's UI thread.
        unsafe {
            factory.MakeWindowAssociation(hwnd, DXGI_MWA_NO_ALT_ENTER | DXGI_MWA_NO_WINDOW_CHANGES)
        }
        .map_err(|error| publisher_windows_error(error, "configure video swap chain"))?;
        let (enumerator, processor) = create_video_processor(
            &video_device,
            source_width,
            source_height,
            output_width,
            output_height,
        )?;
        configure_video_processor_color_spaces(&video_context, &processor);
        let (output_view, render_target) =
            create_back_buffer_views(&device, &video_device, &enumerator, &swap_chain)?;
        let device_identity = device.as_raw() as usize;
        let adapter_luid = adapter_luid(&device).ok();
        Ok(Self {
            device,
            device_identity,
            context,
            video_device,
            video_context,
            swap_chain,
            enumerator,
            processor,
            output_view: Some(output_view),
            render_target: Some(render_target),
            compatibility_input: None,
            input_compatibility_copies: 0,
            source_width,
            source_height,
            output_width,
            output_height,
            adapter_luid,
        })
    }

    fn reconfigure(
        &mut self,
        source_width: u32,
        source_height: u32,
        output_width: u32,
        output_height: u32,
    ) -> Result<(), BackendError> {
        if output_width == 0 || output_height == 0 {
            return Err(publication_error(
                BackendErrorKind::PublicationFailure,
                RecoveryDisposition::RetryPipeline,
                None,
                "swap-chain resize dimensions must be non-zero",
            ));
        }
        let output_changed =
            self.output_width != output_width || self.output_height != output_height;
        let source_changed =
            self.source_width != source_width || self.source_height != source_height;
        self.output_view = None;
        self.render_target = None;
        if source_changed {
            self.compatibility_input = None;
        }
        if output_changed {
            // SAFETY: all references to the old back buffer were released
            // above and the swap chain remains windowed with two buffers.
            unsafe {
                self.swap_chain.ResizeBuffers(
                    PRESENTATION_SWAP_CHAIN_BUFFERS,
                    output_width,
                    output_height,
                    DXGI_FORMAT_UNKNOWN,
                    DXGI_SWAP_CHAIN_FLAG(0),
                )
            }
            .map_err(|error| publisher_windows_error(error, "resize video swap chain"))?;
        }
        let (enumerator, processor) = create_video_processor(
            &self.video_device,
            source_width,
            source_height,
            output_width,
            output_height,
        )?;
        configure_video_processor_color_spaces(&self.video_context, &processor);
        let (output_view, render_target) = create_back_buffer_views(
            &self.device,
            &self.video_device,
            &enumerator,
            &self.swap_chain,
        )?;
        self.enumerator = enumerator;
        self.processor = processor;
        self.output_view = Some(output_view);
        self.render_target = Some(render_target);
        self.source_width = source_width;
        self.source_height = source_height;
        self.output_width = output_width;
        self.output_height = output_height;
        Ok(())
    }

    fn blit_and_present(&mut self, texture: &ID3D11Texture2D) -> Result<i32, BackendError> {
        let input_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV {
                    MipSlice: 0,
                    ArraySlice: 0,
                },
            },
        };
        let mut input_view = None;
        // SAFETY: texture and enumerator are from the exact same D3D11 device
        // and the descriptor selects its sole array slice.
        let direct_view = unsafe {
            self.video_device.CreateVideoProcessorInputView(
                texture,
                &self.enumerator,
                &input_desc,
                Some(&mut input_view),
            )
        };
        if let Err(direct_error) = direct_view {
            if self.compatibility_input.is_none() {
                self.compatibility_input = Some(create_video_input_texture(
                    &self.device,
                    self.source_width,
                    self.source_height,
                )?);
            }
            let compatibility = self
                .compatibility_input
                .as_ref()
                .expect("compatibility texture was just installed");
            // SAFETY: both exact-size NV12 resources belong to the same
            // multithread-protected device. The copy is ordered before Blt.
            unsafe { self.context.CopyResource(compatibility, texture) };
            // SAFETY: status query on the live device surfaces deferred copy
            // failures before the compatibility frame is presented.
            unsafe { self.device.GetDeviceRemovedReason() }.map_err(|error| {
                publisher_windows_error(error, "copy NV12 into bounded video-input texture")
            })?;
            self.input_compatibility_copies = self.input_compatibility_copies.saturating_add(1);
            input_view = None;
            // SAFETY: the compatibility texture carries an explicit video
            // input bind and selects its only array slice.
            unsafe {
                self.video_device.CreateVideoProcessorInputView(
                    compatibility,
                    &self.enumerator,
                    &input_desc,
                    Some(&mut input_view),
                )
            }
            .map_err(|fallback_error| {
                publisher_windows_error(
                    fallback_error,
                    &format!(
                        "create bounded NV12 processor input view after direct view failed ({direct_error})"
                    ),
                )
            })?;
        }

        let source = RECT {
            left: 0,
            top: 0,
            right: self.source_width as i32,
            bottom: self.source_height as i32,
        };
        let output = PhysicalVideoRect::new(0, 0, self.output_width, self.output_height);
        let destination = crate::fit_aspect_ratio(output, self.source_width, self.source_height)
            .map_err(|error| {
                publication_error(
                    BackendErrorKind::PublicationFailure,
                    RecoveryDisposition::RetryPipeline,
                    None,
                    error.to_string(),
                )
            })?;
        let destination = RECT {
            left: destination.x,
            top: destination.y,
            right: destination.right(),
            bottom: destination.bottom(),
        };
        let output_target = RECT {
            left: 0,
            top: 0,
            right: self.output_width as i32,
            bottom: self.output_height as i32,
        };
        // SAFETY: processor and rectangles belong to this live pipeline.
        unsafe {
            self.video_context.VideoProcessorSetStreamSourceRect(
                &self.processor,
                0,
                true,
                Some(&source),
            );
            self.video_context.VideoProcessorSetStreamDestRect(
                &self.processor,
                0,
                true,
                Some(&destination),
            );
            self.video_context.VideoProcessorSetOutputTargetRect(
                &self.processor,
                true,
                Some(&output_target),
            );
            self.video_context
                .VideoProcessorSetStreamAutoProcessingMode(&self.processor, 0, false);
            self.context.ClearRenderTargetView(
                self.render_target
                    .as_ref()
                    .expect("configured pipeline retains its render target"),
                &[0.0, 0.0, 0.0, 1.0],
            );
        }

        let stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            pInputSurface: std::mem::ManuallyDrop::new(input_view),
            ..Default::default()
        };
        // SAFETY: one enabled stream and both views belong to the configured
        // processor. No future/past surfaces are retained.
        let blit = unsafe {
            self.video_context.VideoProcessorBlt(
                &self.processor,
                self.output_view
                    .as_ref()
                    .expect("configured pipeline retains its output view"),
                0,
                std::slice::from_ref(&stream),
            )
        };
        // The generated stream uses ManuallyDrop for its COM reference.
        drop(std::mem::ManuallyDrop::into_inner(stream.pInputSurface));
        blit.map_err(|error| publisher_windows_error(error, "convert NV12 into swap-chain BGRA"))?;
        // SAFETY: non-blocking flip-model presentation on the live swap chain.
        Ok(unsafe { self.swap_chain.Present(0, DXGI_PRESENT_DO_NOT_WAIT) }.0)
    }
}

fn swap_chain_desc(width: u32, height: u32) -> Result<DXGI_SWAP_CHAIN_DESC1, BackendError> {
    if width == 0 || height == 0 || width > i32::MAX as u32 || height > i32::MAX as u32 {
        return Err(publication_error(
            BackendErrorKind::PublicationFailure,
            RecoveryDisposition::RetryPipeline,
            None,
            "swap-chain dimensions exceed the bounded Win32 range",
        ));
    }
    Ok(DXGI_SWAP_CHAIN_DESC1 {
        Width: width,
        Height: height,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        Stereo: false.into(),
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
        BufferCount: PRESENTATION_SWAP_CHAIN_BUFFERS,
        Scaling: DXGI_SCALING_STRETCH,
        SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
        AlphaMode: DXGI_ALPHA_MODE_IGNORE,
        Flags: 0,
    })
}

fn create_video_input_texture(
    device: &ID3D11Device,
    width: u32,
    height: u32,
) -> Result<ID3D11Texture2D, BackendError> {
    let create = |bind_flags: u32| {
        let descriptor = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: bind_flags,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut texture = None;
        // SAFETY: descriptor and output slot are valid for a bounded default
        // NV12 texture on the presentation device.
        unsafe { device.CreateTexture2D(&descriptor, None, Some(&mut texture)) }
            .map(|()| texture.expect("texture out-parameter is set on success"))
    };

    match create((D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32) {
        Ok(texture) => Ok(texture),
        Err(render_bind_error) => create(D3D11_BIND_DECODER.0 as u32).map_err(|decoder_bind_error| {
            publisher_windows_error(
                decoder_bind_error,
                &format!(
                    "create one bounded decoder-bound NV12 input after render-target bind failed ({render_bind_error})"
                ),
            )
        }),
    }
}

fn create_video_processor(
    video_device: &ID3D11VideoDevice,
    source_width: u32,
    source_height: u32,
    output_width: u32,
    output_height: u32,
) -> Result<(ID3D11VideoProcessorEnumerator, ID3D11VideoProcessor), BackendError> {
    let descriptor = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
        InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
        InputFrameRate: DXGI_RATIONAL {
            Numerator: 60,
            Denominator: 1,
        },
        InputWidth: source_width,
        InputHeight: source_height,
        OutputFrameRate: DXGI_RATIONAL {
            Numerator: 60,
            Denominator: 1,
        },
        OutputWidth: output_width,
        OutputHeight: output_height,
        Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
    };
    // SAFETY: descriptor is fully initialized and dimensions are bounded by
    // the caller's texture and client-rectangle validation.
    let enumerator = unsafe { video_device.CreateVideoProcessorEnumerator(&descriptor) }
        .map_err(|error| publisher_windows_error(error, "create video-processor enumerator"))?;
    // SAFETY: capability index zero is the baseline rate-conversion processor.
    let processor = unsafe { video_device.CreateVideoProcessor(&enumerator, 0) }
        .map_err(|error| publisher_windows_error(error, "create video processor"))?;
    Ok((enumerator, processor))
}

fn create_back_buffer_views(
    device: &ID3D11Device,
    video_device: &ID3D11VideoDevice,
    enumerator: &ID3D11VideoProcessorEnumerator,
    swap_chain: &IDXGISwapChain1,
) -> Result<(ID3D11VideoProcessorOutputView, ID3D11RenderTargetView), BackendError> {
    // SAFETY: buffer zero is a live BGRA texture in the two-buffer swap chain.
    let back_buffer: ID3D11Texture2D = unsafe { swap_chain.GetBuffer(0) }
        .map_err(|error| publisher_windows_error(error, "query video swap-chain back buffer"))?;
    let output_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
        ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
        Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
            Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
        },
    };
    let mut output_view = None;
    // SAFETY: the back buffer and enumerator belong to the same device.
    unsafe {
        video_device.CreateVideoProcessorOutputView(
            &back_buffer,
            enumerator,
            &output_desc,
            Some(&mut output_view),
        )
    }
    .map_err(|error| publisher_windows_error(error, "create BGRA processor output view"))?;
    let mut render_target = None;
    // SAFETY: a BGRA swap-chain buffer supports the default render-target view.
    unsafe { device.CreateRenderTargetView(&back_buffer, None, Some(&mut render_target)) }
        .map_err(|error| publisher_windows_error(error, "create letterbox render target"))?;
    Ok((
        output_view.ok_or_else(|| {
            publication_error(
                BackendErrorKind::PublicationFailure,
                RecoveryDisposition::RecreateComponent,
                None,
                "D3D11 returned no processor output view",
            )
        })?,
        render_target.ok_or_else(|| {
            publication_error(
                BackendErrorKind::PublicationFailure,
                RecoveryDisposition::RecreateComponent,
                None,
                "D3D11 returned no render-target view",
            )
        })?,
    ))
}

fn configure_video_processor_color_spaces(
    video_context: &ID3D11VideoContext,
    processor: &ID3D11VideoProcessor,
) {
    if let Ok(context1) = video_context.cast::<ID3D11VideoContext1>() {
        // SAFETY: the processor belongs to this context. Clipline-authored NV12
        // is limited-range Rec.709 and the child swap chain is full-range BGRA.
        unsafe {
            context1.VideoProcessorSetStreamColorSpace1(
                processor,
                0,
                DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
            );
            context1.VideoProcessorSetOutputColorSpace1(
                processor,
                DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
            );
        }
        return;
    }

    let ycbcr_limited_709 = D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
        _bitfield: (1 << 2) | (1 << 4),
    };
    let rgb_full_709 = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 2 << 4 };
    // SAFETY: D3D11.0 color-space fallback on this live processor.
    unsafe {
        video_context.VideoProcessorSetStreamColorSpace(processor, 0, &ycbcr_limited_709);
        video_context.VideoProcessorSetOutputColorSpace(processor, &rgb_full_709);
    }
}

fn child_client_size(hwnd: HWND) -> Result<(u32, u32), BackendError> {
    let mut rectangle = RECT::default();
    // SAFETY: the target lease keeps the child HWND alive through publication.
    unsafe { GetClientRect(hwnd, &mut rectangle) }
        .map_err(|error| publisher_windows_error(error, "query video child client rectangle"))?;
    let width = rectangle.right.checked_sub(rectangle.left).ok_or_else(|| {
        publication_error(
            BackendErrorKind::PublicationFailure,
            RecoveryDisposition::RetryPipeline,
            None,
            "video child width overflow",
        )
    })?;
    let height = rectangle.bottom.checked_sub(rectangle.top).ok_or_else(|| {
        publication_error(
            BackendErrorKind::PublicationFailure,
            RecoveryDisposition::RetryPipeline,
            None,
            "video child height overflow",
        )
    })?;
    let width = u32::try_from(width).map_err(|_| {
        publication_error(
            BackendErrorKind::PublicationFailure,
            RecoveryDisposition::RetryPipeline,
            None,
            "video child width is negative",
        )
    })?;
    let height = u32::try_from(height).map_err(|_| {
        publication_error(
            BackendErrorKind::PublicationFailure,
            RecoveryDisposition::RetryPipeline,
            None,
            "video child height is negative",
        )
    })?;
    Ok((width, height))
}

fn publisher_windows_error(error: windows_core::Error, operation: &str) -> BackendError {
    let code = error.code().0;
    let outcome = classify_present_result(code);
    let (kind, recovery) = if code == E_NOINTERFACE.0 {
        (BackendErrorKind::Unavailable, RecoveryDisposition::Fatal)
    } else if outcome == PresentOutcome::DeviceLost {
        (
            BackendErrorKind::DeviceLost,
            RecoveryDisposition::RecreateComponent,
        )
    } else {
        (
            BackendErrorKind::PublicationFailure,
            RecoveryDisposition::RecreateComponent,
        )
    };
    publication_error(
        kind,
        recovery,
        Some(i64::from(code)),
        format!("{operation}: {error}"),
    )
}

fn publication_error(
    kind: BackendErrorKind,
    recovery: RecoveryDisposition,
    native_code: Option<i64>,
    message: impl Into<String>,
) -> BackendError {
    BackendError {
        component: BackendComponent::FramePublisher,
        kind,
        recovery,
        native_code,
        message: message.into(),
    }
}

fn geometry_revision_changed(
    last_revision: Option<u64>,
    current_revision: u64,
) -> Result<bool, BackendError> {
    match last_revision {
        Some(last) if current_revision < last => Err(publication_error(
            BackendErrorKind::StaleWork,
            RecoveryDisposition::RetryPipeline,
            None,
            format!(
                "stale video geometry revision {current_revision}; latest accepted revision is {last}"
            ),
        )),
        Some(last) => Ok(current_revision != last),
        None => Ok(true),
    }
}

pub fn validate_video_bounds(bounds: PhysicalVideoRect) -> Result<(), VideoHostError> {
    if bounds.x < 0
        || bounds.y < 0
        || bounds.width > i32::MAX as u32
        || bounds.height > i32::MAX as u32
        || bounds.x.checked_add(bounds.width as i32).is_none()
        || bounds.y.checked_add(bounds.height as i32).is_none()
    {
        return Err(VideoHostError::InvalidBounds);
    }
    Ok(())
}

fn hwnd(raw: NonZeroIsize) -> HWND {
    HWND(raw.get() as *mut c_void)
}

fn native_error(error: windows_core::Error, operation: &str) -> VideoHostError {
    VideoHostError::Native {
        native_code: error.code().0 as u32,
        message: format!("{operation}: {error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use clipline_mp4::{IndexedMovie, PlaybackTrackConfig};
    use windows::Win32::UI::WindowsAndMessaging::{GetClientRect, SW_SHOWNA, WS_POPUP};

    use crate::windows::{DecoderPreference, WindowsH264Decoder};
    use crate::{
        plan_video_sample_buffers, EncodedVideoPacket, SubmitStatus, TimelineDuration,
        TimelinePosition, VideoDecoder, VideoSampleTransport, WorkGeneration, PLAYBACK_TIMELINE_HZ,
    };

    const DEVICE_TEST_TOKEN: PipelineToken = PipelineToken::new(WorkGeneration::new(1, 0), 0);

    struct TestParent(HWND);

    impl TestParent {
        fn new() -> Self {
            // SAFETY: the built-in STATIC class needs no registration and all
            // optional pointer parameters are null.
            let hwnd = unsafe {
                CreateWindowExW(
                    Default::default(),
                    w!("STATIC"),
                    w!("Clipline presenter test parent"),
                    WS_POPUP,
                    0,
                    0,
                    640,
                    480,
                    None,
                    None,
                    None,
                    None,
                )
            }
            .expect("create hidden test parent");
            Self(hwnd)
        }

        fn raw(&self) -> isize {
            self.0 .0 as isize
        }

        fn show(&self) {
            // SAFETY: this hidden test owns the live top-level window.
            let _ = unsafe { ShowWindow(self.0, SW_SHOWNA) };
        }
    }

    impl Drop for TestParent {
        fn drop(&mut self) {
            // SAFETY: the test owns this hidden top-level window.
            let _ = unsafe { DestroyWindow(self.0) };
        }
    }

    #[test]
    fn child_window_tracks_latest_geometry_and_closes_once() {
        fn assert_send<T: Send>() {}
        assert_send::<WindowsVideoTarget>();
        let parent = TestParent::new();
        let mut host = WindowsVideoHost::attach(parent.raw()).expect("attach child host");
        let target = host.take_target().expect("take playback target");
        let child_raw = target.raw_handle();
        assert_eq!(
            host.take_target().expect_err("target is a single lease"),
            VideoHostError::TargetAlreadyAttached
        );
        assert!(target.is_alive());
        assert!(!target.is_presentable());

        let bounds = PhysicalVideoRect::new(17, 23, 426, 240);
        assert_eq!(
            host.update(bounds, PresentationState::Visible)
                .expect("show child"),
            PresentationUpdate::Changed {
                revision: 1,
                release_pending_frame: false,
            }
        );
        assert_eq!(target.latest_revision(), 1);
        assert!(target.is_presentable());

        let mut client = windows::Win32::Foundation::RECT::default();
        // SAFETY: target still points to the live child owned by `host`.
        unsafe { GetClientRect(hwnd(target.raw_handle()), &mut client) }
            .expect("query child client size");
        assert_eq!(client.right - client.left, 426);
        assert_eq!(client.bottom - client.top, 240);

        assert_eq!(
            host.update(bounds, PresentationState::Visible)
                .expect("repeat geometry"),
            PresentationUpdate::Unchanged { revision: 1 }
        );
        assert_eq!(
            host.update(bounds, PresentationState::Minimized)
                .expect("minimize child"),
            PresentationUpdate::Changed {
                revision: 2,
                release_pending_frame: true,
            }
        );
        assert!(!target.is_presentable());

        assert_eq!(
            host.close().expect_err("publisher must close first"),
            VideoHostError::PublisherStillAttached
        );
        drop(target);
        host.close().expect("close child");
        host.close().expect("close child twice");
        // SAFETY: checking a stale opaque HWND is permitted.
        assert!(!unsafe { IsWindow(Some(hwnd(child_raw))) }.as_bool());
    }

    #[test]
    fn parent_from_another_thread_is_rejected_before_child_creation() {
        let (handle_tx, handle_rx) = std::sync::mpsc::sync_channel(1);
        let (close_tx, close_rx) = std::sync::mpsc::sync_channel(1);
        let owner = std::thread::spawn(move || {
            let parent = TestParent::new();
            handle_tx.send(parent.raw()).expect("publish test HWND");
            close_rx.recv().expect("wait before destroying parent");
        });
        let parent_raw = handle_rx.recv().expect("receive test HWND");
        assert!(matches!(
            WindowsVideoHost::attach(parent_raw),
            Err(VideoHostError::ParentThreadMismatch { .. })
        ));
        close_tx.send(()).expect("release parent thread");
        owner.join().expect("join parent thread");
    }

    #[test]
    fn swap_chain_contract_is_exactly_two_bgra_flip_buffers() {
        let descriptor = swap_chain_desc(640, 360).expect("bounded descriptor");
        assert_eq!(descriptor.BufferCount, PRESENTATION_SWAP_CHAIN_BUFFERS);
        assert_eq!(MAX_PRESENTATION_INPUT_SURFACES, 1);
        assert_eq!(descriptor.Format, DXGI_FORMAT_B8G8R8A8_UNORM);
        assert_eq!(descriptor.SwapEffect, DXGI_SWAP_EFFECT_FLIP_DISCARD);
        assert_eq!(descriptor.SampleDesc.Count, 1);
        assert!(swap_chain_desc(0, 360).is_err());
        assert!(swap_chain_desc(640, 0).is_err());
        let unavailable = publisher_windows_error(
            windows_core::Error::from_hresult(E_NOINTERFACE),
            "query video processor",
        );
        assert_eq!(unavailable.kind, BackendErrorKind::Unavailable);
        assert_eq!(unavailable.recovery, RecoveryDisposition::Fatal);
        assert!(geometry_revision_changed(None, 1).expect("first revision"));
        assert!(geometry_revision_changed(Some(1), 2).expect("newer revision"));
        assert!(!geometry_revision_changed(Some(2), 2).expect("same revision"));
        let stale = geometry_revision_changed(Some(3), 2).expect_err("stale revision");
        assert_eq!(stale.kind, BackendErrorKind::StaleWork);
        assert_eq!(stale.recovery, RecoveryDisposition::RetryPipeline);
    }

    #[test]
    fn decoded_surface_presents_resizes_and_releases_before_host_close() {
        if std::env::var_os("CI").is_some() {
            eprintln!("SKIP: D3D11 presenter device test is disabled under CI");
            return;
        }

        let fixture_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/playback/hybrid-writer-h264-two-opus-5s.mp4");
        let movie = IndexedMovie::open(fixture_path).expect("open production-writer fixture");
        let video_track_index = movie
            .index()
            .tracks
            .iter()
            .position(|track| matches!(track.config, PlaybackTrackConfig::H264 { .. }))
            .expect("fixture H.264 track");
        let track = &movie.index().tracks[video_track_index];
        let sample_count = track.samples.len();
        let timescale = track.timescale;
        let plan = plan_video_sample_buffers(track, Default::default()).expect("H.264 plan");
        let mut transport =
            VideoSampleTransport::new(movie, video_track_index, DEVICE_TEST_TOKEN.work())
                .expect("video transport");
        let mut decoder = match WindowsH264Decoder::new(DecoderPreference::SoftwareOnly) {
            Ok(decoder) => decoder,
            Err(error) if error.kind == BackendErrorKind::Unavailable => {
                eprintln!("SKIP: Windows H.264 decoder unavailable for presenter test: {error}");
                return;
            }
            Err(error) => panic!("create Windows H.264 decoder: {error}"),
        };
        match decoder.configure(&plan.config, DEVICE_TEST_TOKEN) {
            Ok(()) => {}
            Err(error) if error.kind == BackendErrorKind::Unavailable => {
                eprintln!("SKIP: Windows H.264 decoder unavailable for presenter test: {error}");
                return;
            }
            Err(error) => panic!("configure Windows H.264 decoder: {error}"),
        }

        let parent = TestParent::new();
        parent.show();
        let mut host = WindowsVideoHost::attach(parent.raw()).expect("attach presenter host");
        host.update(
            PhysicalVideoRect::new(0, 0, 640, 360),
            PresentationState::Visible,
        )
        .expect("show video stage");
        let target = host.take_target().expect("take publisher target");
        let mut publisher = WindowsD3D11Publisher::new(target);
        publisher.clear(DEVICE_TEST_TOKEN).expect("prime token");

        let mut attempted = 0usize;
        let mut saw_occluded = false;
        let deadline = Instant::now() + Duration::from_secs(10);
        for sample_index in 0..sample_count {
            loop {
                let submission = {
                    let unit = transport
                        .read_sample(sample_index, DEVICE_TEST_TOKEN.work())
                        .expect("read fixture video sample");
                    let pts = timeline_position(unit.pts, timescale);
                    let duration = timeline_duration(unit.duration, timescale);
                    let status = decoder
                        .submit(
                            EncodedVideoPacket {
                                bytes: unit.bytes,
                                sample_index: unit.sample_index,
                                pts,
                                duration,
                                is_sync: unit.is_sync,
                            },
                            DEVICE_TEST_TOKEN,
                        )
                        .expect("submit fixture sample");
                    (status, unit.parameter_set_submission)
                };
                match submission.0 {
                    SubmitStatus::Accepted => {
                        if let Some(parameter_sets) = submission.1 {
                            assert!(transport.commit_parameter_sets(parameter_sets));
                        }
                        break;
                    }
                    SubmitStatus::Backpressured => {
                        assert!(Instant::now() < deadline, "decoder backpressure timed out");
                        if decoder
                            .receive()
                            .expect("receive backpressure frame")
                            .is_none()
                        {
                            std::thread::yield_now();
                        }
                    }
                }
            }

            while let Some(frame) = decoder.receive().expect("receive fixture frame") {
                match attempted {
                    1 => {
                        host.update(
                            PhysicalVideoRect::new(0, 0, 800, 450),
                            PresentationState::Visible,
                        )
                        .expect("resize video stage");
                    }
                    2 => {
                        host.update(
                            PhysicalVideoRect::new(0, 0, 800, 450),
                            PresentationState::Occluded,
                        )
                        .expect("occlude video stage");
                    }
                    3 => {
                        host.update(
                            PhysicalVideoRect::new(0, 0, 720, 405),
                            PresentationState::Visible,
                        )
                        .expect("restore video stage");
                    }
                    _ => {}
                }
                let receipt = match publisher.publish(frame) {
                    Ok(receipt) => receipt,
                    Err(error) if attempted == 0 && error.kind == BackendErrorKind::Unavailable => {
                        eprintln!(
                            "SKIP: D3D11 NV12 presentation is unavailable on this adapter: {error}"
                        );
                        publisher.close();
                        host.close().expect("close skipped presenter host");
                        return;
                    }
                    Err(error) => panic!("present decoded fixture frame: {error}"),
                };
                saw_occluded |= receipt == PublicationReceipt::Occluded;
                attempted += 1;
                if attempted == 4 {
                    break;
                }
            }
            if attempted == 4 {
                break;
            }
        }

        assert_eq!(attempted, 4, "fixture must produce four presenter frames");
        assert!(saw_occluded, "occluded host must reject publication");
        let telemetry = publisher.telemetry();
        assert_eq!(telemetry.swap_chain_creations, 1);
        assert!(telemetry.swap_chain_resizes >= 2);
        assert!(telemetry.processor_reconfigurations >= 3);
        assert_eq!(telemetry.occluded_frames, 1);
        assert_eq!(telemetry.device_losses, 0);
        assert!(
            telemetry.presented_frames > 0,
            "an available D3D presenter must complete a real Present"
        );
        assert_eq!(
            telemetry
                .presented_frames
                .saturating_add(telemetry.backpressured_frames)
                .saturating_add(telemetry.occluded_frames),
            attempted as u64
        );

        publisher.close();
        host.close().expect("publisher releases target before host");
        decoder.close();
    }

    fn timeline_position(pts: i64, timescale: u32) -> TimelinePosition {
        let pts = u64::try_from(pts).expect("fixture video PTS is non-negative");
        TimelinePosition::new(
            (u128::from(pts) * u128::from(PLAYBACK_TIMELINE_HZ) / u128::from(timescale)) as u64,
        )
    }

    fn timeline_duration(duration: u32, timescale: u32) -> TimelineDuration {
        let ticks = u128::from(duration) * u128::from(PLAYBACK_TIMELINE_HZ) / u128::from(timescale);
        TimelineDuration::new(u64::try_from(ticks).expect("fixture duration fits u64"))
            .expect("fixture frame duration is non-zero")
    }
}
