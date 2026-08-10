use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime};

use regex::Regex;
use reqwest::multipart::{Form, Part};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::State;
use tokio_util::io::ReaderStream;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use zip::write::SimpleFileOptions;

use super::{diagnostics, AudioDeviceLists, RuntimeState};
use crate::settings::AppSettings;

const DESCRIPTION_MIN_CHARS: usize = 10;
const DESCRIPTION_MAX_CHARS: usize = 4_000;
const PREPARED_LIFETIME: Duration = Duration::from_secs(30 * 60);
const ABANDONED_STAGE_AGE: Duration = PREPARED_LIFETIME;
const MAX_BUNDLE_BYTES: u64 = 25 * 1024 * 1024;
const FRONTEND_MESSAGE_BYTES: usize = 8 * 1024;
const FRONTEND_STACK_BYTES: usize = 16 * 1024;
const FRONTEND_EVENTS_PER_MINUTE: u32 = 60;
const SUPPORT_ENDPOINT: &str = env!("CLIPLINE_BUG_REPORT_ENDPOINT");

static SECRET_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)\b(authorization|(?:[a-z0-9]+[_-])*(?:token|secret|password|api[_-]?key))\b["']?\s*[:=]\s*["']?(?:(?:bearer|basic)\s+)?[^\s"',;}]+"#,
    )
    .expect("secret redaction regex")
});
static AUTH_SCHEME_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)\b(bearer)\s+[^\s"',;}]+"#).expect("authorization scheme redaction regex")
});
static EMAIL_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b[A-Z0-9._%+-]+@[A-Z0-9.-]+\.[A-Z]{2,}\b").expect("email redaction regex")
});
static PATH_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)\b[A-Z]:\\+(?:[^\\\r\n"]+\\+)*[^\\\r\n"]*"#).expect("path redaction regex")
});
static URL_QUERY_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(https?://[^\s?"']+)\?[^\s"']*"#).expect("URL redaction regex"));

pub(super) struct SupportState {
    prepared: Mutex<HashMap<String, PreparedReport>>,
    frontend_rate: Mutex<FrontendRate>,
}

impl Default for SupportState {
    fn default() -> Self {
        prune_abandoned_staging();
        Self {
            prepared: Mutex::new(HashMap::new()),
            frontend_rate: Mutex::new(FrontendRate::default()),
        }
    }
}

#[derive(Clone)]
struct PreparedReport {
    directory: PathBuf,
    bundle: PathBuf,
    submission_id: Uuid,
    description: String,
    sha256: String,
    compressed_bytes: u64,
    created_at: chrono::DateTime<chrono::Utc>,
    expires_at: SystemTime,
    cancel: UploadCancellation,
}

#[derive(Clone, Default)]
struct UploadCancellation {
    token: Arc<Mutex<CancellationToken>>,
}

impl UploadCancellation {
    fn token(&self) -> Result<CancellationToken, String> {
        self.token
            .lock()
            .map(|token| token.clone())
            .map_err(|_| "bug report cancellation lock was poisoned".to_string())
    }

    fn cancel(&self) -> Result<(), String> {
        self.token
            .lock()
            .map_err(|_| "bug report cancellation lock was poisoned".to_string())?
            .cancel();
        Ok(())
    }

    fn reset(&self) {
        if let Ok(mut token) = self.token.lock() {
            *token = CancellationToken::new();
        }
    }
}

struct FrontendRate {
    started: Instant,
    accepted: u32,
    suppressed: u64,
}

impl Default for FrontendRate {
    fn default() -> Self {
        Self {
            started: Instant::now(),
            accepted: 0,
            suppressed: 0,
        }
    }
}

#[derive(Debug, Serialize)]
pub(super) struct PreparedBugReport {
    token: String,
    submission_id: Uuid,
    files: Vec<String>,
    compressed_bytes: u64,
    expires_at: String,
}

#[derive(Debug, Serialize)]
pub(super) struct SupportCapabilities {
    upload_available: bool,
}

#[derive(Debug, Deserialize, Serialize)]
pub(super) struct SubmittedBugReport {
    report_id: String,
    received_at: String,
    expires_at: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct FrontendDiagnosticInput {
    level: String,
    event: String,
    message: String,
    #[serde(default)]
    stack: Option<String>,
}

#[derive(Serialize)]
struct UploadMetadata<'a> {
    schema_version: u32,
    submission_id: Uuid,
    description: &'a str,
    app_version: &'static str,
    build_commit: &'static str,
    generated_at: String,
    bundle_sha256: &'a str,
    bundle_bytes: u64,
}

#[tauri::command(async)]
pub(super) async fn prepare_bug_report(
    state: State<'_, SupportState>,
    runtime: State<'_, RuntimeState>,
    description: String,
) -> Result<PreparedBugReport, String> {
    validate_description(&description)?;
    remove_expired_prepared(&state);
    let settings = runtime.settings();
    let runtime_snapshot = runtime_snapshot(&runtime);
    let token = Uuid::new_v4().to_string();
    let submission_id = Uuid::new_v4();
    let directory = staging_root().join(&token);
    let bundle = directory.join("clipline-support.zip");
    let build_directory = directory.clone();
    let build_bundle = bundle.clone();
    let build = tauri::async_runtime::spawn_blocking(move || {
        build_support_bundle(
            &build_directory,
            &build_bundle,
            &settings,
            runtime_snapshot,
            submission_id,
        )
    })
    .await
    .map_err(|error| format!("support bundle task failed: {error}"))??;
    let created_at = chrono::Utc::now();
    let expires_at = SystemTime::now() + PREPARED_LIFETIME;
    let prepared = PreparedReport {
        directory,
        bundle,
        submission_id,
        description,
        sha256: build.sha256,
        compressed_bytes: build.compressed_bytes,
        created_at,
        expires_at,
        cancel: UploadCancellation::default(),
    };
    state
        .prepared
        .lock()
        .map_err(|_| "prepared report state lock was poisoned".to_string())?
        .insert(token.clone(), prepared);
    tracing::info!(
        event = "bug_report_prepared",
        submission_id = %submission_id,
        compressed_bytes = build.compressed_bytes,
        files = build.files.len()
    );
    Ok(PreparedBugReport {
        token,
        submission_id,
        files: build.files,
        compressed_bytes: build.compressed_bytes,
        expires_at: rfc3339(expires_at),
    })
}

#[tauri::command]
pub(super) async fn submit_bug_report(
    state: State<'_, SupportState>,
    token: String,
) -> Result<SubmittedBugReport, String> {
    let prepared = prepared_report(&state, &token)?;
    let cancel = prepared.cancel.token()?;
    if cancel.is_cancelled() {
        prepared.cancel.reset();
        return Err("bug report upload cancelled".into());
    }
    let endpoint = support_report_url()?;
    let file = tokio::fs::File::open(&prepared.bundle)
        .await
        .map_err(|error| format!("open prepared support bundle: {error}"))?;
    let stream = ReaderStream::new(file);
    let bundle = Part::stream_with_length(
        reqwest::Body::wrap_stream(stream),
        prepared.compressed_bytes,
    )
    .file_name("clipline-support.zip")
    .mime_str("application/zip")
    .map_err(|error| format!("build support attachment: {error}"))?;
    let metadata = UploadMetadata {
        schema_version: 1,
        submission_id: prepared.submission_id,
        description: &prepared.description,
        app_version: env!("CARGO_PKG_VERSION"),
        build_commit: build_commit(),
        generated_at: prepared.created_at.to_rfc3339(),
        bundle_sha256: &prepared.sha256,
        bundle_bytes: prepared.compressed_bytes,
    };
    let metadata = Part::text(
        serde_json::to_string(&metadata)
            .map_err(|error| format!("serialize bug report metadata: {error}"))?,
    )
    .mime_str("application/json")
    .map_err(|error| format!("build support metadata: {error}"))?;
    let form = Form::new()
        .part("metadata", metadata)
        .part("bundle", bundle);
    let request = crate::bounded_http::authenticated_stream_client()?
        .post(endpoint)
        .header("Idempotency-Key", prepared.submission_id.to_string())
        .timeout(crate::bounded_http::upload_timeout(
            prepared.compressed_bytes,
        ))
        .multipart(form)
        .send();
    let response = tokio::select! {
        response = request => response.map_err(|error| format!("send private bug report: {error}"))?,
        () = cancel.cancelled() => {
            prepared.cancel.reset();
            return Err("bug report upload cancelled".into());
        },
    };
    let status = response.status();
    if !status.is_success() {
        let message =
            crate::bounded_http::response_error_message(response, status, "bug report").await;
        tracing::warn!(
            event = "bug_report_upload_failed",
            submission_id = %prepared.submission_id,
            status = %status
        );
        return Err(format!("bug report was not accepted: {message}"));
    }
    let submitted: SubmittedBugReport = crate::bounded_http::response_json_limited(
        response,
        crate::bounded_http::ERROR_BODY_MAX_BYTES,
        "bug report",
    )
    .await?;
    if let Err(error) = remove_prepared(&state, &token, true) {
        tracing::warn!(
            event = "bug_report_staging_cleanup_failed",
            submission_id = %prepared.submission_id,
            error = %error
        );
    }
    tracing::info!(
        event = "bug_report_submitted",
        submission_id = %prepared.submission_id,
        report_id = %submitted.report_id
    );
    Ok(submitted)
}

#[tauri::command]
pub(super) fn cancel_bug_report(
    state: State<'_, SupportState>,
    token: String,
) -> Result<(), String> {
    let prepared = prepared_report(&state, &token)?;
    prepared.cancel.cancel()?;
    tracing::info!(
        event = "bug_report_cancel_requested",
        submission_id = %prepared.submission_id
    );
    Ok(())
}

#[tauri::command]
pub(super) fn discard_bug_report(
    state: State<'_, SupportState>,
    token: String,
) -> Result<(), String> {
    remove_prepared(&state, &token, true)
}

#[tauri::command(async)]
pub(super) async fn save_prepared_bug_report(
    state: State<'_, SupportState>,
    token: String,
) -> Result<String, String> {
    let prepared = prepared_report(&state, &token)?;
    let source = prepared.bundle;
    tauri::async_runtime::spawn_blocking(move || {
        let Some(target) = rfd::FileDialog::new()
            .set_title("Save Clipline support bundle")
            .set_file_name("clipline-support.zip")
            .add_filter("ZIP archive", &["zip"])
            .save_file()
        else {
            return Err("save cancelled".to_string());
        };
        std::fs::copy(&source, &target)
            .map_err(|error| format!("save support bundle {target:?}: {error}"))?;
        Ok(target.to_string_lossy().into_owned())
    })
    .await
    .map_err(|error| format!("save support bundle task failed: {error}"))?
}

#[tauri::command]
pub(super) fn open_diagnostics_folder() -> Result<(), String> {
    let directory = diagnostics::diagnostics_directory()
        .ok_or_else(|| "diagnostics are not initialized".to_string())?;
    crate::windows::open_with_shell(directory.as_os_str(), "open diagnostics folder")
}

#[tauri::command]
pub(super) fn diagnostics_location() -> Result<String, String> {
    diagnostics::diagnostics_directory()
        .map(|directory| directory.to_string_lossy().into_owned())
        .ok_or_else(|| "diagnostics are not initialized".to_string())
}

#[tauri::command]
pub(super) fn support_capabilities() -> SupportCapabilities {
    SupportCapabilities {
        upload_available: support_report_url().is_ok(),
    }
}

#[tauri::command]
pub(super) fn log_frontend_event(
    state: State<'_, SupportState>,
    input: FrontendDiagnosticInput,
) -> Result<(), String> {
    validate_frontend_event(&input)?;
    let suppressed = {
        let mut rate = state
            .frontend_rate
            .lock()
            .map_err(|_| "frontend diagnostic rate lock was poisoned".to_string())?;
        if rate.started.elapsed() >= Duration::from_secs(60) {
            rate.started = Instant::now();
            rate.accepted = 0;
        }
        if rate.accepted >= FRONTEND_EVENTS_PER_MINUTE {
            rate.suppressed = rate.suppressed.saturating_add(1);
            return Ok(());
        }
        rate.accepted += 1;
        std::mem::take(&mut rate.suppressed)
    };
    let message = redact_generic(&input.message);
    let stack = input.stack.as_deref().map(redact_generic);
    match input.level.as_str() {
        "debug" => tracing::debug!(
            event = "frontend_diagnostic",
            frontend_event = %input.event,
            message = %message,
            stack = stack.as_deref().unwrap_or(""),
            suppressed_since_last = suppressed
        ),
        "info" => tracing::info!(
            event = "frontend_diagnostic",
            frontend_event = %input.event,
            message = %message,
            stack = stack.as_deref().unwrap_or(""),
            suppressed_since_last = suppressed
        ),
        "warn" => tracing::warn!(
            event = "frontend_diagnostic",
            frontend_event = %input.event,
            message = %message,
            stack = stack.as_deref().unwrap_or(""),
            suppressed_since_last = suppressed
        ),
        "error" => tracing::error!(
            event = "frontend_diagnostic",
            frontend_event = %input.event,
            message = %message,
            stack = stack.as_deref().unwrap_or(""),
            suppressed_since_last = suppressed
        ),
        _ => unreachable!("level validated"),
    }
    Ok(())
}

fn validate_description(description: &str) -> Result<(), String> {
    let length = description.trim().chars().count();
    if length < DESCRIPTION_MIN_CHARS {
        return Err(format!(
            "Describe the problem in at least {DESCRIPTION_MIN_CHARS} characters."
        ));
    }
    if length > DESCRIPTION_MAX_CHARS {
        return Err(format!(
            "Problem descriptions are limited to {DESCRIPTION_MAX_CHARS} characters."
        ));
    }
    Ok(())
}

fn validate_frontend_event(input: &FrontendDiagnosticInput) -> Result<(), String> {
    if !matches!(input.level.as_str(), "debug" | "info" | "warn" | "error") {
        return Err("frontend diagnostic level is invalid".into());
    }
    if input.event.is_empty()
        || input.event.len() > 64
        || !input
            .event
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    {
        return Err("frontend diagnostic event name is invalid".into());
    }
    if input.message.len() > FRONTEND_MESSAGE_BYTES {
        return Err("frontend diagnostic message is too large".into());
    }
    if input
        .stack
        .as_ref()
        .is_some_and(|stack| stack.len() > FRONTEND_STACK_BYTES)
    {
        return Err("frontend diagnostic stack is too large".into());
    }
    Ok(())
}

struct BundleBuild {
    files: Vec<String>,
    compressed_bytes: u64,
    sha256: String,
}

struct StagingBuildGuard {
    directory: PathBuf,
    preserve: bool,
}

impl StagingBuildGuard {
    fn new(directory: &Path) -> Self {
        Self {
            directory: directory.to_path_buf(),
            preserve: false,
        }
    }

    fn preserve(&mut self) {
        self.preserve = true;
    }
}

impl Drop for StagingBuildGuard {
    fn drop(&mut self) {
        if !self.preserve {
            let _ = std::fs::remove_dir_all(&self.directory);
        }
    }
}

fn build_support_bundle(
    directory: &Path,
    bundle: &Path,
    settings: &AppSettings,
    runtime: serde_json::Value,
    submission_id: Uuid,
) -> Result<BundleBuild, String> {
    std::fs::create_dir_all(directory)
        .map_err(|error| format!("create support staging directory: {error}"))?;
    let mut staging_guard = StagingBuildGuard::new(directory);
    let snapshot = directory.join("snapshot");
    let log_files = diagnostics::snapshot_to(&snapshot)?;
    let redactor = BundleRedactor::from_settings(settings);
    let mut entries = Vec::<(String, Vec<u8>)>::new();
    for path in log_files {
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let text = std::fs::read_to_string(&path)
            .map_err(|error| format!("read diagnostic snapshot {path:?}: {error}"))?;
        entries.push((format!("logs/{name}"), redactor.redact(&text).into_bytes()));
    }
    entries.push(("system.json".into(), json_bytes(&system_snapshot())?));
    entries.push((
        "settings.redacted.json".into(),
        json_bytes(&safe_settings(settings))?,
    ));
    entries.push(("runtime.json".into(), json_bytes(&runtime)?));
    entries.sort_by(|left, right| left.0.cmp(&right.0));

    let files = entries
        .iter()
        .map(|(name, bytes)| {
            serde_json::json!({
                "path": name,
                "bytes": bytes.len(),
                "sha256": hex_sha256(bytes),
            })
        })
        .collect::<Vec<_>>();
    let manifest = serde_json::json!({
        "schema_version": 1,
        "submission_id": submission_id,
        "generated_at": chrono::Utc::now().to_rfc3339(),
        "app": {
            "version": env!("CARGO_PKG_VERSION"),
            "build_commit": build_commit(),
            "channel": "desktop",
        },
        "logger": {
            "dropped_lines": diagnostics::dropped_lines(),
            "write_errors": diagnostics::write_errors(),
            "max_local_bytes": diagnostics::max_local_bytes(),
        },
        "redactions": [
            "paths", "window_titles", "device_ids", "account_fields",
            "credentials", "email_addresses", "url_queries"
        ],
        "files": files,
    });
    entries.insert(0, ("manifest.json".into(), json_bytes(&manifest)?));

    let file = std::fs::File::create(bundle)
        .map_err(|error| format!("create support bundle {bundle:?}: {error}"))?;
    let mut archive = zip::ZipWriter::new(file);
    let options = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(0o600);
    for (name, bytes) in &entries {
        archive
            .start_file(name, options)
            .map_err(|error| format!("start support bundle entry {name}: {error}"))?;
        archive
            .write_all(bytes)
            .map_err(|error| format!("write support bundle entry {name}: {error}"))?;
    }
    let file = archive
        .finish()
        .map_err(|error| format!("finish support bundle: {error}"))?;
    file.sync_all()
        .map_err(|error| format!("sync support bundle: {error}"))?;
    let compressed_bytes = file
        .metadata()
        .map_err(|error| format!("read support bundle size: {error}"))?
        .len();
    if compressed_bytes > MAX_BUNDLE_BYTES {
        return Err(format!(
            "support bundle is too large ({compressed_bytes} bytes; limit {MAX_BUNDLE_BYTES})"
        ));
    }
    let bundle_bytes =
        std::fs::read(bundle).map_err(|error| format!("hash support bundle: {error}"))?;
    let files = entries.into_iter().map(|(name, _)| name).collect();
    let _ = std::fs::remove_dir_all(snapshot);
    staging_guard.preserve();
    Ok(BundleBuild {
        files,
        compressed_bytes,
        sha256: hex_sha256(&bundle_bytes),
    })
}

fn json_bytes(value: &serde_json::Value) -> Result<Vec<u8>, String> {
    serde_json::to_vec_pretty(&redact_json_strings(value))
        .map_err(|error| format!("serialize support bundle JSON: {error}"))
}

fn redact_json_strings(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::String(value) => serde_json::Value::String(redact_generic(value)),
        serde_json::Value::Array(values) => {
            serde_json::Value::Array(values.iter().map(redact_json_strings).collect())
        }
        serde_json::Value::Object(values) => serde_json::Value::Object(
            values
                .iter()
                .map(|(key, value)| (key.clone(), redact_json_strings(value)))
                .collect(),
        ),
        value => value.clone(),
    }
}

fn safe_settings(settings: &AppSettings) -> serde_json::Value {
    let enabled_plugins = settings
        .games
        .plugins
        .iter()
        .filter_map(|(id, plugin)| plugin.enabled.then_some(id.clone()))
        .collect::<Vec<_>>();
    serde_json::json!({
        "capture_mode": settings.capture_mode,
        "capture_backend": settings.capture_backend,
        "capture_region": {
            "width": settings.capture_region.width,
            "height": settings.capture_region.height,
        },
        "audio": {
            "output_enabled": settings.audio.output_enabled,
            "split_output_by_process": settings.audio.split_output_by_process,
            "mic_enabled": settings.audio.mic_enabled,
            "mic_channels": settings.audio.mic_channels,
        },
        "replay_window_s": settings.replay_window_s,
        "video_quality": settings.video_quality,
        "bitrate_mbps": settings.bitrate_mbps,
        "fps": settings.fps,
        "advanced_recording": settings.advanced_recording,
        "video_encoder": settings.video_encoder,
        "output_resolution": settings.output_resolution,
        "disk_quota_gb": settings.disk_quota_gb,
        "auto_delete_when_over_quota": settings.auto_delete_when_over_quota,
        "replay_storage": {
            "mode": settings.replay_storage.mode,
            "disk_quota_gb": settings.replay_storage.disk_quota_gb,
        },
        "features": {
            "open_on_startup": settings.open_on_startup,
            "close_to_tray": settings.close_to_tray,
            "minimize_to_tray": settings.minimize_to_tray,
            "legacy_timeline_editor": settings.legacy_timeline_editor,
            "ui_theme": settings.ui_theme,
            "update_channel": settings.update_channel,
            "game_auto_detect": settings.games.auto_detect,
            "enabled_game_plugins": enabled_plugins,
            "custom_game_count": settings.games.custom_games.len(),
            "cloud_configured": settings.cloud.connected(),
            "osu_configured": settings.osu.client_id.is_some(),
        }
    })
}

fn system_snapshot() -> serde_json::Value {
    let encoders = super::probe_encoders()
        .into_iter()
        .map(|encoder| {
            serde_json::json!({
                "id": encoder.id,
                "name": encoder.name,
                "codec": encoder.codec,
            })
        })
        .collect::<Vec<_>>();
    let display_count = super::list_displays().map_or(0, |displays| displays.len());
    let AudioDeviceLists { outputs, inputs } =
        super::list_audio_devices().unwrap_or(AudioDeviceLists {
            outputs: Vec::new(),
            inputs: Vec::new(),
        });
    serde_json::json!({
        "windows_build": clipline_capture::windows::wasapi::windows_build_number(),
        "architecture": std::env::consts::ARCH,
        "logical_cpus": std::thread::available_parallelism().map_or(1, usize::from),
        "total_memory_bytes": total_physical_memory(),
        "webview2": super::webview2_runtime_diagnostic(),
        "display_count": display_count,
        "audio_output_count": outputs.len(),
        "audio_input_count": inputs.len(),
        "encoders": encoders,
    })
}

fn runtime_snapshot(runtime: &RuntimeState) -> serde_json::Value {
    runtime
        .0
        .lock()
        .map(|inner| {
            serde_json::json!({
                "recording_desired": inner.recording_desired,
                "recorder_connected": inner.tx.is_some(),
                "recording_generation": inner.recording_generation,
                "active_game": inner.active_game.is_some(),
                "configured_capture_backend": inner.settings.capture_backend,
                "configured_video_encoder": inner.settings.video_encoder,
                "configured_replay_storage_mode": inner.settings.replay_storage.mode,
                "last_recorder_status": inner.last_recorder_status.as_ref().map(|status| {
                    serde_json::json!({
                        "recording": status.recording,
                        "waiting_for_game": status.waiting_for_game,
                        "segments": status.segments,
                        "buffered_s": status.buffered_s,
                        "buffered_mb": status.buffered_mb,
                        "full_session": status.full_session,
                        "actual_encoder": status.encoder,
                        "actual_capture_backend": status.capture_backend,
                    })
                }),
                "last_storage_status": inner.last_storage_status.as_ref().map(|status| {
                    serde_json::json!({
                        "total_bytes": status.total_bytes,
                        "quota_bytes": status.quota_bytes,
                        "over_quota": status.over_quota,
                    })
                }),
                "recent_recorder_error": inner.recent_recorder_error,
                "decodable_codecs": inner
                    .decodable_codecs
                    .iter()
                    .map(|codec| format!("{codec:?}"))
                    .collect::<Vec<_>>(),
            })
        })
        .unwrap_or_else(|_| serde_json::json!({"state_error": "runtime lock poisoned"}))
}

fn total_physical_memory() -> Option<u64> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    let mut status = MEMORYSTATUSEX {
        dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    (unsafe { GlobalMemoryStatusEx(&mut status) } != 0).then_some(status.ullTotalPhys)
}

fn support_report_url() -> Result<reqwest::Url, String> {
    let endpoint = reqwest::Url::parse(SUPPORT_ENDPOINT)
        .map_err(|error| format!("private bug report endpoint is invalid: {error}"))?;
    if endpoint.scheme() != "https" {
        return Err("private bug report endpoint must use HTTPS".into());
    }
    Ok(endpoint)
}

fn prepared_report(state: &SupportState, token: &str) -> Result<PreparedReport, String> {
    remove_expired_prepared(state);
    state
        .prepared
        .lock()
        .map_err(|_| "prepared report state lock was poisoned".to_string())?
        .get(token)
        .cloned()
        .ok_or_else(|| "prepared bug report is missing or expired".to_string())
}

fn remove_prepared(state: &SupportState, token: &str, delete: bool) -> Result<(), String> {
    let report = state
        .prepared
        .lock()
        .map_err(|_| "prepared report state lock was poisoned".to_string())?
        .remove(token)
        .ok_or_else(|| "prepared bug report is missing or expired".to_string())?;
    if delete {
        std::fs::remove_dir_all(&report.directory)
            .map_err(|error| format!("remove prepared support bundle: {error}"))?;
    }
    Ok(())
}

fn remove_expired_prepared(state: &SupportState) {
    let Ok(mut prepared) = state.prepared.lock() else {
        return;
    };
    let now = SystemTime::now();
    let expired = prepared
        .iter()
        .filter_map(|(token, report)| (report.expires_at <= now).then_some(token.clone()))
        .collect::<Vec<_>>();
    for token in expired {
        if let Some(report) = prepared.remove(&token) {
            let _ = std::fs::remove_dir_all(report.directory);
        }
    }
}

fn staging_root() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("Clipline")
        .join("support-staging")
}

fn prune_abandoned_staging() {
    let root = staging_root();
    let Ok(entries) = std::fs::read_dir(&root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let abandoned = entry
            .metadata()
            .ok()
            .and_then(|metadata| metadata.modified().ok())
            .and_then(|modified| SystemTime::now().duration_since(modified).ok())
            .is_some_and(|age| age >= ABANDONED_STAGE_AGE);
        if abandoned && path.is_dir() {
            let _ = std::fs::remove_dir_all(path);
        }
    }
}

struct BundleRedactor {
    sensitive: Vec<(String, String)>,
}

impl BundleRedactor {
    fn from_settings(settings: &AppSettings) -> Self {
        let user_profile = std::env::var("USERPROFILE").ok();
        let username = std::env::var("USERNAME").ok();
        let appdata = std::env::var("APPDATA").ok();
        let local_appdata = std::env::var("LOCALAPPDATA").ok();
        let mut values: Vec<(String, &str)> = vec![
            ("window".into(), settings.window_title.as_str()),
            ("media_dir".into(), settings.media_dir.as_str()),
            (
                "replay_dir".into(),
                settings.replay_storage.disk_dir.as_str(),
            ),
            ("cloud_host".into(), settings.cloud.host_url.as_str()),
        ];
        for value in [
            settings.cloud.public_url.as_deref(),
            settings.cloud.connected_user_id.as_deref(),
            settings.cloud.connected_username.as_deref(),
            settings.cloud.connected_display_name.as_deref(),
            settings.cloud.credential_target.as_deref(),
            settings.osu.client_id.as_deref(),
            settings.osu.user.as_deref(),
            user_profile.as_deref(),
            appdata.as_deref(),
            local_appdata.as_deref(),
            username.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            values.push(("private".into(), value));
        }
        for value in [
            settings.audio.output_device_id.as_deref(),
            settings.audio.mic_device_id.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            values.push(("audio_device".into(), value));
        }
        if let Some(display_id) = settings.capture_region.display_id.as_deref() {
            values.push(("display_device".into(), display_id));
        }
        for game in &settings.games.custom_games {
            for value in [
                game.name.as_str(),
                game.exe_name.as_str(),
                game.window_title.as_str(),
                game.process_path.as_deref().unwrap_or(""),
            ] {
                values.push(("custom_game".into(), value));
            }
        }
        let mut counter = HashMap::<String, usize>::new();
        let mut sensitive = values
            .into_iter()
            .filter(|(_, value)| value.trim().len() >= 3)
            .map(|(kind, value)| {
                let index = counter.entry(kind.clone()).or_default();
                *index += 1;
                (value.to_string(), format!("<{kind}:{index}>"))
            })
            .collect::<Vec<_>>();
        sensitive.sort_by_key(|item| std::cmp::Reverse(item.0.len()));
        Self { sensitive }
    }

    fn redact(&self, text: &str) -> String {
        let mut output = text.to_string();
        for (value, replacement) in &self.sensitive {
            output = replace_ascii_case_insensitive(&output, value, replacement);
        }
        redact_generic(&output)
    }
}

fn redact_generic(text: &str) -> String {
    let text = SECRET_PATTERN.replace_all(text, "$1=<redacted>");
    let text = AUTH_SCHEME_PATTERN.replace_all(&text, "$1 <redacted>");
    let text = EMAIL_PATTERN.replace_all(&text, "<email>");
    let text = URL_QUERY_PATTERN.replace_all(&text, "$1?<query-redacted>");
    PATH_PATTERN.replace_all(&text, "<path>").into_owned()
}

fn replace_ascii_case_insensitive(haystack: &str, needle: &str, replacement: &str) -> String {
    if needle.is_empty() {
        return haystack.to_string();
    }
    let lower_haystack = haystack.to_ascii_lowercase();
    let lower_needle = needle.to_ascii_lowercase();
    let mut output = String::with_capacity(haystack.len());
    let mut start = 0;
    while let Some(relative) = lower_haystack[start..].find(&lower_needle) {
        let found = start + relative;
        output.push_str(&haystack[start..found]);
        output.push_str(replacement);
        start = found + needle.len();
    }
    output.push_str(&haystack[start..]);
    output
}

fn hex_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn rfc3339(time: SystemTime) -> String {
    chrono::DateTime::<chrono::Utc>::from(time).to_rfc3339()
}

fn build_commit() -> &'static str {
    option_env!("CLIPLINE_BUILD_COMMIT").unwrap_or("unknown")
}

#[cfg(test)]
mod tests {
    use super::*;
    use clipline_test_utils::TestDir;

    #[test]
    fn descriptions_are_validated_by_trimmed_character_count() {
        assert!(validate_description("too short").is_err());
        assert!(validate_description("capture stopped after I changed displays").is_ok());
        assert!(validate_description(&"é".repeat(DESCRIPTION_MAX_CHARS + 1)).is_err());
    }

    #[test]
    fn frontend_events_are_bounded_and_named_safely() {
        assert!(validate_frontend_event(&FrontendDiagnosticInput {
            level: "error".into(),
            event: "unhandled_rejection".into(),
            message: "boom".into(),
            stack: Some("stack".into()),
        })
        .is_ok());
        assert!(validate_frontend_event(&FrontendDiagnosticInput {
            level: "error".into(),
            event: "../bad".into(),
            message: "boom".into(),
            stack: None,
        })
        .is_err());
    }

    #[test]
    fn export_redaction_removes_paths_accounts_queries_and_secrets() {
        let mut settings = AppSettings {
            media_dir: r"C:\Users\Alice\Videos\Clipline".into(),
            window_title: "Alice's ranked game".into(),
            ..AppSettings::default()
        };
        settings.cloud.connected_username = Some("alice99".into());
        settings.audio.mic_device_id = Some("private-microphone-id".into());
        let redactor = BundleRedactor::from_settings(&settings);
        let redacted = redactor.redact(
            r#"C:\Users\Alice\Videos\Clipline alice99 Alice's ranked game private-microphone-id user@example.com https://example.com/a?token=abc password=hunter2"#,
        );
        assert!(!redacted.contains("Alice"));
        assert!(!redacted.contains("alice99"));
        assert!(!redacted.contains("example.com/a?token=abc"));
        assert!(!redacted.contains("hunter2"));
        assert!(!redacted.contains("private-microphone-id"));
        assert!(redacted.contains("<audio_device:1>"));
        assert!(redacted.contains("<email>"));
    }

    #[test]
    fn export_redaction_consumes_authorization_schemes_and_quoted_json_values() {
        for (input, forbidden) in [
            (
                "Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.private.signature",
                "eyJhbGciOiJIUzI1NiJ9.private.signature",
            ),
            (r#""token": "abc123secretvalue""#, "abc123secretvalue"),
            (
                r#"{"client_secret":"oauth-client-secret"}"#,
                "oauth-client-secret",
            ),
            (
                "request failed with Bearer raw-standalone-token",
                "raw-standalone-token",
            ),
        ] {
            let redacted = redact_generic(input);
            assert!(
                !redacted.contains(forbidden),
                "secret value remained in redacted output: {redacted}"
            );
            assert!(
                redacted.contains("<redacted>"),
                "redaction marker was missing from: {redacted}"
            );
        }
    }

    #[test]
    fn export_redaction_preserves_non_authentication_basic_prose() {
        assert_eq!(
            redact_generic("basic recording started with fallback settings"),
            "basic recording started with fallback settings"
        );
    }

    #[test]
    fn export_redaction_preserves_json_with_escaped_windows_paths() {
        let source = serde_json::json!({
            "fields": { "path": r"C:\Users\JsonUser\Videos\Clipline\clip.mp4" }
        })
        .to_string();

        let redacted = BundleRedactor::from_settings(&AppSettings::default()).redact(&source);
        let parsed: serde_json::Value =
            serde_json::from_str(&redacted).expect("redacted diagnostic line must remain JSON");

        assert_eq!(parsed["fields"]["path"], "<path>");
        assert!(!redacted.contains("JsonUser"));
    }

    #[test]
    fn bundled_json_redacts_nested_string_values() {
        let bytes = json_bytes(&serde_json::json!({
            "nested": {
                "message": "Authorization: Bearer should-not-ship",
                "items": ["user@example.com", "safe"]
            }
        }))
        .unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(!text.contains("should-not-ship"));
        assert!(!text.contains("user@example.com"));
        assert!(serde_json::from_str::<serde_json::Value>(&text).is_ok());
    }

    #[tokio::test]
    async fn upload_cancellation_requested_before_wait_registration_is_sticky() {
        let cancel = UploadCancellation::default();
        cancel.cancel().unwrap();
        let token = cancel.token().unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(20), token.cancelled())
                .await
                .is_ok(),
            "a cancellation requested during upload setup must still stop the later request"
        );
        cancel.reset();
        assert!(
            !cancel.token().unwrap().is_cancelled(),
            "a completed cancelled attempt must leave the prepared report retryable"
        );
    }

    #[test]
    fn failed_bundle_build_removes_staging_directory() {
        let root = TestDir::new("clipline-app", "support-bundle-failure-cleanup");
        let directory = root.path().join("prepared");
        let bundle = directory.join("clipline-support.zip");
        let result = build_support_bundle(
            &directory,
            &bundle,
            &AppSettings::default(),
            serde_json::json!({}),
            Uuid::new_v4(),
        );
        assert!(
            result.is_err(),
            "diagnostics are intentionally uninitialized"
        );
        assert!(
            !directory.exists(),
            "failed preparation must not retain copied diagnostic data"
        );
    }

    #[test]
    fn safe_settings_never_contains_raw_private_fields() {
        let mut settings = AppSettings {
            media_dir: r"C:\private\clips".into(),
            window_title: "private window".into(),
            ..AppSettings::default()
        };
        settings.cloud.connected_username = Some("private-user".into());
        settings.osu.client_id = Some("private-client".into());
        let json = safe_settings(&settings).to_string();
        for forbidden in [
            "private\\clips",
            "private window",
            "private-user",
            "private-client",
        ] {
            assert!(!json.contains(forbidden));
        }
    }

    #[test]
    fn generated_bundle_contains_only_allowlisted_entries() {
        let directory = TestDir::new("clipline-app", "support-bundle-fixture");
        let bundle = directory.path().join("report.zip");
        let settings = AppSettings::default();
        let entries = vec![
            (
                "system.json".to_string(),
                json_bytes(&serde_json::json!({"windows_build": 1})).unwrap(),
            ),
            (
                "settings.redacted.json".to_string(),
                json_bytes(&safe_settings(&settings)).unwrap(),
            ),
        ];
        let file = std::fs::File::create(&bundle).unwrap();
        let mut archive = zip::ZipWriter::new(file);
        let options = SimpleFileOptions::default();
        for (name, bytes) in entries {
            archive.start_file(name, options).unwrap();
            archive.write_all(&bytes).unwrap();
        }
        archive.finish().unwrap();

        let file = std::fs::File::open(bundle).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();
        let names = (0..archive.len())
            .map(|index| archive.by_index(index).unwrap().name().to_string())
            .collect::<Vec<_>>();
        assert_eq!(names, ["system.json", "settings.redacted.json"]);
    }

    #[test]
    fn sha256_is_stable_and_lowercase() {
        assert_eq!(
            hex_sha256(b"clipline"),
            "ba236189ece3d0fae04a9a2770472ac2c7b0820d21d20e793077dc89d679cde3"
        );
    }

    #[test]
    fn configured_report_url_is_the_exact_official_intake_route() {
        assert_eq!(
            support_report_url().unwrap().as_str(),
            "https://support.dain.cafe/api/v1/reports"
        );
    }
}
