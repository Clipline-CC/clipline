//! Bounded native playback primitives for Clipline-authored media.

mod annexb;
mod command;
mod sample_buffer;
mod state;

pub use annexb::{
    AnnexBError, AnnexBLimits, ConvertedAnnexB, H264AnnexBConverter, H264DecoderConfig,
    NativeVideoCapability, ParameterSetSubmission, UnsupportedVideoCodec,
    MAX_ANNEX_B_ACCESS_UNIT_BYTES, MAX_ENCODED_VIDEO_SAMPLE_BYTES,
};
pub use clipline_mp4::PlaybackTime;
pub use command::{
    CommandInbox, EnqueueError, EnqueueOutcome, PlaybackCommand, COMMAND_INBOX_CAPACITY,
};
pub use sample_buffer::{
    plan_video_sample_buffers, SampleBufferTelemetry, VideoAccessUnit, VideoSampleBufferPlan,
    VideoSampleTransport,
};
pub use state::{
    CommandError, PlaybackEvent, PlaybackPhase, PlaybackSnapshot, PlaybackState, WorkGeneration,
};
