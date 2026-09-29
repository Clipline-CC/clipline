//! On-demand managed FFmpeg install (slim-core Milestone B Task B3).
//!
//! Native single-flight state machine. Progress events are notifications; the
//! WebView must re-query status after recreate. Downloaded bytes are never
//! executed before allowlist hash verification.

#[path = "ffmpeg_install/controller.rs"]
mod controller;
#[path = "ffmpeg_install/download.rs"]
mod download;
#[path = "ffmpeg_install/status.rs"]
mod status;
#[path = "ffmpeg_install/verify.rs"]
mod verify;

pub use controller::FfmpegInstallController;
pub use download::{has_sufficient_free_space, managed_root, staging_root};
pub use status::{build_install_snapshot, committed_manifest, runtime_status_for_dirs};
pub use verify::{
    assert_archive_size, install_managed_runtime_from_archive, verify_archive_sha256,
};
use download::download_ffmpeg_archive;
use status::status_snapshot_async;

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Serialize;
use tauri::{Emitter, Manager};

use crate::ffmpeg_runtime::{
    free_space_required_bytes, FfmpegDiscoveryKind, FfmpegRuntimeStatus, ManagedRuntimeInfo,
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

fn local_app_data_dir() -> Result<PathBuf, String> {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .ok_or_else(|| "LOCALAPPDATA is not set".to_string())
}

fn current_locate_path() -> Option<PathBuf> {
    clipline_capture::ffmpeg::locate()
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

#[cfg(test)]
mod tests {
    use super::download::{DownloadAbortReason, download_should_abort, write_download_chunk};
    use super::verify::{copy_with_cancel, hex_sha256_file};
    use super::*;
    use crate::ffmpeg_runtime::{FfmpegAllowedFile, FfmpegRuntimeManifest};
    use sha2::{Digest, Sha256};
    use std::fs::File;
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
