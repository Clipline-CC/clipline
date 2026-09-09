pub mod annexb;
pub mod av1;
pub mod avsync;
pub mod clock;
#[cfg(any(windows, test))]
mod capture_geometry;
#[cfg(any(windows, test))]
mod reopen_resource;
pub mod cpu_video;
pub mod diagnostics;
pub mod ffmpeg;
pub mod ffmpeg_encoder;
pub mod framing;
pub mod hevc;
#[cfg(any(windows, test))]
mod hybrid_policy;
pub mod mock;
pub mod opus;
pub mod pcm;
pub mod pipeline;
pub mod probe;
#[doc(hidden)]
pub mod print_protocol;
pub mod traits;
mod video_layout;
#[cfg(windows)]
pub mod windows;

pub use annexb::{annexb_to_avcc, even_dimensions, extract_sps_pps, nal_type, split_annexb};
pub use avsync::{validate_timeline, SyncReport, SyncTolerances, SyncViolation};
pub use clock::{qpc_to_ticks_100ns, RelativeClock};
pub use mock::{LimitedCapture, MockAudioSource, MockCapture, MockEncoder};
pub use opus::OpusFrameEncoder;
pub use pcm::{extract_stereo, LoopbackAssembler};
pub use pipeline::{PipelineError, Recorder, ReplayStorageConfig};
pub use probe::{
    rank_encoders, Codec, EncoderApi, EncoderBackend, EncoderCandidate, EncoderCapability,
    EncoderPreference,
};
pub use traits::{
    AudioPacket, AudioSource, CaptureEngine, CaptureError, EncodeError, EncodedPacket, Encoder,
    Frame, FrameData,
};

pub(crate) fn replay_gop_frames(fps: u32) -> u32 {
    (fps / 2).max(1)
}
