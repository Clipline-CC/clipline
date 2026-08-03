//! Filesystem persistence for settings: path resolution, atomic writes,
//! legacy field repair, and the JSON `load_from`/`save_to` impls.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{Map, Value};

use super::hotkey::normalize_hotkey;
use super::types::{
    AdvancedRecordingSettings, AudioSettings, CaptureMode, CaptureRegionSettings, ReplayStorageMode,
};
use super::validation::{
    repair_disk_quota_gb, repair_fps, repair_video_quality_from_legacy_bitrate, MAX_BITRATE_MBPS,
    MAX_REPLAY_WINDOW_S, MIN_BITRATE_MBPS, MIN_REPLAY_WINDOW_S,
};
use super::AppSettings;
use super::{CloudSettings, CloudUploadRecord, OsuApiSettings, VideoEncoder};

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

pub struct SettingsStartupLoad {
    pub settings: AppSettings,
    pub warnings: Vec<String>,
    pub source: SettingsLoadSource,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingsLoadSource {
    Primary,
    Backup,
    Defaults,
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

    pub fn load_for_startup() -> SettingsStartupLoad {
        Self::load_for_startup_from(&super::settings_path())
    }

    pub fn load_for_startup_from(path: &Path) -> SettingsStartupLoad {
        let backup = backup_path(path);
        match load_classified(path) {
            Ok((settings, _)) => SettingsStartupLoad {
                settings,
                warnings: Vec::new(),
                source: SettingsLoadSource::Primary,
            },
            Err(SettingsLoadError::Missing) => match load_classified(&backup) {
                Ok((settings, _)) => SettingsStartupLoad {
                    settings,
                    warnings: vec![format!(
                        "Settings were recovered from {} because {} was missing.",
                        backup.display(),
                        path.display()
                    )],
                    source: SettingsLoadSource::Backup,
                },
                Err(SettingsLoadError::Missing) => SettingsStartupLoad {
                    settings: Self::default(),
                    warnings: Vec::new(),
                    source: SettingsLoadSource::Defaults,
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
                        source: SettingsLoadSource::Backup,
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

    pub fn load_from_object(object: &Map<String, Value>) -> Self {
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
        let (_, json) = self.normalized_json_bytes()?;
        persist_normalized_bytes(path, &json)
    }

    fn normalized_json_bytes(&self) -> Result<(Self, Vec<u8>), String> {
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
        let json = serde_json::to_vec_pretty(&settings).map_err(|e| e.to_string())?;
        Ok((settings, json))
    }

    pub fn save(&self) -> Result<(), String> {
        self.save_to(&super::settings_path())
    }
}

fn persist_normalized_bytes(path: &Path, json: &[u8]) -> Result<(), String> {
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
    write_file_atomically(path, json)
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
        source: SettingsLoadSource::Defaults,
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
    std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("Videos")
        .join("Clipline")
        .display()
        .to_string()
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

pub fn sibling_tmp_path(path: &Path) -> Result<PathBuf, String> {
    let file_name = path
        .file_name()
        .ok_or_else(|| "settings path must include a file name".to_string())?;
    let suffix = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut tmp_name = file_name.to_os_string();
    tmp_name.push(format!(".{}.{}.tmp", std::process::id(), suffix));
    Ok(path.with_file_name(tmp_name))
}

fn replace_file(from: &Path, to: &Path) -> Result<(), String> {
    clipline_shell::replace_file(from, to)
        .map_err(|error| format!("replace settings file {to:?}: {error}"))
}

fn normalize_channel(channel: super::UpdateChannel) -> super::UpdateChannel {
    if channel.enabled() {
        channel
    } else {
        super::UpdateChannel::Nightly
    }
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct SettingsRevision(u64);

impl SettingsRevision {
    pub const INITIAL: Self = Self(1);

    pub const fn get(self) -> u64 {
        self.0
    }

    fn checked_next(self) -> Result<Self, SettingsTransactionError> {
        self.0
            .checked_add(1)
            .map(Self)
            .ok_or(SettingsTransactionError::RevisionExhausted)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct AccountGeneration(u64);

impl AccountGeneration {
    pub const INITIAL: Self = Self(1);

    pub const fn get(self) -> u64 {
        self.0
    }

    fn checked_next(self) -> Result<Self, SettingsTransactionError> {
        self.0
            .checked_add(1)
            .map(Self)
            .ok_or(SettingsTransactionError::AccountGenerationExhausted)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CloudAccountIdentity {
    pub host_url: String,
    pub connected_user_id: Option<String>,
    pub credential_target: Option<String>,
}

impl CloudAccountIdentity {
    pub fn from_settings(settings: &CloudSettings) -> Self {
        Self {
            host_url: settings.host_url.clone(),
            connected_user_id: settings.connected_user_id.clone(),
            credential_target: settings.credential_target.clone(),
        }
    }
}

/// One transaction may reconcile a small, exact set of legacy/path-alias
/// records. Bounding the vector prevents a malformed adapter from turning a
/// settings commit into unbounded quadratic work.
pub const MAX_CLOUD_RECORD_CAS_SLOTS: usize = 64;

/// The durable operation which owns a whole-settings upload-record CAS.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloudRecordCasKind {
    /// Establish a new upload owner. Its generation must be newer than the
    /// expected record at the stable local key, when one exists.
    Admit { upload_generation: u64 },
    /// Advance state owned by one exact in-flight upload generation.
    Advance { upload_generation: u64 },
    /// Reconcile remote status without taking ownership from an in-flight
    /// upload or changing its durable identities.
    StatusSync,
}

/// An exact map slot. `record: None` means that the key must be absent.
#[derive(Clone, Debug, PartialEq)]
pub struct CloudRecordSlot {
    pub key: String,
    pub record: Option<CloudUploadRecord>,
}

/// Compare every expected slot, then remove only the explicitly expected path
/// aliases and optionally insert one replacement. The surrounding
/// [`SettingsTransaction`] supplies the document revision; the duplicated
/// account generation here binds the durable upload owner itself and prevents
/// an adapter from combining a current outer snapshot with a stale owner.
#[derive(Clone, Debug, PartialEq)]
pub struct CloudRecordCas {
    pub account: CloudAccountIdentity,
    pub account_generation: AccountGeneration,
    pub kind: CloudRecordCasKind,
    pub expected: Vec<CloudRecordSlot>,
    pub replacement: Option<CloudRecordSlot>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SettingsSnapshot {
    pub document: AppSettings,
    pub revision: SettingsRevision,
    pub account_generation: AccountGeneration,
    pub account: CloudAccountIdentity,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SettingsChange {
    ReplaceDocument(AppSettings),
    ReplaceUiPreferences(AppSettings),
    SetMediaRoot(String),
    ReplaceCloudSettings(CloudSettings),
    ReplaceCloudProfile(CloudSettings),
    UpsertCloudRecord {
        account: CloudAccountIdentity,
        key: String,
        expected: Option<CloudUploadRecord>,
        record: CloudUploadRecord,
    },
    RemoveCloudRecord {
        account: CloudAccountIdentity,
        key: String,
        expected: CloudUploadRecord,
    },
    CompareExchangeCloudRecords(CloudRecordCas),
    ReplaceOsuProfile(OsuApiSettings),
}

#[derive(Clone, Debug, PartialEq)]
pub struct SettingsTransaction {
    pub expected_revision: SettingsRevision,
    pub expected_account_generation: AccountGeneration,
    pub change: SettingsChange,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SettingsTransactionError {
    #[error("stale settings revision {expected:?}; current is {current:?}")]
    StaleRevision {
        expected: SettingsRevision,
        current: SettingsRevision,
    },
    #[error("stale cloud account generation {expected:?}; current is {current:?}")]
    StaleAccountGeneration {
        expected: AccountGeneration,
        current: AccountGeneration,
    },
    #[error("cloud account changed while the settings operation was in flight")]
    AccountChanged,
    #[error("cloud upload record changed while the settings operation was in flight")]
    StaleCloudRecord,
    #[error("settings revision is exhausted")]
    RevisionExhausted,
    #[error("cloud account generation is exhausted")]
    AccountGenerationExhausted,
    #[error("settings file changed outside this process")]
    ExternalModification,
    #[error("invalid settings transaction: {0}")]
    Validation(String),
    #[error("persist settings transaction: {0}")]
    Persistence(String),
    #[error("settings store lock poisoned")]
    LockPoisoned,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettingsProfile {
    settings_path: PathBuf,
    local_cache_dir: PathBuf,
    default_media_dir: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum SettingsProfileError {
    #[error("resolve isolated settings profile root {path:?}: {source}")]
    Resolve {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl SettingsProfile {
    pub fn installed() -> Self {
        Self {
            settings_path: settings_path(),
            local_cache_dir: local_cache_base(),
            default_media_dir: PathBuf::from(default_media_dir()),
        }
    }

    pub fn isolated(root: impl AsRef<Path>) -> Self {
        Self::try_isolated(root).expect("isolated settings profile root must be resolvable")
    }

    pub fn try_isolated(root: impl AsRef<Path>) -> Result<Self, SettingsProfileError> {
        let requested = root.as_ref().to_path_buf();
        let root =
            std::path::absolute(&requested).map_err(|source| SettingsProfileError::Resolve {
                path: requested,
                source,
            })?;
        Ok(Self {
            settings_path: root.join("settings.json"),
            local_cache_dir: root.join("cache"),
            default_media_dir: root.join("media"),
        })
    }

    pub fn settings_path(&self) -> &Path {
        &self.settings_path
    }

    pub fn local_cache_dir(&self) -> &Path {
        &self.local_cache_dir
    }

    pub fn default_media_dir(&self) -> &Path {
        &self.default_media_dir
    }
}

pub trait SettingsPathResolver {
    fn resolve_settings_profile(&self) -> SettingsProfile;
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum PrimaryState {
    Missing,
    Bytes(Vec<u8>),
    Unreadable(String),
}

struct StoreState {
    snapshot: SettingsSnapshot,
    primary: PrimaryState,
}

struct SettingsStoreInner {
    profile: SettingsProfile,
    startup_warnings: Vec<String>,
    commit_lock: Arc<Mutex<()>>,
    state: Mutex<StoreState>,
}

#[derive(Clone)]
pub struct SettingsStore {
    inner: Arc<SettingsStoreInner>,
}

impl SettingsStore {
    pub fn open(profile: SettingsProfile) -> Self {
        let mut startup = AppSettings::load_for_startup_from(profile.settings_path());
        if startup.source == SettingsLoadSource::Defaults {
            startup.settings.media_dir = profile.default_media_dir().display().to_string();
        }
        let account = CloudAccountIdentity::from_settings(&startup.settings.cloud);
        let primary = read_primary_state(profile.settings_path());
        Self {
            inner: Arc::new(SettingsStoreInner {
                commit_lock: shared_commit_lock(profile.settings_path()),
                profile,
                startup_warnings: startup.warnings,
                state: Mutex::new(StoreState {
                    snapshot: SettingsSnapshot {
                        document: startup.settings,
                        revision: SettingsRevision::INITIAL,
                        account_generation: AccountGeneration::INITIAL,
                        account,
                    },
                    primary,
                }),
            }),
        }
    }

    pub fn open_resolved(resolver: &dyn SettingsPathResolver) -> Self {
        Self::open(resolver.resolve_settings_profile())
    }

    pub fn profile(&self) -> &SettingsProfile {
        &self.inner.profile
    }

    pub fn startup_warnings(&self) -> &[String] {
        &self.inner.startup_warnings
    }

    pub fn snapshot(&self) -> Result<SettingsSnapshot, SettingsTransactionError> {
        self.inner
            .state
            .lock()
            .map(|state| state.snapshot.clone())
            .map_err(|_| SettingsTransactionError::LockPoisoned)
    }

    pub fn transact(
        &self,
        transaction: SettingsTransaction,
    ) -> Result<SettingsSnapshot, SettingsTransactionError> {
        // `SettingsStore` instances opened independently for the same profile
        // must serialize the compare-and-swap check with the atomic replace.
        // Clones naturally share this lock through `SettingsStoreInner`; the
        // registry extends that guarantee to separately opened adapters in
        // this process.
        let _commit_guard = self
            .inner
            .commit_lock
            .lock()
            .map_err(|_| SettingsTransactionError::LockPoisoned)?;
        let mut state = self
            .inner
            .state
            .lock()
            .map_err(|_| SettingsTransactionError::LockPoisoned)?;
        if transaction.expected_revision != state.snapshot.revision {
            return Err(SettingsTransactionError::StaleRevision {
                expected: transaction.expected_revision,
                current: state.snapshot.revision,
            });
        }
        if transaction.expected_account_generation != state.snapshot.account_generation {
            return Err(SettingsTransactionError::StaleAccountGeneration {
                expected: transaction.expected_account_generation,
                current: state.snapshot.account_generation,
            });
        }

        let mut next = state.snapshot.document.clone();
        apply_change(
            &mut next,
            transaction.change,
            &state.snapshot.account,
            state.snapshot.account_generation,
        )?;
        let (next, json) = next
            .normalized_json_bytes()
            .map_err(SettingsTransactionError::Validation)?;
        let next_account = CloudAccountIdentity::from_settings(&next.cloud);
        let next_revision = state.snapshot.revision.checked_next()?;
        let next_account_generation = if next_account == state.snapshot.account {
            state.snapshot.account_generation
        } else {
            state.snapshot.account_generation.checked_next()?
        };

        let current_primary = read_primary_state(self.inner.profile.settings_path());
        if current_primary != state.primary {
            return Err(SettingsTransactionError::ExternalModification);
        }
        persist_store_transaction(self.inner.profile.settings_path(), &json)
            .map_err(SettingsTransactionError::Persistence)?;

        state.primary = PrimaryState::Bytes(json);
        state.snapshot = SettingsSnapshot {
            document: next,
            revision: next_revision,
            account_generation: next_account_generation,
            account: next_account,
        };
        Ok(state.snapshot.clone())
    }

    pub fn replace_document(
        &self,
        expected: &SettingsSnapshot,
        document: AppSettings,
    ) -> Result<SettingsSnapshot, SettingsTransactionError> {
        self.transact(SettingsTransaction {
            expected_revision: expected.revision,
            expected_account_generation: expected.account_generation,
            change: SettingsChange::ReplaceDocument(document),
        })
    }
}

type SharedCommitLocks = Vec<(PathBuf, Weak<Mutex<()>>)>;

fn shared_commit_lock(path: &Path) -> Arc<Mutex<()>> {
    static LOCKS: OnceLock<Mutex<SharedCommitLocks>> = OnceLock::new();
    let locks = LOCKS.get_or_init(|| Mutex::new(Vec::new()));
    let mut locks = locks
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    locks.retain(|(_, lock)| lock.strong_count() > 0);
    if let Some(lock) = locks
        .iter()
        .find_map(|(candidate, lock)| (candidate == path).then(|| lock.upgrade()).flatten())
    {
        return lock;
    }
    let lock = Arc::new(Mutex::new(()));
    locks.push((path.to_path_buf(), Arc::downgrade(&lock)));
    lock
}

fn apply_change(
    document: &mut AppSettings,
    change: SettingsChange,
    current_account: &CloudAccountIdentity,
    current_account_generation: AccountGeneration,
) -> Result<(), SettingsTransactionError> {
    match change {
        SettingsChange::ReplaceDocument(next) => *document = next,
        SettingsChange::ReplaceUiPreferences(mut next) => {
            next.cloud.host_url = document.cloud.host_url.clone();
            next.cloud.public_url = document.cloud.public_url.clone();
            next.cloud.connected_user_id = document.cloud.connected_user_id.clone();
            next.cloud.connected_username = document.cloud.connected_username.clone();
            next.cloud.connected_display_name = document.cloud.connected_display_name.clone();
            next.cloud.credential_target = document.cloud.credential_target.clone();
            next.cloud.credential_cleanup_targets =
                document.cloud.credential_cleanup_targets.clone();
            next.cloud.uploads = document.cloud.uploads.clone();
            next.osu = document.osu.clone();
            *document = next;
        }
        SettingsChange::SetMediaRoot(media_dir) => document.media_dir = media_dir,
        SettingsChange::ReplaceCloudSettings(cloud) => document.cloud = cloud,
        SettingsChange::ReplaceCloudProfile(profile) => {
            document.cloud.host_url = profile.host_url;
            document.cloud.public_url = profile.public_url;
            document.cloud.connected_user_id = profile.connected_user_id;
            document.cloud.connected_username = profile.connected_username;
            document.cloud.connected_display_name = profile.connected_display_name;
            document.cloud.credential_target = profile.credential_target;
            document.cloud.credential_cleanup_targets = profile.credential_cleanup_targets;
        }
        SettingsChange::UpsertCloudRecord {
            account,
            key,
            expected,
            record,
        } => {
            if &account != current_account {
                return Err(SettingsTransactionError::AccountChanged);
            }
            if document.cloud.uploads.get(&key) != expected.as_ref() {
                return Err(SettingsTransactionError::StaleCloudRecord);
            }
            document.cloud.uploads.insert(key, record);
        }
        SettingsChange::RemoveCloudRecord {
            account,
            key,
            expected,
        } => {
            if &account != current_account {
                return Err(SettingsTransactionError::AccountChanged);
            }
            if document.cloud.uploads.get(&key) != Some(&expected) {
                return Err(SettingsTransactionError::StaleCloudRecord);
            }
            document.cloud.uploads.remove(&key);
        }
        SettingsChange::CompareExchangeCloudRecords(change) => apply_cloud_record_cas(
            &mut document.cloud,
            change,
            current_account,
            current_account_generation,
        )?,
        SettingsChange::ReplaceOsuProfile(osu) => document.osu = osu,
    }
    Ok(())
}

fn apply_cloud_record_cas(
    cloud: &mut CloudSettings,
    change: CloudRecordCas,
    current_account: &CloudAccountIdentity,
    current_account_generation: AccountGeneration,
) -> Result<(), SettingsTransactionError> {
    if &change.account != current_account {
        return Err(SettingsTransactionError::AccountChanged);
    }
    if change.account_generation != current_account_generation {
        return Err(SettingsTransactionError::StaleAccountGeneration {
            expected: change.account_generation,
            current: current_account_generation,
        });
    }
    validate_cloud_record_cas_shape(&change)?;

    // Exact slot comparison deliberately precedes generation-transition
    // validation. A delayed result is stale even if its proposed replacement
    // is also no longer a valid transition from the current record.
    for slot in &change.expected {
        if cloud.uploads.get(&slot.key) != slot.record.as_ref() {
            return Err(SettingsTransactionError::StaleCloudRecord);
        }
    }

    validate_cloud_record_cas_transition(&change)?;

    for slot in &change.expected {
        if slot.record.is_some() {
            cloud.uploads.remove(&slot.key);
        }
    }
    if let Some(replacement) = change.replacement {
        let record = replacement
            .record
            .expect("validated Cloud record replacement must be present");
        cloud.uploads.insert(replacement.key, record);
    }
    Ok(())
}

fn validate_cloud_record_cas_shape(
    change: &CloudRecordCas,
) -> Result<(), SettingsTransactionError> {
    if change.expected.is_empty() || change.expected.len() > MAX_CLOUD_RECORD_CAS_SLOTS {
        return invalid_cloud_record_cas(format!(
            "expected slot count must be between 1 and {MAX_CLOUD_RECORD_CAS_SLOTS}"
        ));
    }
    let mut keys = std::collections::BTreeSet::new();
    for slot in &change.expected {
        validate_cloud_record_slot_key(&slot.key)?;
        if !keys.insert(slot.key.as_str()) {
            return invalid_cloud_record_cas("expected slot keys must be unique");
        }
    }
    match &change.replacement {
        Some(replacement) => {
            validate_cloud_record_slot_key(&replacement.key)?;
            let Some(record) = replacement.record.as_ref() else {
                return invalid_cloud_record_cas("replacement slot must contain a record");
            };
            if record.local_clip_id != replacement.key {
                return invalid_cloud_record_cas(
                    "replacement key must exactly equal record local_clip_id",
                );
            }
            if !change
                .expected
                .iter()
                .any(|slot| slot.key == replacement.key)
            {
                return invalid_cloud_record_cas(
                    "replacement key must have an exact expected slot",
                );
            }
            let mut normalized = record.clone();
            normalized.normalize();
            if &normalized != record {
                return invalid_cloud_record_cas("replacement record must already be normalized");
            }
        }
        None if !matches!(change.kind, CloudRecordCasKind::StatusSync) => {
            return invalid_cloud_record_cas("admit and advance require a replacement record");
        }
        None => {}
    }
    Ok(())
}

fn validate_cloud_record_slot_key(key: &str) -> Result<(), SettingsTransactionError> {
    if key.is_empty() || key.trim() != key {
        return invalid_cloud_record_cas("record keys must be non-empty and normalized");
    }
    if key.len() > super::cloud::MAX_CLOUD_UPLOAD_ID_BYTES {
        return invalid_cloud_record_cas(format!(
            "record key is {} bytes; maximum is {}",
            key.len(),
            super::cloud::MAX_CLOUD_UPLOAD_ID_BYTES
        ));
    }
    Ok(())
}

fn validate_cloud_record_cas_transition(
    change: &CloudRecordCas,
) -> Result<(), SettingsTransactionError> {
    let replacement = change.replacement.as_ref();
    let replacement_record = replacement.and_then(|slot| slot.record.as_ref());
    let replacement_expected = replacement.and_then(|replacement| {
        change
            .expected
            .iter()
            .find(|slot| slot.key == replacement.key)
            .and_then(|slot| slot.record.as_ref())
    });

    match change.kind {
        CloudRecordCasKind::Admit { upload_generation } => {
            let replacement = replacement_record.expect("shape requires replacement");
            if replacement.upload_generation != Some(upload_generation) {
                return invalid_cloud_record_cas(
                    "admit replacement must carry its exact upload generation",
                );
            }
            if replacement_expected
                .is_some_and(|previous| previous.local_clip_id != replacement.local_clip_id)
            {
                return invalid_cloud_record_cas(
                    "admit cannot take a stable key owned by another local_clip_id",
                );
            }
            if let Some(previous_generation) =
                replacement_expected.and_then(|record| record.upload_generation)
            {
                if upload_generation <= previous_generation {
                    return invalid_cloud_record_cas(
                        "admit upload generation must be newer than the prior record",
                    );
                }
            }
        }
        CloudRecordCasKind::Advance { upload_generation } => {
            let Some(previous) = replacement_expected else {
                return invalid_cloud_record_cas(
                    "advance requires the prior record at the replacement key",
                );
            };
            let replacement = replacement_record.expect("shape requires replacement");
            if previous.upload_generation != Some(upload_generation)
                || replacement.upload_generation != Some(upload_generation)
            {
                return invalid_cloud_record_cas(
                    "advance must retain the exact owned upload generation",
                );
            }
            validate_stable_record_identities(previous, replacement)?;
        }
        CloudRecordCasKind::StatusSync => {
            if let Some(replacement) = replacement_record {
                let Some(previous) = replacement_expected else {
                    return invalid_cloud_record_cas(
                        "status sync replacement requires its prior record",
                    );
                };
                if previous.upload_generation != replacement.upload_generation {
                    return invalid_cloud_record_cas(
                        "status sync cannot change the upload generation",
                    );
                }
                validate_stable_record_identities(previous, replacement)?;
            }
        }
    }

    validate_cloud_record_cas_paths(change, replacement_expected, replacement_record)
}

fn validate_stable_record_identities(
    previous: &CloudUploadRecord,
    replacement: &CloudUploadRecord,
) -> Result<(), SettingsTransactionError> {
    if previous.local_clip_id != replacement.local_clip_id {
        return invalid_cloud_record_cas("record local_clip_id cannot change");
    }
    if previous.client_clip_id.is_some() && previous.client_clip_id != replacement.client_clip_id {
        return invalid_cloud_record_cas("record client_clip_id cannot change once assigned");
    }
    Ok(())
}

fn validate_cloud_record_cas_paths(
    change: &CloudRecordCas,
    replacement_expected: Option<&CloudUploadRecord>,
    replacement: Option<&CloudUploadRecord>,
) -> Result<(), SettingsTransactionError> {
    let anchor = replacement
        .or(replacement_expected)
        .or_else(|| change.expected.iter().find_map(|slot| slot.record.as_ref()))
        .ok_or_else(|| {
            SettingsTransactionError::Validation(
                "cloud record CAS without a replacement must remove an existing record".into(),
            )
        })?;
    let replacement_key = change.replacement.as_ref().map(|slot| slot.key.as_str());
    for slot in &change.expected {
        let Some(record) = slot.record.as_ref() else {
            if Some(slot.key.as_str()) != replacement_key {
                return invalid_cloud_record_cas(
                    "only the replacement key may have an absent expected slot",
                );
            }
            continue;
        };
        if Some(slot.key.as_str()) == replacement_key {
            continue;
        }
        let matches_new = replacement
            .is_some_and(|replacement| cloud_paths_equivalent(&record.path, &replacement.path));
        let matches_old = replacement_expected
            .is_some_and(|previous| cloud_paths_equivalent(&record.path, &previous.path));
        let matches_anchor = cloud_paths_equivalent(&record.path, &anchor.path);
        if !(matches_new || matches_old || matches_anchor) {
            return invalid_cloud_record_cas(
                "expected superseded records must have path-equivalent clip paths",
            );
        }
    }
    Ok(())
}

fn invalid_cloud_record_cas<T>(message: impl Into<String>) -> Result<T, SettingsTransactionError> {
    Err(SettingsTransactionError::Validation(format!(
        "cloud record CAS: {}",
        message.into()
    )))
}

/// Pure lexical equivalence used for durable record reconciliation. Windows
/// drive/UNC spellings are slash-, case-, and verbatim-prefix-insensitive on
/// every host so settings written on Windows behave identically in neutral CI.
#[must_use]
pub fn cloud_paths_equivalent(left: &str, right: &str) -> bool {
    if left == right {
        return true;
    }
    matches!(
        (windows_cloud_path_key(left), windows_cloud_path_key(right)),
        (Some(left), Some(right)) if left == right
    )
}

fn windows_cloud_path_key(path: &str) -> Option<String> {
    let mut normalized = path.trim().replace('/', "\\");
    let lower = normalized.to_ascii_lowercase();
    if lower.starts_with(r"\\?\unc\") {
        normalized = format!(r"\\{}", &normalized[8..]);
    } else if lower.starts_with(r"\\?\") {
        normalized = normalized[4..].to_string();
    }
    let bytes = normalized.as_bytes();
    let drive_path =
        bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'\\';
    if !drive_path && !normalized.starts_with(r"\\") {
        return None;
    }
    Some(normalized.to_ascii_lowercase())
}

fn read_primary_state(path: &Path) -> PrimaryState {
    match std::fs::read(path) {
        Ok(bytes) => PrimaryState::Bytes(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => PrimaryState::Missing,
        Err(error) => PrimaryState::Unreadable(error.to_string()),
    }
}

fn read_optional_bytes(path: &Path) -> Result<Option<Vec<u8>>, String> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

fn restore_optional_bytes(path: &Path, bytes: Option<&[u8]>) -> Result<(), String> {
    match bytes {
        Some(bytes) => write_file_atomically(path, bytes),
        None => match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.to_string()),
        },
    }
}

fn persist_store_transaction(path: &Path, json: &[u8]) -> Result<(), String> {
    persist_store_transaction_with(path, json, persist_normalized_bytes)
}

fn persist_store_transaction_with(
    path: &Path,
    json: &[u8],
    persist: impl FnOnce(&Path, &[u8]) -> Result<(), String>,
) -> Result<(), String> {
    let backup = backup_path(path);
    let old_primary = read_optional_bytes(path)?;
    let old_backup = read_optional_bytes(&backup)?;
    if let Err(primary) = persist(path, json) {
        let mut rollback = Vec::new();
        if let Err(error) = restore_optional_bytes(path, old_primary.as_deref()) {
            rollback.push(format!("restore primary: {error}"));
        }
        if let Err(error) = restore_optional_bytes(&backup, old_backup.as_deref()) {
            rollback.push(format!("restore backup: {error}"));
        }
        if rollback.is_empty() {
            return Err(primary);
        }
        return Err(format!(
            "{primary}; settings transaction rollback incomplete: {}",
            rollback.join(", ")
        ));
    }
    Ok(())
}

pub trait LibraryConfig {
    fn media_root(&self) -> Result<PathBuf, SettingsTransactionError>;
    fn disk_quota_bytes(&self) -> Result<Option<u64>, SettingsTransactionError>;
}

impl LibraryConfig for SettingsStore {
    fn media_root(&self) -> Result<PathBuf, SettingsTransactionError> {
        self.snapshot()?
            .document
            .media_dir_path()
            .map_err(SettingsTransactionError::Validation)
    }

    fn disk_quota_bytes(&self) -> Result<Option<u64>, SettingsTransactionError> {
        quota_bytes_from_gb(self.snapshot()?.document.disk_quota_gb)
            .map_err(SettingsTransactionError::Validation)
    }
}

pub trait CloudAccountStore {
    fn cloud_snapshot(&self) -> Result<SettingsSnapshot, SettingsTransactionError>;
    fn apply_cloud_transaction(
        &self,
        transaction: SettingsTransaction,
    ) -> Result<SettingsSnapshot, SettingsTransactionError>;
}

impl CloudAccountStore for SettingsStore {
    fn cloud_snapshot(&self) -> Result<SettingsSnapshot, SettingsTransactionError> {
        self.snapshot()
    }

    fn apply_cloud_transaction(
        &self,
        transaction: SettingsTransaction,
    ) -> Result<SettingsSnapshot, SettingsTransactionError> {
        self.transact(transaction)
    }
}

#[cfg(test)]
mod transaction_tests {
    use super::*;
    use clipline_test_utils::TestDir;

    #[test]
    fn checked_counters_fail_closed_at_exhaustion() {
        assert_eq!(
            SettingsRevision(u64::MAX).checked_next(),
            Err(SettingsTransactionError::RevisionExhausted)
        );
        assert_eq!(
            AccountGeneration(u64::MAX).checked_next(),
            Err(SettingsTransactionError::AccountGenerationExhausted)
        );
    }

    #[test]
    fn post_mutation_persistence_failure_restores_primary_and_backup_exactly() {
        let dir = TestDir::new("clipline-settings", "rollback-after-mutation");
        let primary_path = dir.path().join("settings.json");
        let backup_file = backup_path(&primary_path);
        let primary = b"original primary";
        let backup = b"original backup";
        std::fs::write(&primary_path, primary).unwrap();
        std::fs::write(&backup_file, backup).unwrap();

        let error =
            persist_store_transaction_with(&primary_path, b"replacement", |path, _replacement| {
                write_file_atomically(&backup_path(path), b"mutated backup")?;
                write_file_atomically(path, b"partial primary")?;
                Err("injected persistence failure after mutation".into())
            })
            .unwrap_err();

        assert_eq!(error, "injected persistence failure after mutation");
        assert_eq!(std::fs::read(&primary_path).unwrap(), primary);
        assert_eq!(std::fs::read(&backup_file).unwrap(), backup);
    }
}
