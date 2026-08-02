//! Framework-neutral catalog contracts for Clipline's native and compatibility UIs.
//!
//! This crate deliberately owns no UI, platform, filesystem, network, or credential
//! implementation. It defines bounded values and ownership-fenced delivery so those
//! implementations can be shared without coupling them to Tauri or Slint.

mod channel;
mod contract;
mod identity;

pub use channel::*;
pub use contract::*;
pub use identity::*;
