//! Shared, frontend-independent game identity and discovery services.

pub mod channel;
pub mod controller;
pub mod detection;
pub mod detector;
pub mod discovery;
pub mod icon;
pub mod identity;
pub mod osu;
pub mod osu_enrichment;
pub mod osu_http;
pub mod plugin;
pub mod presentation;
#[cfg(windows)]
pub mod windows;
