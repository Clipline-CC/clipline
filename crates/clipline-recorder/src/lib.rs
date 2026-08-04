//! Shared recorder runtime and frontend-independent recording services.

pub mod marker_source;
pub mod media_root;
pub mod time;

#[cfg(windows)]
mod service;
#[cfg(windows)]
pub use service::*;
