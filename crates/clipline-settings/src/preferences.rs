//! Framework-neutral projection of the settings fields owned by the UI.
//!
//! Account identity, credentials, durable uploads, and osu! connection state
//! intentionally do not appear in this DTO. Applying a preference draft is a
//! narrow merge into the latest document, never a whole-document replacement.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::games::validate_custom_game_id;
use crate::types::MAX_ICON_DATA_URL_LEN;
use crate::validation::{MAX_CAPTURE_REGION_SIDE, MIN_CAPTURE_REGION_SIDE};
use crate::{
    normalize_cloud_visibility, normalize_hotkey, normalize_media_dir, normalize_replay_cache_dir,
    replay_cache_quota_bytes_from_gb, AdvancedRecordingSettings, AppSettings, AudioSettings,
    CaptureBackend, CaptureMode, CaptureRegionSettings, CustomGameSettings,
    GamePluginReviewSettings, GamePluginSettings, GameSettings, MatchEventSettings,
    OutputResolution, ReplayStorageSettings, TimelineMarkerSettings, UiTheme, UpdateChannel,
    VideoEncoder, VideoQuality,
};

/// Maximum first-party/plugin preference entries retained by one settings draft.
pub const MAX_SETTINGS_GAME_PLUGINS: usize = 16;
/// Maximum custom games retained by one settings draft.
pub const MAX_SETTINGS_CUSTOM_GAMES: usize = 128;
/// Maximum UTF-8 bytes retained by one ordinary settings field or label.
pub const MAX_SETTINGS_FIELD_BYTES: usize = 4 * 1024;
/// Maximum aggregate UTF-8 bytes retained by a Settings preferences projection.
pub const MAX_SETTINGS_COLLECTION_BYTES: usize = 8 * 1024 * 1024;
const MAX_CUSTOM_GAME_LEGACY_IDS: usize = 8;
const MAX_CUSTOM_GAME_LEGACY_ID_BYTES: usize = 256;

/// The three Cloud values controlled by the Settings UI.
///
/// Connection metadata, credential targets, cleanup targets, upload generation,
/// and upload records remain backend-owned and cannot enter this projection.
#[derive(Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CloudUploadPreferences {
    pub default_visibility: String,
    pub delete_local_after_upload: bool,
    pub auto_upload_rules: bool,
}

/// One bounded game-plugin preference retained by the UI draft.
///
/// The durable settings document uses a `BTreeMap`; the draft deliberately
/// uses a pre-reservable vector so every controller duplication can report an
/// allocation failure instead of growing a node-allocating map implicitly.
#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct GamePluginPreference {
    pub id: String,
    pub settings: GamePluginSettings,
}

#[derive(Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct GamePreferences {
    pub auto_detect: bool,
    pub pause_when_no_game: bool,
    pub plugins: Vec<GamePluginPreference>,
    pub custom_games: Vec<CustomGameSettings>,
}

/// Complete, framework-neutral set of preferences controlled by the Settings UI.
///
/// `buffer_seconds` is intentionally absent. It is a compatibility mirror derived
/// from `replay_window_s` whenever preferences are applied.
#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct SettingsPreferences {
    pub capture_mode: CaptureMode,
    pub capture_backend: CaptureBackend,
    pub window_title: String,
    pub capture_region: CaptureRegionSettings,
    pub games: GamePreferences,
    pub audio: AudioSettings,
    pub replay_window_s: f64,
    pub video_quality: VideoQuality,
    pub bitrate_mbps: f64,
    pub fps: u32,
    pub advanced_recording: AdvancedRecordingSettings,
    pub video_encoder: VideoEncoder,
    pub output_resolution: OutputResolution,
    pub disk_quota_gb: f64,
    pub media_dir: String,
    pub replay_storage: ReplayStorageSettings,
    pub hotkey: String,
    pub hotkey_secondary: Option<String>,
    pub open_on_startup: bool,
    pub close_to_tray: bool,
    pub minimize_to_tray: bool,
    pub legacy_timeline_editor: bool,
    pub ui_theme: UiTheme,
    pub update_channel: UpdateChannel,
    pub cloud: CloudUploadPreferences,
}

impl SettingsPreferences {
    /// Projects only UI-owned preferences from a complete durable document.
    pub fn from_document(document: &AppSettings) -> Result<Self, String> {
        validate_document_resources(document)?;
        Self {
            capture_mode: document.capture_mode.clone(),
            capture_backend: document.capture_backend,
            window_title: try_clone_string("window title", &document.window_title)?,
            capture_region: try_clone_capture_region(&document.capture_region)?,
            games: try_project_games(&document.games)?,
            audio: try_clone_audio(&document.audio)?,
            replay_window_s: document.replay_window_s,
            video_quality: document.video_quality,
            bitrate_mbps: document.bitrate_mbps,
            fps: document.fps,
            advanced_recording: document.advanced_recording,
            video_encoder: document.video_encoder,
            output_resolution: document.output_resolution,
            disk_quota_gb: document.disk_quota_gb,
            media_dir: try_clone_string("media directory", &document.media_dir)?,
            replay_storage: try_clone_replay_storage(&document.replay_storage)?,
            hotkey: try_clone_string("primary hotkey", &document.hotkey)?,
            hotkey_secondary: try_clone_option_string(
                "secondary hotkey",
                document.hotkey_secondary.as_deref(),
            )?,
            open_on_startup: document.open_on_startup,
            close_to_tray: document.close_to_tray,
            minimize_to_tray: document.minimize_to_tray,
            legacy_timeline_editor: document.legacy_timeline_editor,
            ui_theme: document.ui_theme,
            update_channel: document.update_channel,
            cloud: CloudUploadPreferences {
                default_visibility: try_clone_string(
                    "cloud default visibility",
                    &document.cloud.default_visibility,
                )?,
                delete_local_after_upload: document.cloud.delete_local_after_upload,
                auto_upload_rules: document.cloud.auto_upload_rules,
            },
        }
        .normalized()
    }

    /// Returns the canonical, validated form used for dirty comparison and commit.
    pub fn normalized(&self) -> Result<Self, String> {
        validate_preference_resources(self)?;
        validate_hidden_fields(self)?;
        let mut normalized = try_clone_preferences(self)?;
        normalized.hotkey = normalize_hotkey(&normalized.hotkey)?;
        normalized.hotkey_secondary = match normalized.hotkey_secondary.as_deref() {
            Some(raw) if !raw.trim().is_empty() => Some(normalize_hotkey(raw)?),
            _ => None,
        };
        normalize_game_preferences(&mut normalized.games)?;
        normalized.cloud.default_visibility = normalize_cloud_visibility(
            &normalized
                .cloud
                .default_visibility
                .trim()
                .to_ascii_lowercase(),
        );
        normalized.media_dir = normalize_media_dir(&normalized.media_dir)?
            .display()
            .to_string();
        normalized.advanced_recording = normalized.advanced_recording.repaired();
        normalized.bitrate_mbps = if normalized.advanced_recording.enabled {
            normalized.advanced_recording.bitrate_mbps
        } else {
            normalized
                .video_quality
                .bitrate_mbps(normalized.output_resolution)
        };
        if !normalized.replay_storage.disk_dir.trim().is_empty() {
            normalized.replay_storage.disk_dir =
                normalize_replay_cache_dir(&normalized.replay_storage.disk_dir)?
                    .display()
                    .to_string();
        }

        validate_preference_resources(&normalized)?;
        validate_hidden_fields(&normalized)?;

        validate_normalized_games(&normalized.games)?;

        // Validate the scalar/path preference set against a clean backend
        // profile without constructing the compatibility BTreeMap. Controller
        // normalization must remain allocation-fallible; map conversion occurs
        // only at the final document-commit boundary below.
        let result = try_clone_preferences(&normalized)?;
        let mut validation_document = AppSettings::default();
        normalized.write_owned_fields_without_games(&mut validation_document);
        validation_document.validate()?;
        Ok(result)
    }

    /// Fallibly duplicates an already bounded preferences value.
    pub fn try_clone_bounded(&self) -> Result<Self, String> {
        validate_preference_resources(self)?;
        validate_hidden_fields(self)?;
        try_clone_preferences(self)
    }

    /// Atomically merges UI-owned preferences into the latest durable document.
    ///
    /// The current document's Cloud connection, credential and upload fields,
    /// plus its complete osu! profile, are retained byte-for-byte. On failure,
    /// `document` is unchanged.
    pub fn apply_to_document(&self, document: &mut AppSettings) -> Result<(), String> {
        let normalized = self.normalized()?;
        normalized.write_owned_fields(document);
        Ok(())
    }

    fn write_owned_fields(mut self, document: &mut AppSettings) {
        let games = into_game_settings(std::mem::take(&mut self.games));
        self.write_owned_fields_without_games(document);
        document.games = games;
    }

    fn write_owned_fields_without_games(self, document: &mut AppSettings) {
        document.capture_mode = self.capture_mode;
        document.capture_backend = self.capture_backend;
        document.window_title = self.window_title;
        document.capture_region = self.capture_region;
        document.audio = self.audio;
        document.replay_window_s = self.replay_window_s;
        document.buffer_seconds = self.replay_window_s;
        document.video_quality = self.video_quality;
        document.bitrate_mbps = self.bitrate_mbps;
        document.fps = self.fps;
        document.advanced_recording = self.advanced_recording;
        document.video_encoder = self.video_encoder;
        document.output_resolution = self.output_resolution;
        document.disk_quota_gb = self.disk_quota_gb;
        document.media_dir = self.media_dir;
        document.replay_storage = self.replay_storage;
        document.hotkey = self.hotkey;
        document.hotkey_secondary = self.hotkey_secondary;
        document.open_on_startup = self.open_on_startup;
        document.close_to_tray = self.close_to_tray;
        document.minimize_to_tray = self.minimize_to_tray;
        document.legacy_timeline_editor = self.legacy_timeline_editor;
        document.ui_theme = self.ui_theme;
        document.update_channel = self.update_channel;
        document.cloud.default_visibility = self.cloud.default_visibility;
        document.cloud.delete_local_after_upload = self.cloud.delete_local_after_upload;
        document.cloud.auto_upload_rules = self.cloud.auto_upload_rules;
    }
}

fn validate_document_resources(document: &AppSettings) -> Result<(), String> {
    let mut aggregate = 0;
    validate_resources(
        &mut aggregate,
        PreferenceResourceView {
            window_title: &document.window_title,
            capture_region: &document.capture_region,
            audio: &document.audio,
            media_dir: &document.media_dir,
            replay_storage: &document.replay_storage,
            hotkey: &document.hotkey,
            hotkey_secondary: document.hotkey_secondary.as_deref(),
            cloud_visibility: &document.cloud.default_visibility,
        },
    )?;
    validate_game_resources(
        &mut aggregate,
        document.games.plugins.len(),
        document.games.plugins.keys().map(String::as_str),
        &document.games.custom_games,
    )?;
    validate_hidden_values(
        &document.capture_region,
        document.replay_storage.disk_quota_gb,
    )
}

fn validate_preference_resources(preferences: &SettingsPreferences) -> Result<(), String> {
    let mut aggregate = 0;
    validate_resources(
        &mut aggregate,
        PreferenceResourceView {
            window_title: &preferences.window_title,
            capture_region: &preferences.capture_region,
            audio: &preferences.audio,
            media_dir: &preferences.media_dir,
            replay_storage: &preferences.replay_storage,
            hotkey: &preferences.hotkey,
            hotkey_secondary: preferences.hotkey_secondary.as_deref(),
            cloud_visibility: &preferences.cloud.default_visibility,
        },
    )?;
    validate_game_resources(
        &mut aggregate,
        preferences.games.plugins.len(),
        preferences
            .games
            .plugins
            .iter()
            .map(|plugin| plugin.id.as_str()),
        &preferences.games.custom_games,
    )
}

struct PreferenceResourceView<'a> {
    window_title: &'a str,
    capture_region: &'a CaptureRegionSettings,
    audio: &'a AudioSettings,
    media_dir: &'a str,
    replay_storage: &'a ReplayStorageSettings,
    hotkey: &'a str,
    hotkey_secondary: Option<&'a str>,
    cloud_visibility: &'a str,
}

fn validate_resources(
    aggregate: &mut usize,
    view: PreferenceResourceView<'_>,
) -> Result<(), String> {
    account_field(aggregate, "window title", view.window_title)?;
    if let Some(display_id) = view.capture_region.display_id.as_deref() {
        if display_id.trim().is_empty() {
            return Err("capture display id must not be empty".into());
        }
        account_field(aggregate, "capture display id", display_id)?;
    }
    account_optional_field(
        aggregate,
        "output device id",
        view.audio.output_device_id.as_deref(),
    )?;
    account_optional_field(
        aggregate,
        "microphone device id",
        view.audio.mic_device_id.as_deref(),
    )?;
    account_field(aggregate, "media directory", view.media_dir)?;
    account_field(
        aggregate,
        "replay cache directory",
        &view.replay_storage.disk_dir,
    )?;
    account_field(aggregate, "primary hotkey", view.hotkey)?;
    account_optional_field(aggregate, "secondary hotkey", view.hotkey_secondary)?;
    account_field(aggregate, "cloud default visibility", view.cloud_visibility)?;
    Ok(())
}

fn validate_game_resources<'a>(
    aggregate: &mut usize,
    plugin_count: usize,
    plugin_ids: impl Iterator<Item = &'a str>,
    custom_games: &[CustomGameSettings],
) -> Result<(), String> {
    if plugin_count > MAX_SETTINGS_GAME_PLUGINS {
        return Err(format!(
            "game plugins must contain at most {MAX_SETTINGS_GAME_PLUGINS} entries"
        ));
    }
    if custom_games.len() > MAX_SETTINGS_CUSTOM_GAMES {
        return Err(format!(
            "custom games must contain at most {MAX_SETTINGS_CUSTOM_GAMES} entries"
        ));
    }
    for plugin_id in plugin_ids {
        account_field(aggregate, "game plugin id", plugin_id)?;
    }
    for game in custom_games {
        account_field(aggregate, "custom game id", &game.id)?;
        if game.legacy_ids.len() > MAX_CUSTOM_GAME_LEGACY_IDS {
            return Err(format!(
                "custom game legacy ids must contain at most {MAX_CUSTOM_GAME_LEGACY_IDS} entries"
            ));
        }
        for legacy_id in &game.legacy_ids {
            account_text(
                aggregate,
                "custom game legacy id",
                legacy_id,
                MAX_CUSTOM_GAME_LEGACY_ID_BYTES,
            )?;
        }
        account_field(aggregate, "custom game name", &game.name)?;
        account_field(aggregate, "custom game executable", &game.exe_name)?;
        account_optional_field(
            aggregate,
            "custom game process path",
            game.process_path.as_deref(),
        )?;
        account_field(aggregate, "custom game window title", &game.window_title)?;
        if let Some(icon) = game.icon.as_deref() {
            account_text(aggregate, "custom game icon", icon, MAX_ICON_DATA_URL_LEN)?;
        }
    }
    Ok(())
}

fn account_optional_field(
    aggregate: &mut usize,
    label: &str,
    value: Option<&str>,
) -> Result<(), String> {
    if let Some(value) = value {
        account_field(aggregate, label, value)?;
    }
    Ok(())
}

fn account_field(aggregate: &mut usize, label: &str, value: &str) -> Result<(), String> {
    account_text(aggregate, label, value, MAX_SETTINGS_FIELD_BYTES)
}

fn account_text(
    aggregate: &mut usize,
    label: &str,
    value: &str,
    maximum: usize,
) -> Result<(), String> {
    if value.len() > maximum {
        return Err(format!(
            "{label} is {} UTF-8 bytes; maximum is {maximum}",
            value.len()
        ));
    }
    *aggregate = aggregate
        .checked_add(value.len())
        .ok_or_else(|| "settings aggregate byte count overflowed".to_string())?;
    if *aggregate > MAX_SETTINGS_COLLECTION_BYTES {
        return Err(format!(
            "settings aggregate is {aggregate} UTF-8 bytes; maximum is {MAX_SETTINGS_COLLECTION_BYTES}"
        ));
    }
    Ok(())
}

fn validate_hidden_fields(preferences: &SettingsPreferences) -> Result<(), String> {
    validate_hidden_values(
        &preferences.capture_region,
        preferences.replay_storage.disk_quota_gb,
    )
}

fn validate_hidden_values(
    region: &CaptureRegionSettings,
    replay_cache_quota_gb: f64,
) -> Result<(), String> {
    if region.width < MIN_CAPTURE_REGION_SIDE || region.height < MIN_CAPTURE_REGION_SIDE {
        return Err("capture region must be at least 2x2 pixels".into());
    }
    if region.width > MAX_CAPTURE_REGION_SIDE || region.height > MAX_CAPTURE_REGION_SIDE {
        return Err("capture region is too large".into());
    }
    i64::from(region.x)
        .checked_add(i64::from(region.width))
        .and_then(|edge| i32::try_from(edge).ok())
        .ok_or_else(|| "capture region horizontal edge overflows".to_string())?;
    i64::from(region.y)
        .checked_add(i64::from(region.height))
        .and_then(|edge| i32::try_from(edge).ok())
        .ok_or_else(|| "capture region vertical edge overflows".to_string())?;
    replay_cache_quota_bytes_from_gb(replay_cache_quota_gb)
        .map_err(|error| format!("replay cache quota: {error}"))?;
    Ok(())
}

fn normalize_game_preferences(games: &mut GamePreferences) -> Result<(), String> {
    let mut normalized_plugins: Vec<GamePluginPreference> = Vec::new();
    normalized_plugins
        .try_reserve_exact(games.plugins.len())
        .map_err(|_| "allocate normalized game plugin preferences".to_string())?;
    for mut plugin in std::mem::take(&mut games.plugins) {
        plugin.id = try_normalize_plugin_id(&plugin.id)?;
        if plugin.id.is_empty() {
            continue;
        }
        if let Some(existing) = normalized_plugins
            .iter_mut()
            .find(|existing| existing.id == plugin.id)
        {
            existing.settings = plugin.settings;
        } else {
            normalized_plugins.push(plugin);
        }
    }
    normalized_plugins.sort_unstable_by(|left, right| left.id.cmp(&right.id));
    games.plugins = normalized_plugins;

    for game in &mut games.custom_games {
        try_normalize_custom_game(game)?;
    }
    for index in 0..games.custom_games.len() {
        if validate_custom_game_id(&games.custom_games[index].id).is_ok() {
            continue;
        }
        let legacy_id = std::mem::take(&mut games.custom_games[index].id);
        let base = try_migrated_custom_game_id(&legacy_id, &games.custom_games[index].name)?;
        let mut candidate = try_clone_string("migrated custom game id", &base)?;
        let mut suffix = 2_u32;
        while custom_game_id_is_occupied(&games.custom_games, index, &candidate) {
            candidate = try_suffixed_custom_game_id(&base, suffix)?;
            suffix = suffix
                .checked_add(1)
                .ok_or_else(|| "custom game id suffix exhausted".to_string())?;
        }

        let game = &mut games.custom_games[index];
        game.id = candidate;
        if !legacy_id.is_empty()
            && !game.legacy_ids.contains(&legacy_id)
            && game.legacy_ids.len() < MAX_CUSTOM_GAME_LEGACY_IDS
        {
            game.legacy_ids
                .try_reserve(1)
                .map_err(|_| "allocate custom game legacy id".to_string())?;
            game.legacy_ids.push(legacy_id);
        }
        try_normalize_custom_game(game)?;
    }
    Ok(())
}

fn validate_normalized_games(games: &GamePreferences) -> Result<(), String> {
    for (index, game) in games.custom_games.iter().enumerate() {
        validate_custom_game_id(&game.id)?;
        if games.custom_games[..index]
            .iter()
            .any(|prior| prior.id == game.id)
        {
            return Err(format!("custom game id {:?} is duplicated", game.id));
        }
        if game.name.is_empty() {
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

fn try_normalize_plugin_id(raw: &str) -> Result<String, String> {
    let mut normalized = String::new();
    normalized
        .try_reserve_exact(raw.trim().len())
        .map_err(|_| "allocate normalized game plugin id".to_string())?;
    let mut separator_pending = false;
    for character in raw.trim().chars() {
        if character.is_ascii_alphanumeric() {
            if separator_pending && !normalized.is_empty() {
                normalized.push('_');
            }
            normalized.push(character.to_ascii_lowercase());
            separator_pending = false;
        } else if !normalized.is_empty() {
            separator_pending = true;
        }
    }
    Ok(normalized)
}

fn try_normalize_custom_game(game: &mut CustomGameSettings) -> Result<(), String> {
    trim_string_in_place(&mut game.id);
    trim_string_in_place(&mut game.name);
    trim_string_in_place(&mut game.exe_name);
    trim_string_in_place(&mut game.window_title);

    if let Some(path) = &mut game.process_path {
        trim_string_in_place(path);
        if path.is_empty() {
            game.process_path = None;
        }
    }
    if game
        .icon
        .as_deref()
        .is_some_and(|icon| !icon.starts_with("data:image/") || icon.len() > MAX_ICON_DATA_URL_LEN)
    {
        game.icon = None;
    }

    let legacy_ids = std::mem::take(&mut game.legacy_ids);
    let mut normalized_legacy_ids = Vec::new();
    normalized_legacy_ids
        .try_reserve_exact(legacy_ids.len().min(MAX_CUSTOM_GAME_LEGACY_IDS))
        .map_err(|_| "allocate normalized custom game legacy ids".to_string())?;
    for mut id in legacy_ids {
        trim_string_in_place(&mut id);
        if id.is_empty()
            || id.len() > MAX_CUSTOM_GAME_LEGACY_ID_BYTES
            || normalized_legacy_ids.contains(&id)
            || normalized_legacy_ids.len() == MAX_CUSTOM_GAME_LEGACY_IDS
        {
            continue;
        }
        normalized_legacy_ids.push(id);
    }
    game.legacy_ids = normalized_legacy_ids;
    Ok(())
}

fn trim_string_in_place(value: &mut String) {
    let trimmed_start = value.len() - value.trim_start().len();
    let trimmed_end = value.trim_end().len();
    value.truncate(trimmed_end);
    if trimmed_start != 0 {
        value.drain(..trimmed_start);
    }
}

const MAX_CUSTOM_GAME_ID_BYTES: usize = 96;
const MIGRATED_CUSTOM_GAME_PREFIX: &str = "custom-migrated-";

fn try_migrated_custom_game_id(raw_id: &str, fallback_name: &str) -> Result<String, String> {
    let source = if raw_id.trim().is_empty() {
        fallback_name
    } else {
        raw_id
    };
    let mut result = String::new();
    result
        .try_reserve_exact(MAX_CUSTOM_GAME_ID_BYTES)
        .map_err(|_| "allocate migrated custom game id".to_string())?;
    result.push_str(MIGRATED_CUSTOM_GAME_PREFIX);
    let slug_start = result.len();
    let mut separator_pending = false;
    for character in source.trim().chars() {
        if character.is_ascii_alphanumeric() {
            if separator_pending
                && result.len() > slug_start
                && result.len() < MAX_CUSTOM_GAME_ID_BYTES
            {
                result.push('-');
            }
            if result.len() == MAX_CUSTOM_GAME_ID_BYTES {
                break;
            }
            result.push(character.to_ascii_lowercase());
            separator_pending = false;
        } else if result.len() > slug_start {
            separator_pending = true;
        }
    }
    if result.len() == slug_start {
        result.push_str("game");
    }
    Ok(result)
}

fn custom_game_id_is_occupied(
    games: &[CustomGameSettings],
    current_index: usize,
    candidate: &str,
) -> bool {
    games.iter().enumerate().any(|(index, game)| {
        index != current_index && validate_custom_game_id(&game.id).is_ok() && game.id == candidate
    })
}

fn try_suffixed_custom_game_id(base: &str, suffix: u32) -> Result<String, String> {
    let mut digits = [0_u8; 10];
    let mut value = suffix;
    let mut start = digits.len();
    loop {
        start -= 1;
        digits[start] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    let suffix_digits = std::str::from_utf8(&digits[start..])
        .map_err(|_| "format custom game id suffix".to_string())?;
    let suffix_len = 1_usize
        .checked_add(suffix_digits.len())
        .ok_or_else(|| "custom game id suffix length overflowed".to_string())?;
    let stem_len = MAX_CUSTOM_GAME_ID_BYTES
        .checked_sub(suffix_len)
        .ok_or_else(|| "custom game id suffix is too long".to_string())?;
    let mut candidate = String::new();
    candidate
        .try_reserve_exact(MAX_CUSTOM_GAME_ID_BYTES)
        .map_err(|_| "allocate suffixed custom game id".to_string())?;
    candidate.push_str(&base[..base.len().min(stem_len)]);
    candidate.push('-');
    candidate.push_str(suffix_digits);
    Ok(candidate)
}

fn into_game_settings(source: GamePreferences) -> GameSettings {
    // The controller never retains a node-allocating map. Conversion happens
    // once at the already validated document-commit boundary because the
    // compatibility schema is a BTreeMap and std exposes no fallible insert.
    let mut plugins = BTreeMap::new();
    for plugin in source.plugins {
        plugins.insert(plugin.id, plugin.settings);
    }
    GameSettings {
        auto_detect: source.auto_detect,
        pause_when_no_game: source.pause_when_no_game,
        plugins,
        custom_games: source.custom_games,
    }
}

fn try_clone_preferences(source: &SettingsPreferences) -> Result<SettingsPreferences, String> {
    Ok(SettingsPreferences {
        capture_mode: source.capture_mode.clone(),
        capture_backend: source.capture_backend,
        window_title: try_clone_string("window title", &source.window_title)?,
        capture_region: try_clone_capture_region(&source.capture_region)?,
        games: try_clone_game_preferences(&source.games)?,
        audio: try_clone_audio(&source.audio)?,
        replay_window_s: source.replay_window_s,
        video_quality: source.video_quality,
        bitrate_mbps: source.bitrate_mbps,
        fps: source.fps,
        advanced_recording: source.advanced_recording,
        video_encoder: source.video_encoder,
        output_resolution: source.output_resolution,
        disk_quota_gb: source.disk_quota_gb,
        media_dir: try_clone_string("media directory", &source.media_dir)?,
        replay_storage: try_clone_replay_storage(&source.replay_storage)?,
        hotkey: try_clone_string("primary hotkey", &source.hotkey)?,
        hotkey_secondary: try_clone_option_string(
            "secondary hotkey",
            source.hotkey_secondary.as_deref(),
        )?,
        open_on_startup: source.open_on_startup,
        close_to_tray: source.close_to_tray,
        minimize_to_tray: source.minimize_to_tray,
        legacy_timeline_editor: source.legacy_timeline_editor,
        ui_theme: source.ui_theme,
        update_channel: source.update_channel,
        cloud: CloudUploadPreferences {
            default_visibility: try_clone_string(
                "cloud default visibility",
                &source.cloud.default_visibility,
            )?,
            delete_local_after_upload: source.cloud.delete_local_after_upload,
            auto_upload_rules: source.cloud.auto_upload_rules,
        },
    })
}

fn try_clone_capture_region(
    source: &CaptureRegionSettings,
) -> Result<CaptureRegionSettings, String> {
    Ok(CaptureRegionSettings {
        display_id: try_clone_option_string("capture display id", source.display_id.as_deref())?,
        x: source.x,
        y: source.y,
        width: source.width,
        height: source.height,
    })
}

fn try_clone_audio(source: &AudioSettings) -> Result<AudioSettings, String> {
    Ok(AudioSettings {
        output_enabled: source.output_enabled,
        output_device_id: try_clone_option_string(
            "output device id",
            source.output_device_id.as_deref(),
        )?,
        output_volume: source.output_volume,
        split_output_by_process: source.split_output_by_process,
        mic_enabled: source.mic_enabled,
        mic_device_id: try_clone_option_string(
            "microphone device id",
            source.mic_device_id.as_deref(),
        )?,
        mic_volume: source.mic_volume,
        mic_channels: source.mic_channels,
    })
}

fn try_clone_replay_storage(
    source: &ReplayStorageSettings,
) -> Result<ReplayStorageSettings, String> {
    Ok(ReplayStorageSettings {
        mode: source.mode,
        disk_dir: try_clone_string("replay cache directory", &source.disk_dir)?,
        disk_quota_gb: source.disk_quota_gb,
        disk_acknowledged: source.disk_acknowledged,
    })
}

fn try_project_games(source: &GameSettings) -> Result<GamePreferences, String> {
    let mut plugins = Vec::new();
    plugins
        .try_reserve_exact(source.plugins.len())
        .map_err(|_| "allocate game plugin preferences".to_string())?;
    for (id, settings) in &source.plugins {
        plugins.push(GamePluginPreference {
            id: try_clone_string("game plugin id", id)?,
            settings: copy_game_plugin_settings(settings),
        });
    }
    Ok(GamePreferences {
        auto_detect: source.auto_detect,
        pause_when_no_game: source.pause_when_no_game,
        plugins,
        custom_games: try_clone_custom_games(&source.custom_games)?,
    })
}

fn try_clone_game_preferences(source: &GamePreferences) -> Result<GamePreferences, String> {
    let mut plugins = Vec::new();
    plugins
        .try_reserve_exact(source.plugins.len())
        .map_err(|_| "allocate game plugin preferences".to_string())?;
    for plugin in &source.plugins {
        plugins.push(GamePluginPreference {
            id: try_clone_string("game plugin id", &plugin.id)?,
            settings: copy_game_plugin_settings(&plugin.settings),
        });
    }
    Ok(GamePreferences {
        auto_detect: source.auto_detect,
        pause_when_no_game: source.pause_when_no_game,
        plugins,
        custom_games: try_clone_custom_games(&source.custom_games)?,
    })
}

fn try_clone_custom_games(
    source: &[CustomGameSettings],
) -> Result<Vec<CustomGameSettings>, String> {
    let mut custom_games = Vec::new();
    custom_games
        .try_reserve_exact(source.len())
        .map_err(|_| "allocate custom game preferences".to_string())?;
    for game in source {
        custom_games.push(try_clone_custom_game(game)?);
    }
    Ok(custom_games)
}

fn copy_game_plugin_settings(source: &GamePluginSettings) -> GamePluginSettings {
    GamePluginSettings {
        enabled: source.enabled,
        recording_mode: source.recording_mode,
        review: GamePluginReviewSettings {
            enabled: source.review.enabled,
            match_events: MatchEventSettings {
                enabled: source.review.match_events.enabled,
                user_kills: source.review.match_events.user_kills,
                user_deaths: source.review.match_events.user_deaths,
                user_assists: source.review.match_events.user_assists,
                team_kills: source.review.match_events.team_kills,
                team_deaths: source.review.match_events.team_deaths,
                enemy_kills: source.review.match_events.enemy_kills,
                enemy_deaths: source.review.match_events.enemy_deaths,
                objectives: source.review.match_events.objectives,
                turrets: source.review.match_events.turrets,
            },
            timeline_markers: TimelineMarkerSettings {
                enabled: source.review.timeline_markers.enabled,
                user_kills: source.review.timeline_markers.user_kills,
                user_deaths: source.review.timeline_markers.user_deaths,
                user_assists: source.review.timeline_markers.user_assists,
                objectives: source.review.timeline_markers.objectives,
                turrets: source.review.timeline_markers.turrets,
            },
        },
    }
}

fn try_clone_custom_game(source: &CustomGameSettings) -> Result<CustomGameSettings, String> {
    let mut legacy_ids = Vec::new();
    legacy_ids
        .try_reserve_exact(source.legacy_ids.len())
        .map_err(|_| "allocate custom game legacy ids".to_string())?;
    for id in &source.legacy_ids {
        legacy_ids.push(try_clone_string("custom game legacy id", id)?);
    }
    Ok(CustomGameSettings {
        id: try_clone_string("custom game id", &source.id)?,
        legacy_ids,
        name: try_clone_string("custom game name", &source.name)?,
        enabled: source.enabled,
        exe_name: try_clone_string("custom game executable", &source.exe_name)?,
        process_path: try_clone_option_string(
            "custom game process path",
            source.process_path.as_deref(),
        )?,
        window_title: try_clone_string("custom game window title", &source.window_title)?,
        recording_mode: source.recording_mode,
        icon: try_clone_option_string("custom game icon", source.icon.as_deref())?,
    })
}

fn try_clone_option_string(label: &str, value: Option<&str>) -> Result<Option<String>, String> {
    value
        .map(|value| try_clone_string(label, value))
        .transpose()
}

fn try_clone_string(label: &str, value: &str) -> Result<String, String> {
    let mut clone = String::new();
    clone
        .try_reserve_exact(value.len())
        .map_err(|_| format!("allocate {label}"))?;
    clone.push_str(value);
    Ok(clone)
}
