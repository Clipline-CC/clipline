//! Shared, frontend-independent game identity and discovery services.

pub mod detection;
pub mod discovery;
pub mod icon;
pub mod identity;
pub mod plugin;
pub mod presentation;
#[cfg(windows)]
pub mod windows;
