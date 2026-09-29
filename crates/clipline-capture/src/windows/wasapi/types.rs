use super::*;

pub(crate) const OPUS_SAMPLE_RATE: u32 = 48_000;
pub(crate) const POLLING_BUFFER_DURATION_100NS: i64 = 10_000_000; // One second.
pub(crate) const AUDIO_DELIVERY_HEADROOM_S: f64 = FRAME_DURATION_S + 0.010;
pub(crate) const TERMINAL_AUDIO_DRAIN_S: f64 = FRAME_DURATION_S * 3.0;
pub(crate) const DEVICE_REACTIVATION_RETRY_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, Clone)]
pub struct AudioDeviceInfo {
    pub id: String,
    pub name: String,
    pub is_default: bool,
}

#[derive(Debug, Clone)]
pub struct AudioDeviceList {
    pub outputs: Vec<AudioDeviceInfo>,
    pub inputs: Vec<AudioDeviceInfo>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct AudioLevel {
    pub rms: f32,
    pub peak: f32,
    pub sample_count: usize,
}

#[derive(Debug, Clone)]
pub struct WasapiMonitorChunk {
    pub level: AudioLevel,
    pub samples: Vec<f32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WasapiChannelMode {
    Mono,
    Stereo,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum EndpointMode {
    OutputLoopback,
    InputCapture(WasapiChannelMode),
}

impl EndpointMode {
    pub(crate) fn diagnostic_label(self) -> &'static str {
        match self {
            Self::OutputLoopback => "output",
            Self::InputCapture(_) => "microphone",
        }
    }
}

pub(crate) fn init(e: windows::core::Error) -> CaptureError {
    CaptureError::Init(format!("WASAPI: {e}"))
}
