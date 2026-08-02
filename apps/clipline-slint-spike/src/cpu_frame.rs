use std::fmt;
use std::sync::{Arc, Mutex};

use clipline_playback::PipelineToken;

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
}

impl CpuFrameConsumer {
    pub fn take_latest(&mut self, token: PipelineToken) -> Option<CpuRgbFrame> {
        let mut state = self.state.lock().ok()?;
        let frame = state.pending.take()?;
        if frame.token != token || state.active_token != Some(token) {
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
