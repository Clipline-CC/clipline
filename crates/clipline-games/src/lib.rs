//! Shared, frontend-independent game identity and discovery services.

pub mod detection;
pub mod discovery;
pub mod identity;
pub mod plugin;
#[cfg(windows)]
pub mod windows;
