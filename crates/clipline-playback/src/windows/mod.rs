//! Safe Windows playback backends.
//!
//! All first-party COM, Media Foundation, WASAPI, and D3D11 `unsafe` is
//! confined to this module tree. Callers interact through safe platform
//! traits and move-only surface wrappers.

mod com;
mod d3d11;
mod mft_decode;
mod presenter;
mod wasapi_render;

pub use mft_decode::{
    classify_device_failure, probe_h264_decoders, D3D11VideoSurface, DecoderCapabilities,
    DecoderOwnershipTelemetry, DecoderPreference, WindowsH264Decoder,
};
pub use presenter::{
    classify_present_result, validate_video_bounds, PresentOutcome, VideoHostError,
    WindowsVideoHost, WindowsVideoTarget,
};
pub use wasapi_render::{
    classify_audio_failure, WasapiInitializationPath, WasapiRendererTelemetry,
    WindowsWasapiRenderer,
};
