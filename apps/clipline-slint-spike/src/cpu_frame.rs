use std::fmt;
use std::sync::{Arc, Mutex};

use clipline_playback::{
    BackendComponent, BackendError, BackendErrorKind, DecodedVideoFrame, FramePublisher,
    PipelineToken, PublicationReceipt, RecoveryDisposition,
};

#[cfg(windows)]
use clipline_playback::windows::{
    D3D11VideoSurface, Nv12ReadbackTelemetry, WindowsNv12Readback,
};
#[cfg(windows)]
use slint::{Rgb8Pixel, SharedPixelBuffer};

#[cfg(windows)]
use crate::CliplineSpike;

pub const MAX_CPU_FRAME_PIXELS: usize = 3_840 * 2_160;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CpuFrameTelemetry {
    pub rgb_capacity: usize,
    pub allocation_count: u64,
    pub replaced_frames: u64,
    pub stale_frames: u64,
    pub backpressured_frames: u64,
    pub pending_high_water: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CpuFrameError {
    StaleToken,
    InvalidDimensions { width: u32, height: u32 },
    FrameTooLarge { pixels: usize, max: usize },
    Backpressured,
    AllocationFailed { bytes: usize },
}

impl fmt::Display for CpuFrameError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for CpuFrameError {}

pub struct CpuFrameWrite {
    token: PipelineToken,
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

impl CpuFrameWrite {
    pub fn pixels_mut(&mut self) -> &mut [u8] {
        &mut self.pixels
    }

    pub const fn token(&self) -> PipelineToken {
        self.token
    }

    pub const fn width(&self) -> u32 {
        self.width
    }

    pub const fn height(&self) -> u32 {
        self.height
    }
}

pub struct CpuRgbFrame {
    token: PipelineToken,
    width: u32,
    height: u32,
    pixels: Vec<u8>,
    copy_time_100ns: u64,
}

impl CpuRgbFrame {
    pub const fn token(&self) -> PipelineToken {
        self.token
    }

    pub const fn width(&self) -> u32 {
        self.width
    }

    pub const fn height(&self) -> u32 {
        self.height
    }

    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }

    pub const fn copy_time_100ns(&self) -> u64 {
        self.copy_time_100ns
    }
}

#[derive(Default)]
struct MailboxState {
    active_token: Option<PipelineToken>,
    pending: Option<CpuRgbFrame>,
    recycled: Option<Vec<u8>>,
    delivery_scheduled: bool,
    telemetry: CpuFrameTelemetry,
}

#[derive(Clone)]
pub struct CpuFrameProducer {
    state: Arc<Mutex<MailboxState>>,
}

#[derive(Clone)]
pub struct CpuFrameConsumer {
    state: Arc<Mutex<MailboxState>>,
}

pub fn cpu_frame_mailbox() -> (CpuFrameProducer, CpuFrameConsumer) {
    let state = Arc::new(Mutex::new(MailboxState::default()));
    (
        CpuFrameProducer {
            state: Arc::clone(&state),
        },
        CpuFrameConsumer { state },
    )
}

impl CpuFrameProducer {
    pub fn clear(&mut self, token: PipelineToken) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        if let Some(frame) = state.pending.take() {
            recycle_buffer(&mut state, frame.pixels);
        }
        state.active_token = Some(token);
    }

    pub fn acquire(
        &mut self,
        token: PipelineToken,
        width: u32,
        height: u32,
    ) -> Result<CpuFrameWrite, CpuFrameError> {
        let bytes = rgb_frame_bytes(width, height)?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| CpuFrameError::Backpressured)?;
        if state.active_token != Some(token) {
            state.telemetry.stale_frames = state.telemetry.stale_frames.saturating_add(1);
            return Err(CpuFrameError::StaleToken);
        }

        let mut pixels = if let Some(buffer) = state.recycled.take() {
            buffer
        } else if let Some(frame) = state.pending.take() {
            state.telemetry.replaced_frames = state.telemetry.replaced_frames.saturating_add(1);
            frame.pixels
        } else if state.telemetry.allocation_count == 0 {
            let mut buffer = Vec::new();
            buffer
                .try_reserve_exact(bytes)
                .map_err(|_| CpuFrameError::AllocationFailed { bytes })?;
            state.telemetry.allocation_count = 1;
            buffer
        } else {
            state.telemetry.backpressured_frames =
                state.telemetry.backpressured_frames.saturating_add(1);
            return Err(CpuFrameError::Backpressured);
        };

        if pixels.capacity() < bytes {
            let additional = bytes.saturating_sub(pixels.len());
            pixels
                .try_reserve_exact(additional)
                .map_err(|_| CpuFrameError::AllocationFailed { bytes })?;
            state.telemetry.allocation_count = state.telemetry.allocation_count.saturating_add(1);
        }
        pixels.resize(bytes, 0);
        state.telemetry.rgb_capacity = state.telemetry.rgb_capacity.max(pixels.capacity());
        Ok(CpuFrameWrite {
            token,
            width,
            height,
            pixels,
        })
    }

    pub fn commit(
        &mut self,
        write: CpuFrameWrite,
        copy_time_100ns: u64,
    ) -> Result<(), CpuFrameError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| CpuFrameError::Backpressured)?;
        if state.active_token != Some(write.token) {
            state.telemetry.stale_frames = state.telemetry.stale_frames.saturating_add(1);
            recycle_buffer(&mut state, write.pixels);
            return Err(CpuFrameError::StaleToken);
        }
        if let Some(frame) = state.pending.take() {
            state.telemetry.replaced_frames = state.telemetry.replaced_frames.saturating_add(1);
            recycle_buffer(&mut state, frame.pixels);
        }
        state.pending = Some(CpuRgbFrame {
            token: write.token,
            width: write.width,
            height: write.height,
            pixels: write.pixels,
            copy_time_100ns,
        });
        state.telemetry.pending_high_water = 1;
        Ok(())
    }

    pub fn telemetry(&self) -> CpuFrameTelemetry {
        self.state
            .lock()
            .map_or_else(|_| CpuFrameTelemetry::default(), |state| state.telemetry)
    }

    /// Returns true exactly once while a UI delivery is outstanding.
    pub fn request_delivery(&mut self) -> bool {
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        if state.pending.is_none() || state.delivery_scheduled {
            return false;
        }
        state.delivery_scheduled = true;
        true
    }
}

impl CpuFrameConsumer {
    pub fn take_latest(&mut self, token: PipelineToken) -> Option<CpuRgbFrame> {
        let mut state = self.state.lock().ok()?;
        if state.active_token != Some(token) {
            return None;
        }
        let frame = state.pending.take()?;
        if frame.token != token {
            state.telemetry.stale_frames = state.telemetry.stale_frames.saturating_add(1);
            recycle_buffer(&mut state, frame.pixels);
            return None;
        }
        Some(frame)
    }

    pub fn recycle(&mut self, frame: CpuRgbFrame) {
        if let Ok(mut state) = self.state.lock() {
            recycle_buffer(&mut state, frame.pixels);
        }
    }

    /// Completes one UI delivery and returns the token of a newer pending frame
    /// that arrived while the event-loop closure converted the previous one.
    pub fn finish_delivery(&mut self) -> Option<PipelineToken> {
        let Ok(mut state) = self.state.lock() else {
            return None;
        };
        if let Some(frame) = state.pending.as_ref() {
            Some(frame.token)
        } else {
            state.delivery_scheduled = false;
            None
        }
    }

    pub fn cancel_delivery(&mut self) {
        if let Ok(mut state) = self.state.lock() {
            state.delivery_scheduled = false;
        }
    }
}

fn rgb_frame_bytes(width: u32, height: u32) -> Result<usize, CpuFrameError> {
    if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
        return Err(CpuFrameError::InvalidDimensions { width, height });
    }
    let pixels = usize::try_from(width)
        .ok()
        .and_then(|width| {
            usize::try_from(height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .ok_or(CpuFrameError::FrameTooLarge {
            pixels: usize::MAX,
            max: MAX_CPU_FRAME_PIXELS,
        })?;
    if pixels > MAX_CPU_FRAME_PIXELS {
        return Err(CpuFrameError::FrameTooLarge {
            pixels,
            max: MAX_CPU_FRAME_PIXELS,
        });
    }
    pixels.checked_mul(3).ok_or(CpuFrameError::FrameTooLarge {
        pixels,
        max: MAX_CPU_FRAME_PIXELS,
    })
}

fn recycle_buffer(state: &mut MailboxState, buffer: Vec<u8>) {
    let replace = state
        .recycled
        .as_ref()
        .is_none_or(|current| current.capacity() < buffer.capacity());
    if replace {
        state.recycled = Some(buffer);
    }
}

#[cfg(windows)]
pub struct CpuDiagnosticPublisher {
    readback: WindowsNv12Readback,
    producer: CpuFrameProducer,
    consumer: CpuFrameConsumer,
    window: slint::Weak<CliplineSpike>,
}

#[cfg(windows)]
impl CpuDiagnosticPublisher {
    pub fn new(window: slint::Weak<CliplineSpike>) -> Self {
        let (producer, consumer) = cpu_frame_mailbox();
        Self {
            readback: WindowsNv12Readback::new(),
            producer,
            consumer,
            window,
        }
    }

    pub fn frame_telemetry(&self) -> CpuFrameTelemetry {
        self.producer.telemetry()
    }

    pub const fn readback_telemetry(&self) -> Nv12ReadbackTelemetry {
        self.readback.telemetry()
    }

    fn schedule_delivery(&mut self, token: PipelineToken) -> Result<(), BackendError> {
        if !self.producer.request_delivery() {
            return Ok(());
        }
        enqueue_cpu_delivery(self.window.clone(), self.consumer.clone(), token).map_err(|error| {
            self.consumer.cancel_delivery();
            publication_failure(format!("queue CPU diagnostic frame on Slint event loop: {error}"))
        })
    }
}

#[cfg(windows)]
impl FramePublisher<D3D11VideoSurface> for CpuDiagnosticPublisher {
    fn publish(
        &mut self,
        frame: DecodedVideoFrame<D3D11VideoSurface>,
    ) -> Result<PublicationReceipt, BackendError> {
        let token = frame.token();
        let format = self.readback.configure(frame.surface())?;
        let mut write = match self.producer.acquire(token, format.width, format.height) {
            Ok(write) => write,
            Err(CpuFrameError::Backpressured) => {
                drop(frame);
                return Ok(PublicationReceipt::Backpressured);
            }
            Err(error) => return Err(cpu_frame_failure(error)),
        };
        let sample = self
            .readback
            .read_rgb8(frame.surface(), write.pixels_mut())?;
        drop(frame);
        self.producer
            .commit(write, sample.copy_time_100ns)
            .map_err(cpu_frame_failure)?;
        self.schedule_delivery(token)?;
        Ok(PublicationReceipt::Presented)
    }

    fn clear(&mut self, token: PipelineToken) -> Result<(), BackendError> {
        self.producer.clear(token);
        Ok(())
    }
}

#[cfg(windows)]
fn enqueue_cpu_delivery(
    window: slint::Weak<CliplineSpike>,
    mut consumer: CpuFrameConsumer,
    token: PipelineToken,
) -> Result<(), slint::EventLoopError> {
    slint::invoke_from_event_loop(move || {
        if let Some(frame) = consumer.take_latest(token) {
            let pixels = SharedPixelBuffer::<Rgb8Pixel>::clone_from_slice(
                frame.pixels(),
                frame.width(),
                frame.height(),
            );
            if let Some(window) = window.upgrade() {
                window.set_cpu_video_frame(slint::Image::from_rgb8(pixels));
            }
            consumer.recycle(frame);
        }
        if let Some(next_token) = consumer.finish_delivery() {
            let _ = enqueue_cpu_delivery(window, consumer, next_token);
        }
    })
}

#[cfg(windows)]
fn cpu_frame_failure(error: CpuFrameError) -> BackendError {
    let (kind, recovery) = match error {
        CpuFrameError::StaleToken => (
            BackendErrorKind::StaleWork,
            RecoveryDisposition::RetryPipeline,
        ),
        CpuFrameError::Backpressured => (
            BackendErrorKind::PublicationFailure,
            RecoveryDisposition::RetryPipeline,
        ),
        CpuFrameError::InvalidDimensions { .. }
        | CpuFrameError::FrameTooLarge { .. }
        | CpuFrameError::AllocationFailed { .. } => (
            BackendErrorKind::CorruptInput,
            RecoveryDisposition::RetryPipeline,
        ),
    };
    BackendError {
        component: BackendComponent::FramePublisher,
        kind,
        recovery,
        native_code: None,
        message: error.to_string(),
    }
}

#[cfg(windows)]
fn publication_failure(message: String) -> BackendError {
    BackendError {
        component: BackendComponent::FramePublisher,
        kind: BackendErrorKind::PublicationFailure,
        recovery: RecoveryDisposition::RetryPipeline,
        native_code: None,
        message,
    }
}
