//! Bounded native playback primitives for Clipline-authored media.

mod annexb;
mod audio;
mod backend;
mod command;
mod ring;
mod sample_buffer;
mod scheduler;
mod state;
mod worker;

pub use annexb::{
    AnnexBError, AnnexBLimits, ConvertedAnnexB, H264AnnexBConverter, H264DecoderConfig,
    NativeVideoCapability, ParameterSetSubmission, UnsupportedVideoCodec,
    MAX_ANNEX_B_ACCESS_UNIT_BYTES, MAX_ENCODED_VIDEO_SAMPLE_BYTES,
};
pub use audio::{
    mix_stereo_frames_into, AudioError, AudioPacketTelemetry, AudioResetPoint, AudioTrackSpec,
    DecoderBankChange, IndexedAudioPacket, IndexedAudioPacketReader, MixOutcome, OpusDecodeStats,
    OpusDecoderBank, OpusTrackDecoder, TimelineAudioMixer, TimelineAudioStats,
    MAX_OPUS_FRAME_SAMPLES, MAX_OPUS_PACKET_BYTES, MAX_SELECTED_AUDIO_TRACKS,
};
pub use backend::{
    AudioRenderer, AudioRendererInfo, AudioSampleFormat, BackendComponent, BackendError,
    BackendErrorKind, ClockError, DecodedVideoFrame, EncodedVideoPacket, FramePublisher,
    MonotonicTime100ns, PipelineToken, PublicationReceipt, RawAudioClock, RecoveryDisposition,
    SubmitStatus, TimelineDuration, TimelinePosition, VideoAcceleration, VideoDecoder,
    VideoDecoderInfo, VideoPixelFormat, PLAYBACK_TIMELINE_HZ,
};
pub use clipline_mp4::PlaybackTime;
pub use command::{
    AcceptedCommand, CommandInbox, EnqueueError, EnqueueOutcome, PlaybackCommand,
    COMMAND_INBOX_CAPACITY,
};
pub use ring::{RingTelemetry, StereoRingBuffer, MAX_AUDIO_QUEUE_FRAMES};
pub use sample_buffer::{
    plan_video_sample_buffers, LoadedVideoSample, SampleBufferTelemetry, VideoAccessUnit,
    VideoSampleBufferPlan, VideoSampleTransport,
};
pub use scheduler::{
    plan_audio_fill, AdmitOutcome, AudioAvailability, AudioFillPlan, EndOfStreamTracker,
    FrameScheduler, MetricHistogram, PlaybackMetrics, RebasedAudioClock, SchedulerError,
    SeekTarget, MAX_AUDIO_WRITE_FRAMES, METRIC_HISTOGRAM_MAX_MILLIS,
};
pub use state::{
    CommandError, PlaybackEvent, PlaybackPhase, PlaybackSnapshot, PlaybackState, WorkGeneration,
};
pub use worker::{
    PlaybackWorker, WorkerAction, WorkerActionKind, WorkerCompletion, WorkerError, WorkerSeekPlan,
    MAX_PIPELINE_RECOVERY_ATTEMPTS,
};
