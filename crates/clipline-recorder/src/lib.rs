//! Shared recorder runtime and frontend-independent recording services.

pub mod marker_source;
pub mod media_root;
pub mod probe;
#[cfg(windows)]
pub mod restart;
pub mod time;

#[cfg(windows)]
mod service;
#[cfg(windows)]
pub use service::*;

#[cfg(windows)]
pub use clipline_playback::PlaybackCapabilities;
#[cfg(windows)]
pub use probe::{
    available_encoder_options, native_decodable_codecs, native_playback_warning,
    probe_playback_capabilities, probe_playback_capabilities_with_checkpoint, SettingsProbeCatalog,
};
pub use probe::{EncoderOption, NativePlaybackWarning};
#[cfg(windows)]
pub use restart::{active_recorder_workers, PreparedRecorderRestart, RecorderEventStream};
