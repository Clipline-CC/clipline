//! Validation for the whole `AppSettings`: range checks, capture-region
//! bounds, replay-storage overlap, game identity, and cloud state. Plus
//! shared quota/path helpers used by both validation and persistence.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use super::types::ReplayStorageMode;
use super::{
    normalize_replay_cache_dir, replay_cache_quota_bytes_from_gb, AppSettings, CaptureMode,
};
use crate::service::OutputResolution;

pub const MIN_REPLAY_WINDOW_S: f64 = 5.0;
pub const MAX_REPLAY_WINDOW_S: f64 = 120.0;
pub const MIN_BITRATE_MBPS: f64 = 1.0;
pub const MAX_BITRATE_MBPS: f64 = 100.0;
pub const MIN_EXACT_FPS: u32 = 1;
pub const MAX_EXACT_FPS: u32 = 240;
pub const MIN_ADVANCED_OUTPUT_WIDTH: u32 = 640;
pub const MIN_ADVANCED_OUTPUT_HEIGHT: u32 = 360;
pub const MIN_AUDIO_VOLUME: f64 = 0.0;
pub const MAX_AUDIO_VOLUME: f64 = 2.0;
pub const MIN_CAPTURE_REGION_SIDE: u32 = 2;
pub const MAX_CAPTURE_REGION_SIDE: u32 = 16_384;

impl AppSettings {
    pub fn validate(&self) -> Result<(), String> {
        if matches!(self.capture_mode, CaptureMode::WindowTitle)
            && self.window_title.trim().is_empty()
        {
            return Err("window title is required for window capture".into());
        }
        if matches!(self.capture_mode, CaptureMode::DisplayRegion) {
            if self.capture_region.width < MIN_CAPTURE_REGION_SIDE
                || self.capture_region.height < MIN_CAPTURE_REGION_SIDE
            {
                return Err("capture region must be at least 2x2 pixels".into());
            }
            if self.capture_region.width > MAX_CAPTURE_REGION_SIDE
                || self.capture_region.height > MAX_CAPTURE_REGION_SIDE
            {
                return Err("capture region is too large".into());
            }
        }
        self.validate_games()?;
        validate_range(
            "output volume",
            self.audio.output_volume,
            MIN_AUDIO_VOLUME,
            MAX_AUDIO_VOLUME,
        )?;
        validate_range(
            "microphone volume",
            self.audio.mic_volume,
            MIN_AUDIO_VOLUME,
            MAX_AUDIO_VOLUME,
        )?;
        validate_range(
            "replay seconds",
            self.replay_window_s,
            MIN_REPLAY_WINDOW_S,
            MAX_REPLAY_WINDOW_S,
        )?;
        validate_range(
            "bitrate Mbps",
            self.effective_bitrate_mbps(),
            MIN_BITRATE_MBPS,
            MAX_BITRATE_MBPS,
        )?;
        if !matches!(self.fps, 30 | 60 | 90 | 120) {
            return Err("fps must be 30, 60, 90, or 120".into());
        }
        if self.advanced_recording.enabled
            && !(MIN_EXACT_FPS..=MAX_EXACT_FPS).contains(&self.advanced_recording.fps)
        {
            return Err(format!(
                "advanced fps must be between {MIN_EXACT_FPS} and {MAX_EXACT_FPS}"
            ));
        }
        if self.advanced_recording.enabled {
            validate_range(
                "advanced output width",
                f64::from(self.advanced_recording.output_width),
                f64::from(MIN_ADVANCED_OUTPUT_WIDTH),
                f64::from(MAX_CAPTURE_REGION_SIDE),
            )?;
            validate_range(
                "advanced output height",
                f64::from(self.advanced_recording.output_height),
                f64::from(MIN_ADVANCED_OUTPUT_HEIGHT),
                f64::from(MAX_CAPTURE_REGION_SIDE),
            )?;
        }
        if !self.update_channel.enabled() {
            return Err(format!(
                "{} update channel is not available yet",
                self.update_channel.label()
            ));
        }
        super::quota_bytes_from_gb(self.disk_quota_gb)?;
        self.media_dir_path()?;
        self.validate_replay_storage()?;
        let primary = super::hotkey::normalize_hotkey(&self.hotkey)?;
        let secondary = self
            .hotkey_secondary
            .as_deref()
            .filter(|secondary| !secondary.trim().is_empty())
            .map(super::hotkey::normalize_hotkey)
            .transpose()?;
        if secondary.as_deref() == Some(primary.as_str()) {
            return Err("secondary hotkey matches the primary hotkey".into());
        }
        let recording_primary = self
            .recording_hotkey
            .as_deref()
            .filter(|hotkey| !hotkey.trim().is_empty())
            .map(super::hotkey::normalize_hotkey)
            .transpose()?;
        let recording_secondary = self
            .recording_hotkey_secondary
            .as_deref()
            .filter(|hotkey| !hotkey.trim().is_empty())
            .map(super::hotkey::normalize_hotkey)
            .transpose()?;
        for recording in [&recording_primary, &recording_secondary]
            .into_iter()
            .flatten()
        {
            if recording == &primary || secondary.as_ref() == Some(recording) {
                return Err("recording hotkey matches a Save Replay hotkey".into());
            }
        }
        if recording_secondary == recording_primary && recording_primary.is_some() {
            return Err("secondary recording hotkey matches the primary recording hotkey".into());
        }
        let bookmark_primary = self
            .bookmark_hotkey
            .as_deref()
            .filter(|hotkey| !hotkey.trim().is_empty())
            .map(super::hotkey::normalize_hotkey)
            .transpose()?;
        let bookmark_secondary = self
            .bookmark_hotkey_secondary
            .as_deref()
            .filter(|hotkey| !hotkey.trim().is_empty())
            .map(super::hotkey::normalize_hotkey)
            .transpose()?;
        for bookmark in [&bookmark_primary, &bookmark_secondary]
            .into_iter()
            .flatten()
        {
            if bookmark == &primary || secondary.as_ref() == Some(bookmark) {
                return Err("bookmark hotkey matches a Save Replay hotkey".into());
            }
            if recording_primary.as_ref() == Some(bookmark)
                || recording_secondary.as_ref() == Some(bookmark)
            {
                return Err("bookmark hotkey matches a recording hotkey".into());
            }
        }
        if bookmark_secondary == bookmark_primary && bookmark_primary.is_some() {
            return Err("secondary bookmark hotkey matches the primary bookmark hotkey".into());
        }
        let screenshot_region_primary = self
            .screenshot_region_hotkey
            .as_deref()
            .filter(|hotkey| !hotkey.trim().is_empty())
            .map(super::hotkey::normalize_hotkey)
            .transpose()?;
        let screenshot_region_secondary = self
            .screenshot_region_hotkey_secondary
            .as_deref()
            .filter(|hotkey| !hotkey.trim().is_empty())
            .map(super::hotkey::normalize_hotkey)
            .transpose()?;
        let screenshot_screen_primary = self
            .screenshot_screen_hotkey
            .as_deref()
            .filter(|hotkey| !hotkey.trim().is_empty())
            .map(super::hotkey::normalize_hotkey)
            .transpose()?;
        let screenshot_screen_secondary = self
            .screenshot_screen_hotkey_secondary
            .as_deref()
            .filter(|hotkey| !hotkey.trim().is_empty())
            .map(super::hotkey::normalize_hotkey)
            .transpose()?;
        let screenshot_window_primary = self
            .screenshot_window_hotkey
            .as_deref()
            .filter(|hotkey| !hotkey.trim().is_empty())
            .map(super::hotkey::normalize_hotkey)
            .transpose()?;
        let screenshot_window_secondary = self
            .screenshot_window_hotkey_secondary
            .as_deref()
            .filter(|hotkey| !hotkey.trim().is_empty())
            .map(super::hotkey::normalize_hotkey)
            .transpose()?;
        let screenshot_binds = [
            ("region", &screenshot_region_primary, &screenshot_region_secondary),
            ("screen", &screenshot_screen_primary, &screenshot_screen_secondary),
            (
                "window",
                &screenshot_window_primary,
                &screenshot_window_secondary,
            ),
        ];
        for (label, shot_primary, shot_secondary) in screenshot_binds.iter() {
            for hotkey in [*shot_primary, *shot_secondary].into_iter().flatten() {
                if hotkey == &primary || secondary.as_ref() == Some(hotkey) {
                    return Err(format!(
                        "screenshot {label} hotkey matches a Save Replay hotkey"
                    ));
                }
                if recording_primary.as_ref() == Some(hotkey)
                    || recording_secondary.as_ref() == Some(hotkey)
                {
                    return Err(format!(
                        "screenshot {label} hotkey matches a recording hotkey"
                    ));
                }
                if bookmark_primary.as_ref() == Some(hotkey)
                    || bookmark_secondary.as_ref() == Some(hotkey)
                {
                    return Err(format!(
                        "screenshot {label} hotkey matches a bookmark hotkey"
                    ));
                }
            }
        }
        for (label_a, shot_primary_a, shot_secondary_a) in screenshot_binds.iter() {
            for (label_b, shot_primary_b, shot_secondary_b) in screenshot_binds.iter() {
                if label_a >= label_b {
                    continue;
                }
                let binds_a = [*shot_primary_a, *shot_secondary_a].into_iter().flatten();
                let binds_b = [*shot_primary_b, *shot_secondary_b].into_iter().flatten();
                for hotkey_a in binds_a.clone() {
                    for hotkey_b in binds_b.clone() {
                        if hotkey_a == hotkey_b {
                            return Err(format!(
                                "screenshot {label_a} hotkey matches a screenshot {label_b} hotkey"
                            ));
                        }
                    }
                }
            }
        }
        for (label, shot_primary, shot_secondary) in screenshot_binds.iter() {
            if shot_secondary.is_some() && shot_primary == shot_secondary && shot_primary.is_some()
            {
                return Err(format!(
                    "secondary screenshot {label} hotkey matches the primary screenshot {label} hotkey"
                ));
            }
        }
        self.cloud.validate()?;
        self.osu.validate()?;
        Ok(())
    }

    pub fn effective_bitrate_mbps(&self) -> f64 {
        if self.advanced_recording.enabled {
            self.advanced_recording.bitrate_mbps
        } else {
            self.video_quality.bitrate_mbps(self.output_resolution)
        }
    }

    fn validate_games(&self) -> Result<(), String> {
        let mut ids = HashSet::new();
        for game in &self.games.custom_games {
            let id = game.id.trim();
            crate::game_identity::validate_custom_game_id(id)?;
            if !ids.insert(id.to_ascii_lowercase()) {
                return Err(format!("custom game id {id:?} is duplicated"));
            }
            if game.legacy_ids.len() > 8
                || game
                    .legacy_ids
                    .iter()
                    .any(|legacy| legacy.is_empty() || legacy.len() > 256)
            {
                return Err(format!("custom game {id:?} has invalid legacy ids"));
            }
            if game.name.trim().is_empty() {
                return Err("custom game name is required".into());
            }
            if !game.has_match_identity() {
                return Err(format!(
                    "custom game {:?} needs a process or window identity",
                    game.name
                ));
            }
        }
        Ok(())
    }

    fn validate_replay_storage(&self) -> Result<(), String> {
        if matches!(self.replay_storage.mode, ReplayStorageMode::Memory) {
            return Ok(());
        }
        if !self.replay_storage.disk_acknowledged {
            return Err("disk replay buffer requires acknowledging SSD wear".into());
        }
        replay_cache_quota_bytes_from_gb(self.replay_storage.disk_quota_gb)?;
        let cache_dir = normalize_replay_cache_dir(&self.replay_storage.disk_dir)?;
        let media_dir = self.media_dir_path()?;
        if same_or_nested_path(&cache_dir, &media_dir)
            || same_or_nested_path(&media_dir, &cache_dir)
        {
            return Err("replay cache folder must be separate from the media folder".into());
        }
        Ok(())
    }
}

pub fn validate_range(name: &str, value: f64, min: f64, max: f64) -> Result<(), String> {
    if !value.is_finite() || value < min || value > max {
        return Err(format!("{name} must be between {min} and {max}"));
    }
    Ok(())
}

pub fn same_or_nested_path(child: &Path, parent: &Path) -> bool {
    if cfg!(windows) {
        let child = windows_component_keys(child);
        let parent = windows_component_keys(parent);
        return child == parent
            || (child.len() > parent.len() && child[..parent.len()] == parent[..]);
    }
    let child = normalize_components(child);
    let parent = normalize_components(parent);
    child == parent || child.starts_with(&parent)
}

fn normalize_components(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        out.push(component.as_os_str());
    }
    out
}

fn windows_component_keys(path: &Path) -> Vec<String> {
    path.components()
        .filter_map(|component| match component {
            std::path::Component::Prefix(prefix) => Some(windows_prefix_key(prefix.kind())),
            std::path::Component::RootDir => Some("\\".to_string()),
            std::path::Component::CurDir => None,
            std::path::Component::ParentDir => Some("..".to_string()),
            std::path::Component::Normal(value) => {
                Some(value.to_string_lossy().to_ascii_lowercase())
            }
        })
        .collect()
}

fn windows_prefix_key(prefix: std::path::Prefix<'_>) -> String {
    match prefix {
        std::path::Prefix::Disk(drive) | std::path::Prefix::VerbatimDisk(drive) => {
            format!("disk:{}", char::from(drive).to_ascii_lowercase())
        }
        std::path::Prefix::UNC(server, share) | std::path::Prefix::VerbatimUNC(server, share) => {
            format!(
                "unc:{}\\{}",
                server.to_string_lossy().to_ascii_lowercase(),
                share.to_string_lossy().to_ascii_lowercase()
            )
        }
        std::path::Prefix::Verbatim(value) => {
            format!("verbatim:{}", value.to_string_lossy().to_ascii_lowercase())
        }
        std::path::Prefix::DeviceNS(value) => {
            format!("device:{}", value.to_string_lossy().to_ascii_lowercase())
        }
    }
}

pub fn repair_fps(value: i64) -> u32 {
    const FPS_STOPS: [u32; 4] = [30, 60, 90, 120];
    let value = super::persistence::clamp_u32(value, FPS_STOPS[0], *FPS_STOPS.last().unwrap());
    FPS_STOPS
        .into_iter()
        .min_by_key(|candidate| value.abs_diff(*candidate))
        .unwrap_or(AppSettings::default().fps)
}

pub fn repair_video_quality_from_legacy_bitrate(
    mbps: f64,
    resolution: OutputResolution,
) -> super::types::VideoQuality {
    use super::types::VideoQuality;
    [
        VideoQuality::Compact,
        VideoQuality::Balanced,
        VideoQuality::Sharp,
        VideoQuality::Maximum,
    ]
    .into_iter()
    .min_by(|left, right| {
        let left = left.bitrate_mbps(resolution);
        let right = right.bitrate_mbps(resolution);
        (mbps - left)
            .abs()
            .partial_cmp(&(mbps - right).abs())
            .unwrap_or(std::cmp::Ordering::Equal)
    })
    .unwrap_or_default()
}

pub fn repair_disk_quota_gb(value: f64) -> f64 {
    const GIB_BYTES: u64 = 1024 * 1024 * 1024;
    value.clamp(0.0, (u64::MAX / GIB_BYTES) as f64)
}
