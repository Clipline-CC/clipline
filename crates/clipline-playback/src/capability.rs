use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PlaybackCodec {
    H264,
    Hevc,
    Av1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum H264PlaybackSupport {
    ConfiguredHardware,
    ConfiguredSoftware,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LimitedNativePlayback {
    Ungated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackSupport {
    Hardware,
    Software,
    Unavailable,
    LimitedNativePlayback,
}

/// Exact configured native decoder truth for the probed adapter.
///
/// H.264 is published only after a decoder transform accepts Clipline's real
/// input and NV12 output types. HEVC and AV1 remain deliberately ungated until
/// their full native playback pipelines satisfy the media gates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct PlaybackCapabilities {
    pub adapter_luid: Option<u64>,
    pub h264: H264PlaybackSupport,
    pub hevc: LimitedNativePlayback,
    pub av1: LimitedNativePlayback,
}

impl PlaybackCapabilities {
    pub const fn new(adapter_luid: Option<u64>, h264: H264PlaybackSupport) -> Self {
        Self {
            adapter_luid,
            h264,
            hevc: LimitedNativePlayback::Ungated,
            av1: LimitedNativePlayback::Ungated,
        }
    }

    pub const fn support(self, codec: PlaybackCodec) -> PlaybackSupport {
        match codec {
            PlaybackCodec::H264 => match self.h264 {
                H264PlaybackSupport::ConfiguredHardware => PlaybackSupport::Hardware,
                H264PlaybackSupport::ConfiguredSoftware => PlaybackSupport::Software,
                H264PlaybackSupport::Unavailable => PlaybackSupport::Unavailable,
            },
            PlaybackCodec::Hevc | PlaybackCodec::Av1 => PlaybackSupport::LimitedNativePlayback,
        }
    }

    pub const fn native_decodable(self, codec: PlaybackCodec) -> bool {
        matches!(
            self.support(codec),
            PlaybackSupport::Hardware | PlaybackSupport::Software
        )
    }
}
