//! On-demand managed FFmpeg install (slim-core Milestone B Task B3).
//!
//! Native single-flight state machine. Progress events are notifications; the
//! WebView must re-query status after recreate. Downloaded bytes are never
//! executed before allowlist hash verification.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use serde::Serialize;
use sha2::{Digest, Sha256};
use tauri::{Emitter, Manager};
use zip::ZipArchive;

use crate::ffmpeg_runtime::{
    free_space_required_bytes, parse_ffmpeg_runtime_manifest, verify_managed_ffmpeg_runtime,
    FfmpegAllowedFile, FfmpegDiscoveryKind, FfmpegRuntimeManifest, FfmpegRuntimeStatus,
    ManagedRuntimeInfo, ManagedRuntimeVerifyError,
};

pub const FFMPEG_RUNTIME_MANIFEST_JSON: &str = include_str!("../ffmpeg-runtime.json");
pub const FREE_SPACE_MARGIN_BYTES: u64 = 64 * 1024 * 1024;
pub const FFMPEG_INSTALL_EVENT: &str = "ffmpeg-install";

/// Native install state owned by the app process (not the WebView).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum FfmpegInstallState {
    #[default]
    Idle,
    Checking,
    Downloading {
        bytes: u64,
        total: u64,
    },
    Verifying,
    Publishing,
    Ready,
    Failed {
        message: String,
    },
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FfmpegInstallSnapshot {
    pub state: FfmpegInstallState,
    pub discovery: FfmpegDiscoveryKind,
    pub managed: Option<ManagedRuntimeInfoDto>,
    pub locate_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ManagedRuntimeInfoDto {
    pub dir: String,
    pub ffmpeg_exe: String,
    pub release_tag: String,
    pub archive_sha256: String,
    pub manifest_sha256: String,
}

impl From<&ManagedRuntimeInfo> for ManagedRuntimeInfoDto {
    fn from(info: &ManagedRuntimeInfo) -> Self {
        Self {
            dir: info.dir.display().to_string(),
            ffmpeg_exe: info.ffmpeg_exe.display().to_string(),
            release_tag: info.release_tag.clone(),
            archive_sha256: info.archive_sha256.clone(),
            manifest_sha256: info.manifest_sha256.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadAbortReason {
    Overflow,
    Cancelled,
}

#[derive(Debug, Default)]
struct InstallInner {
    state: FfmpegInstallState,
    job_active: bool,
}

/// Process-global install controller: queryable across UI destroy/recreate.
pub struct FfmpegInstallController {
    inner: Mutex<InstallInner>,
    cancel: AtomicBool,
}

impl Default for FfmpegInstallController {
    fn default() -> Self {
        Self {
            inner: Mutex::new(InstallInner::default()),
            cancel: AtomicBool::new(false),
        }
    }
}

impl FfmpegInstallController {
    pub fn snapshot_state(&self) -> FfmpegInstallState {
        self.inner
            .lock()
            .map(|guard| guard.state.clone())
            .unwrap_or(FfmpegInstallState::Failed {
                message: "ffmpeg install state lock poisoned".into(),
            })
    }

    pub fn set_state(&self, state: FfmpegInstallState) {
        if let Ok(mut guard) = self.inner.lock() {
            guard.state = state;
        }
    }

    pub fn request_cancel(&self) -> FfmpegInstallState {
        let guard = match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if guard.job_active && !matches!(guard.state, FfmpegInstallState::Publishing) {
            self.cancel.store(true, Ordering::Release);
        }
        guard.state.clone()
    }

    #[allow(dead_code)]
    pub fn clear_cancel(&self) {
        self.cancel.store(false, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Acquire)
    }

    /// Atomically cross the last cancellable boundary. Once this succeeds,
    /// cancellation is ignored until the publish operation completes.
    pub fn begin_publishing(&self) -> Result<bool, String> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| "ffmpeg install state lock poisoned".to_string())?;
        if !guard.job_active || self.cancel.load(Ordering::Acquire) {
            return Ok(false);
        }
        guard.state = FfmpegInstallState::Publishing;
        Ok(true)
    }

    /// Returns true when this caller should start the job; false when coalesced.
    pub fn try_begin_job(&self) -> Result<bool, String> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| "ffmpeg install state lock poisoned".to_string())?;
        if guard.job_active {
            return Ok(false);
        }
        match &guard.state {
            FfmpegInstallState::Checking
            | FfmpegInstallState::Downloading { .. }
            | FfmpegInstallState::Verifying
            | FfmpegInstallState::Publishing => return Ok(false),
            FfmpegInstallState::Ready
            | FfmpegInstallState::Idle
            | FfmpegInstallState::Failed { .. }
            | FfmpegInstallState::Cancelled => {}
        }
        guard.job_active = true;
        guard.state = FfmpegInstallState::Checking;
        self.cancel.store(false, Ordering::Release);
        Ok(true)
    }

    pub fn end_job(&self, state: FfmpegInstallState) {
        if let Ok(mut guard) = self.inner.lock() {
            guard.job_active = false;
            guard.state = state;
        }
    }
}

pub fn committed_manifest() -> Result<(FfmpegRuntimeManifest, String), ManagedRuntimeVerifyError> {
    let manifest = parse_ffmpeg_runtime_manifest(FFMPEG_RUNTIME_MANIFEST_JSON)?;
    let hash = {
        let digest = Sha256::digest(FFMPEG_RUNTIME_MANIFEST_JSON.as_bytes());
        digest.iter().map(|byte| format!("{byte:02x}")).collect()
    };
    Ok((manifest, hash))
}

pub fn staging_root(local_app_data: &Path) -> PathBuf {
    local_app_data.join("Clipline").join("ffmpeg-staging")
}

pub fn managed_root(local_app_data: &Path) -> PathBuf {
    local_app_data.join("Clipline").join("ffmpeg")
}

pub fn download_partial_path(staging: &Path, archive_name: &str) -> PathBuf {
    staging
        .join("download")
        .join(format!("{archive_name}.partial"))
}

pub fn download_final_path(staging: &Path, archive_name: &str) -> PathBuf {
    staging.join("download").join(archive_name)
}

pub fn staging_tree_path(staging: &Path) -> PathBuf {
    staging.join("tree")
}

/// Remove abandoned staging artifacts (crash recovery / cancel cleanup).
pub fn sweep_abandoned_staging(staging: &Path) -> io::Result<Vec<PathBuf>> {
    let mut removed = Vec::new();
    if !staging.exists() {
        return Ok(removed);
    }
    for entry in fs::read_dir(staging)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            fs::remove_dir_all(&path)?;
        } else {
            fs::remove_file(&path)?;
        }
        removed.push(path);
    }
    Ok(removed)
}

pub fn download_should_abort(
    written: u64,
    archive_size: u64,
    cancelled: bool,
) -> Option<DownloadAbortReason> {
    if cancelled {
        return Some(DownloadAbortReason::Cancelled);
    }
    if written > archive_size {
        return Some(DownloadAbortReason::Overflow);
    }
    None
}

pub fn has_sufficient_free_space(free_bytes: u64, required_bytes: u64) -> bool {
    free_bytes >= required_bytes
}

fn hex_sha256_reader(mut reader: impl io::Read) -> io::Result<String> {
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

fn hex_sha256_file(path: &Path) -> io::Result<String> {
    hex_sha256_reader(File::open(path)?)
}

fn validate_allowlist_entry(file: &FfmpegAllowedFile) -> Result<(), String> {
    if file.staged_name.trim().is_empty()
        || file.staged_name
            != Path::new(&file.staged_name)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("")
        || file.archive_path.contains("..")
        || Path::new(&file.archive_path).is_absolute()
    {
        return Err(format!(
            "unsafe FFmpeg allowlist entry: {} -> {}",
            file.archive_path, file.staged_name
        ));
    }
    Ok(())
}

fn copy_with_cancel(
    reader: &mut impl Read,
    writer: &mut impl Write,
    cancelled: &dyn Fn() -> bool,
) -> Result<(u64, String), String> {
    let mut copied = 0_u64;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        if cancelled() {
            return Err("ffmpeg install cancelled".into());
        }
        let read = reader
            .read(&mut buffer)
            .map_err(|e| format!("read archive entry: {e}"))?;
        if read == 0 {
            let hash = hasher
                .finalize()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            return Ok((copied, hash));
        }
        writer
            .write_all(&buffer[..read])
            .map_err(|e| format!("write staged file: {e}"))?;
        hasher.update(&buffer[..read]);
        copied = copied.saturating_add(read as u64);
    }
}

/// Extract only allowlisted entries from a verified archive into `tree_dir`.
pub fn extract_allowlisted_ffmpeg_archive(
    archive_path: &Path,
    tree_dir: &Path,
    manifest: &FfmpegRuntimeManifest,
    cancelled: &dyn Fn() -> bool,
) -> Result<Vec<(String, u64, String)>, String> {
    if cancelled() {
        return Err("ffmpeg install cancelled".into());
    }
    if tree_dir.exists() {
        fs::remove_dir_all(tree_dir).map_err(|e| format!("clear staging tree: {e}"))?;
    }
    fs::create_dir_all(tree_dir).map_err(|e| format!("create staging tree: {e}"))?;

    let file = File::open(archive_path).map_err(|e| format!("open archive: {e}"))?;
    let mut zip = ZipArchive::new(file).map_err(|e| format!("open zip: {e}"))?;
    let mut verified = Vec::with_capacity(manifest.allowed_files.len());

    for allowed in &manifest.allowed_files {
        if cancelled() {
            return Err("ffmpeg install cancelled".into());
        }
        validate_allowlist_entry(allowed)?;
        let entry_name = format!(
            "{}/{}",
            manifest.archive_root.trim_end_matches('/'),
            allowed.archive_path.replace('\\', "/")
        );
        let mut entry = zip
            .by_name(&entry_name)
            .map_err(|_| format!("missing archive entry {entry_name}"))?;
        if entry.size() != allowed.size {
            return Err(format!(
                "archive entry {entry_name} size mismatch: expected {}, got {}",
                allowed.size,
                entry.size()
            ));
        }
        let output = tree_dir.join(&allowed.staged_name);
        let (copied, hash) = {
            let mut out = File::create(&output)
                .map_err(|e| format!("create staged {}: {e}", allowed.staged_name))?;
            copy_with_cancel(&mut entry, &mut out, cancelled)
                .map_err(|e| format!("extract {}: {e}", allowed.staged_name))?
        };
        if cancelled() {
            return Err("ffmpeg install cancelled".into());
        }
        if copied != allowed.size {
            return Err(format!(
                "staged {} size mismatch: expected {}, got {copied}",
                allowed.staged_name, allowed.size
            ));
        }
        if hash != allowed.sha256.to_ascii_lowercase() {
            return Err(format!(
                "staged {} SHA-256 mismatch: expected {}, got {hash}",
                allowed.staged_name, allowed.sha256
            ));
        }
        verified.push((allowed.staged_name.clone(), allowed.size, hash));
    }
    Ok(verified)
}

pub fn write_managed_provenance(
    tree_dir: &Path,
    manifest: &FfmpegRuntimeManifest,
    manifest_sha256: &str,
    files: &[(String, u64, String)],
) -> Result<(), String> {
    let provenance = serde_json::json!({
        "schema_version": 1,
        "provider": manifest.provider,
        "release_tag": manifest.release_tag,
        "published_at": manifest.published_at,
        "archive_name": manifest.archive_name,
        "archive_url": manifest.archive_url,
        "archive_sha256": manifest.archive_sha256,
        "manifest_sha256": manifest_sha256,
        "ffmpeg_version": manifest.version_line,
        "source_offer_url": manifest.source_offer_url,
        "ffmpeg_source_url": manifest.ffmpeg_source_url,
        "files": files.iter().map(|(name, size, sha)| serde_json::json!({
            "name": name,
            "size": size,
            "sha256": sha,
        })).collect::<Vec<_>>(),
    });
    fs::write(
        tree_dir.join("PROVENANCE.json"),
        serde_json::to_string_pretty(&provenance).map_err(|e| e.to_string())? + "\n",
    )
    .map_err(|e| format!("write PROVENANCE.json: {e}"))
}

/// Atomically replace `dest` with `tree_dir` (backup+rename).
pub fn publish_managed_runtime_atomic(tree_dir: &Path, dest: &Path) -> Result<(), String> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("create managed parent: {e}"))?;
    }
    let backup = dest.with_file_name(format!(
        ".ffmpeg-previous-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    ));
    if dest.exists() {
        fs::rename(dest, &backup).map_err(|e| format!("backup existing managed runtime: {e}"))?;
    }
    match fs::rename(tree_dir, dest) {
        Ok(()) => {
            let _ = fs::remove_dir_all(&backup);
            Ok(())
        }
        Err(error) => {
            if backup.exists() && !dest.exists() {
                let _ = fs::rename(&backup, dest);
            }
            Err(format!("publish managed runtime: {error}"))
        }
    }
}

pub fn verify_archive_sha256(archive_path: &Path, expected: &str) -> Result<String, String> {
    let actual = hex_sha256_file(archive_path).map_err(|e| format!("hash archive: {e}"))?;
    let expected = expected.to_ascii_lowercase();
    if actual != expected {
        return Err(format!(
            "FFmpeg archive SHA-256 mismatch: expected {expected}, got {actual}"
        ));
    }
    Ok(actual)
}

pub fn assert_archive_size(archive_path: &Path, expected_size: u64) -> Result<(), String> {
    let actual = fs::metadata(archive_path)
        .map_err(|e| format!("archive metadata: {e}"))?
        .len();
    if actual != expected_size {
        return Err(format!(
            "FFmpeg archive size mismatch: expected {expected_size}, got {actual}"
        ));
    }
    Ok(())
}

/// Install from a local archive that already passed size/hash checks.
pub fn install_managed_runtime_from_archive(
    archive_path: &Path,
    local_app_data: &Path,
    manifest: &FfmpegRuntimeManifest,
    manifest_sha256: &str,
    cancelled: &dyn Fn() -> bool,
    before_publish: impl FnOnce() -> Result<(), String>,
) -> Result<ManagedRuntimeInfo, String> {
    if cancelled() {
        return Err("ffmpeg install cancelled".into());
    }
    assert_archive_size(archive_path, manifest.archive_size)?;
    verify_archive_sha256(archive_path, &manifest.archive_sha256)?;

    let staging = staging_root(local_app_data);
    let tree = staging_tree_path(&staging);
    let files = extract_allowlisted_ffmpeg_archive(archive_path, &tree, manifest, cancelled)?;
    write_managed_provenance(&tree, manifest, manifest_sha256, &files)?;
    let mut info = verify_managed_ffmpeg_runtime(&tree, manifest, manifest_sha256)
        .map_err(|e| e.to_string())?;
    if cancelled() {
        return Err("ffmpeg install cancelled".into());
    }
    let dest = managed_root(local_app_data);
    before_publish()?;
    publish_managed_runtime_atomic(&tree, &dest)?;
    info.dir = dest.clone();
    info.ffmpeg_exe = dest.join("ffmpeg.exe");
    Ok(info)
}

pub fn runtime_status_for_dirs(
    managed_dir: Option<&Path>,
    locate_path: Option<&Path>,
) -> Result<FfmpegRuntimeStatus, String> {
    let (manifest, manifest_sha256) = committed_manifest().map_err(|e| e.to_string())?;
    Ok(crate::ffmpeg_runtime::ffmpeg_runtime_status(
        managed_dir,
        &manifest,
        &manifest_sha256,
        locate_path,
    ))
}

pub fn build_install_snapshot(
    state: FfmpegInstallState,
    status: &FfmpegRuntimeStatus,
) -> FfmpegInstallSnapshot {
    FfmpegInstallSnapshot {
        state,
        discovery: status.kind,
        managed: status.managed.as_ref().map(ManagedRuntimeInfoDto::from),
        locate_path: status
            .locate_path
            .as_ref()
            .map(|path| path.display().to_string()),
    }
}

fn install_progress_snapshot(state: FfmpegInstallState) -> FfmpegInstallSnapshot {
    FfmpegInstallSnapshot {
        state,
        discovery: FfmpegDiscoveryKind::Missing,
        managed: None,
        locate_path: None,
    }
}

fn emit_install_progress(
    app: &tauri::AppHandle,
    controller: &FfmpegInstallController,
    state: FfmpegInstallState,
) {
    controller.set_state(state.clone());
    let _ = app.emit(FFMPEG_INSTALL_EVENT, install_progress_snapshot(state));
}

/// Write download bytes with hard cap / cancel checks. Caller supplies chunks.
pub fn write_download_chunk(
    file: &mut File,
    written: &mut u64,
    chunk: &[u8],
    archive_size: u64,
    cancelled: bool,
) -> Result<(), String> {
    if let Some(reason) = download_should_abort(*written, archive_size, cancelled) {
        return Err(match reason {
            DownloadAbortReason::Cancelled => "ffmpeg download cancelled".into(),
            DownloadAbortReason::Overflow => "ffmpeg download exceeded archive_size".into(),
        });
    }
    let next = written.saturating_add(chunk.len() as u64);
    if next > archive_size {
        return Err("ffmpeg download exceeded archive_size".into());
    }
    file.write_all(chunk)
        .map_err(|e| format!("write ffmpeg download chunk: {e}"))?;
    *written = next;
    if let Some(reason) = download_should_abort(*written, archive_size, cancelled) {
        return Err(match reason {
            DownloadAbortReason::Cancelled => "ffmpeg download cancelled".into(),
            DownloadAbortReason::Overflow => "ffmpeg download exceeded archive_size".into(),
        });
    }
    Ok(())
}

fn local_app_data_dir() -> Result<PathBuf, String> {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .ok_or_else(|| "LOCALAPPDATA is not set".to_string())
}

fn current_locate_path() -> Option<PathBuf> {
    clipline_capture::ffmpeg::locate()
}

fn status_snapshot_for_state(state: FfmpegInstallState) -> Result<FfmpegInstallSnapshot, String> {
    let local = local_app_data_dir().ok();
    let managed = local.as_ref().map(|path| managed_root(path));
    let (manifest, manifest_sha256) = committed_manifest().map_err(|e| e.to_string())?;
    if let Some(info) = managed
        .as_deref()
        .and_then(|dir| verify_managed_ffmpeg_runtime(dir, &manifest, &manifest_sha256).ok())
    {
        let status = FfmpegRuntimeStatus {
            kind: FfmpegDiscoveryKind::ManagedVerified,
            locate_path: Some(info.ffmpeg_exe.clone()),
            managed: Some(info),
        };
        return Ok(build_install_snapshot(state, &status));
    }
    let status = crate::ffmpeg_runtime::ffmpeg_runtime_status(
        None,
        &manifest,
        &manifest_sha256,
        current_locate_path().as_deref(),
    );
    Ok(build_install_snapshot(state, &status))
}

async fn status_snapshot_async(state: FfmpegInstallState) -> Result<FfmpegInstallSnapshot, String> {
    tauri::async_runtime::spawn_blocking(move || status_snapshot_for_state(state))
        .await
        .map_err(|e| format!("ffmpeg status worker join: {e}"))?
}

#[tauri::command]
pub async fn ffmpeg_runtime_status(
    controller: tauri::State<'_, FfmpegInstallController>,
) -> Result<FfmpegInstallSnapshot, String> {
    status_snapshot_async(controller.snapshot_state()).await
}

#[tauri::command]
pub fn cancel_ffmpeg_runtime_install(
    app: tauri::AppHandle,
    controller: tauri::State<'_, FfmpegInstallController>,
) -> Result<FfmpegInstallSnapshot, String> {
    let snap = install_progress_snapshot(controller.request_cancel());
    let _ = app.emit(FFMPEG_INSTALL_EVENT, snap.clone());
    Ok(snap)
}

#[tauri::command]
pub async fn ensure_ffmpeg_runtime(
    app: tauri::AppHandle,
    controller: tauri::State<'_, FfmpegInstallController>,
) -> Result<FfmpegInstallSnapshot, String> {
    let local = local_app_data_dir()?;
    let status = status_snapshot_async(controller.snapshot_state()).await?;
    if matches!(status.discovery, FfmpegDiscoveryKind::ManagedVerified) {
        controller.end_job(FfmpegInstallState::Ready);
        let mut snap = status;
        snap.state = FfmpegInstallState::Ready;
        let _ = app.emit(FFMPEG_INSTALL_EVENT, snap.clone());
        return Ok(snap);
    }

    let (manifest, manifest_sha256) = committed_manifest().map_err(|e| e.to_string())?;
    if !controller.try_begin_job()? {
        return status_snapshot_async(controller.snapshot_state()).await;
    }

    let app2 = app.clone();
    // Run install job on blocking pool so we can use sync zip/fs.
    let result = tauri::async_runtime::spawn_blocking({
        let app = app.clone();
        let local = local.clone();
        let manifest = manifest.clone();
        let manifest_sha256 = manifest_sha256.clone();
        move || -> Result<ManagedRuntimeInfo, String> {
            // The controller is process-managed; re-fetch via app.state in sync context.
            let controller = app.state::<FfmpegInstallController>();
            let staging = staging_root(&local);
            fs::create_dir_all(staging.join("download"))
                .map_err(|e| format!("create ffmpeg staging: {e}"))?;
            let _ = sweep_abandoned_staging(&staging);
            fs::create_dir_all(staging.join("download"))
                .map_err(|e| format!("recreate ffmpeg staging download: {e}"))?;

            if controller.is_cancelled() {
                return Err("ffmpeg install cancelled".into());
            }

            let required = free_space_required_bytes(&manifest, FREE_SPACE_MARGIN_BYTES);
            let free = crate::windows::available_space_bytes(
                &local,
                "read free space for FFmpeg runtime install",
            )?;
            if !has_sufficient_free_space(free, required) {
                return Err(format!(
                    "not enough free disk space for FFmpeg runtime (need {required} bytes, have {free})"
                ));
            }

            // Prefer an already-fetched release-input archive when hash/size match.
            let release_input = local
                .join("Clipline")
                .join("release-inputs")
                .join(&manifest.archive_name);
            let archive_path = if release_input.is_file()
                && assert_archive_size(&release_input, manifest.archive_size).is_ok()
                && verify_archive_sha256(&release_input, &manifest.archive_sha256).is_ok()
            {
                release_input
            } else {
                emit_install_progress(
                    &app,
                    &controller,
                    FfmpegInstallState::Downloading {
                        bytes: 0,
                        total: manifest.archive_size,
                    },
                );
                download_ffmpeg_archive(&app, &controller, &staging, &manifest)?
            };

            if controller.is_cancelled() {
                let _ = sweep_abandoned_staging(&staging);
                return Err("ffmpeg download cancelled".into());
            }
            emit_install_progress(&app, &controller, FfmpegInstallState::Verifying);
            let result = install_managed_runtime_from_archive(
                &archive_path,
                &local,
                &manifest,
                &manifest_sha256,
                &|| controller.is_cancelled(),
                || {
                    if !controller.begin_publishing()? {
                        return Err("ffmpeg install cancelled".into());
                    }
                    let _ = app.emit(
                        FFMPEG_INSTALL_EVENT,
                        install_progress_snapshot(FfmpegInstallState::Publishing),
                    );
                    Ok(())
                },
            );
            let _ = sweep_abandoned_staging(&staging);
            result
        }
    })
    .await
    .unwrap_or_else(|e| Err(format!("ffmpeg ensure worker join: {e}")));

    match result {
        Ok(info) => {
            controller.end_job(FfmpegInstallState::Ready);
            let options = crate::service::refresh_ffmpeg_encoder_capabilities();
            let _ = app2.emit("encoders-changed", &options);
            let status = FfmpegRuntimeStatus {
                kind: FfmpegDiscoveryKind::ManagedVerified,
                locate_path: Some(info.ffmpeg_exe.clone()),
                managed: Some(info),
            };
            let snap = build_install_snapshot(FfmpegInstallState::Ready, &status);
            let _ = app2.emit(FFMPEG_INSTALL_EVENT, snap.clone());
            Ok(snap)
        }
        Err(message) => {
            if controller.is_cancelled() || message.contains("cancelled") {
                controller.end_job(FfmpegInstallState::Cancelled);
            } else {
                controller.end_job(FfmpegInstallState::Failed {
                    message: message.clone(),
                });
            }
            let snap = status_snapshot_async(controller.snapshot_state()).await?;
            let _ = app2.emit(FFMPEG_INSTALL_EVENT, snap.clone());
            Err(message)
        }
    }
}

fn download_ffmpeg_archive(
    app: &tauri::AppHandle,
    controller: &FfmpegInstallController,
    staging: &Path,
    manifest: &FfmpegRuntimeManifest,
) -> Result<PathBuf, String> {
    let partial = download_partial_path(staging, &manifest.archive_name);
    let final_path = download_final_path(staging, &manifest.archive_name);
    if let Some(parent) = partial.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("create download dir: {e}"))?;
    }
    if partial.exists() {
        let _ = fs::remove_file(&partial);
    }

    // Blocking reqwest client with redirects for GitHub release assets.
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(30 * 60))
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()
        .map_err(|e| format!("build ffmpeg download client: {e}"))?;
    let mut response = client
        .get(&manifest.archive_url)
        .send()
        .map_err(|e| format!("download ffmpeg archive: {e}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "download ffmpeg archive failed with status {}",
            response.status()
        ));
    }
    if response
        .content_length()
        .is_some_and(|length| length > manifest.archive_size)
    {
        return Err("ffmpeg download Content-Length exceeds archive_size".into());
    }

    let mut file = File::create(&partial).map_err(|e| format!("create partial download: {e}"))?;
    let mut written = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        if controller.is_cancelled() {
            drop(file);
            let _ = fs::remove_file(&partial);
            return Err("ffmpeg download cancelled".into());
        }
        let read = io::Read::read(&mut response, &mut buffer)
            .map_err(|e| format!("read ffmpeg download: {e}"))?;
        if read == 0 {
            break;
        }
        write_download_chunk(
            &mut file,
            &mut written,
            &buffer[..read],
            manifest.archive_size,
            controller.is_cancelled(),
        )?;
        controller.set_state(FfmpegInstallState::Downloading {
            bytes: written,
            total: manifest.archive_size,
        });
        if written == manifest.archive_size || written.is_multiple_of(512 * 1024) {
            let _ = app.emit(
                FFMPEG_INSTALL_EVENT,
                FfmpegInstallSnapshot {
                    state: controller.snapshot_state(),
                    discovery: FfmpegDiscoveryKind::Missing,
                    managed: None,
                    locate_path: None,
                },
            );
        }
    }
    if written != manifest.archive_size {
        let _ = fs::remove_file(&partial);
        return Err(format!(
            "ffmpeg download size mismatch: expected {}, got {written}",
            manifest.archive_size
        ));
    }
    drop(file);
    verify_archive_sha256(&partial, &manifest.archive_sha256)?;
    if final_path.exists() {
        let _ = fs::remove_file(&final_path);
    }
    fs::rename(&partial, &final_path).map_err(|e| format!("publish downloaded archive: {e}"))?;
    Ok(final_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ffmpeg_runtime::{FfmpegAllowedFile, FfmpegRuntimeManifest};
    use std::io::Write;

    fn tiny_manifest(files: Vec<FfmpegAllowedFile>, archive_size: u64) -> FfmpegRuntimeManifest {
        FfmpegRuntimeManifest {
            schema_version: 1,
            provider: "test-provider".into(),
            release_tag: "test-tag".into(),
            published_at: "2026-01-01T00:00:00Z".into(),
            archive_name: "ffmpeg-test.zip".into(),
            archive_url: "https://example.test/ffmpeg-test.zip".into(),
            archive_sha256: "00".repeat(32),
            archive_size,
            archive_root: "root".into(),
            version_line: "ffmpeg version test".into(),
            source_offer_url: "https://example.test/source".into(),
            ffmpeg_source_url: "https://example.test/ffmpeg".into(),
            allowed_files: files,
        }
    }

    fn sha(bytes: &[u8]) -> String {
        Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    fn write_tiny_zip(path: &Path, files: &[(&str, &[u8])]) {
        let file = File::create(path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (name, bytes) in files {
            zip.start_file(*name, options).unwrap();
            zip.write_all(bytes).unwrap();
        }
        zip.finish().unwrap();
    }

    #[test]
    fn free_space_planner_includes_archive_allowlist_and_margin() {
        let manifest = tiny_manifest(vec![], 100);
        // empty allowlist not parse-valid, but helper is pure
        let mut manifest = manifest;
        manifest.allowed_files = vec![FfmpegAllowedFile {
            archive_path: "bin/a".into(),
            staged_name: "a".into(),
            size: 40,
            sha256: sha(b"a"),
        }];
        assert_eq!(free_space_required_bytes(&manifest, 10), 100 + 40 + 10);
        assert!(has_sufficient_free_space(150, 150));
        assert!(!has_sufficient_free_space(149, 150));
    }

    #[test]
    fn download_abort_on_overflow_or_cancel() {
        assert_eq!(download_should_abort(10, 10, false), None);
        assert_eq!(
            download_should_abort(11, 10, false),
            Some(DownloadAbortReason::Overflow)
        );
        assert_eq!(
            download_should_abort(0, 10, true),
            Some(DownloadAbortReason::Cancelled)
        );
    }

    #[test]
    fn single_flight_coalesces_concurrent_begin() {
        let controller = FfmpegInstallController::default();
        assert!(controller.try_begin_job().unwrap());
        assert!(!controller.try_begin_job().unwrap());
        assert!(matches!(
            controller.snapshot_state(),
            FfmpegInstallState::Checking
        ));
        controller.end_job(FfmpegInstallState::Ready);
        assert!(controller.try_begin_job().unwrap());
    }

    #[test]
    fn cancel_requests_worker_shutdown_without_publishing_a_terminal_state_early() {
        let controller = FfmpegInstallController::default();
        assert!(controller.try_begin_job().unwrap());
        let state = controller.request_cancel();
        assert_eq!(state, FfmpegInstallState::Checking);
        assert!(controller.is_cancelled());
    }

    #[test]
    fn cancel_is_ignored_without_a_cancellable_active_job() {
        let controller = FfmpegInstallController::default();
        assert_eq!(controller.request_cancel(), FfmpegInstallState::Idle);
        assert!(!controller.is_cancelled());

        assert!(controller.try_begin_job().unwrap());
        assert!(controller.begin_publishing().unwrap());
        assert_eq!(controller.request_cancel(), FfmpegInstallState::Publishing);
        assert!(!controller.is_cancelled());
    }

    #[test]
    fn cancellation_wins_before_the_publish_boundary() {
        let controller = FfmpegInstallController::default();
        assert!(controller.try_begin_job().unwrap());
        controller.set_state(FfmpegInstallState::Verifying);
        controller.request_cancel();

        assert!(!controller.begin_publishing().unwrap());
        assert_eq!(controller.snapshot_state(), FfmpegInstallState::Verifying);
    }

    #[test]
    fn extraction_copy_observes_cancellation_between_chunks() {
        let input = vec![7_u8; 256 * 1024];
        let checks = std::cell::Cell::new(0_u32);
        let mut output = Vec::new();
        let error = copy_with_cancel(&mut input.as_slice(), &mut output, &|| {
            checks.set(checks.get() + 1);
            checks.get() >= 3
        })
        .expect_err("copy should stop when cancellation is requested");

        assert!(error.contains("cancelled"));
        assert!(output.len() < input.len());
    }

    #[test]
    fn sweep_removes_abandoned_staging_tree() {
        let dir = clipline_test_utils::TestDir::new("clipline-ffmpeg", "sweep");
        let staging = staging_root(dir.path());
        fs::create_dir_all(staging.join("download")).unwrap();
        fs::write(staging.join("download").join("x.partial"), b"abc").unwrap();
        let removed = sweep_abandoned_staging(&staging).unwrap();
        assert!(!removed.is_empty());
        assert!(fs::read_dir(&staging).unwrap().next().is_none());
    }

    #[test]
    fn install_from_tiny_archive_publishes_managed_tree() {
        let root = clipline_test_utils::TestDir::new("clipline-ffmpeg", "install");
        let exe = b"ffmpeg-bytes";
        let dll = b"avcodec-bytes";
        let files = vec![
            FfmpegAllowedFile {
                archive_path: "bin/ffmpeg.exe".into(),
                staged_name: "ffmpeg.exe".into(),
                size: exe.len() as u64,
                sha256: sha(exe),
            },
            FfmpegAllowedFile {
                archive_path: "bin/avcodec-62.dll".into(),
                staged_name: "avcodec-62.dll".into(),
                size: dll.len() as u64,
                sha256: sha(dll),
            },
        ];
        let zip_path = root.path().join("ffmpeg-test.zip");
        write_tiny_zip(
            &zip_path,
            &[
                ("root/bin/ffmpeg.exe", exe),
                ("root/bin/avcodec-62.dll", dll),
            ],
        );
        let archive_hash = hex_sha256_file(&zip_path).unwrap();
        let archive_size = fs::metadata(&zip_path).unwrap().len();
        let mut manifest = tiny_manifest(files, archive_size);
        manifest.archive_sha256 = archive_hash;
        let manifest_sha256 = sha(b"committed-manifest");

        let info = install_managed_runtime_from_archive(
            &zip_path,
            root.path(),
            &manifest,
            &manifest_sha256,
            &|| false,
            || Ok(()),
        )
        .expect("install tiny archive");
        assert_eq!(info.release_tag, "test-tag");
        assert!(info.ffmpeg_exe.is_file());
        assert!(managed_root(root.path()).join("PROVENANCE.json").is_file());
    }

    #[test]
    fn write_download_chunk_enforces_cap() {
        let dir = clipline_test_utils::TestDir::new("clipline-ffmpeg", "chunk");
        let path = dir.path().join("partial");
        let mut file = File::create(&path).unwrap();
        let mut written = 0u64;
        write_download_chunk(&mut file, &mut written, b"abcd", 4, false).unwrap();
        assert_eq!(written, 4);
        let err = write_download_chunk(&mut file, &mut written, b"x", 4, false).unwrap_err();
        assert!(err.contains("archive_size"));
    }

    #[test]
    fn committed_manifest_exposes_archive_size() {
        let (manifest, hash) = committed_manifest().expect("committed manifest");
        assert_eq!(manifest.archive_size, 70103338);
        assert!(!manifest.archive_root.is_empty());
        assert_eq!(hash.len(), 64);
        assert!(
            free_space_required_bytes(&manifest, FREE_SPACE_MARGIN_BYTES) > manifest.archive_size
        );
    }
}
