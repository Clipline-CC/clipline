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
mod upload;
#[path = "upload/preparation.rs"]
mod upload_preparation;
#[path = "upload/remote.rs"]
mod upload_remote;
#[path = "upload/transport.rs"]
mod upload_transport;

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
pub use upload::*;
pub use upload_preparation::*;
pub use upload_remote::*;
pub use upload_transport::*;
