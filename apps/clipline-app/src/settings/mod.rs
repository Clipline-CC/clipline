//! Persisted application settings and mapping to recorder service options.
//!
//! Split into focused submodules:
//! - [`types`]: data model structs/enums + per-type conversions
//! - [`games`]: game detection settings + legacy migration
//! - [`cloud`]: Clipline Cloud connection + upload records
//! - [`osu`]: osu! API connection metadata
//! - [`hotkey`]: hotkey parsing
//! - [`validation`]: `validate` impls + path/quota helpers
//! - [`persistence`]: file I/O, atomic writes, legacy field repair, load/save
//! - [`tests`]: unit tests
//!
//! `AppSettings` itself lives here: the aggregate struct, its `Default`,
//! and the `to_service_options` mapping. All public items are re-exported
//! from this module so `crate::settings::X` keeps working unchanged.

use serde::{Deserialize, Serialize};

use crate::service::{
    CaptureBackend, CaptureSource, OutputResolution, RecordingMode, ServiceOptions,
};
use crate::updates::UpdateChannel;

pub mod cloud;
pub mod games;
pub mod hotkey;
pub mod osu;
pub mod persistence;
pub mod types;
pub(crate) mod validation;

pub use cloud::{normalize_cloud_visibility, CloudSettings, CloudUploadRecord};
#[allow(unused_imports)]
pub use games::{
    GamePluginReviewSettings, GamePluginSettings, GameRecordingMode, GameSettings,
    MatchEventSettings, TimelineMarkerSettings,
};
pub use hotkey::{is_global_shortcut_hotkey, normalize_hotkey, parse_hotkey};
pub use osu::OsuApiSettings;
pub use persistence::{
    audio_preview_cache_dir, icon_cache_dir, normalize_media_dir, normalize_replay_cache_dir,
    quota_bytes_from_gb, replay_cache_quota_bytes_from_gb, settings_path, share_export_cache_dir,
};
#[allow(unused_imports)]
pub use types::{
    AdvancedRecordingSettings, AudioSettings, CaptureMode, CaptureRegionSettings,
    CustomGameSettings, ReplayStorageMode, ReplayStorageSettings, VideoQuality,
};

const DEFAULT_REPLAY_CACHE_QUOTA_GB: f64 = 2.0;

/// UI color theme. Booth is the warm amber default; alternate palettes use
/// [data-theme] overrides in styles.css.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UiTheme {
    #[default]
    Booth,
    Classic,
    Purple,
    Oled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AppSettings {
    pub capture_mode: CaptureMode,
    #[serde(default)]
    pub capture_backend: CaptureBackend,
    pub window_title: String,
    #[serde(default)]
    pub capture_region: CaptureRegionSettings,
    #[serde(default)]
    pub games: GameSettings,
    #[serde(default)]
    pub audio: AudioSettings,
    /// Legacy persistence mirror of `replay_window_s`; ignored at runtime and
    /// normalized whenever settings cross the persistence boundary.
    pub buffer_seconds: f64,
    pub replay_window_s: f64,
    #[serde(default)]
    pub video_quality: VideoQuality,
    pub bitrate_mbps: f64,
    pub fps: u32,
    #[serde(default)]
    pub advanced_recording: AdvancedRecordingSettings,
    #[serde(default, deserialize_with = "persistence::deserialize_video_encoder")]
    pub video_encoder: crate::service::VideoEncoder,
    #[serde(default)]
    pub output_resolution: OutputResolution,
    pub disk_quota_gb: f64,
    #[serde(default = "default_media_dir")]
    pub media_dir: String,
    #[serde(default)]
    pub replay_storage: ReplayStorageSettings,
    pub hotkey: String,
    /// Optional second keybind for Save Replay; `None` disables it.
    #[serde(default)]
    pub hotkey_secondary: Option<String>,
    /// Optional system-wide keybind for starting or stopping a full-session recording.
    #[serde(default)]
    pub recording_hotkey: Option<String>,
    /// Optional second keybind for starting or stopping a full-session recording.
    #[serde(default)]
    pub recording_hotkey_secondary: Option<String>,
    #[serde(default)]
    pub open_on_startup: bool,
    #[serde(default = "default_enabled")]
    pub close_to_tray: bool,
    #[serde(default)]
    pub minimize_to_tray: bool,
    #[serde(default)]
    pub legacy_timeline_editor: bool,
    #[serde(default)]
    pub ui_theme: UiTheme,
    #[serde(default)]
    pub update_channel: UpdateChannel,
    #[serde(default)]
    pub cloud: CloudSettings,
    #[serde(default)]
    pub osu: OsuApiSettings,
}

fn default_enabled() -> bool {
    true
}

fn default_media_dir() -> String {
    persistence::default_media_dir()
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            capture_mode: CaptureMode::PrimaryMonitor,
            capture_backend: CaptureBackend::Auto,
            window_title: String::new(),
            capture_region: CaptureRegionSettings::default(),
            games: GameSettings::default(),
            audio: AudioSettings::default(),
            buffer_seconds: 60.0,
            replay_window_s: 60.0,
            video_quality: VideoQuality::Balanced,
            bitrate_mbps: 12.0,
            fps: 60,
            advanced_recording: AdvancedRecordingSettings::default(),
            video_encoder: crate::service::VideoEncoder::Auto,
            output_resolution: OutputResolution::Source,
            disk_quota_gb: 10.0,
            media_dir: default_media_dir(),
            replay_storage: ReplayStorageSettings::default(),
            hotkey: "F6".into(),
            hotkey_secondary: None,
            recording_hotkey: None,
            recording_hotkey_secondary: None,
            open_on_startup: false,
            close_to_tray: true,
            minimize_to_tray: false,
            legacy_timeline_editor: false,
            ui_theme: UiTheme::default(),
            update_channel: UpdateChannel::Nightly,
            cloud: CloudSettings::default(),
            osu: OsuApiSettings::default(),
        }
    }
}

impl AppSettings {
    pub fn media_dir_path(&self) -> Result<std::path::PathBuf, String> {
        normalize_media_dir(&self.media_dir)
    }

    /// All configured Save Replay keybinds: the primary plus the optional
    /// secondary. Blank secondaries are treated as disabled.
    pub fn hotkeys(&self) -> Vec<&str> {
        let mut hotkeys = vec![self.hotkey.as_str()];
        if let Some(secondary) = self.hotkey_secondary.as_deref() {
            if !secondary.trim().is_empty() {
                hotkeys.push(secondary);
            }
        }
        hotkeys
    }

    pub fn recording_hotkeys(&self) -> Vec<&str> {
        [
            self.recording_hotkey.as_deref(),
            self.recording_hotkey_secondary.as_deref(),
        ]
        .into_iter()
        .flatten()
        .filter(|hotkey| !hotkey.trim().is_empty())
        .collect()
    }

    pub fn to_service_options(&self, lol_url: Option<String>) -> Result<ServiceOptions, String> {
        self.validate()?;
        Ok(ServiceOptions {
            capture_source: match self.capture_mode {
                CaptureMode::PrimaryMonitor => CaptureSource::PrimaryMonitor,
                CaptureMode::WindowTitle => {
                    CaptureSource::WindowTitle(self.window_title.trim().to_string())
                }
                CaptureMode::DisplayRegion => {
                    CaptureSource::DisplayRegion(self.capture_region.to_service_region())
                }
            },
            capture_backend: self.capture_backend,
            active_game: None,
            media_dir: self.media_dir_path()?,
            recover_abandoned_recordings: true,
            lol_url,
            replay_window_s: self.replay_window_s,
            buffer_bytes: estimated_buffer_bytes(
                self.replay_window_s,
                self.effective_bitrate_mbps(),
            ),
            replay_storage: self.replay_storage.to_service_options()?,
            disk_quota_bytes: quota_bytes_from_gb(self.disk_quota_gb)?,
            recording_mode: RecordingMode::ReplaysOnly,
            fps: self.effective_fps(),
            bitrate_bps: (self.effective_bitrate_mbps() * 1_000_000.0).round() as u32,
            video_encoder: self.video_encoder,
            output_resolution: self.output_resolution,
            output_resolution_bounds: self.effective_output_resolution_bounds(),
            decodable_codecs: vec![clipline_capture::probe::Codec::H264],
            audio: self.audio.to_service_options(),
        })
    }

    pub fn effective_fps(&self) -> u32 {
        if self.advanced_recording.enabled {
            self.advanced_recording.fps
        } else {
            self.fps
        }
    }

    pub fn effective_output_resolution_bounds(
        &self,
    ) -> Option<crate::service::OutputResolutionBounds> {
        self.advanced_recording.repaired().output_bounds()
    }
}

fn compatibility_buffer_seconds(settings: &AppSettings) -> f64 {
    settings.replay_window_s
}

fn estimated_buffer_bytes(replay_window_s: f64, bitrate_mbps: f64) -> usize {
    const MIN_BUFFER_BYTES: f64 = 64.0 * 1024.0 * 1024.0;
    const ENCODER_OVERSHOOT_HEADROOM: f64 = 2.0;

    let video_bytes = bitrate_mbps * 1_000_000.0 / 8.0 * replay_window_s;
    (video_bytes * ENCODER_OVERSHOOT_HEADROOM).max(MIN_BUFFER_BYTES) as usize
}

#[cfg(test)]
mod tests;
