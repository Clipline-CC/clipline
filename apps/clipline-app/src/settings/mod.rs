//! Compatibility adapter over the framework-neutral settings document/store.

pub use clipline_settings::*;

pub mod cloud {
    #[allow(unused_imports)]
    pub use clipline_settings::cloud::*;
}

pub mod games {
    #[allow(unused_imports)]
    pub use clipline_settings::games::*;
}

pub mod hotkey {
    #[allow(unused_imports)]
    pub use clipline_settings::hotkey::*;
}

pub mod osu {
    #[allow(unused_imports)]
    pub use clipline_settings::osu::*;
}

pub mod persistence {
    pub use clipline_settings::persistence::*;
}

pub mod types {
    #[allow(unused_imports)]
    pub use clipline_settings::types::*;
}

pub mod validation {
    pub use clipline_settings::validation::*;
}

use crate::service::{
    AudioOptions, CaptureRegion, CaptureSource, RecordingMode, ReplayStorageOptions, ServiceOptions,
};

pub trait AppSettingsServiceExt {
    fn to_service_options(&self, lol_url: Option<String>) -> Result<ServiceOptions, String>;
}

impl AppSettingsServiceExt for AppSettings {
    fn to_service_options(&self, lol_url: Option<String>) -> Result<ServiceOptions, String> {
        self.validate()?;
        Ok(ServiceOptions {
            capture_source: match self.capture_mode {
                CaptureMode::PrimaryMonitor => CaptureSource::PrimaryMonitor,
                CaptureMode::WindowTitle => {
                    CaptureSource::WindowTitle(self.window_title.trim().to_string())
                }
                CaptureMode::DisplayRegion => CaptureSource::DisplayRegion(CaptureRegion {
                    display_id: self.capture_region.display_id.clone(),
                    x: self.capture_region.x,
                    y: self.capture_region.y,
                    width: self.capture_region.width,
                    height: self.capture_region.height,
                }),
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
            replay_storage: match self.replay_storage.mode {
                ReplayStorageMode::Memory => ReplayStorageOptions::Memory,
                ReplayStorageMode::Disk => ReplayStorageOptions::Disk {
                    dir: normalize_replay_cache_dir(&self.replay_storage.disk_dir)?,
                    quota_bytes: replay_cache_quota_bytes_from_gb(
                        self.replay_storage.disk_quota_gb,
                    )?,
                },
            },
            disk_quota_bytes: quota_bytes_from_gb(self.disk_quota_gb)?,
            recording_mode: RecordingMode::ReplaysOnly,
            fps: self.effective_fps(),
            bitrate_bps: (self.effective_bitrate_mbps() * 1_000_000.0).round() as u32,
            video_encoder: self.video_encoder,
            output_resolution: self.output_resolution,
            output_resolution_bounds: self.effective_output_resolution_bounds(),
            decodable_codecs: vec![clipline_capture::probe::Codec::H264],
            audio: AudioOptions {
                output_enabled: self.audio.output_enabled,
                output_device_id: self
                    .audio
                    .output_device_id
                    .clone()
                    .filter(|id| !id.trim().is_empty()),
                output_volume: self.audio.output_volume,
                split_output_by_process: self.audio.split_output_by_process,
                mic_enabled: self.audio.mic_enabled,
                mic_device_id: self
                    .audio
                    .mic_device_id
                    .clone()
                    .filter(|id| !id.trim().is_empty()),
                mic_volume: self.audio.mic_volume,
                mic_channels: self.audio.mic_channels,
            },
        })
    }
}

impl From<GameRecordingMode> for RecordingMode {
    fn from(value: GameRecordingMode) -> Self {
        match value {
            GameRecordingMode::FullSession => Self::FullSession,
            GameRecordingMode::ReplaysOnly => Self::ReplaysOnly,
        }
    }
}

#[cfg(test)]
mod tests;
