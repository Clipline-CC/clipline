//! Clip library commands: inventory of the configured media folder for the UI and
//! a path-validated delete. The webview never touches the filesystem
//! directly — playback goes through the asset protocol.

#[path = "library/naming.rs"]
mod naming;
use naming::{
    inferred_clip_kind_for_path, is_reserved_windows_file_name, normalized_clip_file_name,
    normalized_clip_title,
};

use std::collections::{hash_map::DefaultHasher, HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::io::{self, Read};
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::ptr;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex, OnceLock,
};
use std::time::{Duration, Instant};

use clipline_capture::{Codec, EncoderBackend};
use clipline_events::{is_review_event, ClipBookmark, ClipMarker, ClipMarkers, ClipPlay};
use clipline_mp4::{
    media_video_codecs_file, remux_with_mixed_audio_track_file,
    remux_with_selected_audio_tracks_file, trim_keyframe_aligned_file, MediaTrackCounts,
    MediaVideoCodec,
};
use clipline_storage::{remove_emptied_session_dir, storage_status as read_storage_status};
use windows_sys::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows_sys::Win32::System::Ole::{CF_HDROP, CF_UNICODETEXT};
use windows_sys::Win32::UI::Shell::DROPFILES;

use tauri::{AppHandle, Manager, Runtime};

use crate::service::{clips_dir, default_clips_dir};
use crate::util;
use crate::windows::last_os_error;

pub struct StorageSettings {
    quota_bytes: Mutex<Option<u64>>,
    media_dir: Mutex<PathBuf>,
}

#[derive(Clone, Default)]
pub struct ClipboardExportState {
    generation: Arc<AtomicU64>,
}

impl ClipboardExportState {
    fn begin(&self) -> ClipboardExportJob {
        let generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        ClipboardExportJob {
            generation,
            current: Arc::clone(&self.generation),
        }
    }

    pub fn cancel(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
    }
}

struct ClipboardExportJob {
    generation: u64,
    current: Arc<AtomicU64>,
}

impl ClipboardExportJob {
    fn is_cancelled(&self) -> bool {
        self.current.load(Ordering::Acquire) != self.generation
    }

    fn ensure_active(&self) -> Result<(), String> {
        if self.is_cancelled() {
            Err("shareable clipboard export cancelled".into())
        } else {
            Ok(())
        }
    }
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

/// The game a clip's session folder is attributed to (see
/// `clipline-session.json`). Drives the library's per-clip game icon.
#[derive(serde::Serialize, serde::Deserialize, Clone)]
pub struct ClipGame {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue: Option<clipline_lol::LeagueQueue>,
}

#[derive(serde::Serialize)]
pub struct ClipInfo {
    pub path: String,
    pub name: String,
    pub title: Option<String>,
    pub kind: String,
    /// User favorite; favorites are never auto-deleted by quota GC.
    pub favorite: bool,
    /// Session folder name; None for legacy clips at the library root.
    pub session: Option<String>,
    pub size_mb: f64,
    pub modified_unix: u64,
    pub duration_s: Option<f64>,
    pub markers: Option<ClipMarkers>,
    /// Game this clip's session belongs to, if recorded under a detected game.
    pub game: Option<ClipGame>,
}

#[derive(serde::Serialize)]
pub struct LocalClipScan {
    pub clips: Vec<ClipInfo>,
    pub warnings: Vec<String>,
}

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

#[derive(serde::Serialize)]
pub struct RenamedClipInfo {
    pub old_path: String,
    pub path: String,
    pub name: String,
    pub title: Option<String>,
    pub kind: String,
}

#[derive(serde::Serialize)]
pub struct SetClipFavoriteInfo {
    pub path: String,
    pub favorite: bool,
}

#[derive(Default, serde::Serialize, serde::Deserialize, Clone)]
struct ClipMetadata {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    /// User favorite; favorites are never auto-deleted by quota GC. Skipped
    /// when false so non-favorite sidecars stay byte-identical.
    #[serde(default, skip_serializing_if = "is_false")]
    favorite: bool,
    /// Favorite-only metadata on an imported MP4 must not opt that file into
    /// destructive storage management. Missing remains owned for compatibility.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    owned: Option<bool>,
}

fn is_false(value: &bool) -> bool {
    !*value
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
    let scope_root = dir.clone();
    let scan = tauri::async_runtime::spawn_blocking(move || list_clips_from_dir(dir))
        .await
        .map_err(|e| format!("list clips task: {e}"))??;
    let canonical_scope_root = canonical_media_root(&scope_root)?;
    for clip in &scan.clips {
        allow_local_clip_asset_from_canonical_root(
            &app,
            &canonical_scope_root,
            Path::new(&clip.path),
        )?;
    }
    Ok(scan)
}

fn list_clips_from_dir(dir: PathBuf) -> Result<LocalClipScan, String> {
    list_clips_from_dir_with_child_reader(dir, push_clips_from)
}

fn list_clips_from_dir_with_child_reader(
    dir: PathBuf,
    mut read_child: impl FnMut(&Path, Option<String>, &mut Vec<ClipInfo>) -> Result<(), String>,
) -> Result<LocalClipScan, String> {
    let mut clips = Vec::new();
    let mut warnings = Vec::new();
    push_clips_from(&dir, None, &mut clips)?;
    for entry in std::fs::read_dir(&dir).map_err(|e| e.to_string())? {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                warnings.push(format!("Skipped an unreadable Library entry: {error}"));
                continue;
            }
        };
        let session = entry.file_name().to_string_lossy().into_owned();
        let is_dir = match entry.metadata() {
            Ok(metadata) => metadata.is_dir(),
            Err(error) => {
                warnings.push(format!(
                    "Skipped Library entry \"{session}\" because its metadata is unavailable: {error}"
                ));
                continue;
            }
        };
        if is_dir {
            if let Err(error) = read_child(&entry.path(), Some(session.clone()), &mut clips) {
                warnings.push(format!(
                    "Skipped Library session \"{session}\" because it could not be read: {error}"
                ));
            }
        }
    }
    for warning in &warnings {
        tracing::warn!(event = "library_scan_partial", message = %warning);
    }
    clips.sort_by_key(|c| std::cmp::Reverse(c.modified_unix));
    Ok(LocalClipScan { clips, warnings })
}

fn push_clips_from(
    dir: &Path,
    session: Option<String>,
    clips: &mut Vec<ClipInfo>,
) -> Result<(), String> {
    // One game tag per session folder, shared by every clip inside it.
    let session_game: Option<ClipGame> = std::fs::read_to_string(dir.join("clipline-session.json"))
        .ok()
        .and_then(|json| serde_json::from_str::<ClipGame>(&json).ok());
    for entry in std::fs::read_dir(dir).map_err(|e| e.to_string())? {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("mp4") {
            continue;
        }
        let meta = entry.metadata().ok();
        if meta.as_ref().is_some_and(|m| !m.is_file()) {
            continue;
        }
        let modified_unix = meta
            .as_ref()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let size_mb = meta
            .map(|m| m.len() as f64 / (1024.0 * 1024.0))
            .unwrap_or(0.0);
        let raw_markers = util::read_markers_raw(&path).map(filter_review_markers);
        let clip_metadata = read_clip_metadata(&path).unwrap_or_default();
        let duration_s = raw_markers
            .as_ref()
            .map(|markers| markers.duration_s)
            .filter(|duration| duration.is_finite() && *duration >= 0.0);
        let markers = util::markers_with_inferred_audio_tracks(&path, raw_markers);
        let title = clip_title_from_metadata(&clip_metadata);
        let kind = clip_kind_from_metadata(&path, &clip_metadata).to_string();
        // Prefer the session sidecar; fall back to the game named in markers
        // so clips recorded before session tagging still show an icon.
        let game = session_game
            .clone()
            .or_else(|| game_from_markers(markers.as_ref()));
        clips.push(ClipInfo {
            path: path.display().to_string(),
            name: path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            title,
            kind,
            favorite: clip_metadata.favorite,
            session: session.clone(),
            size_mb,
            modified_unix,
            duration_s,
            markers,
            game,
        });
    }
    Ok(())
}

/// Fall back to the game named in a clip's markers when its session folder has
/// no game sidecar (clips recorded before session tagging existed). Only games
/// with a matching plugin resolve to an icon in the UI.
fn game_from_markers(markers: Option<&ClipMarkers>) -> Option<ClipGame> {
    let game_id = markers?.markers.first()?.event.game_id;
    let plugin_id = crate::game_plugins::plugin_id_for_game_id(game_id);
    let name = crate::game_plugins::all()
        .iter()
        .find(|plugin| plugin.id() == plugin_id)
        .map(|plugin| plugin.manifest.name.clone())?;
    Some(ClipGame {
        id: plugin_id.to_string(),
        name,
        queue: None,
    })
}

/// Only two heavyweight ffmpeg poster children may exist at once.
const MAX_CONCURRENT_POSTER_EXTRACTIONS: usize = 2;
type PosterExtractionResult = Result<PathBuf, String>;

struct PosterExtractionFlight {
    result: tokio::sync::watch::Sender<Option<PosterExtractionResult>>,
}

struct PosterExtractionCoordinator {
    permits: Arc<tokio::sync::Semaphore>,
    flights: tokio::sync::Mutex<HashMap<PathBuf, Arc<PosterExtractionFlight>>>,
}

impl PosterExtractionCoordinator {
    fn new(max_concurrent: usize) -> Self {
        assert!(
            max_concurrent > 0,
            "poster extraction concurrency must be non-zero"
        );
        Self {
            permits: Arc::new(tokio::sync::Semaphore::new(max_concurrent)),
            flights: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Run one blocking extraction per canonical clip path. The worker is
    /// detached from any individual command future so a cancelled caller
    /// cannot strand followers or release its concurrency permit while ffmpeg
    /// is still alive.
    async fn run(
        self: &Arc<Self>,
        canonical_clip: PathBuf,
        work: impl FnOnce() -> PosterExtractionResult + Send + 'static,
    ) -> PosterExtractionResult {
        let (mut result, leader) = {
            let mut flights = self.flights.lock().await;
            if let Some(flight) = flights.get(&canonical_clip) {
                (flight.result.subscribe(), None)
            } else {
                let (result_tx, result_rx) = tokio::sync::watch::channel(None);
                let flight = Arc::new(PosterExtractionFlight { result: result_tx });
                flights.insert(canonical_clip.clone(), Arc::clone(&flight));
                (result_rx, Some(flight))
            }
        };

        if let Some(flight) = leader {
            let coordinator = Arc::clone(self);
            tauri::async_runtime::spawn(async move {
                let completed = coordinator.run_worker(work).await;
                flight.result.send_replace(Some(completed));

                let mut flights = coordinator.flights.lock().await;
                if flights
                    .get(&canonical_clip)
                    .is_some_and(|current| Arc::ptr_eq(current, &flight))
                {
                    flights.remove(&canonical_clip);
                }
            });
        }

        loop {
            if let Some(completed) = result.borrow().clone() {
                return completed;
            }
            result
                .changed()
                .await
                .map_err(|_| "poster extraction ended without a result".to_string())?;
        }
    }

    async fn run_worker(
        &self,
        work: impl FnOnce() -> PosterExtractionResult + Send + 'static,
    ) -> PosterExtractionResult {
        let _permit = Arc::clone(&self.permits)
            .acquire_owned()
            .await
            .map_err(|_| "poster extraction coordinator closed".to_string())?;
        tauri::async_runtime::spawn_blocking(work)
            .await
            .map_err(|error| format!("clip poster task: {error}"))?
    }

    #[cfg(test)]
    async fn joined_callers(&self, canonical_clip: &Path) -> usize {
        self.flights
            .lock()
            .await
            .get(canonical_clip)
            .map_or(0, |flight| flight.result.receiver_count())
    }
}

fn poster_extraction_coordinator() -> Arc<PosterExtractionCoordinator> {
    static COORDINATOR: OnceLock<Arc<PosterExtractionCoordinator>> = OnceLock::new();
    Arc::clone(COORDINATOR.get_or_init(|| {
        Arc::new(PosterExtractionCoordinator::new(
            MAX_CONCURRENT_POSTER_EXTRACTIONS,
        ))
    }))
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
    let poster = if let Some(poster) = crate::poster::cached_poster(&target) {
        poster
    } else {
        let canonical_clip = target.clone();
        poster_extraction_coordinator()
            .run(canonical_clip, move || {
                let seek_s = poster_seek_seconds(&target);
                crate::poster::ensure_poster(&target, seek_s)
            })
            .await
            .inspect_err(|error| log_poster_failure_once(error))?
    };
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

/// The frame to grab a poster from: prefer a local-player review event, then
/// the first review event, else a little into the clip to skip black opening.
fn poster_seek_seconds(clip: &Path) -> f64 {
    let Some(markers) = util::read_markers_raw(clip) else {
        return 1.0;
    };
    let markers = filter_review_markers(markers);
    let duration_ok = markers.duration_s.is_finite() && markers.duration_s > 0.0;
    if let Some(first) = markers
        .markers
        .iter()
        .find(|marker| marker.event.involves_local_player)
        .or_else(|| markers.markers.first())
    {
        let t = first.t_s.max(0.0);
        return if duration_ok {
            t.min((markers.duration_s - 0.2).max(0.0))
        } else {
            t
        };
    }
    if duration_ok {
        (markers.duration_s * 0.15).min(5.0)
    } else {
        1.0
    }
}

#[tauri::command]
pub fn delete_clip(path: String, settings: tauri::State<StorageSettings>) -> Result<(), String> {
    let target = validate_clip_path(&settings, &path)?;
    let media_root = settings.clips_dir()?;
    remove_clip_files(&target, &media_root)
}

/// Marks or unmarks a clip as a favorite. Favorites are never auto-deleted by
/// quota GC, and the Library's Favorites chip isolates them.
#[tauri::command]
pub fn set_clip_favorite(
    path: String,
    favorite: bool,
    settings: tauri::State<StorageSettings>,
) -> Result<SetClipFavoriteInfo, String> {
    let target = validate_clip_path(&settings, &path)?;
    set_clip_favorite_impl(&target, favorite)
}

pub(crate) fn set_clip_favorite_impl(
    target: &Path,
    favorite: bool,
) -> Result<SetClipFavoriteInfo, String> {
    let _guard = clipline_storage::lock_clip_mutations();
    if !target.is_file() {
        return Err("clip no longer exists".into());
    }
    let owned = clipline_storage::is_clip_owned(target);
    let mut metadata = read_clip_metadata(target).unwrap_or_default();
    if !owned {
        metadata.owned = Some(false);
    }
    metadata.favorite = favorite;
    write_clip_metadata(target, &metadata)?;
    Ok(SetClipFavoriteInfo {
        path: target.display().to_string(),
        favorite,
    })
}

/// Every file that lives and dies with a clip: the MP4 plus one sidecar per
/// storage-recognized suffix. `with_extension` replaces the extension, so each
/// suffix is written relative to the clip stem.
pub(crate) fn clip_sidecar_paths(target: &Path) -> Vec<PathBuf> {
    clipline_storage::CLIP_SIDECAR_SUFFIXES
        .iter()
        .map(|suffix| {
            target.with_extension(suffix.trim_start_matches('.'))
        })
        .collect()
}

fn remove_clip_files(target: &Path, media_root: &Path) -> Result<(), String> {
    if let Some(error) = crate::cloud_upload::active_upload_source_error(target) {
        return Err(error);
    }
    std::fs::remove_file(target).map_err(|error| {
        crate::cloud_upload::active_upload_source_error(target).unwrap_or_else(|| error.to_string())
    })?;
    for sidecar in clip_sidecar_paths(target) {
        let _ = std::fs::remove_file(sidecar);
    }
    if let Some(parent) = target.parent() {
        let _ = remove_emptied_session_dir(parent, media_root);
    }
    Ok(())
}

/// A bulk-delete result: the paths that were removed and the (path, reason)
/// pairs that could not be. Surface `failed` to the UI so partial success is
/// visible rather than silently swallowed.
#[derive(serde::Serialize)]
pub struct DeletedClipsReport {
    pub deleted: Vec<String>,
    pub failed: Vec<(String, String)>,
}

/// Testable core of [`delete_clips`]: deletes each already-validated clip plus
/// its four sidecars (best effort), recording any removal failures. `failed`
/// carries inputs that already failed validation so the caller's report stays
/// complete in one place.
fn delete_clips_impl(
    media_root: PathBuf,
    validated: Vec<(String, PathBuf)>,
    mut failed: Vec<(String, String)>,
) -> DeletedClipsReport {
    let mut deleted = Vec::new();
    for (path, target) in validated {
        match remove_clip_files(&target, &media_root) {
            Ok(_) => deleted.push(path),
            Err(e) => failed.push((path, e.to_string())),
        }
    }
    DeletedClipsReport { deleted, failed }
}

/// Delete many clips in one round trip. Validation runs up front while the
/// `StorageSettings` borrow is live; owned `PathBuf`s then move into a single
/// blocking task so the UI does not pay N async hops.
#[tauri::command]
pub async fn delete_clips(
    paths: Vec<String>,
    settings: tauri::State<'_, StorageSettings>,
) -> Result<DeletedClipsReport, String> {
    let mut validated: Vec<(String, PathBuf)> = Vec::with_capacity(paths.len());
    let mut failed: Vec<(String, String)> = Vec::new();
    let media_root = settings.clips_dir()?;
    for path in paths {
        match validate_clip_path(&settings, &path) {
            Ok(target) => validated.push((path, target)),
            Err(e) => failed.push((path, e)),
        }
    }
    let result = tauri::async_runtime::spawn_blocking(move || {
        delete_clips_impl(media_root, validated, failed)
    })
    .await
    .map_err(|e| format!("delete clips task: {e}"))?;
    Ok(result)
}

#[tauri::command]
pub async fn rename_clip(
    path: String,
    name: String,
    settings: tauri::State<'_, StorageSettings>,
    _state: tauri::State<'_, crate::app::RuntimeState>,
) -> Result<RenamedClipInfo, String> {
    let source = validate_clip_path(&settings, &path)?;
    let title = normalized_clip_title(&name)?;
    let old_path = path.clone();
    tauri::async_runtime::spawn_blocking(move || rename_clip_title(source, old_path, title))
        .await
        .map_err(|e| format!("rename clip task: {e}"))?
}

#[tauri::command]
pub async fn rename_clip_file(
    path: String,
    name: String,
    settings: tauri::State<'_, StorageSettings>,
    state: tauri::State<'_, crate::app::RuntimeState>,
) -> Result<RenamedClipInfo, String> {
    let source = validate_clip_path(&settings, &path)?;
    let target_name = normalized_clip_file_name(&name)?;
    let old_path = path.clone();
    let renamed = tauri::async_runtime::spawn_blocking(move || {
        rename_clip_files(source, old_path, target_name)
    })
    .await
    .map_err(|e| format!("rename clip task: {e}"))??;

    update_cloud_record_paths(&state, &path, &renamed.path);
    Ok(renamed)
}

fn rename_clip_title(
    source: PathBuf,
    old_path: String,
    title: String,
) -> Result<RenamedClipInfo, String> {
    let _guard = clipline_storage::lock_clip_mutations();
    if !source.is_file() {
        return Err("clip no longer exists".into());
    }
    let mut metadata = read_clip_metadata(&source).unwrap_or_default();
    let kind = clip_kind_from_metadata(&source, &metadata).to_string();
    metadata.title = Some(title.clone());
    metadata.kind = Some(kind.clone());
    metadata.owned = None;
    write_clip_metadata(&source, &metadata)?;
    let name = source
        .file_name()
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or_default();
    Ok(RenamedClipInfo {
        old_path: old_path.clone(),
        path: old_path,
        name,
        title: Some(title),
        kind,
    })
}

fn same_existing_path(first: &Path, second: &Path) -> bool {
    match (first.canonicalize(), second.canonicalize()) {
        (Ok(first), Ok(second)) => first == second,
        _ => first == second,
    }
}

struct PreparedOsuSidecarMove {
    source: PathBuf,
    target: PathBuf,
    staged: PathBuf,
    backup: PathBuf,
}

impl PreparedOsuSidecarMove {
    fn stage(source_clip: &Path, target_clip: &Path) -> Result<Option<Self>, String> {
        let source = crate::osu_enrichment::pending_path(source_clip);
        if !source.exists() {
            return Ok(None);
        }
        let target = crate::osu_enrichment::pending_path(target_clip);
        let target_is_source = same_existing_path(&target, &source);
        if target.exists() && !target_is_source {
            return Err("an osu! enrichment sidecar with that name already exists".into());
        }
        let bytes = std::fs::read(&source)
            .map_err(|error| format!("read osu! enrichment sidecar {source:?}: {error}"))?;
        let mut pending: crate::osu_enrichment::OsuPendingEnrichment =
            serde_json::from_slice(&bytes)
                .map_err(|error| format!("parse osu! enrichment sidecar {source:?}: {error}"))?;
        pending.clip_path = target_clip.display().to_string();
        let staged = target.with_extension("osu-enrichment.rename.tmp");
        let backup = source.with_extension("osu-enrichment.rename.backup");
        if staged.exists() {
            return Err(format!(
                "staged osu! enrichment path already exists: {staged:?}"
            ));
        }
        if backup.exists() {
            return Err(format!(
                "backup osu! enrichment path already exists: {backup:?}"
            ));
        }
        let json = serde_json::to_vec_pretty(&pending)
            .map_err(|error| format!("serialize osu! enrichment sidecar: {error}"))?;
        std::fs::write(&staged, json)
            .map_err(|error| format!("stage osu! enrichment sidecar {staged:?}: {error}"))?;
        Ok(Some(Self {
            source,
            target,
            staged,
            backup,
        }))
    }

    fn commit(&self) -> Result<(), String> {
        std::fs::rename(&self.source, &self.backup)
            .map_err(|error| format!("stage old osu! enrichment sidecar: {error}"))?;
        std::fs::rename(&self.staged, &self.target).map_err(|error| {
            let _ = std::fs::rename(&self.backup, &self.source);
            format!("install renamed osu! enrichment sidecar: {error}")
        })?;
        Ok(())
    }

    fn finish(&self) {
        let _ = std::fs::remove_file(&self.backup);
    }

    fn rollback(&self) {
        let _ = std::fs::remove_file(&self.target);
        if self.backup.exists() && !self.source.exists() {
            let _ = std::fs::rename(&self.backup, &self.source);
        }
        let _ = std::fs::remove_file(&self.staged);
    }
}

impl Drop for PreparedOsuSidecarMove {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.staged);
    }
}

fn rename_clip_files(
    source: PathBuf,
    old_path: String,
    target_name: String,
) -> Result<RenamedClipInfo, String> {
    let _guard = clipline_storage::lock_clip_mutations();
    if let Some(error) = crate::cloud_upload::active_upload_source_error(&source) {
        return Err(error);
    }
    let parent = source
        .parent()
        .ok_or_else(|| "clip has no containing folder".to_string())?;
    let target = parent.join(&target_name);
    let source_metadata = clip_metadata_path(&source);
    let target_metadata = clip_metadata_path(&target);
    let mut metadata = read_clip_metadata(&source).unwrap_or_default();
    metadata.owned = None;
    let title = clip_title_from_metadata(&metadata);
    let kind = clip_kind_from_metadata(&source, &metadata).to_string();

    let target_is_same_file = same_existing_path(&target, &source);
    if target.exists() && !target_is_same_file {
        return Err("a clip with that name already exists".into());
    }

    let source_markers = source.with_extension("markers.json");
    let target_markers = target.with_extension("markers.json");
    let target_markers_same_file = same_existing_path(&target_markers, &source_markers);
    if source_markers.exists() && target_markers.exists() && !target_markers_same_file {
        return Err("a marker sidecar with that name already exists".into());
    }

    let target_metadata_same_file = same_existing_path(&target_metadata, &source_metadata);
    if source_metadata.exists() && target_metadata.exists() && !target_metadata_same_file {
        return Err("a clip metadata sidecar with that name already exists".into());
    }

    let pending_osu_move = PreparedOsuSidecarMove::stage(&source, &target)?;

    if source != target {
        std::fs::rename(&source, &target).map_err(|error| {
            crate::cloud_upload::active_upload_source_error(&source)
                .unwrap_or_else(|| format!("rename clip: {error}"))
        })?;
    }
    if source_markers.exists() && source_markers != target_markers {
        if let Err(error) = std::fs::rename(&source_markers, &target_markers) {
            let _ = std::fs::rename(&target, &source);
            return Err(format!("rename clip markers: {error}"));
        }
    }
    let moved_metadata = source_metadata.exists() && source_metadata != target_metadata;
    if moved_metadata {
        if let Err(error) = std::fs::rename(&source_metadata, &target_metadata) {
            let _ = std::fs::rename(&target_markers, &source_markers);
            let _ = std::fs::rename(&target, &source);
            return Err(format!("rename clip metadata: {error}"));
        }
    }

    if let Some(pending) = &pending_osu_move {
        if let Err(error) = pending.commit() {
            rollback_renamed_clip_files(
                &source,
                &target,
                &source_markers,
                &target_markers,
                moved_metadata.then_some((source_metadata.as_path(), target_metadata.as_path())),
            );
            return Err(error);
        }
    }

    let mut target_metadata_value = read_clip_metadata(&target).unwrap_or(metadata);
    target_metadata_value.title = title.clone();
    target_metadata_value.kind = Some(kind.clone());
    if let Err(error) = write_clip_metadata(&target, &target_metadata_value) {
        if let Some(pending) = &pending_osu_move {
            pending.rollback();
        }
        rollback_renamed_clip_files(
            &source,
            &target,
            &source_markers,
            &target_markers,
            moved_metadata.then_some((source_metadata.as_path(), target_metadata.as_path())),
        );
        return Err(error);
    }
    if let Some(pending) = &pending_osu_move {
        pending.finish();
    }

    // The poster is a regenerable cache, not user data: move it alongside the
    // clip when we can, otherwise drop the stale one so it rebuilds on demand.
    let source_poster = crate::poster::poster_path(&source);
    if source_poster.exists() {
        let target_poster = crate::poster::poster_path(&target);
        if source_poster != target_poster
            && std::fs::rename(&source_poster, &target_poster).is_err()
        {
            let _ = std::fs::remove_file(&source_poster);
        }
    }

    let new_path = display_renamed_clip_path(&old_path, &target_name, parent);
    Ok(RenamedClipInfo {
        old_path,
        path: new_path,
        name: target_name,
        title,
        kind,
    })
}

fn rollback_renamed_clip_files(
    source: &Path,
    target: &Path,
    source_markers: &Path,
    target_markers: &Path,
    metadata: Option<(&Path, &Path)>,
) {
    if let Some((source_metadata, target_metadata)) = metadata {
        if target_metadata.exists() && source_metadata != target_metadata {
            let _ = std::fs::rename(target_metadata, source_metadata);
        }
    }
    if target_markers.exists() && source_markers != target_markers {
        let _ = std::fs::rename(target_markers, source_markers);
    }
    if target.exists() && source != target {
        let _ = std::fs::rename(target, source);
    }
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
    job: &ClipboardExportJob,
) -> Result<PathBuf, String> {
    clipboard_copy_path_with_exporter(
        source,
        selected_audio_track_ids,
        original,
        &crate::settings::share_export_cache_dir(),
        |source, target, mode| export_share_compatible_file(source, target, mode.as_ref(), job),
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
    job: &ClipboardExportJob,
) -> Result<(), String> {
    job.ensure_active()?;
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

    let result = job
        .ensure_active()
        .and_then(|()| transcode_share_file_with_ffmpeg(source, &input, target, has_audio, job));
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
    job: &ClipboardExportJob,
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
        job.ensure_active()?;
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
        match run_share_ffmpeg(&mut command, remaining, || job.is_cancelled()) {
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

fn run_share_ffmpeg(
    command: &mut Command,
    timeout: Duration,
    is_cancelled: impl Fn() -> bool,
) -> Result<ShareFfmpegOutput, String> {
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
        if is_cancelled() {
            let _ = child.kill();
            let _ = child.wait();
            break Err("ffmpeg share export cancelled".into());
        }
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
        && parts.as_chunks::<3>().0.iter().all(|chunk| {
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
    let dir = settings
        .clips_dir()?
        .canonicalize()
        .map_err(|e| e.to_string())?;
    let target = Path::new(path).canonicalize().map_err(|e| e.to_string())?;
    // Legacy clips sit at the root; session clips one folder down.
    let parent_ok = target.parent() == Some(dir.as_path())
        || target.parent().and_then(Path::parent) == Some(dir.as_path());
    if !parent_ok || target.extension().and_then(|e| e.to_str()) != Some("mp4") {
        return Err("refusing to access a clip outside the clips directory".into());
    }
    Ok(target)
}

#[tauri::command]
pub fn reveal_clip(path: String, settings: tauri::State<StorageSettings>) -> Result<(), String> {
    let target = validate_clip_path(&settings, &path)?;
    select_path_in_explorer(&target)
}

fn select_path_in_explorer(target: &Path) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    let canonical = target.display().to_string();
    // Explorer's `/select` rejects verbatim (`\\?\`) paths from `canonicalize`.
    let displayable = if let Some(rest) = canonical.strip_prefix(r"\\?\UNC\") {
        std::borrow::Cow::Owned(format!(r"\\{rest}"))
    } else {
        std::borrow::Cow::Borrowed(
            canonical
                .strip_prefix(r"\\?\")
                .unwrap_or(canonical.as_str()),
        )
    };
    std::process::Command::new("explorer.exe")
        .raw_arg(format!("/select,\"{displayable}\""))
        .spawn()
        .map_err(|e| format!("open explorer: {e}"))?;
    Ok(())
}

#[tauri::command]
pub async fn copy_clip_to_clipboard(
    request: CopyClipToClipboardRequest,
    settings: tauri::State<'_, StorageSettings>,
    exports: tauri::State<'_, ClipboardExportState>,
    window: tauri::WebviewWindow,
) -> Result<(), String> {
    let target = validate_clip_path(&settings, &request.path)?;
    let audio_track_ids = request.audio_track_ids;
    let original = request.original;
    let job = exports.begin();
    let owner = window
        .hwnd()
        .map_err(|error| format!("get Clipline window handle: {error}"))?
        .0 as isize;
    tauri::async_runtime::spawn_blocking(move || {
        let share_path = clipboard_copy_path(&target, audio_track_ids.as_deref(), original, &job)?;
        job.ensure_active()?;
        copy_file_to_clipboard(&share_path, owner as HWND)
    })
    .await
    .map_err(|e| format!("copy clip task: {e}"))?
}

#[tauri::command]
pub async fn copy_text_to_clipboard(
    text: String,
    window: tauri::WebviewWindow,
) -> Result<(), String> {
    const MAX_CLIPBOARD_TEXT_BYTES: usize = 64 * 1024;
    if text.len() > MAX_CLIPBOARD_TEXT_BYTES {
        return Err("clipboard text exceeds 64 KiB".into());
    }
    if text.contains('\0') {
        return Err("clipboard text contains a null character".into());
    }
    let owner = window
        .hwnd()
        .map_err(|error| format!("get Clipline window handle: {error}"))?
        .0 as isize;
    tauri::async_runtime::spawn_blocking(move || {
        copy_text_to_clipboard_native(&text, owner as HWND)
    })
    .await
    .map_err(|error| format!("copy text task: {error}"))?
}

#[tauri::command]
pub fn open_media_folder(settings: tauri::State<StorageSettings>) -> Result<(), String> {
    let dir = settings.clips_dir()?;
    open_folder_path(&dir)
}

fn open_folder_path(dir: &Path) -> Result<(), String> {
    std::process::Command::new("explorer.exe")
        .arg(dir)
        .spawn()
        .map_err(|e| format!("open explorer: {e}"))?;
    Ok(())
}

fn clip_metadata_path(path: &Path) -> PathBuf {
    path.with_extension("clipline.json")
}

fn read_clip_metadata(path: &Path) -> Option<ClipMetadata> {
    std::fs::read_to_string(clip_metadata_path(path))
        .ok()
        .and_then(|json| serde_json::from_str(&json).ok())
}

fn write_clip_metadata(path: &Path, metadata: &ClipMetadata) -> Result<(), String> {
    let target = clip_metadata_path(path);
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create clip metadata folder: {e}"))?;
    }
    let json =
        serde_json::to_vec_pretty(metadata).map_err(|e| format!("serialize clip metadata: {e}"))?;
    let tmp = target.with_extension("clipline.json.tmp");
    std::fs::write(&tmp, json).map_err(|e| format!("write clip metadata: {e}"))?;
    replace_clip_metadata(&tmp, &target)
}

fn replace_clip_metadata(tmp: &Path, target: &Path) -> Result<(), String> {
    match std::fs::rename(tmp, target) {
        Ok(()) => Ok(()),
        Err(error) if target.is_file() => replace_existing_clip_metadata(tmp, target, error),
        Err(error) => {
            let _ = std::fs::remove_file(tmp);
            Err(format!("replace clip metadata: {error}"))
        }
    }
}

fn replace_existing_clip_metadata(
    tmp: &Path,
    target: &Path,
    original_error: std::io::Error,
) -> Result<(), String> {
    let backup = target.with_extension(format!("json.{}.bak", std::process::id()));
    if backup.exists() {
        if let Err(error) = std::fs::remove_file(&backup) {
            let _ = std::fs::remove_file(tmp);
            return Err(format!(
                "replace clip metadata: {original_error}; remove stale clip metadata backup: {error}"
            ));
        }
    }
    if let Err(error) = std::fs::rename(target, &backup) {
        let _ = std::fs::remove_file(tmp);
        return Err(format!(
            "replace clip metadata: {original_error}; backup existing clip metadata: {error}"
        ));
    }
    if let Err(error) = std::fs::rename(tmp, target) {
        let _ = std::fs::rename(&backup, target);
        let _ = std::fs::remove_file(tmp);
        return Err(format!("replace clip metadata: {error}"));
    }
    let _ = std::fs::remove_file(backup);
    Ok(())
}

fn clip_title_from_metadata(metadata: &ClipMetadata) -> Option<String> {
    metadata
        .title
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

pub(crate) fn clip_title_for_path(path: &Path) -> String {
    let metadata = read_clip_metadata(path).unwrap_or_default();
    clip_title_from_metadata(&metadata).unwrap_or_else(|| {
        path.file_stem()
            .or_else(|| path.file_name())
            .map(|value| value.to_string_lossy().to_string())
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "Clipline clip".to_string())
    })
}

fn clip_kind_from_metadata<'a>(path: &'a Path, metadata: &'a ClipMetadata) -> &'a str {
    metadata
        .kind
        .as_deref()
        .map(str::trim)
        .filter(|value| matches!(*value, "replay" | "session" | "trim"))
        .unwrap_or_else(|| inferred_clip_kind_for_path(path))
}

pub(crate) fn clip_kind_for_path(path: &Path) -> String {
    let metadata = read_clip_metadata(path).unwrap_or_default();
    clip_kind_from_metadata(path, &metadata).to_string()
}

pub(crate) fn is_favorite_clip(path: &Path) -> bool {
    read_clip_metadata(path).is_some_and(|metadata| metadata.favorite)
}

fn display_renamed_clip_path(old_path: &str, name: &str, fallback_parent: &Path) -> String {
    Path::new(old_path)
        .parent()
        .map(|parent| parent.join(name))
        .unwrap_or_else(|| fallback_parent.join(name))
        .display()
        .to_string()
}

fn update_cloud_record_paths(state: &crate::app::RuntimeState, old_path: &str, new_path: &str) {
    if old_path == new_path {
        return;
    }
    if let Err(error) = state.update_cloud(|cloud| {
        for record in cloud.uploads.values_mut() {
            if record.path == old_path {
                record.path = new_path.to_string();
            }
        }
    }) {
        tracing::warn!(event = "renamed_clip_cloud_record_update_failed", error = %error);
    }
}

fn copy_file_to_clipboard(path: &Path, owner: HWND) -> Result<(), String> {
    let payload = dropfiles_payload(path);
    copy_payload_to_clipboard(&payload, CF_HDROP as u32, owner, false)
}

fn copy_text_to_clipboard_native(text: &str, owner: HWND) -> Result<(), String> {
    let payload = clipboard_text_payload(text);
    copy_payload_to_clipboard(&payload, CF_UNICODETEXT as u32, owner, true)
}

fn copy_payload_to_clipboard(
    payload: &[u8],
    format: u32,
    owner: HWND,
    empty_first: bool,
) -> Result<(), String> {
    let handle = unsafe { GlobalAlloc(GMEM_MOVEABLE, payload.len()) };
    if handle.is_null() {
        return Err(last_os_error("allocate clipboard memory"));
    }

    let mem = unsafe { GlobalLock(handle) };
    if mem.is_null() {
        let err = last_os_error("lock clipboard memory");
        unsafe {
            GlobalFree(handle);
        }
        return Err(err);
    }
    unsafe {
        ptr::copy_nonoverlapping(payload.as_ptr(), mem.cast::<u8>(), payload.len());
        GlobalUnlock(handle);
    }

    let mut transfer = ClipboardTransfer::new(handle);
    clipboard_transaction(
        8,
        || {
            if unsafe { OpenClipboard(owner) } == 0 {
                Err(last_os_error("open clipboard"))
            } else {
                Ok(())
            }
        },
        || unsafe {
            CloseClipboard();
        },
        || {
            if empty_first && unsafe { EmptyClipboard() } == 0 {
                return Err(last_os_error("empty clipboard"));
            }
            if unsafe { SetClipboardData(format, transfer.handle()) }.is_null() {
                Err(last_os_error("set clipboard data"))
            } else {
                transfer.release();
                Ok(())
            }
        },
        || std::thread::sleep(Duration::from_millis(15)),
    )
}

fn clipboard_transaction<E>(
    attempts: usize,
    mut open: impl FnMut() -> Result<(), E>,
    mut close: impl FnMut(),
    mut set: impl FnMut() -> Result<(), E>,
    mut wait: impl FnMut(),
) -> Result<(), E> {
    let mut last_error = None;
    for attempt in 0..attempts.max(1) {
        match open() {
            Ok(()) => {
                let result = set();
                close();
                return result;
            }
            Err(error) => {
                last_error = Some(error);
                if attempt + 1 < attempts.max(1) {
                    wait();
                }
            }
        }
    }
    Err(last_error.expect("at least one clipboard-open attempt runs"))
}

fn dropfiles_payload(path: &Path) -> Vec<u8> {
    let mut wide = shell_clipboard_path_wide(path);
    wide.extend([0, 0]);

    let header_len = size_of::<DROPFILES>();
    let byte_len = header_len + wide.len() * size_of::<u16>();
    let mut payload = vec![0u8; byte_len];
    let header = DROPFILES {
        pFiles: header_len as u32,
        pt: Default::default(),
        fNC: 0,
        fWide: 1,
    };
    unsafe {
        ptr::write_unaligned(payload.as_mut_ptr().cast::<DROPFILES>(), header);
        ptr::copy_nonoverlapping(
            wide.as_ptr().cast::<u8>(),
            payload.as_mut_ptr().add(header_len),
            wide.len() * size_of::<u16>(),
        );
    }
    payload
}

fn clipboard_text_payload(text: &str) -> Vec<u8> {
    crate::windows::wide_null(std::ffi::OsStr::new(text))
        .into_iter()
        .flat_map(u16::to_le_bytes)
        .collect()
}

fn shell_clipboard_path_wide(path: &Path) -> Vec<u16> {
    const BACKSLASH: u16 = b'\\' as u16;
    const QUESTION: u16 = b'?' as u16;
    const U: u16 = b'U' as u16;
    const N: u16 = b'N' as u16;
    const C: u16 = b'C' as u16;
    const VERBATIM: [u16; 4] = [BACKSLASH, BACKSLASH, QUESTION, BACKSLASH];
    const VERBATIM_UNC: [u16; 8] = [
        BACKSLASH, BACKSLASH, QUESTION, BACKSLASH, U, N, C, BACKSLASH,
    ];

    let wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    if wide.starts_with(&VERBATIM_UNC) {
        let mut plain = vec![BACKSLASH, BACKSLASH];
        plain.extend_from_slice(&wide[VERBATIM_UNC.len()..]);
        plain
    } else if wide.starts_with(&VERBATIM) {
        wide[VERBATIM.len()..].to_vec()
    } else {
        wide
    }
}

struct ClipboardTransfer {
    handle: HGLOBAL,
}

impl ClipboardTransfer {
    fn new(handle: HGLOBAL) -> Self {
        Self { handle }
    }

    fn handle(&self) -> HANDLE {
        self.handle
    }

    fn release(&mut self) {
        self.handle = ptr::null_mut();
    }
}

impl Drop for ClipboardTransfer {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe {
                GlobalFree(self.handle);
            }
        }
    }
}

fn filter_review_markers(mut markers: ClipMarkers) -> ClipMarkers {
    markers.markers.retain(|m| is_review_event(&m.event));
    markers
}

fn has_marker_sidecar_content(markers: &ClipMarkers) -> bool {
    !markers.markers.is_empty()
        || !markers.bookmarks.is_empty()
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
    let bookmarks = markers
        .bookmarks
        .iter()
        .filter(|bookmark| bookmark.t_s >= start_s && bookmark.t_s < end_s)
        .map(|bookmark| ClipBookmark {
            t_s: bookmark.t_s - start_s,
        })
        .collect();
    ClipMarkers {
        recording_start_s: markers.recording_start_s + start_s,
        duration_s: end_s - start_s,
        player_summary: markers.player_summary.clone(),
        audio_tracks: markers.audio_tracks.clone(),
        plays,
        markers: cropped,
        bookmarks,
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
    use clipline_mp4::{
        AudioTrackConfig, FragSample, HybridMp4Writer, TrackConfig, VideoTrackConfig,
    };
    use clipline_test_utils::TestDir;
    use shiguredo_opus::{Encoder, EncoderConfig};

    #[test]
    fn clipboard_export_generation_cancels_only_existing_jobs() {
        let state = ClipboardExportState::default();
        let first = state.begin();
        assert!(!first.is_cancelled());

        state.cancel();
        assert!(first.is_cancelled());

        let second = state.begin();
        assert!(!second.is_cancelled());
        state.cancel();
        assert!(second.is_cancelled());
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
            bookmarks: Vec::new(),
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
    fn crop_markers_crops_and_rebases_user_bookmarks() {
        let markers = ClipMarkers {
            recording_start_s: 10.0,
            duration_s: 5.0,
            player_summary: None,
            audio_tracks: Vec::new(),
            plays: Vec::new(),
            markers: Vec::new(),
            bookmarks: vec![
                ClipBookmark { t_s: 0.5 },
                ClipBookmark { t_s: 1.5 },
                ClipBookmark { t_s: 2.0 },
            ],
        };

        let cropped = crop_markers(&markers, 1.0, 2.0);

        assert_eq!(
            cropped.bookmarks,
            [ClipBookmark { t_s: 0.5 }],
            "inclusive start, exclusive end, re-based like game markers"
        );
        // A bookmark-only trim still has content worth writing a sidecar for.
        assert!(has_marker_sidecar_content(&cropped));
    }

    #[test]
    fn filter_review_markers_keeps_match_event_sources_and_drops_noise() {
        let markers = ClipMarkers {
            bookmarks: Vec::new(),
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
            bookmarks: Vec::new(),
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
            bookmarks: Vec::new(),
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
            bookmarks: Vec::new(),
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
            bookmarks: Vec::new(),
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
            bookmarks: Vec::new(),
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
            bookmarks: Vec::new(),
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
            bookmarks: Vec::new(),
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
            bookmarks: Vec::new(),
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
            for pair in required.as_chunks::<2>().0 {
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
            bookmarks: Vec::new(),
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
    fn list_clips_uses_marker_duration_without_parsing_mp4() {
        let dir = TestDir::new("clipline-library", "list-marker-duration");
        let media = dir.path().join("media");
        let clip = media.join("broken-but-listed.mp4");
        touch_mp4(&clip);
        let markers = ClipMarkers {
            bookmarks: Vec::new(),
            recording_start_s: 0.0,
            duration_s: 42.5,
            player_summary: None,
            audio_tracks: Vec::new(),
            plays: Vec::new(),
            markers: vec![marker(1.0)],
        };
        std::fs::write(
            clip.with_extension("markers.json"),
            serde_json::to_string(&markers).unwrap(),
        )
        .unwrap();

        let clips = list_clips_from_dir(media).unwrap().clips;

        assert_eq!(clips.len(), 1);
        assert_eq!(clips[0].duration_s, Some(42.5));
        assert_eq!(clips[0].markers.as_ref().unwrap().markers.len(), 1);
    }

    #[test]
    fn list_clips_preserves_optional_league_queue_metadata() {
        let dir = TestDir::new("clipline-library", "league-queue-metadata");
        let media = dir.path().join("media");
        let session = media.join("ranked-match");
        touch_mp4(&session.join("session_1.mp4"));
        std::fs::write(
            session.join("clipline-session.json"),
            r#"{
              "id":"league_of_legends",
              "name":"League of Legends",
              "queue":{"id":420,"category":"ranked-solo-duo","label":"Ranked Solo/Duo"}
            }"#,
        )
        .unwrap();

        let clips = list_clips_from_dir(media).unwrap().clips;
        let queue = clips[0]
            .game
            .as_ref()
            .and_then(|game| game.queue.as_ref())
            .expect("queue metadata should survive the library scan");
        assert_eq!(queue.id, 420);
        assert_eq!(queue.label, "Ranked Solo/Duo");
    }

    #[test]
    fn local_library_scan_keeps_readable_sessions_and_warns_about_denied_children() {
        let dir = TestDir::new("clipline-library", "partial-session-scan");
        let media = dir.path().join("media");
        let readable = media.join("readable-session");
        let denied = media.join("denied-session");
        touch_mp4(&readable.join("kept.mp4"));
        std::fs::create_dir_all(&denied).unwrap();

        let result = list_clips_from_dir_with_child_reader(media, |path, session, clips| {
            if path.ends_with("denied-session") {
                Err("access denied by test".into())
            } else {
                push_clips_from(path, session, clips)
            }
        })
        .unwrap();

        assert_eq!(result.clips.len(), 1);
        assert_eq!(result.clips[0].name, "kept.mp4");
        assert_eq!(result.warnings.len(), 1);
        assert!(result.warnings[0].contains("denied-session"));
        assert!(result.warnings[0].contains("access denied by test"));
    }

    #[test]
    fn local_library_scan_keeps_root_failures_fatal() {
        let dir = TestDir::new("clipline-library", "missing-root-scan");
        let missing = dir.path().join("missing");

        assert!(list_clips_from_dir(missing).is_err());
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
            bookmarks: Vec::new(),
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

        assert_eq!(poster_seek_seconds(&clip), 8.0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn poster_extraction_is_single_flight_per_canonical_path() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let coordinator = Arc::new(PosterExtractionCoordinator::new(
            MAX_CONCURRENT_POSTER_EXTRACTIONS,
        ));
        let dir = TestDir::new("clipline-library", "poster-single-flight");
        let clip = dir.path().join("clip.mp4");
        touch_mp4(&clip);
        let key = clip.canonicalize().unwrap();
        let expected_poster = crate::poster::poster_path(&key);
        let calls = Arc::new(AtomicUsize::new(0));
        let release = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));

        let leader = {
            let coordinator = Arc::clone(&coordinator);
            let key = key.clone();
            let expected_poster = expected_poster.clone();
            let calls = Arc::clone(&calls);
            let release = Arc::clone(&release);
            tokio::spawn(async move {
                coordinator
                    .run(key, move || {
                        calls.fetch_add(1, Ordering::SeqCst);
                        let (lock, ready) = &*release;
                        let mut released = lock.lock().expect("release lock");
                        while !*released {
                            released = ready.wait(released).expect("release wait");
                        }
                        Ok(expected_poster)
                    })
                    .await
            })
        };

        let leader_started = tokio::time::timeout(Duration::from_secs(2), async {
            while calls.load(Ordering::SeqCst) != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await;
        if leader_started.is_err() {
            let (lock, ready) = &*release;
            *lock.lock().expect("release lock") = true;
            ready.notify_all();
        }
        leader_started.expect("leader starts");

        let follower = {
            let coordinator = Arc::clone(&coordinator);
            let key = key.clone();
            let calls = Arc::clone(&calls);
            tokio::spawn(async move {
                coordinator
                    .run(key, move || {
                        calls.fetch_add(1, Ordering::SeqCst);
                        Ok(PathBuf::from("unexpected-second-poster.jpg"))
                    })
                    .await
            })
        };

        let joined = tokio::time::timeout(Duration::from_secs(2), async {
            while coordinator.joined_callers(&key).await != 2 {
                tokio::task::yield_now().await;
            }
        })
        .await;

        let (lock, ready) = &*release;
        *lock.lock().expect("release lock") = true;
        ready.notify_all();
        joined.expect("follower joins the in-flight extraction");

        let leader_result = leader.await.unwrap().unwrap();
        let follower_result = follower.await.unwrap().unwrap();
        assert_eq!(leader_result, follower_result);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 6)]
    async fn poster_extraction_runs_at_most_two_unique_paths_concurrently() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let coordinator = Arc::new(PosterExtractionCoordinator::new(
            MAX_CONCURRENT_POSTER_EXTRACTIONS,
        ));
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let release = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        let mut tasks = Vec::new();

        for index in 0..6 {
            let coordinator = Arc::clone(&coordinator);
            let active = Arc::clone(&active);
            let peak = Arc::clone(&peak);
            let release = Arc::clone(&release);
            tasks.push(tokio::spawn(async move {
                coordinator
                    .run(
                        PathBuf::from(format!(r"C:\clips\clip-{index}.mp4")),
                        move || {
                            let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                            peak.fetch_max(now, Ordering::SeqCst);
                            let (lock, ready) = &*release;
                            let mut released = lock.lock().expect("release lock");
                            while !*released {
                                released = ready.wait(released).expect("release wait");
                            }
                            active.fetch_sub(1, Ordering::SeqCst);
                            Ok(PathBuf::from(format!(r"C:\clips\clip-{index}.poster.jpg")))
                        },
                    )
                    .await
            }));
        }

        let started = tokio::time::timeout(Duration::from_secs(2), async {
            while active.load(Ordering::SeqCst) != MAX_CONCURRENT_POSTER_EXTRACTIONS {
                tokio::task::yield_now().await;
            }
        })
        .await;

        let (lock, ready) = &*release;
        *lock.lock().expect("release lock") = true;
        ready.notify_all();
        started.expect("two poster jobs start");
        assert_eq!(peak.load(Ordering::SeqCst), 2);
        for task in tasks {
            task.await.unwrap().unwrap();
        }
        assert_eq!(peak.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn list_clips_infers_audio_tracks_for_legacy_multitrack_clip() {
        let dir = TestDir::new("clipline-library", "list-infer-audio-tracks");
        let clip = dir.path().join("legacy.mp4");
        std::fs::write(&clip, two_real_opus_audio_mp4()).unwrap();

        let mut clips = Vec::new();
        push_clips_from(dir.path(), None, &mut clips).unwrap();

        assert_eq!(clips[0].duration_s, None);
        let tracks = &clips[0]
            .markers
            .as_ref()
            .expect("legacy clip gets inferred audio metadata")
            .audio_tracks;
        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].id, "audio:0");
        assert_eq!(tracks[0].track_index, 0);
        assert_eq!(tracks[0].label, "Audio Track 1");
        assert_eq!(tracks[1].id, "audio:1");
        assert_eq!(tracks[1].track_index, 1);
        assert_eq!(tracks[1].label, "Audio Track 2");
    }

    #[test]
    fn normalized_clip_file_name_adds_mp4_and_preserves_valid_text() {
        assert_eq!(
            normalized_clip_file_name("Ranked win").unwrap(),
            "Ranked win.mp4"
        );
        assert_eq!(
            normalized_clip_file_name("Ranked win.Mp4").unwrap(),
            "Ranked win.mp4"
        );
        assert_eq!(
            normalized_clip_file_name("solo.queue.vod").unwrap(),
            "solo.queue.vod.mp4"
        );
    }

    #[test]
    fn normalized_clip_file_name_rejects_paths_reserved_names_and_invalid_chars() {
        for name in [
            "",
            "..",
            "folder/clip",
            r"folder\clip",
            "bad:name",
            "clip?",
            "clip.",
            "CON",
            "LPT1.mp4",
        ] {
            assert!(
                normalized_clip_file_name(name).is_err(),
                "{name:?} should be rejected"
            );
        }
    }

    #[test]
    fn remove_clip_files_deletes_clip_metadata_sidecar() {
        let dir = TestDir::new("clipline-library", "delete-clip-metadata");
        let clip = dir.path().join("clip.mp4");
        touch_mp4(&clip);
        std::fs::write(clip.with_extension("markers.json"), b"{}").unwrap();
        std::fs::write(clip_metadata_path(&clip), br#"{"title":"Old title"}"#).unwrap();

        remove_clip_files(&clip, dir.path()).unwrap();

        assert!(!clip.exists());
        assert!(!clip.with_extension("markers.json").exists());
        assert!(!clip_metadata_path(&clip).exists());
    }

    #[test]
    fn remove_clip_files_removes_emptied_session_folder() {
        let dir = TestDir::new("clipline-library", "delete-empty-session");
        let media = dir.path().join("media");
        let session = media.join("2026-06-12 19-15");
        let clip = session.join("clip.mp4");
        touch_mp4(&clip);
        std::fs::write(clip.with_extension("markers.json"), b"{}").unwrap();
        std::fs::write(clip_metadata_path(&clip), b"{}").unwrap();
        std::fs::write(
            session.join("clipline-session.json"),
            b"{\"id\":\"league\"}",
        )
        .unwrap();
        let sibling = media.join("2026-06-12 19-16").join("keep.mp4");
        touch_mp4(&sibling);

        remove_clip_files(&clip, &media).unwrap();

        assert!(!clip.exists());
        assert!(
            !session.exists(),
            "emptied session folder should be removed"
        );
        assert!(sibling.exists());
        assert!(sibling.parent().unwrap().exists());
        assert!(media.exists(), "media root must stay");
    }

    #[test]
    fn remove_clip_files_keeps_session_folder_with_remaining_clip() {
        let dir = TestDir::new("clipline-library", "delete-keep-session");
        let media = dir.path().join("media");
        let session = media.join("2026-06-12 19-15");
        let gone = session.join("gone.mp4");
        let keep = session.join("keep.mp4");
        touch_mp4(&gone);
        touch_mp4(&keep);
        std::fs::write(session.join("clipline-session.json"), b"{}").unwrap();

        remove_clip_files(&gone, &media).unwrap();

        assert!(!gone.exists());
        assert!(keep.exists());
        assert!(session.exists());
        assert!(session.join("clipline-session.json").exists());
    }

    #[test]
    fn list_clips_does_not_sweep_emptied_session_folders() {
        let dir = TestDir::new("clipline-library", "list-no-sweep-empty-session");
        let media = dir.path().join("media");
        let leftover = media.join("2026-06-13 02-31");
        std::fs::create_dir_all(&leftover).unwrap();
        std::fs::write(leftover.join("clipline-session.json"), b"{}").unwrap();
        let keep = media.join("2026-06-13 03-00").join("clip.mp4");
        touch_mp4(&keep);

        let clips = list_clips_from_dir(media).unwrap().clips;

        assert_eq!(clips.len(), 1);
        assert!(
            leftover.exists(),
            "Library listing is a read path and must not restage or delete session folders"
        );
        assert!(keep.exists());
    }

    #[test]
    fn inferred_clip_kind_only_matches_generated_filename_patterns() {
        assert_eq!(
            inferred_clip_kind_for_path(Path::new("trimming-practice.mp4")),
            "replay"
        );
        assert_eq!(
            inferred_clip_kind_for_path(Path::new("obsession.mp4")),
            "replay"
        );
        assert_eq!(
            inferred_clip_kind_for_path(Path::new("clip_1_trim_001000_002000.mp4")),
            "trim"
        );
        assert_eq!(
            inferred_clip_kind_for_path(Path::new("session_1781377615.mp4")),
            "session"
        );
    }

    #[test]
    fn write_clip_metadata_replaces_existing_sidecar() {
        let dir = TestDir::new("clipline-library", "replace-clip-metadata");
        let clip = dir.path().join("clip.mp4");
        touch_mp4(&clip);

        write_clip_metadata(
            &clip,
            &ClipMetadata {
                title: Some("First title".to_string()),
                kind: Some("replay".to_string()),
                ..Default::default()
            },
        )
        .unwrap();
        write_clip_metadata(
            &clip,
            &ClipMetadata {
                title: Some("Second title".to_string()),
                kind: Some("session".to_string()),
                ..Default::default()
            },
        )
        .unwrap();

        let metadata = read_clip_metadata(&clip).unwrap();
        assert_eq!(metadata.title.as_deref(), Some("Second title"));
        assert_eq!(metadata.kind.as_deref(), Some("session"));
    }

    #[test]
    fn clip_metadata_favorite_round_trips_and_stays_hidden_when_false() {
        let dir = TestDir::new("clipline-library", "favorite-metadata");
        let clip = dir.path().join("clip.mp4");
        touch_mp4(&clip);

        write_clip_metadata(
            &clip,
            &ClipMetadata {
                title: None,
                kind: None,
                favorite: true,
                owned: None,
            },
        )
        .unwrap();
        let metadata = read_clip_metadata(&clip).unwrap();
        assert!(metadata.favorite);

        write_clip_metadata(
            &clip,
            &ClipMetadata {
                title: Some("Keep".to_string()),
                kind: Some("replay".to_string()),
                favorite: false,
                owned: None,
            },
        )
        .unwrap();
        let metadata = read_clip_metadata(&clip).unwrap();
        assert!(!metadata.favorite);
        let json = std::fs::read_to_string(clip_metadata_path(&clip)).unwrap();
        assert!(
            !json.contains("favorite"),
            "non-favorite metadata must stay byte-identical to today"
        );
        assert!(json.contains("\"kind\": \"replay\""));
    }

    #[test]
    fn set_clip_favorite_impl_sets_and_clears_the_flag() {
        let dir = TestDir::new("clipline-library", "set-favorite");
        let clip = dir.path().join("clip.mp4");
        touch_mp4(&clip);
        clipline_storage::ensure_clip_owned(&clip).unwrap();

        let result = set_clip_favorite_impl(&clip, true).unwrap();
        assert!(result.favorite);
        assert_eq!(result.path, clip.display().to_string());
        assert!(read_clip_metadata(&clip).unwrap().favorite);

        let result = set_clip_favorite_impl(&clip, false).unwrap();
        assert!(!result.favorite);
        assert!(!read_clip_metadata(&clip).unwrap().favorite);
    }

    #[test]
    fn favorite_only_metadata_never_adopts_an_imported_mp4() {
        let dir = TestDir::new("clipline-library", "favorite-imported");
        let clip = dir.path().join("vacation.mp4");
        touch_mp4(&clip);

        set_clip_favorite_impl(&clip, true).unwrap();
        assert!(read_clip_metadata(&clip).unwrap().favorite);
        assert_eq!(
            clipline_storage::storage_status(dir.path(), Some(0))
                .unwrap()
                .clip_count,
            0
        );

        set_clip_favorite_impl(&clip, false).unwrap();
        assert_eq!(
            clipline_storage::storage_status(dir.path(), Some(0))
                .unwrap()
                .clip_count,
            0
        );
        assert!(clip.exists());

        rename_clip_title(
            clip.clone(),
            clip.display().to_string(),
            "Imported clip".into(),
        )
        .unwrap();
        assert_eq!(
            clipline_storage::storage_status(dir.path(), Some(0))
                .unwrap()
                .clip_count,
            1,
            "editing the title keeps the existing explicit adoption behavior"
        );
    }

    #[test]
    fn rename_clip_updates_title_metadata_without_moving_file() {
        let dir = TestDir::new("clipline-library", "rename-title-metadata");
        let root = dir.path().join("media");
        let clip = root.join("2026-07-02").join("session_123.mp4");
        touch_mp4(&clip);

        let result = rename_clip_title(
            clip.clone(),
            clip.display().to_string(),
            "Ranked win vs Lux".to_string(),
        )
        .unwrap();

        assert_eq!(result.old_path, result.path);
        assert_eq!(result.name, "session_123.mp4");
        assert_eq!(result.title.as_deref(), Some("Ranked win vs Lux"));
        assert_eq!(result.kind, "session");
        assert!(clip.exists(), "display title rename must not move the MP4");

        let clips = list_clips_from_dir(root).unwrap().clips;
        assert_eq!(clips.len(), 1);
        assert_eq!(clips[0].name, "session_123.mp4");
        assert_eq!(clips[0].title.as_deref(), Some("Ranked win vs Lux"));
        assert_eq!(clips[0].kind, "session");
    }

    #[test]
    fn rename_clip_file_preserves_kind_and_moves_sidecars() {
        let dir = TestDir::new("clipline-library", "rename-file-sidecars");
        let root = dir.path().join("media");
        let source = root.join("session_123.mp4");
        let target = root.join("Ranked win.mp4");
        touch_mp4(&source);
        std::fs::write(source.with_extension("markers.json"), b"{}").unwrap();
        std::fs::write(crate::poster::poster_path(&source), b"poster").unwrap();

        let result = rename_clip_files(
            source.clone(),
            source.display().to_string(),
            normalized_clip_file_name("Ranked win").unwrap(),
        )
        .unwrap();

        assert_eq!(result.old_path, source.display().to_string());
        assert_eq!(result.path, target.display().to_string());
        assert_eq!(result.name, "Ranked win.mp4");
        assert_eq!(result.title, None);
        assert_eq!(result.kind, "session");
        assert!(!source.exists(), "source MP4 should move");
        assert!(target.exists(), "target MP4 should exist");
        assert!(!source.with_extension("markers.json").exists());
        assert!(target.with_extension("markers.json").exists());
        assert!(!crate::poster::poster_path(&source).exists());
        assert!(crate::poster::poster_path(&target).exists());

        let clips = list_clips_from_dir(root).unwrap().clips;
        assert_eq!(clips.len(), 1);
        assert_eq!(clips[0].name, "Ranked win.mp4");
        assert_eq!(clips[0].title, None);
        assert_eq!(clips[0].kind, "session");
    }

    fn pending_osu_enrichment(clip: &Path) -> crate::osu_enrichment::OsuPendingEnrichment {
        crate::osu_enrichment::OsuPendingEnrichment {
            schema_version: 1,
            clip_path: clip.display().to_string(),
            recording_start_unix: 10,
            recording_end_unix: 20,
            clip_duration_s: 10.0,
            status: crate::osu_enrichment::OsuEnrichmentStatus::Pending,
            attempts: 0,
            pagination_ceiling_reached: false,
            title_events: Vec::new(),
            message: None,
        }
    }

    #[test]
    fn prepared_osu_sidecar_move_commit_then_rollback_restores_exact_source() {
        let dir = TestDir::new("clipline-library", "prepared-osu-rollback");
        let source_clip = dir.path().join("session_1.mp4");
        let target_clip = dir.path().join("Ranked win.mp4");
        let source = crate::osu_enrichment::pending_path(&source_clip);
        let target = crate::osu_enrichment::pending_path(&target_clip);
        let original = serde_json::to_vec(&pending_osu_enrichment(&source_clip)).unwrap();
        std::fs::write(&source, &original).unwrap();

        let prepared = PreparedOsuSidecarMove::stage(&source_clip, &target_clip)
            .unwrap()
            .expect("source pending sidecar should prepare a move");
        let staged = prepared.staged.clone();
        let backup = prepared.backup.clone();

        assert_eq!(std::fs::read(&source).unwrap(), original);
        assert!(staged.exists());
        assert!(!target.exists());
        assert!(!backup.exists());

        prepared.commit().unwrap();

        assert!(!source.exists());
        assert!(target.exists());
        assert!(!staged.exists());
        assert!(backup.exists());
        let moved: crate::osu_enrichment::OsuPendingEnrichment =
            serde_json::from_slice(&std::fs::read(&target).unwrap()).unwrap();
        assert_eq!(moved.clip_path, target_clip.display().to_string());

        prepared.rollback();

        assert!(source.exists());
        assert_eq!(std::fs::read(&source).unwrap(), original);
        assert!(!target.exists());
        assert!(!staged.exists());
        assert!(!backup.exists());
    }

    #[test]
    fn prepared_osu_sidecar_move_commit_then_finish_cleans_backup() {
        let dir = TestDir::new("clipline-library", "prepared-osu-finish");
        let source_clip = dir.path().join("session_1.mp4");
        let target_clip = dir.path().join("Ranked win.mp4");
        let source = crate::osu_enrichment::pending_path(&source_clip);
        let target = crate::osu_enrichment::pending_path(&target_clip);
        std::fs::write(
            &source,
            serde_json::to_vec_pretty(&pending_osu_enrichment(&source_clip)).unwrap(),
        )
        .unwrap();

        let prepared = PreparedOsuSidecarMove::stage(&source_clip, &target_clip)
            .unwrap()
            .expect("source pending sidecar should prepare a move");
        let staged = prepared.staged.clone();
        let backup = prepared.backup.clone();
        prepared.commit().unwrap();

        assert!(target.exists());
        assert!(backup.exists());
        prepared.finish();

        let moved: crate::osu_enrichment::OsuPendingEnrichment =
            serde_json::from_slice(&std::fs::read(&target).unwrap()).unwrap();
        assert_eq!(moved.clip_path, target_clip.display().to_string());
        assert!(!source.exists());
        assert!(!staged.exists());
        assert!(!backup.exists());
    }

    #[test]
    fn prepared_osu_sidecar_move_rejects_staging_path_collision_without_mutation() {
        let dir = TestDir::new("clipline-library", "prepared-osu-staged-collision");
        let source_clip = dir.path().join("session_1.mp4");
        let target_clip = dir.path().join("Ranked win.mp4");
        let source = crate::osu_enrichment::pending_path(&source_clip);
        let target = crate::osu_enrichment::pending_path(&target_clip);
        let staged = target.with_extension("osu-enrichment.rename.tmp");
        let backup = source.with_extension("osu-enrichment.rename.backup");
        let original = serde_json::to_vec(&pending_osu_enrichment(&source_clip)).unwrap();
        std::fs::write(&source, &original).unwrap();
        std::fs::write(&staged, b"occupied staged path").unwrap();

        let error = match PreparedOsuSidecarMove::stage(&source_clip, &target_clip) {
            Ok(_) => panic!("occupied staging path must stop preparation"),
            Err(error) => error,
        };

        assert!(error.contains("staged osu! enrichment path"), "{error}");
        assert_eq!(std::fs::read(&source).unwrap(), original);
        assert_eq!(std::fs::read(&staged).unwrap(), b"occupied staged path");
        assert!(!target.exists());
        assert!(!backup.exists());
    }

    #[test]
    fn prepared_osu_sidecar_move_rejects_backup_path_collision_without_mutation() {
        let dir = TestDir::new("clipline-library", "prepared-osu-backup-collision");
        let source_clip = dir.path().join("session_1.mp4");
        let target_clip = dir.path().join("Ranked win.mp4");
        let source = crate::osu_enrichment::pending_path(&source_clip);
        let target = crate::osu_enrichment::pending_path(&target_clip);
        let staged = target.with_extension("osu-enrichment.rename.tmp");
        let backup = source.with_extension("osu-enrichment.rename.backup");
        let original = serde_json::to_vec(&pending_osu_enrichment(&source_clip)).unwrap();
        std::fs::write(&source, &original).unwrap();
        std::fs::write(&backup, b"occupied backup path").unwrap();

        let error = match PreparedOsuSidecarMove::stage(&source_clip, &target_clip) {
            Ok(_) => panic!("occupied backup path must stop preparation"),
            Err(error) => error,
        };

        assert!(error.contains("backup osu! enrichment path"), "{error}");
        assert_eq!(std::fs::read(&source).unwrap(), original);
        assert_eq!(std::fs::read(&backup).unwrap(), b"occupied backup path");
        assert!(!target.exists());
        assert!(!staged.exists());
    }

    #[test]
    fn prepared_osu_sidecar_move_install_failure_restores_source_and_drop_cleans_stage() {
        let dir = TestDir::new("clipline-library", "prepared-osu-install-failure");
        let source_clip = dir.path().join("session_1.mp4");
        let target_clip = dir.path().join("Ranked win.mp4");
        let source = crate::osu_enrichment::pending_path(&source_clip);
        let target = crate::osu_enrichment::pending_path(&target_clip);
        let original = serde_json::to_vec(&pending_osu_enrichment(&source_clip)).unwrap();
        std::fs::write(&source, &original).unwrap();

        let prepared = PreparedOsuSidecarMove::stage(&source_clip, &target_clip)
            .unwrap()
            .expect("source pending sidecar should prepare a move");
        let staged = prepared.staged.clone();
        let backup = prepared.backup.clone();
        std::fs::create_dir(&target).unwrap();

        let error = prepared.commit().unwrap_err();

        assert!(
            error.contains("install renamed osu! enrichment sidecar"),
            "{error}"
        );
        assert_eq!(std::fs::read(&source).unwrap(), original);
        assert!(target.is_dir());
        assert!(staged.exists());
        assert!(!backup.exists());

        drop(prepared);

        assert_eq!(std::fs::read(&source).unwrap(), original);
        assert!(target.is_dir());
        assert!(!staged.exists());
        assert!(!backup.exists());
    }

    #[test]
    fn prepared_osu_sidecar_move_backup_rename_failure_preserves_source_and_drop_cleans_stage() {
        let dir = TestDir::new("clipline-library", "prepared-osu-backup-rename-failure");
        let source_clip = dir.path().join("session_1.mp4");
        let target_clip = dir.path().join("Ranked win.mp4");
        let source = crate::osu_enrichment::pending_path(&source_clip);
        let target = crate::osu_enrichment::pending_path(&target_clip);
        let original = serde_json::to_vec(&pending_osu_enrichment(&source_clip)).unwrap();
        std::fs::write(&source, &original).unwrap();

        let prepared = PreparedOsuSidecarMove::stage(&source_clip, &target_clip)
            .unwrap()
            .expect("source pending sidecar should prepare a move");
        let staged = prepared.staged.clone();
        let backup = prepared.backup.clone();
        std::fs::create_dir(&backup).unwrap();

        let error = prepared.commit().unwrap_err();

        assert!(
            error.contains("stage old osu! enrichment sidecar"),
            "{error}"
        );
        assert_eq!(std::fs::read(&source).unwrap(), original);
        assert!(!target.exists());
        assert!(staged.exists());
        assert!(backup.is_dir());

        drop(prepared);

        assert_eq!(std::fs::read(&source).unwrap(), original);
        assert!(!target.exists());
        assert!(!staged.exists());
        assert!(backup.is_dir());
        std::fs::remove_dir(&backup).unwrap();
    }

    #[test]
    fn prepared_osu_sidecar_move_drop_cleans_uncommitted_stage_without_mutation() {
        let dir = TestDir::new("clipline-library", "prepared-osu-drop");
        let source_clip = dir.path().join("session_1.mp4");
        let target_clip = dir.path().join("Ranked win.mp4");
        let source = crate::osu_enrichment::pending_path(&source_clip);
        let target = crate::osu_enrichment::pending_path(&target_clip);
        let original = serde_json::to_vec(&pending_osu_enrichment(&source_clip)).unwrap();
        std::fs::write(&source, &original).unwrap();

        let staged;
        let backup;
        {
            let prepared = PreparedOsuSidecarMove::stage(&source_clip, &target_clip)
                .unwrap()
                .expect("source pending sidecar should prepare a move");
            staged = prepared.staged.clone();
            backup = prepared.backup.clone();
            assert!(staged.exists());
            assert_eq!(std::fs::read(&source).unwrap(), original);
            assert!(!target.exists());
            assert!(!backup.exists());
        }

        assert_eq!(std::fs::read(&source).unwrap(), original);
        assert!(!target.exists());
        assert!(!staged.exists());
        assert!(!backup.exists());
    }

    #[test]
    fn rename_clip_file_moves_pending_osu_sidecar_and_rewrites_clip_path() {
        let dir = TestDir::new("clipline-library", "rename-osu-pending");
        let source = dir.path().join("session_1.mp4");
        let target = dir.path().join("Ranked win.mp4");
        touch_mp4(&source);
        std::fs::write(
            crate::osu_enrichment::pending_path(&source),
            serde_json::to_vec_pretty(&pending_osu_enrichment(&source)).unwrap(),
        )
        .unwrap();

        rename_clip_files(
            source.clone(),
            source.display().to_string(),
            normalized_clip_file_name("Ranked win").unwrap(),
        )
        .unwrap();

        assert!(!crate::osu_enrichment::pending_path(&source).exists());
        let moved: crate::osu_enrichment::OsuPendingEnrichment = serde_json::from_slice(
            &std::fs::read(crate::osu_enrichment::pending_path(&target)).unwrap(),
        )
        .unwrap();
        assert_eq!(moved.clip_path, target.display().to_string());
    }

    #[test]
    fn rename_clip_file_rejects_malformed_pending_osu_before_moving_mp4() {
        let dir = TestDir::new("clipline-library", "rename-osu-malformed");
        let source = dir.path().join("session_1.mp4");
        touch_mp4(&source);
        std::fs::write(crate::osu_enrichment::pending_path(&source), b"not json").unwrap();

        let error = match rename_clip_files(
            source.clone(),
            source.display().to_string(),
            normalized_clip_file_name("Ranked win").unwrap(),
        ) {
            Ok(_) => panic!("malformed pending enrichment must stop the rename"),
            Err(error) => error,
        };

        assert!(error.contains("osu! enrichment"), "{error}");
        assert!(source.exists());
        assert!(crate::osu_enrichment::pending_path(&source).exists());
        assert!(!dir.path().join("Ranked win.mp4").exists());
    }

    #[test]
    fn rename_clip_file_rejects_pending_osu_destination_collision() {
        let dir = TestDir::new("clipline-library", "rename-osu-collision");
        let source = dir.path().join("session_1.mp4");
        let target = dir.path().join("Ranked win.mp4");
        touch_mp4(&source);
        let source_pending = crate::osu_enrichment::pending_path(&source);
        let target_pending = crate::osu_enrichment::pending_path(&target);
        let original = serde_json::to_vec(&pending_osu_enrichment(&source)).unwrap();
        std::fs::write(&source_pending, &original).unwrap();
        std::fs::write(&target_pending, b"occupied").unwrap();

        let error = match rename_clip_files(
            source.clone(),
            source.display().to_string(),
            normalized_clip_file_name("Ranked win").unwrap(),
        ) {
            Ok(_) => panic!("pending enrichment destination collision must stop the rename"),
            Err(error) => error,
        };

        assert!(error.contains("osu! enrichment sidecar"), "{error}");
        assert!(source.exists());
        assert!(!target.exists());
        assert_eq!(std::fs::read(&source_pending).unwrap(), original);
        assert_eq!(std::fs::read(&target_pending).unwrap(), b"occupied");
        assert!(!target_pending
            .with_extension("osu-enrichment.rename.tmp")
            .exists());
        assert!(!source_pending
            .with_extension("osu-enrichment.rename.backup")
            .exists());
    }

    #[cfg(windows)]
    #[test]
    fn rename_clip_file_case_only_moves_mp4_and_rewrites_pending_osu_path() {
        let dir = TestDir::new("clipline-library", "rename-osu-case-only");
        let source = dir.path().join("session_1.mp4");
        let target = dir.path().join("Session_1.mp4");
        let source_markers = source.with_extension("markers.json");
        let target_markers = target.with_extension("markers.json");
        let source_metadata = clip_metadata_path(&source);
        let target_metadata = clip_metadata_path(&target);
        let marker_bytes = br#"{"marker":"case-only-marker"}"#;
        touch_mp4(&source);
        std::fs::write(&source_markers, marker_bytes).unwrap();
        write_clip_metadata(
            &source,
            &ClipMetadata {
                title: Some("Case-only metadata".to_string()),
                kind: Some("session".to_string()),
                ..Default::default()
            },
        )
        .unwrap();
        let metadata_bytes = std::fs::read(&source_metadata).unwrap();
        std::fs::write(
            crate::osu_enrichment::pending_path(&source),
            serde_json::to_vec_pretty(&pending_osu_enrichment(&source)).unwrap(),
        )
        .unwrap();
        let result = rename_clip_files(
            source.clone(),
            source.display().to_string(),
            normalized_clip_file_name("Session_1").unwrap(),
        )
        .unwrap();

        assert_eq!(result.path, target.display().to_string());
        let names: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(names.iter().any(|name| name == "Session_1.mp4"));
        assert!(!names.iter().any(|name| name == "session_1.mp4"));
        assert!(names.iter().any(|name| name == "Session_1.markers.json"));
        assert!(!names.iter().any(|name| name == "session_1.markers.json"));
        assert!(names.iter().any(|name| name == "Session_1.clipline.json"));
        assert!(!names.iter().any(|name| name == "session_1.clipline.json"));
        assert!(names
            .iter()
            .any(|name| name == "Session_1.osu-enrichment.json"));
        assert!(!names
            .iter()
            .any(|name| name == "session_1.osu-enrichment.json"));
        assert_eq!(std::fs::read(&target_markers).unwrap(), marker_bytes);
        assert_eq!(std::fs::read(&target_metadata).unwrap(), metadata_bytes);

        let moved: crate::osu_enrichment::OsuPendingEnrichment = serde_json::from_slice(
            &std::fs::read(crate::osu_enrichment::pending_path(&target)).unwrap(),
        )
        .unwrap();
        assert_eq!(moved.clip_path, target.display().to_string());
        assert!(!crate::osu_enrichment::pending_path(&target)
            .with_extension("osu-enrichment.rename.tmp")
            .exists());
        assert!(!crate::osu_enrichment::pending_path(&source)
            .with_extension("osu-enrichment.rename.backup")
            .exists());
        assert!(!target_metadata.with_extension("clipline.json.tmp").exists());
        assert!(!names.iter().any(|name| name.contains(".rename.")));
    }

    #[test]
    fn rename_clip_file_rolls_back_when_final_metadata_write_fails() {
        let dir = TestDir::new("clipline-library", "rename-file-metadata-rollback");
        let root = dir.path().join("media");
        let source = root.join("session_123.mp4");
        let target = root.join("Ranked win.mp4");
        touch_mp4(&source);
        let original_pending = pending_osu_enrichment(&source);
        std::fs::write(
            crate::osu_enrichment::pending_path(&source),
            serde_json::to_vec_pretty(&original_pending).unwrap(),
        )
        .unwrap();
        std::fs::write(source.with_extension("markers.json"), b"{}").unwrap();
        std::fs::create_dir_all(clip_metadata_path(&target)).unwrap();

        let err = match rename_clip_files(
            source.clone(),
            source.display().to_string(),
            normalized_clip_file_name("Ranked win").unwrap(),
        ) {
            Ok(_) => panic!("metadata write failure should roll back moved clip files"),
            Err(error) => error,
        };

        assert!(
            err.contains("clip metadata"),
            "unexpected rename error: {err}"
        );
        assert!(source.exists(), "source MP4 should be restored");
        assert!(source.with_extension("markers.json").exists());
        assert!(!target.exists(), "target MP4 should be rolled back");
        assert!(!target.with_extension("markers.json").exists());
        assert!(crate::osu_enrichment::pending_path(&source).exists());
        assert!(!crate::osu_enrichment::pending_path(&target).exists());
        let restored: crate::osu_enrichment::OsuPendingEnrichment = serde_json::from_slice(
            &std::fs::read(crate::osu_enrichment::pending_path(&source)).unwrap(),
        )
        .unwrap();
        assert_eq!(restored.clip_path, source.display().to_string());
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

    #[test]
    fn validate_clip_path_accepts_root_and_session_clips() {
        let dir = TestDir::new("clipline-library", "validate-accept");
        let root = dir.path().join("media");
        let settings = StorageSettings::new(None, root.clone());

        let legacy = root.join("clip.mp4");
        touch_mp4(&legacy);
        let session = root.join("2026-06-12").join("clip.mp4");
        touch_mp4(&session);

        assert!(validate_clip_path(&settings, legacy.to_str().unwrap()).is_ok());
        assert!(validate_clip_path(&settings, session.to_str().unwrap()).is_ok());
    }

    #[test]
    fn validate_clip_path_rejects_escapes_and_non_mp4() {
        let dir = TestDir::new("clipline-library", "validate-reject");
        let root = dir.path().join("media");
        std::fs::create_dir_all(&root).unwrap();
        let settings = StorageSettings::new(None, root.clone());

        // Two folders below the root — deeper than a session clip.
        let too_deep = root.join("a").join("b").join("clip.mp4");
        touch_mp4(&too_deep);
        assert!(validate_clip_path(&settings, too_deep.to_str().unwrap()).is_err());

        // A sibling directory outside the configured root.
        let outside = dir.path().join("elsewhere").join("clip.mp4");
        touch_mp4(&outside);
        assert!(validate_clip_path(&settings, outside.to_str().unwrap()).is_err());

        // Correct location, wrong extension.
        let not_mp4 = root.join("clip.txt");
        touch_mp4(&not_mp4);
        assert!(validate_clip_path(&settings, not_mp4.to_str().unwrap()).is_err());
    }

    #[test]
    fn delete_clips_impl_handles_partial_success_and_sidecars() {
        let dir = TestDir::new("clipline-library", "delete-clips-impl");
        let root = dir.path().join("media");
        std::fs::create_dir_all(&root).unwrap();

        // Two real clips, each with all four sidecars.
        let a = root.join("a.mp4");
        let b = root.join("b.mp4");
        touch_mp4(&a);
        touch_mp4(&b);
        std::fs::write(a.with_extension("markers.json"), b"{}").unwrap();
        std::fs::write(b.with_extension("markers.json"), b"{}").unwrap();
        std::fs::write(clip_metadata_path(&a), b"{}").unwrap();
        std::fs::write(clip_metadata_path(&b), b"{}").unwrap();
        std::fs::write(a.with_extension("osu-enrichment.json"), b"{}").unwrap();
        std::fs::write(b.with_extension("osu-enrichment.json"), b"{}").unwrap();
        std::fs::write(crate::poster::poster_path(&a), b"poster").unwrap();
        std::fs::write(crate::poster::poster_path(&b), b"poster").unwrap();

        // A third clip that should be left untouched (not in the deleted set).
        let c = root.join("c.mp4");
        touch_mp4(&c);
        std::fs::write(c.with_extension("markers.json"), b"{}").unwrap();
        std::fs::write(clip_metadata_path(&c), b"{}").unwrap();
        std::fs::write(c.with_extension("osu-enrichment.json"), b"{}").unwrap();

        let validated = vec![
            (a.to_str().unwrap().to_string(), a.clone()),
            (b.to_str().unwrap().to_string(), b.clone()),
        ];
        // One path already failed validation upstream — passed through as failed.
        let failed_in = vec![("bogus".to_string(), "refused".to_string())];

        let report = delete_clips_impl(root.clone(), validated, failed_in);

        assert_eq!(report.deleted.len(), 2);
        assert_eq!(report.failed.len(), 1);
        assert_eq!(report.failed[0].0, "bogus");
        assert!(!a.exists(), "a.mp4 should be removed");
        assert!(!b.exists(), "b.mp4 should be removed");
        assert!(
            !a.with_extension("markers.json").exists(),
            "a.mp4 markers sidecar should be removed"
        );
        assert!(
            !crate::poster::poster_path(&b).exists(),
            "b.mp4 poster should be removed"
        );
        assert!(
            !clip_metadata_path(&a).exists(),
            "a.mp4 clip metadata should be removed"
        );
        assert!(
            !clip_metadata_path(&b).exists(),
            "b.mp4 clip metadata should be removed"
        );
        assert!(
            !a.with_extension("osu-enrichment.json").exists(),
            "a.mp4 pending osu! sidecar should be removed"
        );
        assert!(
            !b.with_extension("osu-enrichment.json").exists(),
            "b.mp4 pending osu! sidecar should be removed"
        );
        assert!(c.exists(), "c.mp4 must be left untouched");
        assert!(
            c.with_extension("markers.json").exists(),
            "c.mp4 markers sidecar must be left untouched"
        );
        assert!(
            clip_metadata_path(&c).exists(),
            "c.mp4 clip metadata must be left untouched"
        );
        assert!(
            c.with_extension("osu-enrichment.json").exists(),
            "c.mp4 pending osu! sidecar must be left untouched"
        );
    }

    #[test]
    fn dropfiles_payload_strips_verbatim_prefix_and_marks_unicode() {
        let path = Path::new(r"\\?\C:\Users\dain\Videos\Clipline\clïp 雪.mp4");
        let payload = dropfiles_payload(path);
        let p_files = u32::from_le_bytes(payload[0..4].try_into().unwrap()) as usize;

        assert_eq!(p_files, size_of::<DROPFILES>());
        assert_eq!(i32::from_le_bytes(payload[12..16].try_into().unwrap()), 0);
        assert_eq!(i32::from_le_bytes(payload[16..20].try_into().unwrap()), 1);

        let path_units: Vec<u16> = payload[p_files..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_le_bytes(*pair))
            .collect();
        assert_eq!(&path_units[path_units.len() - 2..], &[0, 0]);
        let decoded = String::from_utf16(&path_units[..path_units.len() - 2]).unwrap();
        assert_eq!(decoded, r"C:\Users\dain\Videos\Clipline\clïp 雪.mp4");
    }

    #[test]
    fn clipboard_text_payload_is_null_terminated_utf16() {
        let payload = clipboard_text_payload("https://clipline.example/雪");
        let units: Vec<u16> = payload
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_le_bytes(*pair))
            .collect();

        assert_eq!(units.last(), Some(&0));
        assert_eq!(
            String::from_utf16(&units[..units.len() - 1]).unwrap(),
            "https://clipline.example/雪"
        );
    }

    #[test]
    fn shell_clipboard_path_wide_converts_verbatim_unc_paths() {
        let path = Path::new(r"\\?\UNC\nas\clips\clïp 雪.mp4");
        let decoded = String::from_utf16(&shell_clipboard_path_wide(path)).unwrap();

        assert_eq!(decoded, r"\\nas\clips\clïp 雪.mp4");
    }

    #[test]
    fn clipboard_transaction_retries_open_and_closes_every_opened_path() {
        use std::cell::{Cell, RefCell};

        let events = RefCell::new(Vec::new());
        let opens = Cell::new(0_u32);
        let result = clipboard_transaction(
            3,
            || {
                events.borrow_mut().push("open");
                opens.set(opens.get() + 1);
                if opens.get() < 3 {
                    Err("busy")
                } else {
                    Ok(())
                }
            },
            || events.borrow_mut().push("close"),
            || {
                events.borrow_mut().push("set");
                Ok(())
            },
            || events.borrow_mut().push("wait"),
        );
        assert_eq!(result, Ok(()));
        assert_eq!(
            events.into_inner(),
            vec!["open", "wait", "open", "wait", "open", "set", "close"]
        );

        let events = RefCell::new(Vec::new());
        let result = clipboard_transaction(
            1,
            || {
                events.borrow_mut().push("open");
                Ok::<(), &str>(())
            },
            || events.borrow_mut().push("close"),
            || {
                events.borrow_mut().push("set");
                Err("set")
            },
            || unreachable!(),
        );
        assert_eq!(result, Err("set"));
        assert_eq!(events.into_inner(), vec!["open", "set", "close"]);

        let closes = Cell::new(0);
        let result = clipboard_transaction(
            2,
            || Err::<(), _>("busy"),
            || closes.set(closes.get() + 1),
            || Ok(()),
            || {},
        );
        assert_eq!(result, Err("busy"));
        assert_eq!(
            closes.get(),
            0,
            "never close a clipboard that was not opened"
        );
    }
}
