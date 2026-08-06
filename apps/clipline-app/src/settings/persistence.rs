//! Filesystem persistence for settings: path resolution, atomic writes,
//! legacy field repair, and the JSON `load_from`/`save_to` impls.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::service::VideoEncoder;
use crate::updates::normalize_channel;

use super::hotkey::normalize_hotkey;
use super::types::{
    AdvancedRecordingSettings, AudioSettings, CaptureMode, CaptureRegionSettings, ReplayStorageMode,
};
use super::validation::{
    repair_disk_quota_gb, repair_fps, repair_video_quality_from_legacy_bitrate, MAX_BITRATE_MBPS,
    MAX_REPLAY_WINDOW_S, MIN_BITRATE_MBPS, MIN_REPLAY_WINDOW_S,
};
use super::AppSettings;

static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);
static QUARANTINE_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
enum SettingsLoadError {
    Missing,
    Io(String),
    Invalid(String),
}

impl SettingsLoadError {
    fn describe(&self) -> &str {
        match self {
            Self::Missing => "file not found",
            Self::Io(error) | Self::Invalid(error) => error,
        }
    }

    fn is_invalid(&self) -> bool {
        matches!(self, Self::Invalid(_))
    }
}

pub(crate) struct SettingsStartupLoad {
    pub(crate) settings: AppSettings,
    pub(crate) warnings: Vec<String>,
    pub(crate) fresh_install: bool,
}

impl AppSettings {
    // Kept as the strict, caller-supplied-path loader for unit tests and
    // future import tooling; normal startup uses the recovery-aware wrapper.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn load_from(path: &Path) -> Result<Self, String> {
        load_classified(path)
            .map(|(settings, _)| settings)
            .map_err(|error| match error {
                SettingsLoadError::Missing => "file not found".to_string(),
                SettingsLoadError::Io(error) | SettingsLoadError::Invalid(error) => error,
            })
    }

    pub(crate) fn load_for_startup() -> SettingsStartupLoad {
        Self::load_for_startup_from(&super::settings_path())
    }

    pub(crate) fn load_for_startup_from(path: &Path) -> SettingsStartupLoad {
        let backup = backup_path(path);
        match load_classified(path) {
            Ok((settings, _)) => SettingsStartupLoad {
                settings,
                warnings: Vec::new(),
                fresh_install: false,
            },
            Err(SettingsLoadError::Missing) => match load_classified(&backup) {
                Ok((settings, _)) => SettingsStartupLoad {
                    settings,
                    warnings: vec![format!(
                        "Settings were recovered from {} because {} was missing.",
                        backup.display(),
                        path.display()
                    )],
                    fresh_install: false,
                },
                Err(SettingsLoadError::Missing) => SettingsStartupLoad {
                    settings: Self::default(),
                    warnings: Vec::new(),
                    fresh_install: true,
                },
                Err(backup_error) => startup_defaults_after_failure(
                    path,
                    &SettingsLoadError::Missing,
                    &backup,
                    backup_error,
                ),
            },
            Err(primary_error) => match load_classified(&backup) {
                Ok((settings, _)) => {
                    let quarantine = quarantine_if_invalid(path, &primary_error);
                    let mut warning = format!(
                        "Settings were recovered from {} after {} could not be loaded: {}.",
                        backup.display(),
                        path.display(),
                        primary_error.describe()
                    );
                    append_quarantine_result(&mut warning, path, quarantine);
                    SettingsStartupLoad {
                        settings,
                        warnings: vec![warning],
                        fresh_install: false,
                    }
                }
                Err(backup_error) => {
                    startup_defaults_after_failure(path, &primary_error, &backup, backup_error)
                }
            },
        }
    }

    fn load_from_json_bytes(bytes: &[u8]) -> Result<Self, SettingsLoadError> {
        let json = std::str::from_utf8(bytes)
            .map_err(|e| SettingsLoadError::Invalid(format!("settings are not UTF-8: {e}")))?;
        let value: Value = serde_json::from_str(json)
            .map_err(|e| SettingsLoadError::Invalid(format!("invalid settings JSON: {e}")))?;
        let object = value.as_object().ok_or_else(|| {
            SettingsLoadError::Invalid("settings file must be a JSON object".to_string())
        })?;
        let settings = Self::load_from_object(object);
        settings.validate().map_err(|error| {
            SettingsLoadError::Invalid(format!("invalid settings values: {error}"))
        })?;
        Ok(settings)
    }

    pub(crate) fn load_from_object(object: &Map<String, Value>) -> Self {
        let defaults = Self::default();
        let output_resolution =
            deserialize_field(object, "output_resolution").unwrap_or(defaults.output_resolution);
        let legacy_bitrate_mbps = f64_field(object, "bitrate_mbps")
            .map(|value| value.clamp(MIN_BITRATE_MBPS, MAX_BITRATE_MBPS))
            .unwrap_or(defaults.bitrate_mbps);
        let video_quality = deserialize_field(object, "video_quality").unwrap_or_else(|| {
            repair_video_quality_from_legacy_bitrate(legacy_bitrate_mbps, output_resolution)
        });
        let mut settings = Self {
            capture_mode: deserialize_field(object, "capture_mode")
                .unwrap_or_else(|| defaults.capture_mode.clone()),
            capture_backend: deserialize_field(object, "capture_backend")
                .unwrap_or(defaults.capture_backend),
            window_title: string_field(object, "window_title")
                .unwrap_or_else(|| defaults.window_title.clone()),
            capture_region: CaptureRegionSettings::load_from_value(object.get("capture_region")),
            games: deserialize_field(object, "games").unwrap_or_default(),
            audio: AudioSettings::load_from_value(object.get("audio")),
            buffer_seconds: defaults.buffer_seconds,
            replay_window_s: f64_field(object, "replay_window_s")
                .map(|value| value.clamp(MIN_REPLAY_WINDOW_S, MAX_REPLAY_WINDOW_S))
                .unwrap_or(defaults.replay_window_s),
            video_quality,
            bitrate_mbps: legacy_bitrate_mbps,
            fps: integer_field(object, "fps")
                .map(repair_fps)
                .unwrap_or(defaults.fps),
            advanced_recording: AdvancedRecordingSettings::load_from_value(
                object.get("advanced_recording"),
            ),
            video_encoder: deserialize_field(object, "video_encoder")
                .unwrap_or(defaults.video_encoder),
            output_resolution,
            disk_quota_gb: f64_field(object, "disk_quota_gb")
                .map(repair_disk_quota_gb)
                .unwrap_or(defaults.disk_quota_gb),
            media_dir: string_field(object, "media_dir")
                .and_then(|raw| normalize_media_dir(&raw).ok())
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| defaults.media_dir.clone()),
            replay_storage: deserialize_field(object, "replay_storage").unwrap_or_default(),
            hotkey: string_field(object, "hotkey")
                .and_then(|raw| normalize_hotkey(&raw).ok())
                .unwrap_or_else(|| defaults.hotkey.clone()),
            // An unparseable secondary is dropped (it is optional) instead of
            // failing or resetting the whole file.
            hotkey_secondary: string_field(object, "hotkey_secondary")
                .and_then(|raw| normalize_hotkey(&raw).ok()),
            open_on_startup: bool_field(object, "open_on_startup")
                .unwrap_or(defaults.open_on_startup),
            close_to_tray: bool_field(object, "close_to_tray").unwrap_or(defaults.close_to_tray),
            minimize_to_tray: bool_field(object, "minimize_to_tray")
                .unwrap_or(defaults.minimize_to_tray),
            legacy_timeline_editor: bool_field(object, "legacy_timeline_editor")
                .unwrap_or(defaults.legacy_timeline_editor),
            ui_theme: deserialize_field(object, "ui_theme").unwrap_or(defaults.ui_theme),
            update_channel: deserialize_field(object, "update_channel")
                .map(normalize_channel)
                .unwrap_or(defaults.update_channel),
            cloud: deserialize_field(object, "cloud").unwrap_or_default(),
            osu: deserialize_field(object, "osu").unwrap_or_default(),
        };

        settings.games.normalize();
        settings.cloud.normalize();
        settings.osu.normalize();
        if settings.hotkey_secondary.as_deref() == Some(settings.hotkey.as_str()) {
            settings.hotkey_secondary = None;
        }
        settings.buffer_seconds = super::compatibility_buffer_seconds(&settings);
        settings.bitrate_mbps = settings.effective_bitrate_mbps();
        if matches!(settings.capture_mode, CaptureMode::WindowTitle)
            && settings.window_title.trim().is_empty()
        {
            settings.capture_mode = defaults.capture_mode;
        }
        settings
    }

    pub fn save_to(&self, path: &Path) -> Result<(), String> {
        let mut settings = self.clone();
        settings.hotkey = normalize_hotkey(&settings.hotkey)?;
        settings.hotkey_secondary = match settings.hotkey_secondary.as_deref() {
            Some(raw) if !raw.trim().is_empty() => Some(normalize_hotkey(raw)?),
            _ => None,
        };
        settings.games.normalize();
        settings.cloud.normalize();
        settings.osu.normalize();
        settings.media_dir = settings.media_dir_path()?.display().to_string();
        settings.advanced_recording = settings.advanced_recording.repaired();
        settings.bitrate_mbps = settings.effective_bitrate_mbps();
        if matches!(settings.replay_storage.mode, ReplayStorageMode::Disk) {
            settings.replay_storage.disk_dir =
                normalize_replay_cache_dir(&settings.replay_storage.disk_dir)?
                    .display()
                    .to_string();
        }
        settings.buffer_seconds = super::compatibility_buffer_seconds(&settings);
        settings.validate()?;
        let json = serde_json::to_string_pretty(&settings).map_err(|e| e.to_string())?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let previous = match load_classified(path) {
            Ok((_, bytes)) => Some(bytes),
            Err(SettingsLoadError::Missing) => None,
            Err(error) => {
                return Err(format!(
                    "refusing to overwrite unreadable or invalid settings file {}: {}",
                    path.display(),
                    error.describe()
                ));
            }
        };
        if let Some(previous) = previous {
            write_file_atomically(&backup_path(path), &previous)
                .map_err(|error| format!("preserve last-known-good settings: {error}"))?;
        }
        write_file_atomically(path, json.as_bytes())
    }

    pub fn save(&self) -> Result<(), String> {
        self.save_to(&super::settings_path())
    }
}

fn load_classified(path: &Path) -> Result<(AppSettings, Vec<u8>), SettingsLoadError> {
    let bytes = std::fs::read(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            SettingsLoadError::Missing
        } else {
            SettingsLoadError::Io(error.to_string())
        }
    })?;
    let settings = AppSettings::load_from_json_bytes(&bytes)?;
    Ok((settings, bytes))
}

fn backup_path(path: &Path) -> PathBuf {
    let mut file_name = path.file_name().unwrap_or_default().to_os_string();
    file_name.push(".bak");
    path.with_file_name(file_name)
}

fn quarantine_if_invalid(
    path: &Path,
    error: &SettingsLoadError,
) -> Option<Result<PathBuf, String>> {
    if !error.is_invalid() {
        return None;
    }
    let suffix = QUARANTINE_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut file_name = path.file_name()?.to_os_string();
    file_name.push(format!(".corrupt.{}.{}", std::process::id(), suffix));
    let quarantine = path.with_file_name(file_name);
    Some(
        std::fs::rename(path, &quarantine)
            .map(|()| quarantine)
            .map_err(|error| error.to_string()),
    )
}

fn append_quarantine_result(
    warning: &mut String,
    original: &Path,
    result: Option<Result<PathBuf, String>>,
) {
    match result {
        Some(Ok(path)) => warning.push_str(&format!(
            " The invalid file was preserved as {}.",
            path.display()
        )),
        Some(Err(error)) => warning.push_str(&format!(
            " The invalid file at {} could not be quarantined ({error}); saves will remain blocked.",
            original.display()
        )),
        None => warning.push_str(&format!(
            " The unreadable path at {} was left untouched; saves will remain blocked until it is accessible.",
            original.display()
        )),
    }
}

fn startup_defaults_after_failure(
    primary: &Path,
    primary_error: &SettingsLoadError,
    backup: &Path,
    backup_error: SettingsLoadError,
) -> SettingsStartupLoad {
    let primary_quarantine = quarantine_if_invalid(primary, primary_error);
    let backup_quarantine = quarantine_if_invalid(backup, &backup_error);
    let mut warning = format!(
        "Clipline started with safe defaults because neither {} ({}) nor {} ({}) could be loaded.",
        primary.display(),
        primary_error.describe(),
        backup.display(),
        backup_error.describe()
    );
    if !matches!(primary_error, SettingsLoadError::Missing) {
        append_quarantine_result(&mut warning, primary, primary_quarantine);
    }
    if !matches!(backup_error, SettingsLoadError::Missing) {
        append_quarantine_result(&mut warning, backup, backup_quarantine);
    }
    SettingsStartupLoad {
        settings: AppSettings::default(),
        warnings: vec![warning],
        fresh_install: false,
    }
}

pub fn config_base() -> PathBuf {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("USERPROFILE")
                .map(PathBuf::from)
                .map(|home| home.join("AppData").join("Roaming"))
        })
        .unwrap_or_else(std::env::temp_dir)
        .join("Clipline")
}

pub fn local_cache_base() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("USERPROFILE")
                .map(PathBuf::from)
                .map(|home| home.join("AppData").join("Local"))
        })
        .unwrap_or_else(std::env::temp_dir)
        .join("Clipline")
}

pub fn settings_path() -> PathBuf {
    config_base().join("settings.json")
}

pub fn icon_cache_dir() -> PathBuf {
    config_base().join("icons")
}

pub fn audio_preview_cache_dir() -> PathBuf {
    config_base().join("audio-previews")
}

pub fn share_export_cache_dir() -> PathBuf {
    config_base().join("share-exports")
}

pub fn normalize_media_dir(raw: &str) -> Result<PathBuf, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("media folder is required".into());
    }
    let path = PathBuf::from(trimmed);
    if !path.is_absolute() {
        return Err("media folder must be an absolute path".into());
    }
    validate_media_scope_root(&path)?;
    Ok(path)
}

fn validate_media_scope_root(path: &Path) -> Result<(), String> {
    let comparable = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if comparable.parent().is_none() {
        return Err("media folder cannot be a filesystem or drive root".into());
    }

    for (label, variable) in [
        ("Windows profile", "USERPROFILE"),
        ("Windows", "SystemRoot"),
        ("ProgramData", "ProgramData"),
        ("Program Files", "ProgramFiles"),
        ("Program Files (x86)", "ProgramFiles(x86)"),
    ] {
        let Some(root) = std::env::var_os(variable).map(PathBuf::from) else {
            continue;
        };
        if !root.is_absolute() {
            continue;
        }
        let root = root.canonicalize().unwrap_or(root);
        if same_path(&comparable, &root) {
            return Err(format!("media folder cannot be the {label} root"));
        }
    }
    Ok(())
}

fn same_path(left: &Path, right: &Path) -> bool {
    super::validation::same_or_nested_path(left, right)
        && super::validation::same_or_nested_path(right, left)
}

pub fn default_media_dir() -> String {
    crate::service::default_clips_dir().display().to_string()
}

pub fn normalize_replay_cache_dir(raw: &str) -> Result<PathBuf, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("replay cache folder is required".into());
    }
    let path = PathBuf::from(trimmed);
    if !path.is_absolute() {
        return Err("replay cache folder must be an absolute path".into());
    }
    Ok(path)
}

pub fn replay_cache_quota_bytes_from_gb(gb: f64) -> Result<u64, String> {
    const GIB_BYTES: f64 = 1024.0 * 1024.0 * 1024.0;

    if !gb.is_finite() || gb < 0.25 {
        return Err("replay cache quota must be at least 0.25 GiB".into());
    }
    let bytes = gb * GIB_BYTES;
    if bytes > u64::MAX as f64 {
        return Err("replay cache quota is too large".into());
    }
    Ok(bytes.round() as u64)
}

pub fn quota_bytes_from_gb(gb: f64) -> Result<Option<u64>, String> {
    const GIB_BYTES: f64 = 1024.0 * 1024.0 * 1024.0;

    if !gb.is_finite() || gb < 0.0 {
        return Err("disk quota must be a non-negative finite number".into());
    }
    if gb == 0.0 {
        return Ok(None);
    }
    let bytes = gb * GIB_BYTES;
    if bytes > u64::MAX as f64 {
        return Err("disk quota is too large".into());
    }
    Ok(Some(bytes.round() as u64))
}

fn write_file_atomically(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let tmp = sibling_tmp_path(path)?;
    let legacy_tmp = legacy_sibling_tmp_path(path)?;
    let _ = std::fs::remove_file(&tmp);
    if legacy_tmp != tmp {
        let _ = std::fs::remove_file(&legacy_tmp);
    }
    {
        let mut file = std::fs::File::create(&tmp)
            .map_err(|e| format!("create temporary settings file: {e}"))?;
        file.write_all(bytes)
            .map_err(|e| format!("write temporary settings file: {e}"))?;
        file.sync_all()
            .map_err(|e| format!("sync temporary settings file: {e}"))?;
    }
    if let Err(error) = replace_file(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(error);
    }
    Ok(())
}

fn legacy_sibling_tmp_path(path: &Path) -> Result<PathBuf, String> {
    let file_name = path
        .file_name()
        .ok_or_else(|| "settings path must include a file name".to_string())?;
    let mut tmp_name = file_name.to_os_string();
    tmp_name.push(".tmp");
    Ok(path.with_file_name(tmp_name))
}

pub(crate) fn sibling_tmp_path(path: &Path) -> Result<PathBuf, String> {
    let file_name = path
        .file_name()
        .ok_or_else(|| "settings path must include a file name".to_string())?;
    let suffix = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut tmp_name = file_name.to_os_string();
    tmp_name.push(format!(".{}.{}.tmp", std::process::id(), suffix));
    Ok(path.with_file_name(tmp_name))
}

fn replace_file(from: &Path, to: &Path) -> Result<(), String> {
    crate::windows::replace_file(from, to)
        .map_err(|error| format!("replace settings file {to:?}: {error}"))
}

pub(crate) fn deserialize_field<T>(object: &Map<String, Value>, key: &str) -> Option<T>
where
    T: DeserializeOwned,
{
    object
        .get(key)
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
}

pub(crate) fn bool_field(object: &Map<String, Value>, key: &str) -> Option<bool> {
    object.get(key).and_then(Value::as_bool)
}

pub(crate) fn string_field(object: &Map<String, Value>, key: &str) -> Option<String> {
    object.get(key).and_then(Value::as_str).map(str::to_string)
}

pub(crate) fn optional_string_field(
    object: &Map<String, Value>,
    key: &str,
) -> Option<Option<String>> {
    match object.get(key)? {
        Value::Null => Some(None),
        Value::String(value) if value.trim().is_empty() => Some(None),
        Value::String(value) => Some(Some(value.clone())),
        _ => None,
    }
}

pub(crate) fn f64_field(object: &Map<String, Value>, key: &str) -> Option<f64> {
    object
        .get(key)
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite())
}

pub(crate) fn integer_field(object: &Map<String, Value>, key: &str) -> Option<i64> {
    let value = object.get(key)?;
    if let Some(value) = value.as_i64() {
        return Some(value);
    }
    if let Some(value) = value.as_u64() {
        return Some(value.min(i64::MAX as u64) as i64);
    }
    value.as_f64().and_then(|value| {
        value
            .is_finite()
            .then(|| value.round().clamp(i64::MIN as f64, i64::MAX as f64) as i64)
    })
}

pub(crate) fn i32_field(object: &Map<String, Value>, key: &str) -> Option<i32> {
    integer_field(object, key).map(|value| value.clamp(i32::MIN as i64, i32::MAX as i64) as i32)
}

pub(crate) fn clamp_u32(value: i64, min: u32, max: u32) -> u32 {
    value.clamp(i64::from(min), i64::from(max)) as u32
}

/// Used by `AppSettings::save_to` to tolerate unknown `video_encoder` values
/// (hand-edit, downgrade) by falling back to Auto.
pub(crate) fn deserialize_video_encoder<'de, D>(deserializer: D) -> Result<VideoEncoder, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value(value).unwrap_or(VideoEncoder::Auto))
}
