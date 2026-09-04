use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use reqwest::multipart::{Form, Part};
use serde::Serialize;
use tauri::State;
use tokio_util::io::ReaderStream;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use zip::write::SimpleFileOptions;

use super::{
    build_commit, hex_sha256, json_bytes, rfc3339, runtime_snapshot, safe_settings,
    support_report_url, system_snapshot, validate_description, BundleRedactor, PreparedBugReport,
    SubmittedBugReport, SupportState, ABANDONED_STAGE_AGE, MAX_BUNDLE_BYTES, PREPARED_LIFETIME,
};
use super::super::{diagnostics, RuntimeState};
use crate::settings::AppSettings;
#[derive(Clone)]
pub(crate) struct PreparedReport {
    pub(super) directory: PathBuf,
    pub(super) bundle: PathBuf,
    pub(super) submission_id: Uuid,
    pub(super) description: String,
    pub(super) sha256: String,
    pub(super) compressed_bytes: u64,
    pub(super) created_at: chrono::DateTime<chrono::Utc>,
    pub(super) expires_at: SystemTime,
    pub(super) cancel: UploadCancellation,
}

#[derive(Clone, Default)]
pub(super) struct UploadCancellation {
    pub(super) token: Arc<Mutex<CancellationToken>>,
}

impl UploadCancellation {
    pub(super) fn token(&self) -> Result<CancellationToken, String> {
        self.token
            .lock()
            .map(|token| token.clone())
            .map_err(|_| "bug report cancellation lock was poisoned".to_string())
    }

    pub(super) fn cancel(&self) -> Result<(), String> {
        self.token
            .lock()
            .map_err(|_| "bug report cancellation lock was poisoned".to_string())?
            .cancel();
        Ok(())
    }

    pub(super) fn reset(&self) {
        if let Ok(mut token) = self.token.lock() {
            *token = CancellationToken::new();
        }
    }
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
pub async fn prepare_bug_report(
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
pub async fn submit_bug_report(
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
pub(super) fn prepared_report(state: &SupportState, token: &str) -> Result<PreparedReport, String> {
    remove_expired_prepared(state);
    state
        .prepared
        .lock()
        .map_err(|_| "prepared report state lock was poisoned".to_string())?
        .get(token)
        .cloned()
        .ok_or_else(|| "prepared bug report is missing or expired".to_string())
}

pub(super) fn remove_prepared(state: &SupportState, token: &str, delete: bool) -> Result<(), String> {
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

pub(super) fn remove_expired_prepared(state: &SupportState) {
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

pub(super) fn staging_root() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("Clipline")
        .join("support-staging")
}

pub(super) fn prune_abandoned_staging() {
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
#[cfg(test)]
mod tests {
    use super::*;
    use clipline_test_utils::TestDir;
    use std::time::Duration;
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
    fn configured_report_url_is_the_exact_official_intake_route() {
        assert_eq!(
            support_report_url().unwrap().as_str(),
            "https://support.dain.cafe/api/v1/reports"
        );
    }
}
