//! Clip library commands: inventory of the configured media folder for the UI and
//! a path-validated delete. The webview never touches the filesystem
//! directly — playback goes through the asset protocol.

#[path = "library/naming.rs"]
mod naming;
use naming::is_reserved_windows_file_name;

use std::collections::{hash_map::DefaultHasher, HashSet};
use std::hash::{Hash, Hasher};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use clipline_capture::{Codec, EncoderBackend};
use clipline_events::{is_review_event, ClipMarker, ClipMarkers, ClipPlay};
use clipline_library::{
    ActiveFileRegistry, CompatibilityClipProjection, CompatibilityLocalClipScan,
    KnownGameIdentityResolver, LocalLibraryRepository, LocalLibraryScanner,
    Mp4LegacyAudioTrackProbe, PlatformEffect, StandardRepositoryFileSystem, ValidatedClipPath,
};
pub use clipline_library::{DeletedClipsReport, RenamedClipInfo};
use clipline_mp4::{
    media_video_codecs_file, remux_with_mixed_audio_track_file,
    remux_with_selected_audio_tracks_file, trim_keyframe_aligned_file, MediaTrackCounts,
    MediaVideoCodec,
};
use clipline_storage::storage_status as read_storage_status;
use tauri::{AppHandle, Manager, Runtime};

use crate::service::{clips_dir, default_clips_dir};
use crate::util;

pub struct StorageSettings {
    quota_bytes: Mutex<Option<u64>>,
    media_dir: Mutex<PathBuf>,
}

impl StorageSettings {
    pub fn new(quota_bytes: Option<u64>, media_dir: PathBuf) -> Self {
        Self {
            quota_bytes: Mutex::new(quota_bytes),
            media_dir: Mutex::new(media_dir),
        }
    }

    pub fn quota_bytes(&self) -> Option<u64> {
        match self.quota_bytes.lock() {
            Ok(q) => *q,
            Err(e) => {
                tracing::error!(event = "storage_quota_lock_poisoned", error = %e);
                None
            }
        }
    }

    pub fn set_quota_bytes(&self, quota_bytes: Option<u64>) {
        match self.quota_bytes.lock() {
            Ok(mut q) => *q = quota_bytes,
            Err(e) => tracing::error!(event = "storage_quota_set_lock_poisoned", error = %e),
        }
    }

    pub fn media_dir(&self) -> PathBuf {
        match self.media_dir.lock() {
            Ok(dir) => dir.clone(),
            Err(e) => {
                tracing::error!(event = "media_directory_lock_poisoned", error = %e);
                default_clips_dir()
            }
        }
    }

    pub fn set_media_dir(&self, media_dir: PathBuf) {
        match self.media_dir.lock() {
            Ok(mut dir) => *dir = media_dir,
            Err(e) => tracing::error!(event = "media_directory_set_lock_poisoned", error = %e),
        }
    }

    fn clips_dir(&self) -> Result<PathBuf, String> {
        clips_dir(&self.media_dir())
    }
}

type LocalClipScan = CompatibilityLocalClipScan;

#[derive(serde::Serialize)]
pub struct StorageInfo {
    pub clip_count: usize,
    pub total_bytes: u64,
    pub quota_bytes: Option<u64>,
    pub over_quota: bool,
}

#[derive(serde::Serialize)]
pub struct ExportedClipInfo {
    pub path: String,
    pub name: String,
    pub size_mb: f64,
    pub modified_unix: u64,
    pub requested_start_s: f64,
    pub requested_end_s: f64,
    pub aligned_start_s: f64,
    pub aligned_end_s: f64,
    pub duration_s: f64,
    pub markers: Option<ClipMarkers>,
}

const AUDIO_PREVIEW_CACHE_MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct AudioPreviewPruneReport {
    removed_files: usize,
    removed_bytes: u64,
    reusable_bytes: u64,
}

#[derive(serde::Deserialize)]
pub struct PrepareClipAudioSidecarsRequest {
    pub path: String,
    #[serde(default, rename = "audioTrackIds")]
    pub audio_track_ids: Vec<String>,
    #[serde(default, rename = "protectedPreviewPaths")]
    pub protected_preview_paths: Vec<String>,
}

#[derive(serde::Serialize, Clone, Debug, PartialEq, Eq)]
pub struct PreparedClipAudioSidecar {
    #[serde(rename = "audioTrackId")]
    pub audio_track_id: String,
    pub path: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ResolvedAudioTrackSidecar {
    audio_track_id: String,
    audio_stream_index: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct AudioTrackSidecarOutput {
    audio_track_id: String,
    audio_stream_index: u32,
    final_path: PathBuf,
    tmp_path: PathBuf,
}

#[derive(Debug, Default)]
struct PublishedAudioSidecars {
    created_finals: Vec<PathBuf>,
    committed: bool,
}

impl PublishedAudioSidecars {
    fn record_created(&mut self, path: PathBuf) {
        self.created_finals.push(path);
    }

    fn commit(mut self) {
        self.committed = true;
    }
}

impl Drop for PublishedAudioSidecars {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        cleanup_created_audio_sidecar_finals(&self.created_finals);
    }
}

#[derive(Debug)]
struct PreparedAudioSidecarBatch {
    sidecars: Vec<PreparedClipAudioSidecar>,
    publication: Option<PublishedAudioSidecars>,
}

#[derive(serde::Deserialize)]
pub struct CopyClipToClipboardRequest {
    pub path: String,
    #[serde(default, rename = "audioTrackIds")]
    pub audio_track_ids: Option<Vec<String>>,
    #[serde(default)]
    pub original: bool,
}

#[tauri::command]
pub async fn list_clips<R: Runtime>(
    app: AppHandle<R>,
    settings: tauri::State<'_, StorageSettings>,
) -> Result<LocalClipScan, String> {
    let dir = settings.clips_dir()?;
    let retry_root = dir.clone();
    let enrichment_app = app.clone();
    tauri::async_runtime::spawn(async move {
        if let Err(e) = crate::osu_api::retry_pending_enrichment(&enrichment_app, retry_root).await
        {
            tracing::warn!(event = "library_osu_enrichment_retry_failed", error = %e);
        }
    });
    let (scan, canonical_scope_root) = tauri::async_runtime::spawn_blocking(move || {
        let scanner = LocalLibraryScanner::open(&dir)?;
        let canonical_scope_root = scanner.canonical_root().to_path_buf();
        let probe = Mp4LegacyAudioTrackProbe;
        let games = KnownGameIdentityResolver;
        let scan = scanner.scan(&CompatibilityClipProjection::new(&probe, &games))?;
        Ok::<_, String>((scan, canonical_scope_root))
    })
    .await
    .map_err(|e| format!("list clips task: {e}"))??;
    for clip in &scan.clips {
        allow_local_clip_asset_from_canonical_root(
            &app,
            &canonical_scope_root,
            Path::new(&clip.path),
        )?;
    }
    Ok(scan)
}

fn poster_failure_kind(error: &str) -> &'static str {
    if error
        .trim()
        .eq_ignore_ascii_case("ffmpeg is not available for poster extraction")
    {
        "runtime_unavailable"
    } else if error.starts_with("spawn ffmpeg poster") {
        "spawn_failed"
    } else if error.contains("timed out") {
        "timeout"
    } else if error.starts_with("ffmpeg poster failed") {
        "media_or_codec"
    } else if error.contains("JPEG data") || error.contains("output limit") {
        "invalid_output"
    } else if error.contains("poster temp") || error.contains("finalize poster") {
        "publish_failed"
    } else {
        "unknown"
    }
}

fn log_poster_failure_once(error: &str) {
    static REPORTED: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();
    let kind = poster_failure_kind(error);
    let reported = REPORTED.get_or_init(|| Mutex::new(HashSet::new()));
    let should_report = match reported.lock() {
        Ok(mut reported) => reported.insert(kind),
        Err(_) => true,
    };
    if should_report {
        // Keep clip paths and FFmpeg stderr out of the support log. The
        // category is enough to distinguish discovery, execution, decoding,
        // output, and Rust-owned publication failures.
        tracing::warn!(event = "poster_extraction_failed", kind);
    }
}

/// Return (generating on demand) the cached poster JPEG for a clip, as a path
/// the webview loads through the asset protocol. Lazy and per-clip so the
/// library listing never blocks on ffmpeg.
#[tauri::command]
pub async fn clip_poster<R: Runtime>(
    app: AppHandle<R>,
    path: String,
    settings: tauri::State<'_, StorageSettings>,
) -> Result<String, String> {
    let scope_root = settings.clips_dir()?;
    let target = validate_clip_path(&settings, &path)?;
    let poster = tauri::async_runtime::spawn_blocking(move || {
        let seek_s = clipline_library::poster_seek_seconds(&target);
        crate::poster::ensure_poster(&target, seek_s)
    })
    .await
    .map_err(|error| format!("clip poster task: {error}"))?
    .inspect_err(|error| log_poster_failure_once(error))?;
    allow_local_poster_asset(&app, &scope_root, &poster)?;
    Ok(poster.display().to_string())
}

fn allow_local_clip_asset<R: Runtime>(
    app: &AppHandle<R>,
    root: &Path,
    clip: &Path,
) -> Result<(), String> {
    allow_local_media_asset(app, root, clip, &["mp4"])
}

fn allow_local_clip_asset_from_canonical_root<R: Runtime>(
    app: &AppHandle<R>,
    canonical_root: &Path,
    clip: &Path,
) -> Result<(), String> {
    allow_local_media_asset_from_canonical_root(app, canonical_root, clip, &["mp4"])
}

fn allow_local_poster_asset<R: Runtime>(
    app: &AppHandle<R>,
    root: &Path,
    poster: &Path,
) -> Result<(), String> {
    allow_local_media_asset(app, root, poster, &["jpg", "jpeg"])
}

fn allow_local_media_asset<R: Runtime>(
    app: &AppHandle<R>,
    root: &Path,
    asset: &Path,
    extensions: &[&str],
) -> Result<(), String> {
    let canonical_root = canonical_media_root(root)?;
    allow_local_media_asset_from_canonical_root(app, &canonical_root, asset, extensions)
}

fn canonical_media_root(root: &Path) -> Result<PathBuf, String> {
    root.canonicalize()
        .map_err(|e| format!("canonicalize media root {root:?}: {e}"))
}

fn allow_local_media_asset_from_canonical_root<R: Runtime>(
    app: &AppHandle<R>,
    canonical_root: &Path,
    asset: &Path,
    extensions: &[&str],
) -> Result<(), String> {
    let canonical_asset = asset
        .canonicalize()
        .map_err(|e| format!("canonicalize media asset {asset:?}: {e}"))?;
    if !canonical_asset.starts_with(canonical_root) {
        return Err(format!(
            "media asset {canonical_asset:?} escaped root {canonical_root:?}"
        ));
    }
    let extension = canonical_asset
        .extension()
        .and_then(|extension| extension.to_str())
        .ok_or_else(|| format!("media asset {canonical_asset:?} has no extension"))?;
    if !extensions
        .iter()
        .any(|allowed| extension.eq_ignore_ascii_case(allowed))
    {
        return Err(format!(
            "media asset {canonical_asset:?} has an unsupported extension"
        ));
    }
    app.asset_protocol_scope()
        .allow_file(&canonical_asset)
        .map_err(|e| format!("scope media asset {canonical_asset:?} for playback: {e}"))
}

#[tauri::command]
pub fn delete_clip(
    path: String,
    settings: tauri::State<StorageSettings>,
    active_files: tauri::State<ActiveFileRegistry>,
) -> Result<(), String> {
    let repository = mutation_repository(&settings.clips_dir()?, active_files.inner())?;
    let clip = repository
        .validate_clip_path(&path)
        .map_err(|error| error.to_string())?;
    repository.delete(&clip).map_err(|error| error.to_string())
}

/// Delete many clips in one round trip. Root resolution stays in the adapter;
/// validation and deletion run together in one blocking repository task so
/// the UI does not pay N async hops.
#[tauri::command]
pub async fn delete_clips(
    paths: Vec<String>,
    settings: tauri::State<'_, StorageSettings>,
    active_files: tauri::State<'_, ActiveFileRegistry>,
) -> Result<DeletedClipsReport, String> {
    let root = settings.clips_dir()?;
    let active_files = active_files.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        mutation_repository(&root, &active_files)?
            .delete_many(&paths)
            .map_err(|error| error.to_string())
    })
    .await
    .map_err(|e| format!("delete clips task: {e}"))?
}

#[tauri::command]
pub async fn rename_clip(
    path: String,
    name: String,
    settings: tauri::State<'_, StorageSettings>,
    active_files: tauri::State<'_, ActiveFileRegistry>,
    _state: tauri::State<'_, crate::app::RuntimeState>,
) -> Result<RenamedClipInfo, String> {
    let root = settings.clips_dir()?;
    let active_files = active_files.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let repository = mutation_repository(&root, &active_files)?;
        let clip = repository
            .validate_clip_path(&path)
            .map_err(|error| error.to_string())?;
        repository
            .rename_title(&clip, &name)
            .map_err(|error| error.to_string())
    })
    .await
    .map_err(|e| format!("rename clip task: {e}"))?
}

#[tauri::command]
pub async fn rename_clip_file<R: Runtime>(
    app: AppHandle<R>,
    path: String,
    name: String,
    settings: tauri::State<'_, StorageSettings>,
    active_files: tauri::State<'_, ActiveFileRegistry>,
    state: tauri::State<'_, crate::app::RuntimeState>,
) -> Result<RenamedClipInfo, String> {
    let root = settings.clips_dir()?;
    let task_path = path.clone();
    let active_files = active_files.inner().clone();
    let (renamed, canonical_scope_root) = tauri::async_runtime::spawn_blocking(move || {
        let repository = mutation_repository(&root, &active_files)?;
        let clip = repository
            .validate_clip_path(&task_path)
            .map_err(|error| error.to_string())?;
        let renamed = repository
            .rename_file(&clip, &name)
            .map_err(|error| error.to_string())?;
        Ok::<_, String>((renamed, repository.canonical_root().to_path_buf()))
    })
    .await
    .map_err(|e| format!("rename clip task: {e}"))??;

    update_cloud_record_paths(&state, &path, &renamed.path);
    allow_local_clip_asset_from_canonical_root(
        &app,
        &canonical_scope_root,
        Path::new(&renamed.path),
    )?;
    Ok(renamed)
}

fn mutation_repository(
    root: &Path,
    active_files: &ActiveFileRegistry,
) -> Result<LocalLibraryRepository, String> {
    LocalLibraryRepository::with_seams(
        root,
        Arc::new(StandardRepositoryFileSystem),
        Arc::new(active_files.clone()),
    )
    .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn export_clip<R: Runtime>(
    app: AppHandle<R>,
    path: String,
    start_s: f64,
    end_s: f64,
    title: Option<String>,
    include_markers: Option<bool>,
    settings: tauri::State<'_, StorageSettings>,
) -> Result<ExportedClipInfo, String> {
    let scope_root = settings.clips_dir()?;
    let source = validate_clip_path(&settings, &path)?;
    let include_markers = include_markers.unwrap_or(true);
    let exported = tauri::async_runtime::spawn_blocking(move || {
        export_clip_file(source, start_s, end_s, title, include_markers)
    })
    .await
    .map_err(|e| format!("export clip task: {e}"))??;
    allow_local_clip_asset(&app, &scope_root, Path::new(&exported.path))?;
    Ok(exported)
}

#[tauri::command]
pub async fn prepare_clip_audio_sidecars<R: Runtime>(
    app: AppHandle<R>,
    request: PrepareClipAudioSidecarsRequest,
    settings: tauri::State<'_, StorageSettings>,
) -> Result<Vec<PreparedClipAudioSidecar>, String> {
    let source = validate_clip_path(&settings, &request.path)?;
    let protected_preview_paths: Vec<PathBuf> = request
        .protected_preview_paths
        .into_iter()
        .map(PathBuf::from)
        .collect();
    let sidecars = tauri::async_runtime::spawn_blocking(move || {
        prepare_clip_audio_sidecars_file(source, request.audio_track_ids, protected_preview_paths)
    })
    .await
    .map_err(|e| format!("audio sidecar task: {e}"))??;
    finalize_prepared_audio_sidecars(sidecars, |sidecar| {
        allow_audio_preview_asset(&app, Path::new(&sidecar.path))
    })
}

fn allow_audio_preview_asset<R: Runtime>(app: &AppHandle<R>, preview: &Path) -> Result<(), String> {
    let preview_dir = crate::settings::audio_preview_cache_dir();
    if !preview.starts_with(&preview_dir) {
        return Ok(());
    }
    let canonical_dir = std::fs::canonicalize(&preview_dir)
        .map_err(|e| format!("canonicalize audio preview cache {preview_dir:?}: {e}"))?;
    let canonical_preview = std::fs::canonicalize(preview)
        .map_err(|e| format!("canonicalize audio preview {preview:?}: {e}"))?;
    if !canonical_preview.starts_with(&canonical_dir) {
        return Err(format!(
            "audio preview {canonical_preview:?} escaped cache {canonical_dir:?}"
        ));
    }
    if !canonical_preview
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("mp4"))
    {
        return Err(format!("audio preview {canonical_preview:?} is not an MP4"));
    }

    let preview = canonical_preview.as_path();
    app.asset_protocol_scope()
        .allow_file(preview)
        .map_err(|e| format!("scope audio preview {canonical_preview:?} for playback: {e}"))
}

fn prepare_clip_audio_sidecars_file(
    source: PathBuf,
    selected_audio_track_ids: Vec<String>,
    protected_preview_paths: Vec<PathBuf>,
) -> Result<PreparedAudioSidecarBatch, String> {
    prepare_clip_audio_sidecars_file_with_extractor(
        source,
        selected_audio_track_ids,
        protected_preview_paths
            .into_iter()
            .map(|path| path.display().to_string())
            .collect(),
        crate::settings::audio_preview_cache_dir(),
        extract_audio_sidecars_with_ffmpeg,
    )
}

fn prepare_clip_audio_sidecars_file_with_extractor(
    source: PathBuf,
    selected_audio_track_ids: Vec<String>,
    protected_preview_paths: Vec<String>,
    preview_dir: PathBuf,
    extract_audio_sidecars: impl FnMut(&Path, &[AudioTrackSidecarOutput]) -> Result<(), String>,
) -> Result<PreparedAudioSidecarBatch, String> {
    prepare_clip_audio_sidecars_file_with_extractor_and_limits(
        source,
        selected_audio_track_ids,
        protected_preview_paths,
        preview_dir,
        AUDIO_PREVIEW_CACHE_MAX_BYTES,
        extract_audio_sidecars,
    )
}

fn prepare_clip_audio_sidecars_file_with_extractor_and_limits(
    source: PathBuf,
    selected_audio_track_ids: Vec<String>,
    protected_preview_paths: Vec<String>,
    preview_dir: PathBuf,
    max_cache_bytes: u64,
    mut extract_audio_sidecars: impl FnMut(&Path, &[AudioTrackSidecarOutput]) -> Result<(), String>,
) -> Result<PreparedAudioSidecarBatch, String> {
    let resolved_tracks = resolve_audio_sidecar_tracks(&source, &selected_audio_track_ids)?;
    let source_meta = std::fs::metadata(&source).map_err(|e| format!("read clip metadata: {e}"))?;
    std::fs::create_dir_all(&preview_dir)
        .map_err(|e| format!("create audio preview cache: {e}"))?;

    let currently_active: Vec<PathBuf> = protected_preview_paths
        .into_iter()
        .map(PathBuf::from)
        .collect();
    let requested_final_paths: Vec<PathBuf> = resolved_tracks
        .iter()
        .map(|track| {
            audio_track_sidecar_path(&preview_dir, &source, &source_meta, &track.audio_track_id)
        })
        .collect();
    let protected_before_lookup = [
        currently_active.as_slice(),
        requested_final_paths.as_slice(),
    ]
    .concat();
    prune_audio_preview_cache_logged_with_limit(
        &preview_dir,
        &protected_before_lookup,
        max_cache_bytes,
    );

    let mut ordered = Vec::with_capacity(resolved_tracks.len());
    let mut missing_outputs = Vec::new();
    for (track, final_path) in resolved_tracks.iter().zip(requested_final_paths.iter()) {
        if final_path.exists() {
            match validate_audio_sidecar_file(final_path) {
                Ok(()) => {
                    if let Err(error) = touch_audio_preview(final_path) {
                        tracing::warn!(event = "audio_sidecar_cleanup_failed", error = %error);
                    }
                    ordered.push(Some(PreparedClipAudioSidecar {
                        audio_track_id: track.audio_track_id.clone(),
                        path: final_path.display().to_string(),
                    }));
                    continue;
                }
                Err(error) => {
                    tracing::warn!(event = "audio_sidecar_cleanup_failed", error = %error);
                    let _ = std::fs::remove_file(final_path);
                }
            }
        }

        missing_outputs.push(AudioTrackSidecarOutput {
            audio_track_id: track.audio_track_id.clone(),
            audio_stream_index: track.audio_stream_index,
            final_path: final_path.clone(),
            tmp_path: cached_export_tmp_path(final_path)?,
        });
        ordered.push(None);
    }

    let mut publication = None;

    if !missing_outputs.is_empty() {
        for output in &missing_outputs {
            let _ = std::fs::remove_file(&output.tmp_path);
        }
        if let Err(error) = extract_audio_sidecars(&source, &missing_outputs) {
            cleanup_audio_sidecar_temps(&missing_outputs);
            return Err(error);
        }
        publication = Some(validate_and_publish_audio_sidecars(&missing_outputs)?);
    }

    for ((prepared, track), final_path) in ordered
        .iter_mut()
        .zip(resolved_tracks.iter())
        .zip(requested_final_paths.iter())
    {
        if prepared.is_some() {
            continue;
        }
        validate_audio_sidecar_file(final_path)?;
        *prepared = Some(PreparedClipAudioSidecar {
            audio_track_id: track.audio_track_id.clone(),
            path: final_path.display().to_string(),
        });
    }
    let ordered = ordered
        .into_iter()
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| "audio sidecar preparation left an unresolved track".to_string())?;
    let protected_after: Vec<PathBuf> = ordered
        .iter()
        .map(|sidecar| PathBuf::from(&sidecar.path))
        .collect();
    let protected = [currently_active.as_slice(), protected_after.as_slice()].concat();
    prune_audio_preview_cache_logged_with_limit(&preview_dir, &protected, max_cache_bytes);
    Ok(PreparedAudioSidecarBatch {
        sidecars: ordered,
        publication,
    })
}

fn finalize_prepared_audio_sidecars(
    mut batch: PreparedAudioSidecarBatch,
    mut allow_audio_sidecar: impl FnMut(&PreparedClipAudioSidecar) -> Result<(), String>,
) -> Result<Vec<PreparedClipAudioSidecar>, String> {
    for sidecar in &batch.sidecars {
        allow_audio_sidecar(sidecar)?;
    }
    if let Some(publication) = batch.publication.take() {
        publication.commit();
    }
    Ok(batch.sidecars)
}

fn prune_audio_preview_cache_logged_with_limit(
    preview_dir: &Path,
    protected: &[PathBuf],
    max_cache_bytes: u64,
) {
    if let Err(error) = prune_audio_preview_cache(preview_dir, protected, max_cache_bytes) {
        tracing::warn!(event = "audio_preview_cache_prune_failed", error = %error);
    }
}

fn resolve_audio_sidecar_tracks(
    source: &Path,
    selected_audio_track_ids: &[String],
) -> Result<Vec<ResolvedAudioTrackSidecar>, String> {
    if selected_audio_track_ids.is_empty() {
        return Err("audio track selection must not be empty".into());
    }
    let Some(markers) =
        util::markers_with_inferred_audio_tracks(source, util::read_markers_raw(source))
    else {
        return Err("this clip has no selectable audio track metadata".into());
    };
    if markers.audio_tracks.is_empty() {
        return Err("this clip has no selectable audio track metadata".into());
    }
    let _ = util::selected_audio_track_indices(&markers, selected_audio_track_ids)?;
    let selected_id_set: std::collections::BTreeSet<&str> = selected_audio_track_ids
        .iter()
        .map(String::as_str)
        .collect();
    Ok(markers
        .audio_tracks
        .iter()
        .filter(|track| selected_id_set.contains(track.id.as_str()))
        .map(|track| ResolvedAudioTrackSidecar {
            audio_track_id: track.id.clone(),
            audio_stream_index: track.track_index,
        })
        .collect())
}

fn audio_track_sidecar_path(
    preview_dir: &Path,
    source: &Path,
    meta: &std::fs::Metadata,
    audio_track_id: &str,
) -> PathBuf {
    let mut hasher = DefaultHasher::new();
    "audio-track-sidecar-v1".hash(&mut hasher);
    source.display().to_string().hash(&mut hasher);
    meta.len().hash(&mut hasher);
    meta.modified().ok().hash(&mut hasher);
    audio_track_id.hash(&mut hasher);
    preview_dir.join(format!("audio-preview-{:016x}.mp4", hasher.finish()))
}

fn validate_audio_sidecar_file(path: &Path) -> Result<(), String> {
    let metadata = std::fs::metadata(path)
        .map_err(|error| format!("read audio sidecar metadata {path:?}: {error}"))?;
    if metadata.len() == 0 {
        return Err(format!("audio sidecar {path:?} was empty"));
    }
    let counts = clipline_mp4::media_track_counts_file(path)
        .map_err(|error| format!("inspect audio sidecar {path:?}: {error}"))?;
    if counts != (MediaTrackCounts { video: 0, audio: 1 }) {
        return Err(format!(
            "audio sidecar {path:?} had unexpected tracks: video={}, audio={}",
            counts.video, counts.audio
        ));
    }
    Ok(())
}

fn validate_and_publish_audio_sidecars(
    outputs: &[AudioTrackSidecarOutput],
) -> Result<PublishedAudioSidecars, String> {
    let result = (|| {
        for output in outputs {
            validate_audio_sidecar_file(&output.tmp_path)?;
        }

        let mut published = PublishedAudioSidecars::default();
        for output in outputs {
            if output.final_path.exists() {
                if let Err(error) = validate_audio_sidecar_file(&output.final_path) {
                    return Err(format!(
                        "finalize audio sidecar collision winner {path:?}: {error}",
                        path = output.final_path
                    ));
                }
                let _ = std::fs::remove_file(&output.tmp_path);
                continue;
            }

            match std::fs::rename(&output.tmp_path, &output.final_path) {
                Ok(()) => {
                    published.record_created(output.final_path.clone());
                }
                Err(_) if output.final_path.exists() => {
                    if let Err(error) = validate_audio_sidecar_file(&output.final_path) {
                        return Err(format!(
                            "finalize audio sidecar collision winner {path:?}: {error}",
                            path = output.final_path
                        ));
                    }
                    let _ = std::fs::remove_file(&output.tmp_path);
                }
                Err(error) => {
                    return Err(format!(
                        "finalize audio sidecar {tmp:?} -> {final_path:?}: {error}",
                        tmp = output.tmp_path,
                        final_path = output.final_path
                    ));
                }
            }
        }
        Ok(published)
    })();
    if result.is_err() {
        cleanup_audio_sidecar_temps(outputs);
    }
    result
}

fn cleanup_audio_sidecar_temps(outputs: &[AudioTrackSidecarOutput]) {
    for output in outputs {
        let _ = std::fs::remove_file(&output.tmp_path);
    }
}

fn cleanup_created_audio_sidecar_finals(paths: &[PathBuf]) {
    for path in paths {
        let _ = std::fs::remove_file(path);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum ShareAudioExportMode {
    Remux(Vec<u32>),
    Mix(Vec<u32>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ShareVideoExportMode {
    Copy,
    Encode {
        encoder: String,
        backend: EncoderBackend,
    },
}

const SHARE_H264_BITRATE_BPS: u32 = 8_000_000;
const SHARE_H264_BUFSIZE_BITS: u64 = 16_000_000;

fn clipboard_copy_path(
    source: &Path,
    selected_audio_track_ids: Option<&[String]>,
    original: bool,
) -> Result<PathBuf, String> {
    clipboard_copy_path_with_exporter(
        source,
        selected_audio_track_ids,
        original,
        &crate::settings::share_export_cache_dir(),
        |source, target, mode| export_share_compatible_file(source, target, mode.as_ref()),
    )
}

fn clipboard_copy_path_with_exporter(
    source: &Path,
    selected_audio_track_ids: Option<&[String]>,
    original: bool,
    export_dir: &Path,
    export_audio: impl FnMut(&Path, &Path, Option<ShareAudioExportMode>) -> Result<(), String>,
) -> Result<PathBuf, String> {
    if original {
        return Ok(source.to_path_buf());
    }
    clipboard_share_path_with_exporter(source, selected_audio_track_ids, export_dir, export_audio)
}

fn clipboard_share_path_with_exporter(
    source: &Path,
    selected_audio_track_ids: Option<&[String]>,
    export_dir: &Path,
    mut export_audio: impl FnMut(&Path, &Path, Option<ShareAudioExportMode>) -> Result<(), String>,
) -> Result<PathBuf, String> {
    let mode = clipboard_share_export_mode(source, selected_audio_track_ids)?;

    let meta = std::fs::metadata(source).map_err(|e| format!("read clip metadata: {e}"))?;
    std::fs::create_dir_all(export_dir).map_err(|e| format!("create share export cache: {e}"))?;
    prune_old_share_exports(export_dir);
    let export = share_export_path(
        export_dir,
        source,
        &meta,
        selected_audio_track_ids,
        mode.as_ref(),
    );
    if export.exists() {
        return Ok(export);
    }

    let tmp = share_export_tmp_path(&export)?;
    if let Err(error) = export_audio(source, &tmp, mode) {
        let _ = std::fs::remove_file(&tmp);
        return Err(error);
    }
    match std::fs::rename(&tmp, &export) {
        Ok(()) => {}
        Err(_) if export.exists() => {
            let _ = std::fs::remove_file(&tmp);
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            return Err(format!("finalize share export: {e}"));
        }
    }
    Ok(export)
}

fn clipboard_share_export_mode(
    source: &Path,
    selected_audio_track_ids: Option<&[String]>,
) -> Result<Option<ShareAudioExportMode>, String> {
    let Some(selected_audio_track_ids) = selected_audio_track_ids else {
        return Ok(None);
    };
    let Some(markers) =
        util::markers_with_inferred_audio_tracks(source, util::read_markers_raw(source))
    else {
        return Ok(None);
    };
    let tracks = markers.audio_tracks.as_slice();
    if tracks.is_empty() {
        if selected_audio_track_ids.is_empty() {
            return Ok(Some(ShareAudioExportMode::Remux(Vec::new())));
        }
        return Err("this clip has no selectable audio track metadata".into());
    }
    let selected_indices = util::selected_audio_track_indices(&markers, selected_audio_track_ids)?;
    if selected_indices.len() > 1 {
        Ok(Some(ShareAudioExportMode::Mix(selected_indices)))
    } else {
        Ok(Some(ShareAudioExportMode::Remux(selected_indices)))
    }
}

fn share_export_path(
    export_dir: &Path,
    source: &Path,
    meta: &std::fs::Metadata,
    selected_audio_track_ids: Option<&[String]>,
    mode: Option<&ShareAudioExportMode>,
) -> PathBuf {
    let mut hasher = DefaultHasher::new();
    "share-export-v3-aac-h264-cbr8m".hash(&mut hasher);
    source.display().to_string().hash(&mut hasher);
    meta.len().hash(&mut hasher);
    meta.modified().ok().hash(&mut hasher);
    mode.hash(&mut hasher);
    if let Some(ids) = selected_audio_track_ids {
        for id in ids {
            id.hash(&mut hasher);
        }
    }
    export_dir.join(format!("share-export-{:016x}.mp4", hasher.finish()))
}

fn export_share_compatible_file(
    source: &Path,
    target: &Path,
    audio_mode: Option<&ShareAudioExportMode>,
) -> Result<(), String> {
    let mut intermediate = None;
    let (input, has_audio) = match audio_mode {
        Some(ShareAudioExportMode::Remux(indices)) => {
            let path = cached_export_tmp_path(target)?;
            remux_with_selected_audio_tracks_file(source, &path, indices)
                .map_err(|error| error.to_string())?;
            let has_audio = !indices.is_empty();
            intermediate = Some(path.clone());
            (path, has_audio)
        }
        Some(ShareAudioExportMode::Mix(indices)) => {
            let path = cached_export_tmp_path(target)?;
            remux_with_mixed_audio_track_file(source, &path, indices)
                .map_err(|error| error.to_string())?;
            intermediate = Some(path.clone());
            (path, true)
        }
        None => {
            let counts = clipline_mp4::media_track_counts_file(source)
                .map_err(|error| format!("inspect share audio tracks: {error}"))?;
            (source.to_path_buf(), counts.audio > 0)
        }
    };

    let result = transcode_share_file_with_ffmpeg(source, &input, target, has_audio);
    if let Some(intermediate) = intermediate {
        let _ = std::fs::remove_file(intermediate);
    }
    result
}

fn transcode_share_file_with_ffmpeg(
    source: &Path,
    input: &Path,
    target: &Path,
    has_audio: bool,
) -> Result<(), String> {
    let ffmpeg = clipline_capture::ffmpeg::locate()
        .ok_or_else(|| "ffmpeg is not available for a shareable clipboard export".to_string())?;
    let video_modes = share_video_export_modes(source)?;
    let timeout = share_export_timeout(source);
    let deadline = Instant::now()
        .checked_add(timeout)
        .unwrap_or_else(Instant::now);
    let mut last_error = String::new();

    for mode in video_modes {
        let Some(remaining) = remaining_share_export_timeout(deadline, Instant::now()) else {
            last_error = format!(
                "ffmpeg share export exhausted its {} second timeout",
                timeout.as_secs()
            );
            break;
        };
        let _ = std::fs::remove_file(target);
        let mut command = Command::new(&ffmpeg);
        suppress_console(&mut command);
        command
            .args(ffmpeg_share_export_args(input, target, has_audio, &mode))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        match run_share_ffmpeg(&mut command, remaining) {
            Ok(output) if output.status.success() => return Ok(()),
            Ok(output) => {
                last_error = String::from_utf8_lossy(&output.stderr).trim().to_string();
                if last_error.is_empty() {
                    last_error = format!("ffmpeg exited with {}", output.status);
                }
            }
            Err(error) => last_error = error,
        }
    }

    let _ = std::fs::remove_file(target);
    Err(format!("prepare shareable clipboard clip: {last_error}"))
}

fn share_video_export_modes(source: &Path) -> Result<Vec<ShareVideoExportMode>, String> {
    let codecs = media_video_codecs_file(source)
        .map_err(|error| format!("inspect share video codec: {error}"))?;
    if codecs.as_slice() == [MediaVideoCodec::H264] {
        return Ok(vec![ShareVideoExportMode::Copy]);
    }
    if codecs.len() != 1 {
        return Err(format!(
            "shareable export requires exactly one video track, found {}",
            codecs.len()
        ));
    }

    let mut encoders: Vec<(String, EncoderBackend)> = Vec::new();
    for capability in clipline_capture::ffmpeg::probe() {
        if !capability.codecs.contains(&Codec::H264) {
            continue;
        }
        let Some(name) = clipline_capture::ffmpeg::encoder_name(capability.backend, Codec::H264)
        else {
            continue;
        };
        if !encoders.iter().any(|(existing, _)| existing == name) {
            encoders.push((name.to_string(), capability.backend));
        }
    }
    if encoders.is_empty() {
        return Err("no usable FFmpeg H.264 encoder is available for this clip".into());
    }
    Ok(encoders
        .into_iter()
        .map(|(encoder, backend)| ShareVideoExportMode::Encode { encoder, backend })
        .collect())
}

fn ffmpeg_share_export_args(
    input: &Path,
    target: &Path,
    has_audio: bool,
    video_mode: &ShareVideoExportMode,
) -> Vec<String> {
    let mut args = vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-nostdin".into(),
        "-y".into(),
        "-i".into(),
        input.display().to_string(),
        "-map".into(),
        "0:v:0".into(),
    ];
    if has_audio {
        args.extend(["-map".into(), "0:a:0".into()]);
    }
    args.extend([
        "-map_metadata".into(),
        "-1".into(),
        "-map_chapters".into(),
        "-1".into(),
        "-c:v".into(),
    ]);
    match video_mode {
        ShareVideoExportMode::Copy => args.push("copy".into()),
        ShareVideoExportMode::Encode { encoder, backend } => {
            args.push(encoder.clone());
            args.extend(clipline_capture::ffmpeg_encoder::backend_rate_control(
                *backend,
                SHARE_H264_BITRATE_BPS,
                SHARE_H264_BUFSIZE_BITS,
            ));
            args.extend(["-pix_fmt".into(), "nv12".into()]);
        }
    }
    if has_audio {
        args.extend([
            "-c:a".into(),
            "aac".into(),
            "-profile:a".into(),
            "aac_low".into(),
            "-b:a".into(),
            "192k".into(),
            "-ac".into(),
            "2".into(),
            "-ar".into(),
            "48000".into(),
        ]);
    }
    args.extend([
        "-movflags".into(),
        "+faststart".into(),
        "-f".into(),
        "mp4".into(),
        target.display().to_string(),
    ]);
    args
}

fn share_export_timeout(source: &Path) -> Duration {
    const MIN_SECONDS: u64 = 2 * 60;
    const MAX_SECONDS: u64 = 6 * 60 * 60;
    let duration = clipline_mp4::movie_duration_s_file(source)
        .ok()
        .flatten()
        .unwrap_or(60.0);
    let seconds = (duration * 4.0 + 60.0).ceil().max(0.0) as u64;
    Duration::from_secs(seconds.clamp(MIN_SECONDS, MAX_SECONDS))
}

fn remaining_share_export_timeout(deadline: Instant, now: Instant) -> Option<Duration> {
    deadline
        .checked_duration_since(now)
        .filter(|remaining| !remaining.is_zero())
}

struct ShareFfmpegOutput {
    status: ExitStatus,
    stderr: Vec<u8>,
}

fn run_share_ffmpeg(command: &mut Command, timeout: Duration) -> Result<ShareFfmpegOutput, String> {
    const MAX_STDERR_BYTES: usize = 128 * 1024;

    let mut child = command
        .spawn()
        .map_err(|error| format!("spawn ffmpeg share export: {error}"))?;
    let Some(stderr) = child.stderr.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err("spawn ffmpeg share export: stderr pipe unavailable".into());
    };
    let reader = match std::thread::Builder::new()
        .name("clipline-share-ffmpeg-stderr".into())
        .spawn(move || read_bounded_share_stderr(stderr, MAX_STDERR_BYTES))
    {
        Ok(reader) => reader,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("spawn ffmpeg share stderr reader: {error}"));
        }
    };

    let deadline = Instant::now()
        .checked_add(timeout)
        .unwrap_or_else(Instant::now);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(25));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                break Err(format!(
                    "ffmpeg share export timed out after {} seconds",
                    timeout.as_secs()
                ));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                break Err(format!("wait for ffmpeg share export: {error}"));
            }
        }
    };
    let stderr = reader
        .join()
        .map_err(|_| "ffmpeg share stderr reader panicked".to_string())?
        .map_err(|error| format!("read ffmpeg share stderr: {error}"))?;
    Ok(ShareFfmpegOutput {
        status: status?,
        stderr,
    })
}

fn read_bounded_share_stderr(mut reader: impl Read, max_bytes: usize) -> io::Result<Vec<u8>> {
    let mut retained = Vec::new();
    let mut chunk = [0_u8; 16 * 1024];
    loop {
        let read = reader.read(&mut chunk)?;
        if read == 0 {
            return Ok(retained);
        }
        let keep = read.min(max_bytes.saturating_sub(retained.len()));
        retained.extend_from_slice(&chunk[..keep]);
    }
}

fn share_export_tmp_path(export: &Path) -> Result<PathBuf, String> {
    cached_export_tmp_path(export)
}

fn cached_export_tmp_path(target: &Path) -> Result<PathBuf, String> {
    crate::settings::persistence::sibling_tmp_path(target)
}

fn prune_old_share_exports(export_dir: &Path) {
    const MAX_AGE: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);
    prune_cached_mp4_files(export_dir, MAX_AGE);
}

fn prune_cached_mp4_files(export_dir: &Path, max_age: std::time::Duration) {
    let Ok(entries) = std::fs::read_dir(export_dir) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        if !is_cached_mp4_file(&path) {
            continue;
        }
        let old = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age >= max_age);
        if old {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn is_cached_mp4_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    if !name.starts_with("share-export-") {
        return false;
    }
    if name.ends_with(".mp4") || name.ends_with(".mp4.tmp") {
        return true;
    }
    let Some((_, suffix)) = name.split_once(".mp4.") else {
        return false;
    };
    let parts = suffix.split('.').collect::<Vec<_>>();
    !parts.is_empty()
        && parts.len().is_multiple_of(3)
        && parts.chunks_exact(3).all(|chunk| {
            !chunk[0].is_empty()
                && chunk[0].bytes().all(|byte| byte.is_ascii_digit())
                && !chunk[1].is_empty()
                && chunk[1].bytes().all(|byte| byte.is_ascii_digit())
                && chunk[2] == "tmp"
        })
}

fn extract_audio_sidecars_with_ffmpeg(
    source: &Path,
    outputs: &[AudioTrackSidecarOutput],
) -> Result<(), String> {
    let ffmpeg = clipline_capture::ffmpeg::locate()
        .ok_or_else(|| "ffmpeg is not available for audio sidecar extraction".to_string())?;
    let mut cmd = Command::new(ffmpeg);
    suppress_console(&mut cmd);
    let output = cmd
        .args(ffmpeg_audio_sidecar_args(source, outputs))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("spawn ffmpeg audio sidecar extraction: {e}"))?;
    if !output.status.success() {
        cleanup_audio_sidecar_temps(outputs);
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("ffmpeg audio sidecar extraction failed: {stderr}"));
    }
    Ok(())
}

fn ffmpeg_audio_sidecar_args(source: &Path, outputs: &[AudioTrackSidecarOutput]) -> Vec<String> {
    let mut args = vec![
        "-hide_banner".to_string(),
        "-nostdin".to_string(),
        "-y".to_string(),
        "-i".to_string(),
        source.display().to_string(),
    ];
    for output in outputs {
        args.extend([
            "-map".to_string(),
            format!("0:a:{}", output.audio_stream_index),
            "-vn".to_string(),
            "-map_metadata".to_string(),
            "-1".to_string(),
            "-c:a".to_string(),
            "copy".to_string(),
            "-f".to_string(),
            "mp4".to_string(),
            output.tmp_path.display().to_string(),
        ]);
    }
    args
}

pub(crate) use clipline_capture::ffmpeg::suppress_console;

fn export_clip_file(
    source: PathBuf,
    start_s: f64,
    end_s: f64,
    title: Option<String>,
    include_markers: bool,
) -> Result<ExportedClipInfo, String> {
    let tmp = unique_temp_export_path(&source)?;
    let info = match trim_keyframe_aligned_file(&source, &tmp, start_s, end_s) {
        Ok(info) => info,
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.to_string());
        }
    };
    let target = unique_export_path(&source, info.aligned_start_s, info.aligned_end_s, title)?;
    std::fs::rename(&tmp, &target).map_err(|e| e.to_string())?;

    let exported_markers = export_markers_for_range(
        &source,
        info.aligned_start_s,
        info.aligned_end_s,
        include_markers,
    )?;
    if let Some(markers) = &exported_markers {
        let json = serde_json::to_string_pretty(markers).map_err(|e| e.to_string())?;
        std::fs::write(target.with_extension("markers.json"), json).map_err(|e| e.to_string())?;
    }
    let meta =
        std::fs::metadata(&target).map_err(|e| format!("read exported clip metadata: {e}"))?;
    let modified_unix = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Ok(ExportedClipInfo {
        path: target.display().to_string(),
        name: target
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        size_mb: meta.len() as f64 / (1024.0 * 1024.0),
        modified_unix,
        requested_start_s: info.requested_start_s,
        requested_end_s: info.requested_end_s,
        aligned_start_s: info.aligned_start_s,
        aligned_end_s: info.aligned_end_s,
        duration_s: info.duration_s,
        markers: exported_markers,
    })
}

fn is_audio_preview_mp4(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("audio-preview-") && name.ends_with(".mp4"))
}

fn is_audio_preview_partial(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("audio-preview-") && name.ends_with(".tmp"))
}

#[derive(Debug)]
struct CachedAudioPreview {
    path: PathBuf,
    len: u64,
    modified: std::time::SystemTime,
}

fn audio_preview_path_is_protected(path: &Path, protected: &[PathBuf]) -> bool {
    protected.iter().any(|candidate| {
        path == candidate
            || std::fs::canonicalize(path)
                .ok()
                .zip(std::fs::canonicalize(candidate).ok())
                .is_some_and(|(left, right)| left == right)
    })
}

fn prune_audio_preview_cache(
    dir: &Path,
    protected: &[PathBuf],
    max_bytes: u64,
) -> Result<AudioPreviewPruneReport, String> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Default::default()),
        Err(error) => return Err(format!("read audio preview cache {dir:?}: {error}")),
    };
    let mut report = AudioPreviewPruneReport::default();
    let mut total_bytes = 0_u64;
    let mut candidates = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| format!("read audio preview cache entry: {error}"))?;
        let path = entry.path();
        if is_audio_preview_partial(&path) {
            let len = entry.metadata().map(|metadata| metadata.len()).unwrap_or(0);
            if std::fs::remove_file(&path).is_ok() {
                report.removed_files += 1;
                report.removed_bytes = report.removed_bytes.saturating_add(len);
            }
            continue;
        }
        if !is_audio_preview_mp4(&path) {
            continue;
        }
        let metadata = entry
            .metadata()
            .map_err(|error| format!("read audio preview metadata {path:?}: {error}"))?;
        let len = metadata.len();
        total_bytes = total_bytes.saturating_add(len);
        if audio_preview_path_is_protected(&path, protected) {
            continue;
        }
        report.reusable_bytes = report.reusable_bytes.saturating_add(len);
        candidates.push(CachedAudioPreview {
            path,
            len,
            modified: metadata.modified().unwrap_or(std::time::UNIX_EPOCH),
        });
    }
    candidates.sort_by(|left, right| {
        left.modified
            .cmp(&right.modified)
            .then_with(|| left.path.cmp(&right.path))
    });
    for candidate in candidates {
        if total_bytes <= max_bytes {
            break;
        }
        if std::fs::remove_file(&candidate.path).is_ok() {
            report.removed_files += 1;
            report.removed_bytes = report.removed_bytes.saturating_add(candidate.len);
            report.reusable_bytes = report.reusable_bytes.saturating_sub(candidate.len);
            total_bytes = total_bytes.saturating_sub(candidate.len);
        }
    }
    Ok(report)
}

fn touch_audio_preview(path: &Path) -> Result<(), String> {
    std::fs::File::options()
        .write(true)
        .open(path)
        .and_then(|file| file.set_modified(std::time::SystemTime::now()))
        .map_err(|error| format!("refresh audio preview recency {path:?}: {error}"))
}

pub(crate) fn prune_audio_preview_cache_on_startup() -> Result<AudioPreviewPruneReport, String> {
    let dir = crate::settings::audio_preview_cache_dir();
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("create audio preview cache {dir:?}: {e}"))?;
    prune_audio_preview_cache(&dir, &[], AUDIO_PREVIEW_CACHE_MAX_BYTES)
}

#[tauri::command]
pub async fn storage_status(
    settings: tauri::State<'_, StorageSettings>,
) -> Result<StorageInfo, String> {
    let dir = settings.clips_dir()?;
    let quota_bytes = settings.quota_bytes();
    tauri::async_runtime::spawn_blocking(move || storage_status_for_dir(dir, quota_bytes))
        .await
        .map_err(|e| format!("storage status task: {e}"))?
}

fn storage_status_for_dir(dir: PathBuf, quota_bytes: Option<u64>) -> Result<StorageInfo, String> {
    let status = read_storage_status(&dir, quota_bytes).map_err(|e| e.to_string())?;
    Ok(StorageInfo {
        clip_count: status.clip_count,
        total_bytes: status.total_bytes,
        quota_bytes: status.quota_bytes,
        over_quota: status.is_over_quota(),
    })
}

pub(crate) fn validate_clip_path(
    settings: &StorageSettings,
    path: &str,
) -> Result<PathBuf, String> {
    let repository =
        LocalLibraryRepository::open(settings.clips_dir()?).map_err(|error| error.to_string())?;
    repository
        .validate_clip_path(path)
        .map(|clip| clip.canonical_path().to_path_buf())
        .map_err(|error| error.to_string())
}

pub(crate) fn validate_upload_source(
    settings: &StorageSettings,
    active_files: &ActiveFileRegistry,
    path: &str,
) -> Result<ValidatedClipPath, String> {
    let repository = mutation_repository(&settings.clips_dir()?, active_files)?;
    repository
        .validate_clip_path(path)
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub fn reveal_clip(path: String, settings: tauri::State<StorageSettings>) -> Result<(), String> {
    let repository =
        LocalLibraryRepository::open(settings.clips_dir()?).map_err(|error| error.to_string())?;
    let clip = repository
        .validate_clip_path(&path)
        .map_err(|error| error.to_string())?;
    match repository
        .reveal_effect(&clip)
        .map_err(|error| error.to_string())?
    {
        PlatformEffect::RevealClip(target) => {
            clipline_shell::windows::shell_execute::reveal_in_explorer(
                &target,
                "reveal clip in Explorer",
            )
            .map_err(|error| error.to_string())
        }
        PlatformEffect::OpenFolder(_) => Err("Library returned an invalid reveal effect".into()),
    }
}

#[tauri::command]
pub async fn copy_clip_to_clipboard(
    request: CopyClipToClipboardRequest,
    settings: tauri::State<'_, StorageSettings>,
    window: tauri::WebviewWindow,
) -> Result<(), String> {
    let target = validate_clip_path(&settings, &request.path)?;
    let audio_track_ids = request.audio_track_ids;
    let original = request.original;
    let owner = window
        .hwnd()
        .map_err(|error| format!("get Clipline window handle: {error}"))?
        .0 as isize;
    tauri::async_runtime::spawn_blocking(move || {
        let share_path = clipboard_copy_path(&target, audio_track_ids.as_deref(), original)?;
        clipline_shell::windows::clipboard::copy_file_to_clipboard(&share_path, owner)
            .map_err(|error| error.to_string())
    })
    .await
    .map_err(|e| format!("copy clip task: {e}"))?
}

#[tauri::command]
pub fn open_media_folder(settings: tauri::State<StorageSettings>) -> Result<(), String> {
    let repository =
        LocalLibraryRepository::open(settings.clips_dir()?).map_err(|error| error.to_string())?;
    match repository.open_folder_effect() {
        PlatformEffect::OpenFolder(directory) => {
            clipline_shell::windows::shell_execute::open_folder(&directory, "open media folder")
                .map_err(|error| error.to_string())
        }
        PlatformEffect::RevealClip(_) => {
            Err("Library returned an invalid open-folder effect".into())
        }
    }
}

fn update_cloud_record_paths(state: &crate::app::RuntimeState, old_path: &str, new_path: &str) {
    if let Err(error) = state.reconcile_cloud_record_path(old_path, new_path) {
        tracing::warn!(event = "renamed_clip_cloud_record_update_failed", error = %error);
    }
}

fn filter_review_markers(mut markers: ClipMarkers) -> ClipMarkers {
    markers.markers.retain(|m| is_review_event(&m.event));
    markers
}

fn has_marker_sidecar_content(markers: &ClipMarkers) -> bool {
    !markers.markers.is_empty()
        || markers.player_summary.is_some()
        || !markers.audio_tracks.is_empty()
        || !markers.plays.is_empty()
}

fn crop_markers(markers: &ClipMarkers, start_s: f64, end_s: f64) -> ClipMarkers {
    let cropped = markers
        .markers
        .iter()
        .filter(|m| m.t_s >= start_s && m.t_s < end_s)
        .map(|m| ClipMarker {
            t_s: m.t_s - start_s,
            event: m.event.clone(),
        })
        .collect();
    let plays = markers
        .plays
        .iter()
        .filter_map(|play| crop_play(play, start_s, end_s))
        .collect();
    ClipMarkers {
        recording_start_s: markers.recording_start_s + start_s,
        duration_s: end_s - start_s,
        player_summary: markers.player_summary.clone(),
        audio_tracks: markers.audio_tracks.clone(),
        plays,
        markers: cropped,
    }
}

fn crop_play(play: &ClipPlay, start_s: f64, end_s: f64) -> Option<ClipPlay> {
    if let Some(play_end_s) = play.t_end_s {
        if play_end_s <= start_s || play.t_start_s >= end_s {
            return None;
        }
        let mut cropped = play.clone();
        cropped.t_start_s = play.t_start_s.max(start_s) - start_s;
        cropped.t_end_s = Some(play_end_s.min(end_s) - start_s);
        Some(cropped)
    } else if play.t_start_s >= start_s && play.t_start_s < end_s {
        let mut cropped = play.clone();
        cropped.t_start_s -= start_s;
        Some(cropped)
    } else {
        None
    }
}

fn export_markers_for_range(
    source: &Path,
    start_s: f64,
    end_s: f64,
    include_markers: bool,
) -> Result<Option<ClipMarkers>, String> {
    if !include_markers {
        return Ok(None);
    }
    let Some(markers) = util::read_markers_raw(source).map(filter_review_markers) else {
        return Ok(None);
    };
    let cropped = crop_markers(&markers, start_s, end_s);
    Ok(has_marker_sidecar_content(&cropped).then_some(cropped))
}

fn unique_temp_export_path(source: &Path) -> Result<PathBuf, String> {
    let parent = source
        .parent()
        .ok_or_else(|| "source clip has no parent directory".to_string())?;
    let stem = source
        .file_stem()
        .map(|s| s.to_string_lossy())
        .ok_or_else(|| "source clip has no file stem".to_string())?;
    for suffix in 0..1000u32 {
        let name = format!("{stem}_trim_pending_{suffix:03}.mp4.tmp");
        let candidate = parent.join(name);
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err("could not choose an unused temporary export filename".into())
}

fn unique_export_path(
    source: &Path,
    start_s: f64,
    end_s: f64,
    title: Option<String>,
) -> Result<PathBuf, String> {
    let parent = source
        .parent()
        .ok_or_else(|| "source clip has no parent directory".to_string())?;
    let stem = source
        .file_stem()
        .map(|s| s.to_string_lossy())
        .ok_or_else(|| "source clip has no file stem".to_string())?;
    let start_ms = (start_s * 1000.0).round().max(0.0) as u64;
    let end_ms = (end_s * 1000.0).round().max(0.0) as u64;
    let titled_stem = title.as_deref().and_then(export_title_stem);
    for suffix in 0..1000u32 {
        let name = if let Some(titled_stem) = titled_stem.as_deref() {
            if suffix == 0 {
                format!("{titled_stem}.mp4")
            } else {
                format!("{titled_stem}_{suffix}.mp4")
            }
        } else if suffix == 0 {
            format!("{stem}_trim_{start_ms:06}_{end_ms:06}.mp4")
        } else {
            format!("{stem}_trim_{start_ms:06}_{end_ms:06}_{suffix}.mp4")
        };
        let candidate = parent.join(name);
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err("could not choose an unused export filename".into())
}

fn export_title_stem(title: &str) -> Option<String> {
    let sanitized: String = title
        .chars()
        .map(|ch| {
            if ch.is_ascii_control()
                || matches!(ch, '<' | '>' | ':' | '"' | '|' | '?' | '*' | '/' | '\\')
            {
                ' '
            } else {
                ch
            }
        })
        .collect();
    let collapsed = sanitized.split_whitespace().collect::<Vec<_>>().join(" ");
    let stem = collapsed.trim().trim_end_matches(['.', ' ']);
    if stem.is_empty() || stem == "." || stem == ".." || is_reserved_windows_file_name(stem) {
        None
    } else {
        Some(stem.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    use clipline_events::{ClipAudioTrack, ClipPlay, EventKind, GameEvent, GameId, PlayerSummary};
    use clipline_library::{
        local_clip_id_for_source, CloudAccountGeneration, CloudAccountKey, DurableUploadToken,
        UploadGeneration, ACTIVE_UPLOAD_MUTATION_ERROR,
    };
    use clipline_mp4::{
        AudioTrackConfig, FragSample, HybridMp4Writer, TrackConfig, VideoTrackConfig,
    };
    use clipline_test_utils::TestDir;
    use shiguredo_opus::{Encoder, EncoderConfig};

    #[test]
    fn shipping_mutation_repository_observes_process_upload_lease() {
        let directory = TestDir::new("clipline-app-library", "shared-active-file-registry");
        let root = directory.path().join("media");
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("clip.mp4");
        std::fs::write(&path, b"clip bytes").unwrap();

        let active_files = ActiveFileRegistry::new();
        let repository = mutation_repository(&root, &active_files).unwrap();
        let clip = repository
            .validate_clip_path(path.to_string_lossy().as_ref())
            .unwrap();
        let token = DurableUploadToken {
            account_key: CloudAccountKey::new("account-a").unwrap(),
            account_generation: CloudAccountGeneration::new(1),
            upload_generation: UploadGeneration::new(1),
            local_clip_id: local_clip_id_for_source(clip.file_identity()),
            source_path: clip.comparison_identity().clone(),
        };
        let lease = active_files.acquire_upload(&clip, token).unwrap();

        let error = repository.delete(&clip).unwrap_err();
        assert_eq!(error.to_string(), ACTIVE_UPLOAD_MUTATION_ERROR);
        assert!(path.exists());

        drop(lease);
        repository.delete(&clip).unwrap();
        assert!(!path.exists());
    }

    fn marker(t_s: f64) -> ClipMarker {
        marker_with(t_s, EventKind::ChampionKill, true)
    }

    fn marker_with(t_s: f64, kind: EventKind, involves_local_player: bool) -> ClipMarker {
        ClipMarker {
            t_s,
            event: GameEvent {
                game_id: GameId::LeagueOfLegends,
                kind,
                actor: "Dain".into(),
                victim: None,
                assisters: Vec::new(),
                subtype: None,
                game_time_s: 0.0,
                recording_offset_s: Some(10.0 + t_s),
                importance: 7,
                involves_local_player,
            },
        }
    }

    fn osu_play(t_start_s: f64, t_end_s: Option<f64>, external_id: &str) -> ClipPlay {
        ClipPlay {
            game_id: GameId::Osu,
            source: "osu_api".into(),
            external_id: external_id.into(),
            url: None,
            beatmap_id: Some(123),
            beatmapset_id: Some(456),
            cover_url: None,
            title: "Everything will freeze".into(),
            artist: "UNDEAD CORPORATION".into(),
            difficulty: "Time Freeze".into(),
            mapper: Some("Ekoro".into()),
            star_rating: None,
            mods: vec!["HD".into()],
            rank: Some("A".into()),
            passed: true,
            accuracy: Some(0.9876),
            max_combo: Some(1234),
            total_score: Some(987654),
            pp: Some(123.4),
            started_at: Some("2026-06-30T23:54:00+00:00".into()),
            ended_at: "2026-06-30T23:56:00+00:00".into(),
            derived_start: false,
            t_start_s,
            t_end_s,
        }
    }

    #[test]
    fn crop_markers_rebases_times_and_recording_start() {
        let markers = ClipMarkers {
            recording_start_s: 10.0,
            duration_s: 5.0,
            player_summary: Some(PlayerSummary {
                champion_name: "Nautilus".into(),
                kills: 3,
                deaths: 4,
                assists: 23,
                creep_score: None,
                game_time_s: None,
                player_name: String::new(),
                team: String::new(),
                participants: Vec::new(),
                summoner_spells: Vec::new(),
                items: Vec::new(),
            }),
            audio_tracks: Vec::new(),
            plays: Vec::new(),
            markers: vec![marker(0.5), marker(1.5), marker(2.5)],
        };

        let cropped = crop_markers(&markers, 1.0, 2.0);

        assert_eq!(cropped.markers.len(), 1);
        assert!((cropped.markers[0].t_s - 0.5).abs() < 1e-9);
        assert!((cropped.recording_start_s - 11.0).abs() < 1e-9);
        assert!((cropped.duration_s - 1.0).abs() < 1e-9);
        assert_eq!(
            cropped.player_summary.as_ref().map(|summary| (
                summary.champion_name.as_str(),
                summary.kills,
                summary.deaths,
                summary.assists
            )),
            Some(("Nautilus", 3, 4, 23))
        );
    }

    #[test]
    fn filter_review_markers_keeps_match_event_sources_and_drops_noise() {
        let markers = ClipMarkers {
            recording_start_s: 10.0,
            duration_s: 100.0,
            player_summary: Some(PlayerSummary {
                champion_name: "Nautilus".into(),
                kills: 3,
                deaths: 4,
                assists: 23,
                creep_score: None,
                game_time_s: None,
                player_name: String::new(),
                team: String::new(),
                participants: Vec::new(),
                summoner_spells: Vec::new(),
                items: Vec::new(),
            }),
            audio_tracks: Vec::new(),
            plays: Vec::new(),
            markers: vec![
                marker_with(1.0, EventKind::ChampionKill, true),
                marker_with(2.0, EventKind::ChampionKill, false),
                marker_with(2.5, EventKind::ChampionDeath, true),
                marker_with(3.0, EventKind::TurretKilled, false),
                marker_with(4.0, EventKind::DragonKill, false),
                marker_with(5.0, EventKind::BaronKill, false),
                marker_with(5.5, EventKind::HeraldKill, false),
                marker_with(6.0, EventKind::MinionsSpawning, true),
                marker_with(7.0, EventKind::FirstBlood, true),
                marker_with(8.0, EventKind::FirstBrick, true),
                marker_with(9.0, EventKind::Ace, true),
            ],
        };

        let filtered = filter_review_markers(markers);
        let kinds: Vec<_> = filtered.markers.iter().map(|m| m.event.kind).collect();

        assert_eq!(
            kinds,
            vec![
                EventKind::ChampionKill,
                EventKind::ChampionKill,
                EventKind::ChampionDeath,
                EventKind::TurretKilled,
                EventKind::DragonKill,
                EventKind::BaronKill,
                EventKind::HeraldKill,
            ]
        );
        assert!(filtered.markers[0].event.involves_local_player);
        assert!(!filtered.markers[1].event.involves_local_player);
        assert_eq!(
            filtered.player_summary.as_ref().map(|summary| (
                summary.champion_name.as_str(),
                summary.kills,
                summary.deaths,
                summary.assists
            )),
            Some(("Nautilus", 3, 4, 23))
        );
    }

    #[test]
    fn summary_only_markers_are_export_sidecar_content() {
        let markers = ClipMarkers {
            recording_start_s: 10.0,
            duration_s: 20.0,
            player_summary: Some(PlayerSummary {
                champion_name: "Nautilus".into(),
                kills: 3,
                deaths: 4,
                assists: 23,
                creep_score: None,
                game_time_s: None,
                player_name: String::new(),
                team: String::new(),
                participants: Vec::new(),
                summoner_spells: Vec::new(),
                items: Vec::new(),
            }),
            audio_tracks: Vec::new(),
            plays: Vec::new(),
            markers: Vec::new(),
        };

        assert!(has_marker_sidecar_content(&markers));
    }

    #[test]
    fn empty_markers_are_not_export_sidecar_content() {
        let markers = ClipMarkers {
            recording_start_s: 10.0,
            duration_s: 20.0,
            player_summary: None,
            audio_tracks: Vec::new(),
            plays: Vec::new(),
            markers: Vec::new(),
        };

        assert!(!has_marker_sidecar_content(&markers));
    }

    #[test]
    fn play_only_markers_are_export_sidecar_content() {
        let markers = ClipMarkers {
            recording_start_s: 10.0,
            duration_s: 20.0,
            player_summary: None,
            audio_tracks: Vec::new(),
            plays: vec![osu_play(2.0, Some(8.0), "score-1")],
            markers: Vec::new(),
        };

        assert!(has_marker_sidecar_content(&markers));
    }

    #[test]
    fn export_markers_can_be_suppressed_for_play_exports() {
        let dir = TestDir::new("clipline-library", "export-no-markers");
        let source = dir.path().join("session.mp4");
        std::fs::write(&source, b"mp4").unwrap();
        let markers = ClipMarkers {
            recording_start_s: 10.0,
            duration_s: 20.0,
            player_summary: None,
            audio_tracks: Vec::new(),
            plays: vec![osu_play(2.0, Some(8.0), "score-1")],
            markers: Vec::new(),
        };
        std::fs::write(
            source.with_extension("markers.json"),
            serde_json::to_string(&markers).unwrap(),
        )
        .unwrap();

        assert!(export_markers_for_range(&source, 2.0, 8.0, false)
            .unwrap()
            .is_none());
    }

    #[test]
    fn crop_markers_keeps_and_clamps_overlapping_plays() {
        let markers = ClipMarkers {
            recording_start_s: 10.0,
            duration_s: 20.0,
            player_summary: None,
            audio_tracks: Vec::new(),
            plays: vec![
                osu_play(0.0, Some(2.0), "before"),
                osu_play(2.0, Some(8.0), "overlap"),
                osu_play(5.0, None, "point"),
                osu_play(8.0, Some(12.0), "after"),
            ],
            markers: Vec::new(),
        };

        let cropped = crop_markers(&markers, 4.0, 6.0);

        let ids: Vec<_> = cropped
            .plays
            .iter()
            .map(|play| play.external_id.as_str())
            .collect();
        assert_eq!(ids, vec!["overlap", "point"]);
        assert_eq!(cropped.plays[0].t_start_s, 0.0);
        assert_eq!(cropped.plays[0].t_end_s, Some(2.0));
        assert_eq!(cropped.plays[1].t_start_s, 1.0);
        assert_eq!(cropped.plays[1].t_end_s, None);
    }

    #[test]
    fn audio_tracks_are_export_sidecar_content_and_survive_cropping() {
        let tracks = vec![ClipAudioTrack {
            id: "microphone".into(),
            track_index: 1,
            label: "Microphone".into(),
            kind: Some("microphone".into()),
        }];
        let markers = ClipMarkers {
            recording_start_s: 10.0,
            duration_s: 20.0,
            player_summary: None,
            audio_tracks: tracks.clone(),
            plays: Vec::new(),
            markers: Vec::new(),
        };

        assert!(has_marker_sidecar_content(&markers));
        let cropped = crop_markers(&markers, 3.0, 7.0);

        assert_eq!(cropped.audio_tracks, tracks);
        assert_eq!(cropped.markers.len(), 0);
        assert!((cropped.duration_s - 4.0).abs() < 1e-9);
    }

    #[test]
    fn selected_audio_track_indices_follow_sidecar_order_and_reject_unknown_ids() {
        let markers = ClipMarkers {
            recording_start_s: 0.0,
            duration_s: 10.0,
            player_summary: None,
            audio_tracks: vec![
                ClipAudioTrack {
                    id: "output".into(),
                    track_index: 0,
                    label: "Output Audio".into(),
                    kind: Some("output".into()),
                },
                ClipAudioTrack {
                    id: "microphone".into(),
                    track_index: 1,
                    label: "Microphone".into(),
                    kind: Some("microphone".into()),
                },
            ],
            plays: Vec::new(),
            markers: Vec::new(),
        };

        assert_eq!(
            util::selected_audio_track_indices(&markers, &["microphone".into()]).unwrap(),
            vec![1]
        );
        assert_eq!(
            util::selected_audio_track_indices(&markers, &["microphone".into(), "output".into()])
                .unwrap(),
            vec![0, 1]
        );

        let err = util::selected_audio_track_indices(&markers, &["discord".into()]).unwrap_err();
        assert!(err.contains("unknown audio track"), "{err}");
    }

    #[test]
    fn clipboard_share_export_mixes_multiple_selected_tracks() {
        let dir = TestDir::new("clipline-library", "clipboard-share-mix");
        let source = dir.path().join("clip.mp4");
        std::fs::write(&source, b"source mp4").unwrap();
        let markers = ClipMarkers {
            recording_start_s: 0.0,
            duration_s: 10.0,
            player_summary: None,
            audio_tracks: vec![
                ClipAudioTrack {
                    id: "output".into(),
                    track_index: 0,
                    label: "Output Audio".into(),
                    kind: Some("output".into()),
                },
                ClipAudioTrack {
                    id: "microphone".into(),
                    track_index: 1,
                    label: "Microphone".into(),
                    kind: Some("microphone".into()),
                },
            ],
            plays: Vec::new(),
            markers: Vec::new(),
        };
        std::fs::write(
            source.with_extension("markers.json"),
            serde_json::to_string(&markers).unwrap(),
        )
        .unwrap();

        let selected = vec!["output".to_string(), "microphone".to_string()];
        let export_dir = dir.path().join("share-exports");
        let exported = clipboard_share_path_with_exporter(
            &source,
            Some(&selected),
            &export_dir,
            |input, target, mode| {
                assert_eq!(input, source.as_path());
                assert_eq!(mode, Some(ShareAudioExportMode::Mix(vec![0, 1])));
                std::fs::write(target, b"mixed share mp4").unwrap();
                Ok(())
            },
        )
        .unwrap();

        assert!(exported.starts_with(&export_dir));
        assert_eq!(std::fs::read(exported).unwrap(), b"mixed share mp4");
    }

    #[test]
    fn clipboard_share_without_audio_selection_prepares_compatibility_export() {
        let dir = TestDir::new("clipline-library", "clipboard-share-original");
        let source = dir.path().join("clip.mp4");
        std::fs::write(&source, b"source mp4").unwrap();

        let selected = None::<&[String]>;
        let chosen = clipboard_share_path_with_exporter(
            &source,
            selected,
            &dir.path().join("share"),
            |input, target, mode| {
                assert_eq!(input, source);
                assert_eq!(mode, None);
                std::fs::write(target, b"compatible mp4").unwrap();
                Ok(())
            },
        )
        .unwrap();

        assert_ne!(chosen, source);
        assert_eq!(std::fs::read(chosen).unwrap(), b"compatible mp4");
    }

    #[test]
    fn original_clipboard_copy_bypasses_share_export() {
        let dir = TestDir::new("clipline-library", "clipboard-original-bypass");
        let source = dir.path().join("clip.mp4");
        std::fs::write(&source, b"source mp4").unwrap();

        let chosen = clipboard_copy_path_with_exporter(
            &source,
            Some(&["output".to_string()]),
            true,
            &dir.path().join("share"),
            |_, _, _| panic!("original copy must not prepare a share export"),
        )
        .unwrap();

        assert_eq!(chosen, source);
    }

    #[test]
    fn ffmpeg_share_export_stream_copies_h264_and_encodes_aac_lc() {
        let args = ffmpeg_share_export_args(
            Path::new("selected.mp4"),
            Path::new("share.mp4.tmp"),
            true,
            &ShareVideoExportMode::Copy,
        );

        assert!(args.windows(2).any(|pair| pair == ["-c:v", "copy"]));
        assert!(args.windows(2).any(|pair| pair == ["-map", "0:a:0"]));
        assert!(args.windows(2).any(|pair| pair == ["-c:a", "aac"]));
        assert!(args
            .windows(2)
            .any(|pair| pair == ["-profile:a", "aac_low"]));
        assert!(args
            .windows(2)
            .any(|pair| pair == ["-movflags", "+faststart"]));
        assert_eq!(args.last().map(String::as_str), Some("share.mp4.tmp"));
    }

    #[test]
    fn ffmpeg_share_export_omits_audio_for_muted_selection() {
        let args = ffmpeg_share_export_args(
            Path::new("muted.mp4"),
            Path::new("share.mp4.tmp"),
            false,
            &ShareVideoExportMode::Copy,
        );

        assert!(!args.iter().any(|arg| arg == "0:a:0"));
        assert!(!args.iter().any(|arg| arg == "-c:a"));
    }

    #[test]
    fn ffmpeg_share_export_can_transcode_video_with_mf_fallback() {
        let args = ffmpeg_share_export_args(
            Path::new("av1.mp4"),
            Path::new("share.mp4.tmp"),
            true,
            &ShareVideoExportMode::Encode {
                encoder: "h264_mf".into(),
                backend: clipline_capture::EncoderBackend::MfSoftware,
            },
        );

        assert!(args.windows(2).any(|pair| pair == ["-c:v", "h264_mf"]));
        assert!(args.windows(2).any(|pair| pair == ["-hw_encoding", "0"]));
        assert!(args.windows(2).any(|pair| pair == ["-pix_fmt", "nv12"]));
        assert!(args.windows(2).any(|pair| pair == ["-b:v", "8000000"]));
    }

    #[test]
    fn ffmpeg_share_export_applies_backend_specific_rate_control() {
        use clipline_capture::EncoderBackend;

        for (encoder, backend, required) in [
            (
                "h264_nvenc",
                EncoderBackend::Nvenc,
                ["-rc", "cbr", "-preset", "p4"],
            ),
            (
                "h264_amf",
                EncoderBackend::Amf,
                ["-rc", "cbr", "-usage", "lowlatency"],
            ),
            (
                "h264_qsv",
                EncoderBackend::QuickSync,
                ["-low_power", "0", "-maxrate", "8000000"],
            ),
        ] {
            let args = ffmpeg_share_export_args(
                Path::new("source.mp4"),
                Path::new("share.mp4.tmp"),
                true,
                &ShareVideoExportMode::Encode {
                    encoder: encoder.into(),
                    backend,
                },
            );
            let joined = args.join(" ");
            for pair in required.chunks_exact(2) {
                assert!(
                    joined.contains(&pair.join(" ")),
                    "{encoder} missing {} in {joined}",
                    pair.join(" ")
                );
            }
            assert!(joined.contains("-b:v 8000000"), "{encoder}: {joined}");
            assert!(joined.contains("-bufsize 16000000"), "{encoder}: {joined}");
        }
    }

    #[test]
    fn remaining_share_export_timeout_uses_one_deadline() {
        let start = Instant::now();
        let deadline = start + Duration::from_secs(10);

        assert_eq!(
            remaining_share_export_timeout(deadline, start + Duration::from_secs(3)),
            Some(Duration::from_secs(7))
        );
        assert_eq!(remaining_share_export_timeout(deadline, deadline), None);
        assert_eq!(
            remaining_share_export_timeout(deadline, deadline + Duration::from_secs(1)),
            None
        );
    }

    #[test]
    fn share_export_tmp_path_is_unique_per_writer() {
        let dir = TestDir::new("clipline-library", "share-export-temp");
        let export = dir.path().join("share-export-abc.mp4");

        let first = share_export_tmp_path(&export).unwrap();
        let second = share_export_tmp_path(&export).unwrap();

        assert_ne!(first, second);
        assert_ne!(first, export.with_extension("mp4.tmp"));
        assert_eq!(first.parent(), export.parent());
    }

    #[test]
    fn share_export_prune_removes_orphaned_tmp_files() {
        let dir = TestDir::new("clipline-library", "share-export-prune-tmp");
        let export = dir.path().join("share-export-old.mp4");
        let orphan = dir.path().join("share-export-old.mp4.tmp");
        let unique = share_export_tmp_path(&export).unwrap();
        let nested_unique = cached_export_tmp_path(&unique).unwrap();
        let malformed = dir.path().join("share-export-old.mp4.pid.counter.tmp");
        std::fs::write(&export, b"old export").unwrap();
        std::fs::write(&orphan, b"orphan").unwrap();
        std::fs::write(&unique, b"unique orphan").unwrap();
        std::fs::write(&nested_unique, b"nested unique orphan").unwrap();
        std::fs::write(&malformed, b"not an owned temp shape").unwrap();

        prune_cached_mp4_files(dir.path(), std::time::Duration::ZERO);

        assert!(!export.exists());
        assert!(!orphan.exists());
        assert!(!unique.exists());
        assert!(!nested_unique.exists());
        assert!(malformed.exists());
    }

    #[test]
    fn audio_preview_cache_prunes_lru_and_partials_but_preserves_protected_file() {
        let dir = TestDir::new("clipline-library", "audio-preview-cache-lru");
        let oldest = dir.path().join("audio-preview-0001.mp4");
        let newest = dir.path().join("audio-preview-0002.mp4");
        let protected = dir.path().join("audio-preview-0003.mp4");
        let partial = dir.path().join("audio-preview-0004.mp4.1.2.tmp");
        std::fs::write(&oldest, [0_u8; 6]).unwrap();
        std::fs::write(&newest, [0_u8; 6]).unwrap();
        std::fs::write(&protected, [0_u8; 20]).unwrap();
        std::fs::write(&partial, [0_u8; 3]).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&oldest)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1))
            .unwrap();
        std::fs::File::options()
            .write(true)
            .open(&newest)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(2))
            .unwrap();

        let report =
            prune_audio_preview_cache(dir.path(), std::slice::from_ref(&protected), 26).unwrap();

        assert!(!oldest.exists());
        assert!(newest.exists());
        assert!(protected.exists());
        assert!(!partial.exists());
        assert_eq!(report.reusable_bytes, 6);
    }

    #[test]
    fn audio_preview_cache_keeps_oversized_protected_and_evicts_all_reusable() {
        let dir = TestDir::new(
            "clipline-library",
            "audio-preview-cache-oversized-protected",
        );
        let oldest = dir.path().join("audio-preview-0001.mp4");
        let newest = dir.path().join("audio-preview-0002.mp4");
        let protected = dir.path().join("audio-preview-0003.mp4");
        std::fs::write(&oldest, [0_u8; 6]).unwrap();
        std::fs::write(&newest, [0_u8; 6]).unwrap();
        std::fs::write(&protected, [0_u8; 20]).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&oldest)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1))
            .unwrap();
        std::fs::File::options()
            .write(true)
            .open(&newest)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(2))
            .unwrap();

        let report =
            prune_audio_preview_cache(dir.path(), std::slice::from_ref(&protected), 10).unwrap();

        assert!(!oldest.exists());
        assert!(!newest.exists());
        assert!(protected.exists());
        assert_eq!(report.reusable_bytes, 0);
    }

    #[test]
    fn audio_preview_cache_hit_refreshes_recency() {
        let dir = TestDir::new("clipline-library", "audio-preview-cache-touch");
        let preview = dir.path().join("audio-preview-abcd.mp4");
        std::fs::write(&preview, b"preview").unwrap();
        let old = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1);
        std::fs::File::options()
            .write(true)
            .open(&preview)
            .unwrap()
            .set_modified(old)
            .unwrap();

        touch_audio_preview(&preview).unwrap();

        assert!(std::fs::metadata(&preview).unwrap().modified().unwrap() > old);
    }

    #[test]
    fn audio_sidecar_uncached_tracks_extract_once_and_return_marker_ordered_paths() {
        let dir = TestDir::new("clipline-library", "audio-sidecar-ordered");
        let source = dir.path().join("clip.mp4");
        std::fs::write(&source, two_real_opus_audio_mp4()).unwrap();
        write_audio_track_markers(
            &source,
            vec![
                ("microphone", 1, "Microphone"),
                ("output", 0, "Output Audio"),
            ],
        );
        let preview_dir = dir.path().join("previews");
        let calls = std::cell::RefCell::new(Vec::<Vec<(u32, PathBuf, PathBuf)>>::new());

        let sidecars = finalize_prepared_audio_sidecars(
            prepare_clip_audio_sidecars_file_with_extractor(
                source.clone(),
                vec!["output".into(), "microphone".into()],
                Vec::new(),
                preview_dir.clone(),
                |input, outputs| {
                    assert_eq!(input, source.as_path());
                    calls.borrow_mut().push(
                        outputs
                            .iter()
                            .map(|output| {
                                (
                                    output.audio_stream_index,
                                    output.final_path.clone(),
                                    output.tmp_path.clone(),
                                )
                            })
                            .collect(),
                    );
                    for output in outputs {
                        let bytes = audio_only_opus_mp4_for_stream(output.audio_stream_index);
                        std::fs::write(&output.tmp_path, bytes).unwrap();
                    }
                    Ok(())
                },
            )
            .expect("uncached sidecars should succeed"),
            |_| Ok(()),
        )
        .expect("successful sidecars should commit");

        assert_eq!(calls.borrow().len(), 1);
        assert_eq!(sidecars.len(), 2);
        assert_eq!(sidecars[0].audio_track_id, "microphone");
        assert_eq!(sidecars[1].audio_track_id, "output");
        assert_eq!(
            calls.borrow()[0]
                .iter()
                .map(|(index, _, _)| *index)
                .collect::<Vec<_>>(),
            vec![1, 0]
        );
        assert!(Path::new(&sidecars[0].path).exists());
        assert!(Path::new(&sidecars[1].path).exists());
    }

    #[test]
    fn audio_sidecar_outputs_validate_as_audio_only_and_smaller_than_source() {
        let dir = TestDir::new("clipline-library", "audio-sidecar-audio-only");
        let source = dir.path().join("clip.mp4");
        std::fs::write(&source, two_real_opus_audio_mp4()).unwrap();
        write_audio_track_markers(
            &source,
            vec![
                ("output", 0, "Output Audio"),
                ("microphone", 1, "Microphone"),
            ],
        );

        let sidecars = finalize_prepared_audio_sidecars(
            prepare_clip_audio_sidecars_file_with_extractor(
                source.clone(),
                vec!["output".into(), "microphone".into()],
                Vec::new(),
                dir.path().join("previews"),
                |_, outputs| {
                    for output in outputs {
                        let bytes = audio_only_opus_mp4_for_stream(output.audio_stream_index);
                        std::fs::write(&output.tmp_path, bytes).unwrap();
                    }
                    Ok(())
                },
            )
            .unwrap(),
            |_| Ok(()),
        )
        .unwrap();

        let source_len = std::fs::metadata(&source).unwrap().len();
        for sidecar in sidecars {
            let bytes = std::fs::read(&sidecar.path).unwrap();
            assert_eq!(
                clipline_mp4::media_track_counts(&bytes).unwrap(),
                clipline_mp4::MediaTrackCounts { video: 0, audio: 1 }
            );
            assert!(std::fs::metadata(&sidecar.path).unwrap().len() < source_len);
        }
    }

    #[test]
    fn audio_sidecar_reuses_existing_tracks_and_extracts_only_missing_track() {
        let dir = TestDir::new("clipline-library", "audio-sidecar-reuse");
        let source = dir.path().join("clip.mp4");
        std::fs::write(&source, two_real_opus_audio_mp4()).unwrap();
        write_audio_track_markers(
            &source,
            vec![
                ("output", 0, "Output Audio"),
                ("microphone", 1, "Microphone"),
                ("discord", 1, "Discord"),
            ],
        );
        let preview_dir = dir.path().join("previews");
        let calls = std::cell::RefCell::new(Vec::<Vec<u32>>::new());

        let first = finalize_prepared_audio_sidecars(
            prepare_clip_audio_sidecars_file_with_extractor(
                source.clone(),
                vec!["output".into()],
                Vec::new(),
                preview_dir.clone(),
                |_, outputs| {
                    calls.borrow_mut().push(
                        outputs
                            .iter()
                            .map(|output| output.audio_stream_index)
                            .collect(),
                    );
                    for output in outputs {
                        let bytes = audio_only_opus_mp4_for_stream(output.audio_stream_index);
                        std::fs::write(&output.tmp_path, bytes).unwrap();
                    }
                    Ok(())
                },
            )
            .unwrap(),
            |_| Ok(()),
        )
        .unwrap();

        let second = finalize_prepared_audio_sidecars(
            prepare_clip_audio_sidecars_file_with_extractor(
                source,
                vec!["output".into(), "microphone".into()],
                Vec::new(),
                preview_dir,
                |_, outputs| {
                    calls.borrow_mut().push(
                        outputs
                            .iter()
                            .map(|output| output.audio_stream_index)
                            .collect(),
                    );
                    for output in outputs {
                        let bytes = audio_only_opus_mp4_for_stream(output.audio_stream_index);
                        std::fs::write(&output.tmp_path, bytes).unwrap();
                    }
                    Ok(())
                },
            )
            .unwrap(),
            |_| Ok(()),
        )
        .unwrap();

        assert_eq!(&*calls.borrow(), &[vec![0], vec![1]]);
        assert_eq!(first[0].path, second[0].path);
    }

    #[test]
    fn audio_sidecar_key_is_per_track_not_selection_combination() {
        let dir = TestDir::new("clipline-library", "audio-sidecar-key");
        let source = dir.path().join("clip.mp4");
        std::fs::write(&source, two_real_opus_audio_mp4()).unwrap();
        write_audio_track_markers(
            &source,
            vec![
                ("output", 0, "Output Audio"),
                ("microphone", 1, "Microphone"),
            ],
        );
        let preview_dir = dir.path().join("previews");
        let meta = std::fs::metadata(&source).unwrap();

        let output_only = audio_track_sidecar_path(&preview_dir, &source, &meta, "output");
        let output_with_other = audio_track_sidecar_path(&preview_dir, &source, &meta, "output");
        let mic = audio_track_sidecar_path(&preview_dir, &source, &meta, "microphone");

        assert_eq!(output_only, output_with_other);
        assert_ne!(output_only, mic);
    }

    #[test]
    fn audio_sidecar_prune_protects_active_and_returned_paths() {
        let dir = TestDir::new("clipline-library", "audio-sidecar-prune-protect");
        let source = dir.path().join("clip.mp4");
        std::fs::write(&source, two_real_opus_audio_mp4()).unwrap();
        write_audio_track_markers(
            &source,
            vec![
                ("output", 0, "Output Audio"),
                ("microphone", 1, "Microphone"),
            ],
        );
        let preview_dir = dir.path().join("previews");
        std::fs::create_dir_all(&preview_dir).unwrap();
        let active = preview_dir.join("audio-preview-active.mp4");
        let stale = preview_dir.join("audio-preview-stale.mp4");
        std::fs::write(&active, [0_u8; 40]).unwrap();
        std::fs::write(&stale, [0_u8; 40]).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&stale)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH)
            .unwrap();

        let sidecars = finalize_prepared_audio_sidecars(
            prepare_clip_audio_sidecars_file_with_extractor_and_limits(
                source,
                vec!["output".into(), "microphone".into()],
                vec![active.display().to_string()],
                preview_dir.clone(),
                120,
                |_, outputs| {
                    for output in outputs {
                        let bytes = audio_only_opus_mp4_for_stream(output.audio_stream_index);
                        std::fs::write(&output.tmp_path, bytes).unwrap();
                    }
                    Ok(())
                },
            )
            .unwrap(),
            |_| Ok(()),
        )
        .unwrap();

        assert!(
            active.exists(),
            "frontend-protected active sidecar must survive"
        );
        assert!(
            !stale.exists(),
            "unprotected stale cache entry should be pruned"
        );
        for sidecar in sidecars {
            assert!(
                Path::new(&sidecar.path).exists(),
                "returned sidecar must survive prune"
            );
        }
    }

    #[test]
    fn audio_sidecar_requested_cache_hit_survives_initial_prune_without_extraction() {
        let dir = TestDir::new("clipline-library", "audio-sidecar-requested-hit");
        let source = dir.path().join("clip.mp4");
        std::fs::write(&source, two_real_opus_audio_mp4()).unwrap();
        write_audio_track_markers(&source, vec![("output", 0, "Output Audio")]);

        let preview_dir = dir.path().join("previews");
        std::fs::create_dir_all(&preview_dir).unwrap();
        let meta = std::fs::metadata(&source).unwrap();
        let requested_hit = audio_track_sidecar_path(&preview_dir, &source, &meta, "output");
        std::fs::write(&requested_hit, audio_only_opus_mp4_for_stream(0)).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&requested_hit)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1))
            .unwrap();

        let stale = preview_dir.join("audio-preview-stale.mp4");
        std::fs::write(&stale, [0_u8; 40]).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&stale)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(2))
            .unwrap();

        let sidecars = finalize_prepared_audio_sidecars(
            prepare_clip_audio_sidecars_file_with_extractor_and_limits(
                source,
                vec!["output".into()],
                Vec::new(),
                preview_dir,
                std::fs::metadata(&requested_hit).unwrap().len() + 39,
                |_, _| panic!("extractor must not run for a valid requested cache hit"),
            )
            .unwrap(),
            |_| Ok(()),
        )
        .unwrap();

        assert!(
            requested_hit.exists(),
            "requested hit must survive initial prune"
        );
        assert!(!stale.exists(), "stale unrequested entry should be evicted");
        assert_eq!(sidecars.len(), 1);
        assert_eq!(sidecars[0].path, requested_hit.display().to_string());
    }

    #[test]
    fn audio_sidecar_failure_cleans_temps_and_publishes_nothing() {
        let dir = TestDir::new("clipline-library", "audio-sidecar-cleanup");
        let source = dir.path().join("clip.mp4");
        std::fs::write(&source, two_real_opus_audio_mp4()).unwrap();
        write_audio_track_markers(
            &source,
            vec![
                ("output", 0, "Output Audio"),
                ("microphone", 1, "Microphone"),
            ],
        );
        let preview_dir = dir.path().join("previews");

        let err = prepare_clip_audio_sidecars_file_with_extractor(
            source,
            vec!["output".into(), "microphone".into()],
            Vec::new(),
            preview_dir.clone(),
            |_, outputs| {
                std::fs::write(&outputs[0].tmp_path, b"invalid").unwrap();
                Err("forced extractor failure".into())
            },
        )
        .expect_err("extractor failure should bubble up");

        assert!(err.contains("forced extractor failure"), "{err}");
        assert!(
            preview_dir
                .read_dir()
                .unwrap_or_else(|_| panic!("preview dir should exist"))
                .flatten()
                .all(|entry| {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    !name.ends_with(".tmp") && !name.ends_with(".mp4")
                }),
            "failure must not leave temp or final sidecars behind"
        );
    }

    #[test]
    fn audio_sidecar_ffmpeg_args_use_one_input_and_one_audio_only_output_per_missing_stream() {
        let dir = TestDir::new("clipline-library", "audio-sidecar-ffmpeg-args");
        let source = dir.path().join("clip.mp4");
        let outputs = vec![
            AudioTrackSidecarOutput {
                audio_track_id: "output".into(),
                audio_stream_index: 0,
                final_path: dir.path().join("audio-preview-1.mp4"),
                tmp_path: dir.path().join("audio-preview-1.mp4.tmp"),
            },
            AudioTrackSidecarOutput {
                audio_track_id: "microphone".into(),
                audio_stream_index: 2,
                final_path: dir.path().join("audio-preview-2.mp4"),
                tmp_path: dir.path().join("audio-preview-2.mp4.tmp"),
            },
        ];

        let args = ffmpeg_audio_sidecar_args(&source, &outputs);

        assert_eq!(args.iter().filter(|arg| **arg == "-i").count(), 1);
        assert!(args.windows(2).any(|pair| pair == ["-map", "0:a:0"]));
        assert!(args.windows(2).any(|pair| pair == ["-map", "0:a:2"]));
        assert_eq!(args.iter().filter(|arg| **arg == "-vn").count(), 2);
        assert_eq!(args.iter().filter(|arg| **arg == "-c:a").count(), 2);
        assert_eq!(args.iter().filter(|arg| **arg == "copy").count(), 2);
        assert_eq!(
            args.iter().filter(|arg| **arg == "-map_metadata").count(),
            2
        );
        assert_eq!(args.iter().filter(|arg| **arg == "-1").count(), 2);
        assert!(!args.windows(2).any(|pair| pair == ["-map", "0:v:0"]));
        assert!(!args.iter().any(|arg| *arg == "libopus"));
        assert!(!args.iter().any(|arg| arg.contains("amix")));
    }

    #[test]
    fn audio_sidecar_publication_guard_removes_owned_finals_but_keeps_collision_winner() {
        let dir = TestDir::new("clipline-library", "audio-sidecar-publication-guard");
        let owned_final = dir.path().join("audio-preview-owned.mp4");
        let owned_tmp = dir.path().join("audio-preview-owned.mp4.tmp");
        let collision_final = dir.path().join("audio-preview-collision.mp4");
        let collision_tmp = dir.path().join("audio-preview-collision.mp4.tmp");
        std::fs::write(&owned_tmp, audio_only_opus_mp4_for_stream(0)).unwrap();
        std::fs::write(&collision_tmp, audio_only_opus_mp4_for_stream(1)).unwrap();
        std::fs::write(&collision_final, audio_only_opus_mp4_for_stream(1)).unwrap();

        let outputs = vec![
            AudioTrackSidecarOutput {
                audio_track_id: "owned".into(),
                audio_stream_index: 0,
                final_path: owned_final.clone(),
                tmp_path: owned_tmp.clone(),
            },
            AudioTrackSidecarOutput {
                audio_track_id: "collision".into(),
                audio_stream_index: 1,
                final_path: collision_final.clone(),
                tmp_path: collision_tmp.clone(),
            },
        ];

        let guard = validate_and_publish_audio_sidecars(&outputs).unwrap();
        assert!(
            owned_final.exists(),
            "successful rename should publish owned final"
        );
        assert!(
            collision_final.exists(),
            "existing collision winner must remain"
        );
        assert!(!owned_tmp.exists(), "owned temp should be consumed");
        assert!(!collision_tmp.exists(), "collision temp should be removed");
        drop(guard);

        assert!(
            !owned_final.exists(),
            "dropping uncommitted guard should remove invocation-owned finals"
        );
        assert!(
            collision_final.exists(),
            "dropping uncommitted guard must not delete collision winners"
        );
    }

    #[test]
    fn audio_sidecar_validation_failure_owns_temp_cleanup() {
        let dir = TestDir::new("clipline-library", "audio-sidecar-validation-cleanup");
        let valid_tmp = dir.path().join("audio-preview-valid.mp4.tmp");
        let invalid_tmp = dir.path().join("audio-preview-invalid.mp4.tmp");
        std::fs::write(&valid_tmp, audio_only_opus_mp4_for_stream(0)).unwrap();
        std::fs::write(&invalid_tmp, b"invalid").unwrap();
        let outputs = vec![
            AudioTrackSidecarOutput {
                audio_track_id: "valid".into(),
                audio_stream_index: 0,
                final_path: dir.path().join("audio-preview-valid.mp4"),
                tmp_path: valid_tmp.clone(),
            },
            AudioTrackSidecarOutput {
                audio_track_id: "invalid".into(),
                audio_stream_index: 1,
                final_path: dir.path().join("audio-preview-invalid.mp4"),
                tmp_path: invalid_tmp.clone(),
            },
        ];

        validate_and_publish_audio_sidecars(&outputs)
            .expect_err("invalid extracted sidecar should fail validation");

        assert!(
            !valid_tmp.exists(),
            "validation failure must remove sibling temps"
        );
        assert!(
            !invalid_tmp.exists(),
            "validation failure must remove invalid temp"
        );
    }

    #[test]
    fn audio_sidecar_scope_failure_rolls_back_all_invocation_owned_finals() {
        let dir = TestDir::new("clipline-library", "audio-sidecar-scope-rollback");
        let source = dir.path().join("clip.mp4");
        touch_mp4(&source);
        write_audio_track_markers(
            &source,
            vec![
                ("output", 0, "Output Audio"),
                ("microphone", 1, "Microphone"),
                ("discord", 2, "Discord"),
            ],
        );
        let preview_dir = dir.path().join("previews");
        std::fs::create_dir_all(&preview_dir).unwrap();

        let winner_path = audio_track_sidecar_path(
            &preview_dir,
            &source,
            &std::fs::metadata(&source).unwrap(),
            "output",
        );
        std::fs::write(&winner_path, audio_only_opus_mp4_for_stream(0)).unwrap();
        let winner_bytes = std::fs::read(&winner_path).unwrap();

        let batch = prepare_clip_audio_sidecars_file_with_extractor_and_limits(
            source,
            vec!["output".into(), "microphone".into(), "discord".into()],
            Vec::new(),
            preview_dir.clone(),
            AUDIO_PREVIEW_CACHE_MAX_BYTES,
            |_, outputs| {
                for output in outputs {
                    let bytes = audio_only_opus_mp4_for_stream(output.audio_stream_index);
                    std::fs::write(&output.tmp_path, bytes).unwrap();
                }
                Ok(())
            },
        )
        .unwrap();

        let err = finalize_prepared_audio_sidecars(batch, |prepared| {
            if prepared.audio_track_id == "microphone" {
                return Err("forced scope failure".into());
            }
            Ok(())
        })
        .unwrap_err();

        assert!(err.contains("forced scope failure"), "{err}");
        assert!(
            winner_path.exists(),
            "pre-existing collision winner must survive rollback"
        );
        assert_eq!(
            std::fs::read(&winner_path).unwrap(),
            winner_bytes,
            "collision winner contents must remain untouched"
        );

        let microphone_path = audio_track_sidecar_path(
            &preview_dir,
            &dir.path().join("clip.mp4"),
            &std::fs::metadata(dir.path().join("clip.mp4")).unwrap(),
            "microphone",
        );
        let discord_path = audio_track_sidecar_path(
            &preview_dir,
            &dir.path().join("clip.mp4"),
            &std::fs::metadata(dir.path().join("clip.mp4")).unwrap(),
            "discord",
        );
        assert!(
            !microphone_path.exists(),
            "scope failure must roll back invocation-owned finals"
        );
        assert!(
            !discord_path.exists(),
            "scope failure must remove every invocation-owned final"
        );
    }

    fn write_audio_track_markers(source: &Path, tracks: Vec<(&str, u32, &str)>) {
        let markers = ClipMarkers {
            recording_start_s: 0.0,
            duration_s: 1.0,
            player_summary: None,
            audio_tracks: tracks
                .into_iter()
                .map(|(id, track_index, label)| ClipAudioTrack {
                    id: id.into(),
                    track_index,
                    label: label.into(),
                    kind: Some("test".into()),
                })
                .collect(),
            plays: Vec::new(),
            markers: Vec::new(),
        };
        std::fs::write(
            source.with_extension("markers.json"),
            serde_json::to_string(&markers).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn unique_export_path_appends_suffix_when_needed() {
        let dir = TestDir::new("clipline-library", "export-name");
        let source = dir.path().join("clip_1.mp4");
        let first = dir.path().join("clip_1_trim_001000_002000.mp4");
        std::fs::write(&source, b"source").unwrap();
        std::fs::write(&first, b"existing").unwrap();

        let path = unique_export_path(&source, 1.0, 2.0, None).unwrap();

        assert_eq!(
            path.file_name().unwrap().to_string_lossy(),
            "clip_1_trim_001000_002000_1.mp4"
        );
    }

    #[test]
    fn unique_export_path_uses_requested_clip_title_when_present() {
        let dir = TestDir::new("clipline-library", "export-title");
        let source = dir.path().join("session_123.mp4");
        std::fs::write(&source, b"source").unwrap();

        let path = unique_export_path(
            &source,
            145.783,
            188.167,
            Some("I MY ME MINE - Trouble".to_string()),
        )
        .unwrap();

        assert_eq!(
            path.file_name().unwrap().to_string_lossy(),
            "I MY ME MINE - Trouble.mp4"
        );
    }

    #[test]
    fn poster_diagnostics_classify_failures_without_retaining_paths_or_stderr() {
        assert_eq!(
            poster_failure_kind("ffmpeg is not available for poster extraction"),
            "runtime_unavailable"
        );
        assert_eq!(
            poster_failure_kind("spawn ffmpeg poster: access denied"),
            "spawn_failed"
        );
        assert_eq!(
            poster_failure_kind("ffmpeg poster timed out after 30 seconds"),
            "timeout"
        );
        assert_eq!(
            poster_failure_kind("ffmpeg poster failed: C:\\private\\clip.mp4 is corrupt"),
            "media_or_codec"
        );
        assert_eq!(
            poster_failure_kind(
                "ffmpeg poster failed: clip named ffmpeg is not available for poster extraction"
            ),
            "media_or_codec"
        );
        assert_eq!(
            poster_failure_kind("ffmpeg poster produced no JPEG data"),
            "invalid_output"
        );
        assert_eq!(
            poster_failure_kind("finalize poster: sharing violation"),
            "publish_failed"
        );
    }

    #[test]
    fn poster_seek_seconds_prefers_local_player_marker_for_thumbnail() {
        let dir = TestDir::new("clipline-library", "poster-local-marker");
        let clip = dir.path().join("clip.mp4");
        touch_mp4(&clip);
        let markers = ClipMarkers {
            recording_start_s: 0.0,
            duration_s: 20.0,
            player_summary: None,
            audio_tracks: Vec::new(),
            plays: Vec::new(),
            markers: vec![
                marker_with(1.0, EventKind::DragonKill, false),
                marker_with(8.0, EventKind::ChampionAssist, true),
            ],
        };
        std::fs::write(
            clip.with_extension("markers.json"),
            serde_json::to_string(&markers).unwrap(),
        )
        .unwrap();

        assert_eq!(clipline_library::poster_seek_seconds(&clip), 8.0);
    }

    fn touch_mp4(path: &Path) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, b"\0\0\0\0").unwrap();
    }

    fn two_real_opus_audio_mp4() -> Vec<u8> {
        let tracks = vec![
            TrackConfig::Video(VideoTrackConfig::h264(
                128,
                72,
                90_000,
                vec![0x67, 0x64, 0x00, 0x0A, 0xAC],
                vec![0x68, 0xEE, 0x38, 0x80],
            )),
            TrackConfig::Audio(AudioTrackConfig {
                channels: 2,
                sample_rate: 48_000,
                pre_skip: 312,
            }),
            TrackConfig::Audio(AudioTrackConfig {
                channels: 2,
                sample_rate: 48_000,
                pre_skip: 312,
            }),
        ];
        let mut writer = HybridMp4Writer::new_multi(Cursor::new(Vec::new()), tracks).unwrap();
        let video: Vec<_> = (0..10)
            .map(|i| FragSample {
                data: format!("V{i:05}").into_bytes(),
                duration: 9_000,
                is_sync: i == 0,
            })
            .collect();
        writer
            .write_fragment_multi(&[&video, &opus_audio_packets(0.20), &opus_audio_packets(0.25)])
            .unwrap();
        writer.finalize().unwrap().into_inner()
    }

    fn audio_only_opus_mp4_for_stream(audio_stream_index: u32) -> Vec<u8> {
        let amplitude = 0.20 + 0.05 * audio_stream_index as f32;
        let tracks = vec![TrackConfig::Audio(AudioTrackConfig {
            channels: 2,
            sample_rate: 48_000,
            pre_skip: 312,
        })];
        let mut writer = HybridMp4Writer::new_multi(Cursor::new(Vec::new()), tracks).unwrap();
        let packets = opus_audio_packets(amplitude);
        writer.write_fragment_multi(&[&packets]).unwrap();
        writer.finalize().unwrap().into_inner()
    }

    fn opus_audio_packets(amplitude: f32) -> Vec<FragSample> {
        let mut encoder = Encoder::new(EncoderConfig::new(48_000, 2)).unwrap();
        (0..50)
            .map(|frame_idx| {
                let mut pcm = Vec::with_capacity(960 * 2);
                for sample_idx in 0..960 {
                    let t = (frame_idx * 960 + sample_idx) as f32 / 48_000.0;
                    let sample = (t * 440.0 * std::f32::consts::TAU).sin() * amplitude;
                    pcm.extend([sample, sample]);
                }
                let encoded = encoder.encode_f32(&pcm).unwrap();
                FragSample {
                    data: encoded,
                    duration: 960,
                    is_sync: true,
                }
            })
            .collect()
    }
}
