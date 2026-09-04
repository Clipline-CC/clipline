//! Install download: staging paths, capped cancellable fetch of the release archive.
use std::fs::{self, File};
use std::io;
use std::io::Write;
use std::path::{Path, PathBuf};

use tauri::Emitter;

use super::verify::verify_archive_sha256;
use super::{
    FFMPEG_INSTALL_EVENT, FfmpegInstallController, FfmpegInstallSnapshot, FfmpegInstallState,
};
use crate::ffmpeg_runtime::{FfmpegDiscoveryKind, FfmpegRuntimeManifest};

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadAbortReason {
    Overflow,
    Cancelled,
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

pub(super) fn download_ffmpeg_archive(
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
