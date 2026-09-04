//! Recorder option/config types.
use super::*;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureRegion {
    pub display_id: Option<String>,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CaptureSource {
    PrimaryMonitor,
    WindowTitle(String),
    WindowHandle { hwnd: isize, title: String },
    DisplayRegion(CaptureRegion),
}

/// Which screen-capture backend to use for display/region capture (issue #42).
/// `Auto` and `Wgc` both use Windows Graphics Capture today; `Wgc` is the
/// persisted force-WGC escape hatch that survives any future change to `Auto`.
/// `DesktopDuplication` uses DXGI Desktop Duplication, which has no Windows 10
/// privacy border but is display/region only (never per-window) and silently
/// falls back to WGC when it can't initialize (multi-GPU, rotated display, etc).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureBackend {
    #[default]
    Auto,
    Wgc,
    DesktopDuplication,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioChannelMode {
    #[default]
    Mono,
    Stereo,
}

/// The user's encoder choice. `Auto` prefers H.264 for playback compatibility while
/// respecting backend merit order within a codec; the explicit variants force a
/// (backend, codec) pair (still falling back through Auto if it can't open).
/// Legacy saved values (`auto`, `nvenc_h264`, `amf_h264`, `quick_sync_h264`)
/// still deserialize.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VideoEncoder {
    #[default]
    Auto,
    NvencH264,
    NvencHevc,
    NvencAv1,
    AmfH264,
    AmfHevc,
    AmfAv1,
    QuickSyncH264,
    QuickSyncHevc,
    QuickSyncAv1,
    SvtAv1,
}

impl VideoEncoder {
    pub(super) fn preference(self) -> EncoderPreference {
        let (backend, codec) = match self {
            Self::Auto => return EncoderPreference::Auto,
            Self::NvencH264 => (EncoderBackend::Nvenc, Codec::H264),
            Self::NvencHevc => (EncoderBackend::Nvenc, Codec::Hevc),
            Self::NvencAv1 => (EncoderBackend::Nvenc, Codec::Av1),
            Self::AmfH264 => (EncoderBackend::Amf, Codec::H264),
            Self::AmfHevc => (EncoderBackend::Amf, Codec::Hevc),
            Self::AmfAv1 => (EncoderBackend::Amf, Codec::Av1),
            Self::QuickSyncH264 => (EncoderBackend::QuickSync, Codec::H264),
            Self::QuickSyncHevc => (EncoderBackend::QuickSync, Codec::Hevc),
            Self::QuickSyncAv1 => (EncoderBackend::QuickSync, Codec::Av1),
            Self::SvtAv1 => (EncoderBackend::SvtAv1, Codec::Av1),
        };
        EncoderPreference::Explicit { backend, codec }
    }

    /// The settings/serde id (snake_case). Kept in lockstep with the
    /// `serde(rename_all = "snake_case")` derive by a test.
    pub(super) fn id(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::NvencH264 => "nvenc_h264",
            Self::NvencHevc => "nvenc_hevc",
            Self::NvencAv1 => "nvenc_av1",
            Self::AmfH264 => "amf_h264",
            Self::AmfHevc => "amf_hevc",
            Self::AmfAv1 => "amf_av1",
            Self::QuickSyncH264 => "quick_sync_h264",
            Self::QuickSyncHevc => "quick_sync_hevc",
            Self::QuickSyncAv1 => "quick_sync_av1",
            Self::SvtAv1 => "svt_av1",
        }
    }

    /// The explicit variant for a (backend, codec) pair, if Clipline exposes
    /// it as a user choice. `None` for combinations with no settings id
    /// (e.g. `MfSoftware`, or SvtAv1 paired with a non-AV1 codec).
    pub(super) fn from_parts(backend: EncoderBackend, codec: Codec) -> Option<Self> {
        Some(match (backend, codec) {
            (EncoderBackend::Nvenc, Codec::H264) => Self::NvencH264,
            (EncoderBackend::Nvenc, Codec::Hevc) => Self::NvencHevc,
            (EncoderBackend::Nvenc, Codec::Av1) => Self::NvencAv1,
            (EncoderBackend::Amf, Codec::H264) => Self::AmfH264,
            (EncoderBackend::Amf, Codec::Hevc) => Self::AmfHevc,
            (EncoderBackend::Amf, Codec::Av1) => Self::AmfAv1,
            (EncoderBackend::QuickSync, Codec::H264) => Self::QuickSyncH264,
            (EncoderBackend::QuickSync, Codec::Hevc) => Self::QuickSyncHevc,
            (EncoderBackend::QuickSync, Codec::Av1) => Self::QuickSyncAv1,
            (EncoderBackend::SvtAv1, Codec::Av1) => Self::SvtAv1,
            _ => return None,
        })
    }
}

/// The settings id string for a codec, matching the frontend's decode-probe
/// keys ("h264"/"hevc"/"av1").
pub fn codec_id(codec: Codec) -> &'static str {
    match codec {
        Codec::Av1 => "av1",
        Codec::Hevc => "hevc",
        Codec::H264 => "h264",
    }
}

/// One selectable encoder for the Settings dropdown.
#[derive(serde::Serialize)]
pub struct EncoderOption {
    /// VideoEncoder settings id (e.g. "amf_hevc").
    pub id: String,
    /// Human label (e.g. "AMD AMF · HEVC").
    pub name: String,
    /// Codec key the frontend matches against its decode-capability probe.
    pub codec: String,
}

/// The encoders this machine can actually use, as Settings options. Dedupes
/// the same (backend, codec) offered by both MFT and FFmpeg, ordered by the
/// ddoc merit/preference order.
pub fn available_encoder_options() -> Vec<EncoderOption> {
    let mut seen = std::collections::BTreeSet::new();
    let mut options = Vec::new();
    let capabilities = encoder_capabilities();
    for cap in &capabilities {
        for &codec in &cap.codecs {
            let Some(encoder) = VideoEncoder::from_parts(cap.backend, codec) else {
                continue;
            };
            if !seen.insert(encoder.id()) {
                continue;
            }
            let candidate = EncoderCandidate {
                api: cap.api,
                backend: cap.backend,
                codec,
            };
            options.push(EncoderOption {
                id: encoder.id().to_string(),
                name: encoder_label(candidate),
                codec: codec_id(codec).to_string(),
            });
        }
    }
    options
}

/// A short, human-readable label for the active encoder, shown in the
/// sidebar status (e.g. "AMD AMF · H.264" or "Software · AV1").
pub fn encoder_label(candidate: EncoderCandidate) -> String {
    let backend = match candidate.backend {
        EncoderBackend::Nvenc => "NVIDIA NVENC",
        EncoderBackend::Amf => "AMD AMF",
        EncoderBackend::QuickSync => "Intel Quick Sync",
        EncoderBackend::SvtAv1 => "Software",
        EncoderBackend::MfSoftware => "Software",
    };
    let codec = match candidate.codec {
        Codec::Av1 => "AV1",
        Codec::Hevc => "HEVC",
        Codec::H264 => "H.264",
    };
    format!("{backend} · {codec}")
}

#[derive(Clone, Debug, PartialEq)]
pub struct AudioOptions {
    pub output_enabled: bool,
    pub output_device_id: Option<String>,
    pub output_volume: f64,
    pub split_output_by_process: bool,
    pub mic_enabled: bool,
    pub mic_device_id: Option<String>,
    pub mic_volume: f64,
    pub mic_channels: AudioChannelMode,
}

impl Default for AudioOptions {
    fn default() -> Self {
        Self {
            output_enabled: true,
            output_device_id: None,
            output_volume: 1.0,
            split_output_by_process: false,
            mic_enabled: false,
            mic_device_id: None,
            mic_volume: 1.0,
            mic_channels: AudioChannelMode::Mono,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ReplayStorageOptions {
    #[default]
    Memory,
    Disk {
        dir: PathBuf,
        quota_bytes: u64,
    },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RecordingMode {
    FullSession,
    #[default]
    ReplaysOnly,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OutputResolutionBounds {
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Copy, Debug, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub enum OutputResolution {
    #[default]
    #[serde(rename = "source")]
    Source,
    #[serde(rename = "1440p")]
    P1440,
    #[serde(rename = "1080p")]
    P1080,
    #[serde(rename = "720p")]
    P720,
    #[serde(rename = "480p")]
    P480,
}

impl OutputResolution {
    pub(super) fn bounds(self) -> Option<(u32, u32)> {
        match self {
            Self::Source => None,
            Self::P1440 => Some((2560, 1440)),
            Self::P1080 => Some((1920, 1080)),
            Self::P720 => Some((1280, 720)),
            Self::P480 => Some((854, 480)),
        }
    }
}


/// The game a recording run is attributed to (plugin or custom), recorded
/// alongside saved clips so the library can show its icon.
#[derive(Clone, Debug)]
pub struct ActiveGame {
    pub identity: crate::game_identity::GameIdentity,
    pub name: String,
    pub exe_path: Option<PathBuf>,
    pub process_id: Option<u32>,
}
