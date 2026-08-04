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

pub use probe::EncoderOption;
#[cfg(windows)]
pub use probe::{available_encoder_options, SettingsProbeCatalog};
#[cfg(windows)]
pub use restart::{active_recorder_workers, PreparedRecorderRestart, RecorderEventStream};
