//! Runtime matrix: committed manifest, managed/external/missing discovery, snapshots.
use std::path::Path;

use sha2::{Digest, Sha256};

use super::download::managed_root;
use super::{
    FFMPEG_RUNTIME_MANIFEST_JSON, FfmpegInstallSnapshot, FfmpegInstallState, ManagedRuntimeInfoDto,
    current_locate_path, local_app_data_dir,
};
use crate::ffmpeg_runtime::{
    parse_ffmpeg_runtime_manifest, verify_managed_ffmpeg_runtime, FfmpegDiscoveryKind,
    FfmpegRuntimeManifest, FfmpegRuntimeStatus, ManagedRuntimeVerifyError,
};

pub fn committed_manifest() -> Result<(FfmpegRuntimeManifest, String), ManagedRuntimeVerifyError> {
    let manifest = parse_ffmpeg_runtime_manifest(FFMPEG_RUNTIME_MANIFEST_JSON)?;
    let hash = {
        let digest = Sha256::digest(FFMPEG_RUNTIME_MANIFEST_JSON.as_bytes());
        digest.iter().map(|byte| format!("{byte:02x}")).collect()
    };
    Ok((manifest, hash))
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

pub(super) fn status_snapshot_for_state(state: FfmpegInstallState) -> Result<FfmpegInstallSnapshot, String> {
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

pub(super) async fn status_snapshot_async(state: FfmpegInstallState) -> Result<FfmpegInstallSnapshot, String> {
    tauri::async_runtime::spawn_blocking(move || status_snapshot_for_state(state))
        .await
        .map_err(|e| format!("ffmpeg status worker join: {e}"))?
}
