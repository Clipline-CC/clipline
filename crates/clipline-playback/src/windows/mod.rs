//! Safe Windows playback backends.
//!
//! All first-party COM, Media Foundation, WASAPI, and D3D11 `unsafe` is
//! confined to this module tree. Callers interact through safe platform
//! traits and move-only surface wrappers.

mod com;
mod d3d11;
mod mft_decode;
mod presenter;
mod readback;
mod session;
mod wasapi_render;

pub use mft_decode::{
    classify_device_failure, probe_h264_decoders, D3D11VideoSurface, DecoderCapabilities,
    DecoderOwnershipTelemetry, DecoderPreference, WindowsH264Decoder, MAX_PLAYBACK_SURFACES,
};
pub use presenter::{
    classify_present_result, validate_video_bounds, D3D11PublisherTelemetry, PresentOutcome,
    VideoHostError, WindowsD3D11Publisher, WindowsVideoHost, WindowsVideoTarget,
    MAX_PRESENTATION_INPUT_SURFACES, PRESENTATION_SWAP_CHAIN_BUFFERS,
};
pub use readback::{
    convert_nv12_to_rgb8, Nv12FrameView, Nv12ReadbackError, Nv12ReadbackFormat, Nv12ReadbackSample,
    Nv12ReadbackTelemetry, WindowsNv12Readback, MAX_DIAGNOSTIC_RGB_PIXELS,
};
pub use session::{
    session_channel, SessionClient, SessionExit, SessionReport, SessionRunError, SessionRuntime,
    SessionSendError, SessionTelemetry, SessionUpdate, SessionUpdateError, SessionUpdatePayload,
    UpdatePublishOutcome, SESSION_MAX_WAIT, SESSION_UPDATE_CAPACITY,
};
pub use wasapi_render::{
    classify_audio_failure, WasapiInitializationPath, WasapiRendererTelemetry,
    WindowsWasapiRenderer,
};
