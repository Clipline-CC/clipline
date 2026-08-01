use thiserror::Error;

use crate::{H264DecoderConfig, WorkGeneration};

pub const PLAYBACK_TIMELINE_HZ: u32 = 48_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TimelinePosition(u64);

impl TimelinePosition {
    pub const fn new(ticks: u64) -> Self {
        Self(ticks)
    }

    pub const fn ticks(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TimelineDuration(u64);

impl TimelineDuration {
    pub fn new(ticks: u64) -> Result<Self, ClockError> {
        if ticks == 0 {
            return Err(ClockError::ZeroDuration);
        }
        Ok(Self(ticks))
    }

    pub const fn ticks(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
/// Monotonic host time expressed in 100-nanosecond units.
pub struct MonotonicTime100ns(u64);

impl MonotonicTime100ns {
    pub const fn new(ticks: u64) -> Self {
        Self(ticks)
    }

    pub const fn ticks(self) -> u64 {
        self.0
    }

    pub const fn elapsed_since(self, earlier: Self) -> u64 {
        self.0.saturating_sub(earlier.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PipelineToken {
    work: WorkGeneration,
    revision: u64,
}

impl PipelineToken {
    pub const fn new(work: WorkGeneration, revision: u64) -> Self {
        Self { work, revision }
    }

    pub const fn work(self) -> WorkGeneration {
        self.work
    }

    pub const fn revision(self) -> u64 {
        self.revision
    }

    pub fn next_revision(self) -> Option<Self> {
        self.revision.checked_add(1).map(|revision| Self {
            work: self.work,
            revision,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawAudioClock {
    position: u64,
    frequency: u64,
    endpoint_epoch: u64,
}

impl RawAudioClock {
    pub fn new(position: u64, frequency: u64, endpoint_epoch: u64) -> Result<Self, ClockError> {
        if frequency == 0 {
            return Err(ClockError::ZeroFrequency);
        }
        Ok(Self {
            position,
            frequency,
            endpoint_epoch,
        })
    }

    pub const fn position(self) -> u64 {
        self.position
    }

    pub const fn frequency(self) -> u64 {
        self.frequency
    }

    pub const fn endpoint_epoch(self) -> u64 {
        self.endpoint_epoch
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ClockError {
    #[error("audio clock frequency must be non-zero")]
    ZeroFrequency,
    #[error("timeline duration must be non-zero")]
    ZeroDuration,
    #[error("audio clock position regressed from {anchor} to {actual}")]
    RawPositionRegressed { anchor: u64, actual: u64 },
    #[error("audio endpoint epoch changed from {expected} to {actual} without a rebase")]
    EndpointEpochChanged { expected: u64, actual: u64 },
    #[error("audio clock frequency changed from {expected} to {actual} without a rebase")]
    FrequencyChanged { expected: u64, actual: u64 },
    #[error("timeline clock arithmetic overflow")]
    TimelineOverflow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendComponent {
    VideoDecoder,
    AudioRenderer,
    FramePublisher,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendErrorKind {
    DecoderFailure,
    CorruptInput,
    DeviceLost,
    EndpointInvalidated,
    Unavailable,
    PublicationFailure,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryDisposition {
    RetryPipeline,
    RecreateComponent,
    Fatal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendError {
    pub component: BackendComponent,
    pub kind: BackendErrorKind,
    pub recovery: RecoveryDisposition,
    pub native_code: Option<i64>,
    pub message: String,
}

impl std::fmt::Display for BackendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?} {:?}: {}", self.component, self.kind, self.message)
    }
}

impl std::error::Error for BackendError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitStatus {
    Accepted,
    Backpressured,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoAcceleration {
    Hardware,
    Software,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoPixelFormat {
    Nv12,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoDecoderInfo {
    pub acceleration: VideoAcceleration,
    pub pixel_format: VideoPixelFormat,
    pub width: u16,
    pub height: u16,
    pub adapter_luid: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioSampleFormat {
    F32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioRendererInfo {
    pub sample_rate: u32,
    pub channels: u16,
    pub sample_format: AudioSampleFormat,
    pub buffer_frames: usize,
    pub endpoint_epoch: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct EncodedVideoPacket<'a> {
    pub bytes: &'a [u8],
    pub sample_index: usize,
    pub pts: TimelinePosition,
    pub duration: TimelineDuration,
    pub is_sync: bool,
}

#[derive(Debug)]
pub struct DecodedVideoFrame<S> {
    surface: S,
    sample_index: usize,
    pts: TimelinePosition,
    duration: TimelineDuration,
    token: PipelineToken,
}

impl<S> DecodedVideoFrame<S> {
    pub fn new(
        surface: S,
        sample_index: usize,
        pts: TimelinePosition,
        duration: TimelineDuration,
        token: PipelineToken,
    ) -> Self {
        Self {
            surface,
            sample_index,
            pts,
            duration,
            token,
        }
    }

    pub fn surface(&self) -> &S {
        &self.surface
    }

    pub const fn sample_index(&self) -> usize {
        self.sample_index
    }

    pub const fn pts(&self) -> TimelinePosition {
        self.pts
    }

    pub const fn duration(&self) -> TimelineDuration {
        self.duration
    }

    pub const fn token(&self) -> PipelineToken {
        self.token
    }
}

pub trait VideoDecoder {
    type Surface: Send + 'static;

    fn configure(
        &mut self,
        config: &H264DecoderConfig,
        token: PipelineToken,
    ) -> Result<(), BackendError>;
    fn info(&self) -> Option<VideoDecoderInfo>;
    fn submit(
        &mut self,
        packet: EncodedVideoPacket<'_>,
        token: PipelineToken,
    ) -> Result<SubmitStatus, BackendError>;
    /// Returned frames retain the token captured from their submitted input.
    fn receive(&mut self) -> Result<Option<DecodedVideoFrame<Self::Surface>>, BackendError>;
    fn flush(&mut self, token: PipelineToken) -> Result<(), BackendError>;
    fn drain(&mut self, token: PipelineToken) -> Result<(), BackendError>;
    fn close(&mut self);
}

pub trait AudioRenderer {
    fn info(&self) -> AudioRendererInfo;
    fn reset(&mut self, token: PipelineToken) -> Result<(), BackendError>;
    fn start(&mut self, token: PipelineToken) -> Result<(), BackendError>;
    fn pause(&mut self, token: PipelineToken) -> Result<(), BackendError>;
    fn set_volume(&mut self, volume: f32) -> Result<(), BackendError>;
    fn writable_frames(&mut self) -> Result<usize, BackendError>;
    /// Writes interleaved stereo PCM and returns the number of stereo frames accepted.
    fn write_stereo_frames(
        &mut self,
        pcm: &[f32],
        token: PipelineToken,
    ) -> Result<usize, BackendError>;
    fn raw_clock(&mut self) -> Result<RawAudioClock, BackendError>;
    fn close(&mut self);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublicationReceipt;

pub trait FramePublisher<S> {
    fn publish(&mut self, frame: DecodedVideoFrame<S>) -> Result<PublicationReceipt, BackendError>;
    fn clear(&mut self, token: PipelineToken) -> Result<(), BackendError>;
}
