//! Shared recorder runtime and frontend-independent recording services.

pub mod marker_source;
pub mod media_root;
#[cfg(windows)]
pub mod restart;
pub mod time;

#[cfg(windows)]
mod service;
#[cfg(windows)]
pub use service::*;

#[cfg(windows)]
pub use restart::{active_recorder_workers, PreparedRecorderRestart, RecorderEventStream};
