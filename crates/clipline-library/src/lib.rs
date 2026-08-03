//! Framework-neutral catalog contracts for Clipline's native and compatibility UIs.
//!
//! This crate deliberately owns no UI, platform, filesystem, network, or credential
//! implementation. It defines bounded values and ownership-fenced delivery so those
//! implementations can be shared without coupling them to Tauri or Slint.

mod channel;
mod cloud;
mod cloud_model;
mod contract;
mod detail;
mod gallery;
mod identity;
mod local;
mod naming;
mod poster;
mod presentation;
mod repository;
mod scan;

pub use channel::*;
pub use cloud::*;
pub use cloud_model::*;
pub use contract::*;
pub use detail::*;
pub use gallery::*;
pub use identity::*;
pub use local::*;
pub use naming::*;
pub use poster::*;
pub use presentation::*;
pub use repository::*;
pub use scan::*;
