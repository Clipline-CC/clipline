//! Framework-neutral persisted Clipline settings and transactional storage.
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

pub mod capture_region;
pub mod cloud;
pub mod draft;
pub mod games;
pub mod hotkey;
pub mod osu;
pub mod persistence;
pub mod preferences;
pub mod types;
pub mod validation;

pub use capture_region::{
    Align, DisplayGeometry, DpiScale, LogicalPoint, RegionAction, RegionGeometry,
    RegionGeometryError, MAX_DISPLAY_ID_BYTES,
};
pub use cloud::{
    normalize_cloud_visibility, CloudSettings, CloudUploadRecord, MAX_CLOUD_UPLOAD_ERROR_BYTES,
    MAX_CLOUD_UPLOAD_ID_BYTES, MAX_CLOUD_UPLOAD_PATH_BYTES, MAX_CLOUD_UPLOAD_URL_BYTES,
};
pub use draft::{
    CloseRequest, CloseResult, CloudAccountDisplay, CloudAccountOwner, CloudConfigurationOwner,
    CloudWorkKind, CloudWorkOwner, DirtySummary, DraftError, OwnedCloudWork,
    SettingsBackendDisplay, SettingsDraftController, SettingsField, SettingsSaveToken,
    SettingsSessionGeneration, SettingsTab, TabNavigation, TabProjection,
};
#[allow(unused_imports)]
pub use games::{
    GamePluginReviewSettings, GamePluginSettings, GameRecordingMode, GameSettings,
    MatchEventSettings, TimelineMarkerSettings, BUILT_IN_GAME_IDS, CS2_ID, LEAGUE_OF_LEGENDS_ID,
    OSU_ID, VALORANT_ID,
};
pub use hotkey::{normalize_hotkey, parse_hotkey};
pub use osu::OsuApiSettings;
pub use persistence::{
    audio_preview_cache_dir, cloud_paths_equivalent, icon_cache_dir, normalize_media_dir,
    normalize_replay_cache_dir, quota_bytes_from_gb, replay_cache_quota_bytes_from_gb,
    settings_path, share_export_cache_dir, AccountGeneration, CloudAccountIdentity,
    CloudAccountPublicationOwner, CloudAccountStore, CloudProfileCas, CloudRecordCas,
    CloudRecordCasKind, CloudRecordSlot, LibraryConfig, SettingsChange, SettingsLoadSource,
    SettingsPathResolver, SettingsProfile, SettingsProfileError, SettingsRevision,
    SettingsSnapshot, SettingsStore, SettingsTransaction, SettingsTransactionError,
    MAX_CLOUD_RECORD_CAS_SLOTS,
};
pub use preferences::{
    CloudUploadPreferences, GamePluginPreference, GamePreferences, SettingsPreferences,
    MAX_SETTINGS_COLLECTION_BYTES, MAX_SETTINGS_CUSTOM_GAMES, MAX_SETTINGS_FIELD_BYTES,
    MAX_SETTINGS_GAME_PLUGINS,
};
#[allow(unused_imports)]
pub use types::{
    AdvancedRecordingSettings, AudioChannelMode, AudioSettings, CaptureBackend, CaptureMode,
    CaptureRegionSettings, CustomGameSettings, OutputResolution, OutputResolutionBounds,
    ReplayStorageMode, ReplayStorageSettings, VideoEncoder, VideoQuality,
};

const DEFAULT_REPLAY_CACHE_QUOTA_GB: f64 = 2.0;

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UpdateChannel {
    Stable,
    #[default]
    Nightly,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UpdateVariant {
    #[default]
    Regular,
    Standalone,
}

impl UpdateVariant {
    pub const fn from_standalone(standalone: bool) -> Self {
        if standalone {
            Self::Standalone
        } else {
            Self::Regular
        }
    }

    pub const fn is_standalone(self) -> bool {
        matches!(self, Self::Standalone)
    }
}

impl UpdateChannel {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Stable => "Stable",
            Self::Nightly => "Nightly",
        }
    }

    pub const fn endpoint(self, standalone: bool) -> &'static str {
        match (self, standalone) {
            (Self::Nightly, false) => {
                "https://github.com/dain98/clipline/releases/download/nightly/latest.json"
            }
            (Self::Nightly, true) => {
                "https://github.com/dain98/clipline/releases/download/nightly/latest-standalone.json"
            }
            (Self::Stable, false) => {
                "https://github.com/dain98/clipline/releases/latest/download/latest.json"
            }
            (Self::Stable, true) => {
                "https://github.com/dain98/clipline/releases/latest/download/latest-standalone.json"
            }
        }
    }

    pub const fn manifest_endpoint(self, variant: UpdateVariant) -> &'static str {
        self.endpoint(variant.is_standalone())
    }

    pub const fn enabled(self) -> bool {
        matches!(self, Self::Nightly)
    }
}

/// UI color theme. Booth is the warm amber default; Classic restores the
/// original midnight-blue palette via the [data-theme] override in styles.css.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UiTheme {
    #[default]
    Booth,
    Classic,
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
    pub video_encoder: VideoEncoder,
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

pub fn default_media_dir() -> String {
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
            video_encoder: VideoEncoder::Auto,
            output_resolution: OutputResolution::Source,
            disk_quota_gb: 10.0,
            media_dir: default_media_dir(),
            replay_storage: ReplayStorageSettings::default(),
            hotkey: "Alt+F10".into(),
            hotkey_secondary: None,
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

    pub fn effective_fps(&self) -> u32 {
        if self.advanced_recording.enabled {
            self.advanced_recording.fps
        } else {
            self.fps
        }
    }

    pub fn effective_output_resolution_bounds(&self) -> Option<OutputResolutionBounds> {
        self.advanced_recording.repaired().output_bounds()
    }
}

fn compatibility_buffer_seconds(settings: &AppSettings) -> f64 {
    settings.replay_window_s
}

pub fn estimated_buffer_bytes(replay_window_s: f64, bitrate_mbps: f64) -> usize {
    const MIN_BUFFER_BYTES: f64 = 64.0 * 1024.0 * 1024.0;
    const ENCODER_OVERSHOOT_HEADROOM: f64 = 2.0;

    let video_bytes = bitrate_mbps * 1_000_000.0 / 8.0 * replay_window_s;
    (video_bytes * ENCODER_OVERSHOOT_HEADROOM).max(MIN_BUFFER_BYTES) as usize
}
