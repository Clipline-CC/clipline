use std::collections::HashSet;
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, UNIX_EPOCH};

use chrono::{DateTime, Utc};
use clipline_events::{ClipMarkers, ClipPlay, GameId};
use serde::{Deserialize, Serialize};

use crate::util::unix_now_i64;

const PENDING_SCHEMA_VERSION: u32 = 1;
const SESSION_META_FILE: &str = "clipline-session.json";
const UTC_SKEW_TOLERANCE_S: f64 = 15.0;
const PASSED_RESULTS_SCREEN_PADDING_S: f64 = 1.0;
const TITLE_EVENT_FALLBACK_LOOKBACK_S: i64 = 15 * 60;
const TITLE_EVENT_LENGTH_SLACK_S: i64 = 60;
const PENDING_RETRY_BASE_S: u64 = 60;
const PENDING_RETRY_CAP_S: u64 = 6 * 60 * 60;
const FAILED_RETRY_BASE_S: u64 = 6 * 60 * 60;
const FAILED_RETRY_CAP_S: u64 = 24 * 60 * 60;
static OSU_SIDECAR_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone)]
pub struct OsuSavedClip {
    pub path: PathBuf,
    pub seconds: f64,
    pub full_session: bool,
    pub recording_start_unix: Option<i64>,
    pub recording_end_unix: Option<i64>,
    pub title_events: Vec<OsuTitleEvent>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OsuTitleEvent {
    pub unix_s: i64,
    pub title: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OsuEnrichmentStatus {
    Pending,
    Complete,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OsuPendingEnrichment {
    pub schema_version: u32,
    pub clip_path: String,
    pub recording_start_unix: i64,
    pub recording_end_unix: i64,
    pub clip_duration_s: f64,
    pub status: OsuEnrichmentStatus,
    pub attempts: u32,
    #[serde(default)]
    pub pagination_ceiling_reached: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub title_events: Vec<OsuTitleEvent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// A pending record bound to the filesystem objects from which it was
/// discovered. The serialized `clip_path` is validated for consistency but
/// is never used as an I/O authority after discovery.
#[derive(Debug, Clone)]
pub struct DiscoveredPendingEnrichment {
    record: OsuPendingEnrichment,
    clip_path: PathBuf,
    sidecar_path: PathBuf,
}

impl DiscoveredPendingEnrichment {
    pub fn record(&self) -> &OsuPendingEnrichment {
        &self.record
    }

    #[cfg(test)]
    pub fn clip_path(&self) -> &Path {
        &self.clip_path
    }

    #[cfg(test)]
    fn sidecar_path(&self) -> &Path {
        &self.sidecar_path
    }

    pub fn retry_due(&self, now_unix: u64) -> bool {
        let modified_unix = std::fs::metadata(&self.sidecar_path)
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |duration| duration.as_secs());
        retry_is_due(
            self.record.status.clone(),
            self.record.attempts,
            modified_unix,
            now_unix,
        )
    }
}

fn retry_delay(status: OsuEnrichmentStatus, attempts: u32) -> Duration {
    let (base, cap) = match status {
        OsuEnrichmentStatus::Pending if attempts == 0 => return Duration::ZERO,
        OsuEnrichmentStatus::Pending => (PENDING_RETRY_BASE_S, PENDING_RETRY_CAP_S),
        OsuEnrichmentStatus::Failed => (FAILED_RETRY_BASE_S, FAILED_RETRY_CAP_S),
        OsuEnrichmentStatus::Complete => return Duration::MAX,
    };
    let shift = attempts.saturating_sub(1).min(31);
    Duration::from_secs(base.saturating_mul(1_u64 << shift).min(cap))
}

fn retry_is_due(
    status: OsuEnrichmentStatus,
    attempts: u32,
    modified_unix: u64,
    now_unix: u64,
) -> bool {
    let delay = retry_delay(status, attempts);
    delay != Duration::MAX && now_unix >= modified_unix.saturating_add(delay.as_secs())
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OsuProxyScore {
    pub id: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub beatmap_id: Option<u32>,
    #[serde(default)]
    pub beatmapset_id: Option<u32>,
    #[serde(default)]
    pub cover_url: Option<String>,
    pub title: String,
    pub artist: String,
    pub difficulty: String,
    #[serde(default)]
    pub mapper: Option<String>,
    #[serde(default)]
    pub star_rating: Option<f64>,
    #[serde(default)]
    pub mods: Vec<String>,
    #[serde(default)]
    pub rank: Option<String>,
    pub passed: bool,
    #[serde(default)]
    pub accuracy: Option<f64>,
    #[serde(default)]
    pub max_combo: Option<u32>,
    #[serde(default)]
    pub total_score: Option<u64>,
    #[serde(default)]
    pub pp: Option<f64>,
    #[serde(default)]
    pub started_at_unix: Option<i64>,
    pub ended_at_unix: i64,
    #[serde(default)]
    pub beatmap_total_length_s: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OsuMappedPlays {
    pub plays: Vec<ClipPlay>,
    pub pagination_ceiling_reached: bool,
}

pub fn pending_path(path: &Path) -> PathBuf {
    clipline_storage::clip_sidecar_path(path, clipline_storage::OSU_ENRICHMENT_SUFFIX)
}

struct OwnedSidecarTemp {
    path: PathBuf,
    file: Option<std::fs::File>,
    armed: bool,
}

impl OwnedSidecarTemp {
    fn create(target: &Path) -> Result<Self, String> {
        let parent = target
            .parent()
            .ok_or_else(|| format!("osu! sidecar target has no parent: {target:?}"))?;
        let base = target
            .file_name()
            .map_or_else(|| OsString::from("sidecar"), OsString::from);
        for _ in 0..64 {
            let counter = OSU_SIDECAR_COUNTER.fetch_add(1, Ordering::Relaxed);
            let mut name = base.clone();
            name.push(format!(
                ".clipline-osu-tmp.{}.{counter}",
                std::process::id()
            ));
            let path = parent.join(name);
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(file) => {
                    return Ok(Self {
                        path,
                        file: Some(file),
                        armed: true,
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(format!(
                        "create temporary osu! sidecar in {parent:?}: {error}"
                    ));
                }
            }
        }
        Err(format!(
            "could not allocate a unique temporary osu! sidecar in {parent:?}"
        ))
    }
}

impl Drop for OwnedSidecarTemp {
    fn drop(&mut self) {
        if self.armed {
            self.file.take();
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn write_json_atomically<T: Serialize>(
    target: &Path,
    value: &T,
    context: &str,
) -> Result<(), String> {
    let bytes =
        serde_json::to_vec_pretty(value).map_err(|e| format!("serialize {context}: {e}"))?;
    let mut temp = OwnedSidecarTemp::create(target)?;
    let file = temp.file.as_mut().expect("new sidecar temp owns its file");
    file.write_all(&bytes)
        .map_err(|e| format!("write temporary {context}: {e}"))?;
    file.sync_all()
        .map_err(|e| format!("sync temporary {context}: {e}"))?;
    temp.file.take();
    replace_file(&temp.path, target).map_err(|e| format!("publish {context} {target:?}: {e}"))?;
    temp.armed = false;
    Ok(())
}

#[cfg(windows)]
fn replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    crate::windows::replace_file(from, to)
}

#[cfg(not(windows))]
fn replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::rename(from, to)
}

pub fn write_pending_for_saved_clip(saved: &OsuSavedClip) -> Result<Option<PathBuf>, String> {
    if !saved.full_session || !clip_session_is_osu(&saved.path) {
        return Ok(None);
    }
    let end = saved.recording_end_unix.unwrap_or_else(unix_now_i64);
    let derived_start = end.saturating_sub(saved.seconds.max(0.0).round() as i64);
    let start = saved.recording_start_unix.unwrap_or(derived_start);
    let record = OsuPendingEnrichment {
        schema_version: PENDING_SCHEMA_VERSION,
        clip_path: saved.path.display().to_string(),
        recording_start_unix: start,
        recording_end_unix: end.max(start),
        clip_duration_s: saved.seconds.max(0.0),
        status: OsuEnrichmentStatus::Pending,
        attempts: 0,
        pagination_ceiling_reached: false,
        title_events: saved.title_events.clone(),
        message: None,
    };
    let path = pending_path(&saved.path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create osu! enrichment sidecar dir {parent:?}: {e}"))?;
    }
    write_json_atomically(&path, &record, "osu! enrichment sidecar")?;
    let title_plays = map_title_events_to_clip_plays(&record);
    if !title_plays.is_empty() {
        write_plays_sidecar(&saved.path, &record, title_plays)?;
    }
    Ok(Some(path))
}

pub fn discover_pending(media_root: &Path) -> Result<Vec<DiscoveredPendingEnrichment>, String> {
    if path_is_link_or_reparse(media_root)? {
        return Err(format!(
            "refusing linked/reparse osu! enrichment media root {media_root:?}"
        ));
    }
    let media_root = media_root
        .canonicalize()
        .map_err(|e| format!("canonicalize osu! enrichment media root {media_root:?}: {e}"))?;
    let mut out = Vec::new();
    discover_pending_in_dir(&media_root, &media_root, &mut out)?;
    for entry in std::fs::read_dir(&media_root).map_err(|e| e.to_string())? {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        if metadata.is_dir() && !metadata_is_link_or_reparse(&metadata) {
            discover_pending_in_dir(&media_root, &path, &mut out)?;
        }
    }
    out.sort_by(|a, b| {
        a.record
            .recording_start_unix
            .cmp(&b.record.recording_start_unix)
            .then_with(|| a.clip_path.cmp(&b.clip_path))
    });
    Ok(out)
}

pub fn apply_scores_to_pending(
    pending: &DiscoveredPendingEnrichment,
    scores: &[OsuProxyScore],
    pagination_ceiling_reached: bool,
) -> Result<OsuMappedPlays, String> {
    let mapped =
        map_proxy_scores_to_clip_plays(&pending.record, scores, pagination_ceiling_reached);
    if mapped.plays.is_empty() {
        mark_pending_retry(
            pending,
            "No osu! API plays matched this recording yet; keeping fallback plays and retrying later.",
        )?;
        return Ok(mapped);
    }
    write_plays_sidecar(&pending.clip_path, &pending.record, mapped.plays.clone())?;
    if let Err(e) = std::fs::remove_file(&pending.sidecar_path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            return Err(format!(
                "remove completed osu! enrichment {:?}: {e}",
                pending.sidecar_path
            ));
        }
    }
    Ok(mapped)
}

pub fn mark_pending_retry(
    pending: &DiscoveredPendingEnrichment,
    message: &str,
) -> Result<(), String> {
    let mut next = pending.record.clone();
    next.status = OsuEnrichmentStatus::Pending;
    next.attempts = next.attempts.saturating_add(1);
    next.message = Some(message.to_string());
    write_json_atomically(
        &pending.sidecar_path,
        &next,
        "retryable osu! enrichment sidecar",
    )
}

pub fn mark_pending_failed(
    pending: &DiscoveredPendingEnrichment,
    message: &str,
) -> Result<(), String> {
    let mut next = pending.record.clone();
    next.status = OsuEnrichmentStatus::Failed;
    next.attempts = next.attempts.saturating_add(1);
    next.message = Some(message.to_string());
    write_json_atomically(
        &pending.sidecar_path,
        &next,
        "failed osu! enrichment sidecar",
    )
}

fn write_plays_sidecar(
    clip_path: &Path,
    pending: &OsuPendingEnrichment,
    plays: Vec<ClipPlay>,
) -> Result<(), String> {
    let mut markers = crate::util::read_markers_raw(clip_path).unwrap_or(ClipMarkers {
        bookmarks: Vec::new(),
        recording_start_s: 0.0,
        duration_s: pending.clip_duration_s,
        player_summary: None,
        audio_tracks: Vec::new(),
        plays: Vec::new(),
        markers: Vec::new(),
    });
    if markers.duration_s <= 0.0 || !markers.duration_s.is_finite() {
        markers.duration_s = pending.clip_duration_s;
    }
    markers.plays = plays;

    let path = clip_path.with_extension("markers.json");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create marker sidecar dir {parent:?}: {e}"))?;
    }
    write_json_atomically(&path, &markers, "osu! marker sidecar")
}

pub fn map_proxy_scores_to_clip_plays(
    pending: &OsuPendingEnrichment,
    scores: &[OsuProxyScore],
    pagination_ceiling_reached: bool,
) -> OsuMappedPlays {
    let mut seen = HashSet::new();
    let mut plays = Vec::new();
    let mut last_end_s = 0.0_f64;
    let mut sorted = scores.to_vec();
    sorted.sort_by_key(|score| score.ended_at_unix);

    for score in sorted {
        if !seen.insert(score.id.clone()) {
            continue;
        }
        let Some((start_unix, derived_start, point_marker)) = score_start_unix(&score, pending)
        else {
            continue;
        };
        let score_start = start_unix as f64;
        let score_end = score.ended_at_unix as f64;
        if score_end < pending.recording_start_unix as f64 - UTC_SKEW_TOLERANCE_S
            || score_start > pending.recording_end_unix as f64 + UTC_SKEW_TOLERANCE_S
        {
            continue;
        }

        let end_padding_s = if score.passed && !point_marker {
            PASSED_RESULTS_SCREEN_PADDING_S
        } else {
            0.0
        };
        let clip_end_s = clamp_clip_time(
            score_end - pending.recording_start_unix as f64 + end_padding_s,
            pending,
        );
        let mut clip_start_s =
            clamp_clip_time(score_start - pending.recording_start_unix as f64, pending);
        if derived_start && !point_marker && clip_start_s < last_end_s {
            clip_start_s = last_end_s;
        }
        let t_end_s = if point_marker {
            None
        } else {
            Some(clip_end_s.max(clip_start_s))
        };
        if let Some(end_s) = t_end_s {
            last_end_s = last_end_s.max(end_s);
        } else {
            last_end_s = last_end_s.max(clip_start_s);
        }

        plays.push(ClipPlay {
            game_id: GameId::Osu,
            source: "osu_api".into(),
            external_id: score.id,
            url: score.url,
            beatmap_id: score.beatmap_id,
            beatmapset_id: score.beatmapset_id,
            cover_url: score.cover_url,
            title: score.title,
            artist: score.artist,
            difficulty: score.difficulty,
            mapper: score.mapper,
            star_rating: score.star_rating,
            mods: score.mods,
            rank: score.rank,
            passed: score.passed,
            accuracy: score.accuracy,
            max_combo: score.max_combo,
            total_score: score.total_score,
            pp: score.pp,
            started_at: score.started_at_unix.map(unix_to_rfc3339),
            ended_at: unix_to_rfc3339(score.ended_at_unix),
            derived_start,
            t_start_s: clip_start_s,
            t_end_s,
        });
    }

    OsuMappedPlays {
        plays,
        pagination_ceiling_reached,
    }
}

fn map_title_events_to_clip_plays(pending: &OsuPendingEnrichment) -> Vec<ClipPlay> {
    let mut plays = Vec::new();
    for (index, event) in pending.title_events.iter().enumerate() {
        let Some(info) = parse_osu_title_play(&event.title) else {
            continue;
        };
        let next_unix = pending
            .title_events
            .iter()
            .skip(index + 1)
            .map(|next| next.unix_s)
            .find(|next| *next > event.unix_s)
            .unwrap_or(pending.recording_end_unix);
        if next_unix <= pending.recording_start_unix || event.unix_s >= pending.recording_end_unix {
            continue;
        }
        let start_unix = event.unix_s.max(pending.recording_start_unix);
        let end_unix = next_unix.min(pending.recording_end_unix).max(start_unix);
        let clip_start_s = clamp_clip_time(
            start_unix as f64 - pending.recording_start_unix as f64,
            pending,
        );
        let clip_end_s = clamp_clip_time(
            end_unix as f64 - pending.recording_start_unix as f64,
            pending,
        )
        .max(clip_start_s);
        if clip_end_s <= clip_start_s {
            continue;
        }
        plays.push(ClipPlay {
            game_id: GameId::Osu,
            source: "osu_title".into(),
            external_id: format!("osu-title:{}", event.unix_s),
            url: None,
            beatmap_id: None,
            beatmapset_id: None,
            cover_url: None,
            title: info.title,
            artist: info.artist,
            difficulty: info.difficulty,
            mapper: None,
            star_rating: None,
            mods: Vec::new(),
            rank: None,
            passed: true,
            accuracy: None,
            max_combo: None,
            total_score: None,
            pp: None,
            started_at: Some(unix_to_rfc3339(start_unix)),
            ended_at: unix_to_rfc3339(end_unix),
            derived_start: true,
            t_start_s: clip_start_s,
            t_end_s: Some(clip_end_s),
        });
    }
    plays
}

struct TitlePlayInfo {
    artist: String,
    title: String,
    difficulty: String,
}

fn parse_osu_title_play(title: &str) -> Option<TitlePlayInfo> {
    let raw = title.trim();
    if !raw.to_ascii_lowercase().starts_with("osu!") {
        return None;
    }
    let rest = raw.get(4..)?.trim_start();
    let rest = rest.strip_prefix('-')?.trim();
    if rest.is_empty() {
        return None;
    }

    let (song, difficulty) = if rest.ends_with(']') {
        if let Some(open) = rest.rfind('[') {
            (
                rest[..open].trim_end(),
                rest[open + 1..rest.len().saturating_sub(1)].trim(),
            )
        } else {
            (rest, "")
        }
    } else {
        (rest, "")
    };
    let (artist, title) = song
        .split_once(" - ")
        .map(|(artist, title)| (artist.trim(), title.trim()))
        .unwrap_or(("", song.trim()));
    Some(TitlePlayInfo {
        artist: artist.to_string(),
        title: if title.is_empty() {
            rest.to_string()
        } else {
            title.to_string()
        },
        difficulty: difficulty.to_string(),
    })
}

fn discover_pending_in_dir(
    media_root: &Path,
    dir: &Path,
    out: &mut Vec<DiscoveredPendingEnrichment>,
) -> Result<(), String> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("read pending osu! enrichment dir {dir:?}: {e}")),
    };
    for entry in entries {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        let Some(stem) = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_suffix(".osu-enrichment.json"))
            .filter(|stem| !stem.is_empty())
        else {
            continue;
        };
        match discover_pending_file(media_root, &path, stem) {
            Ok(job) => out.push(job),
            Err(error) => match quarantine_pending_file(&path) {
                Ok(_quarantine) => tracing::warn!(
                    event = "invalid_osu_enrichment_quarantined",
                    error = %error
                ),
                Err(quarantine_error) => tracing::warn!(
                    event = "invalid_osu_enrichment_skipped",
                    error = %error,
                    quarantine_error = %quarantine_error
                ),
            },
        }
    }
    Ok(())
}

fn discover_pending_file(
    media_root: &Path,
    path: &Path,
    stem: &str,
) -> Result<DiscoveredPendingEnrichment, String> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|e| format!("inspect pending osu! enrichment {path:?}: {e}"))?;
    if !metadata.is_file() || metadata_is_link_or_reparse(&metadata) {
        return Err("pending sidecar is not a regular unlinked file".into());
    }
    let sidecar_path = path
        .canonicalize()
        .map_err(|e| format!("canonicalize pending osu! enrichment {path:?}: {e}"))?;
    if !sidecar_path.starts_with(media_root) {
        return Err(format!(
            "pending osu! enrichment {sidecar_path:?} escaped media root {media_root:?}"
        ));
    }
    let clip_candidate = path.with_file_name(format!("{stem}.mp4"));
    let clip_metadata = std::fs::symlink_metadata(&clip_candidate).map_err(|e| {
        format!(
            "pending osu! enrichment {sidecar_path:?} has no expected MP4 {clip_candidate:?}: {e}"
        )
    })?;
    if !clip_metadata.is_file() || metadata_is_link_or_reparse(&clip_metadata) {
        return Err(format!(
            "expected MP4 {clip_candidate:?} is not a regular unlinked file"
        ));
    }
    let clip_path = clip_candidate
        .canonicalize()
        .map_err(|e| format!("canonicalize expected MP4 {clip_candidate:?}: {e}"))?;
    let parent_ok = clip_path.parent() == Some(media_root)
        || clip_path.parent().and_then(Path::parent) == Some(media_root);
    if !parent_ok
        || !clip_path.starts_with(media_root)
        || clip_path
            .extension()
            .and_then(|extension| extension.to_str())
            != Some("mp4")
    {
        return Err(format!(
            "expected MP4 {clip_path:?} is outside the allowed media-root depth"
        ));
    }
    let json = std::fs::read_to_string(path)
        .map_err(|e| format!("read pending osu! enrichment {path:?}: {e}"))?;
    let record: OsuPendingEnrichment = serde_json::from_str(&json)
        .map_err(|e| format!("parse pending osu! enrichment {path:?}: {e}"))?;
    let serialized_clip = Path::new(&record.clip_path).canonicalize().map_err(|e| {
        format!(
            "canonicalize serialized osu! enrichment clip path {:?}: {e}",
            record.clip_path
        )
    })?;
    if serialized_clip != clip_path {
        return Err(format!(
            "serialized osu! enrichment clip path {serialized_clip:?} does not match discovered MP4 {clip_path:?}"
        ));
    }
    Ok(DiscoveredPendingEnrichment {
        record,
        clip_path,
        sidecar_path,
    })
}

fn quarantine_pending_file(path: &Path) -> Result<PathBuf, String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("pending sidecar has no parent: {path:?}"))?;
    let base = path
        .file_name()
        .map_or_else(|| OsString::from("sidecar"), OsString::from);
    for _ in 0..64 {
        let counter = OSU_SIDECAR_COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut name = base.clone();
        name.push(format!(".invalid.{}.{counter}", std::process::id()));
        let quarantine = parent.join(name);
        if quarantine.exists() {
            continue;
        }
        std::fs::rename(path, &quarantine)
            .map_err(|e| format!("rename {path:?} to {quarantine:?}: {e}"))?;
        return Ok(quarantine);
    }
    Err(format!("could not allocate a quarantine path for {path:?}"))
}

fn path_is_link_or_reparse(path: &Path) -> Result<bool, String> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|e| format!("inspect osu! enrichment path {path:?}: {e}"))?;
    Ok(metadata_is_link_or_reparse(&metadata))
}

fn metadata_is_link_or_reparse(metadata: &std::fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        false
    }
}

fn clip_session_is_osu(path: &Path) -> bool {
    let Some(parent) = path.parent() else {
        return false;
    };
    session_game_id(parent).as_deref() == Some(crate::game_plugins::OSU_ID)
}

fn session_game_id(session_dir: &Path) -> Option<String> {
    let json = std::fs::read_to_string(session_dir.join(SESSION_META_FILE)).ok()?;
    let value: serde_json::Value = serde_json::from_str(&json).ok()?;
    value.get("id")?.as_str().map(str::to_string)
}

fn score_start_unix(
    score: &OsuProxyScore,
    pending: &OsuPendingEnrichment,
) -> Option<(i64, bool, bool)> {
    if let Some(started_at) = score.started_at_unix {
        return Some((started_at, false, false));
    }
    if let Some(title_start) = matching_title_event_start_unix(score, pending) {
        return Some((title_start, true, false));
    }
    if !score.passed {
        return Some((score.ended_at_unix, true, true));
    }
    let Some(length_s) = adjusted_total_length_s(score) else {
        return Some((score.ended_at_unix, true, true));
    };
    Some((
        score
            .ended_at_unix
            .saturating_sub(length_s.max(0.0).round() as i64),
        true,
        false,
    ))
}

fn matching_title_event_start_unix(
    score: &OsuProxyScore,
    pending: &OsuPendingEnrichment,
) -> Option<i64> {
    let lookback_s = adjusted_total_length_s(score)
        .map(|length_s| length_s.max(0.0).ceil() as i64 + TITLE_EVENT_LENGTH_SLACK_S)
        .unwrap_or(TITLE_EVENT_FALLBACK_LOOKBACK_S);
    let earliest = score.ended_at_unix.saturating_sub(lookback_s);
    let latest = score.ended_at_unix + UTC_SKEW_TOLERANCE_S.ceil() as i64;

    pending
        .title_events
        .iter()
        .filter(|event| event.unix_s >= earliest && event.unix_s <= latest)
        .filter(|event| title_event_matches_score(&event.title, score))
        .max_by_key(|event| event.unix_s)
        .map(|event| event.unix_s)
}

fn title_event_matches_score(title: &str, score: &OsuProxyScore) -> bool {
    let haystack = normalized_title_match_text(title);
    contains_normalized(&haystack, &score.title)
}

fn contains_normalized(haystack: &str, needle: &str) -> bool {
    let needle = normalized_title_match_text(needle);
    !needle.is_empty() && haystack.contains(&needle)
}

fn normalized_title_match_text(value: &str) -> String {
    let mut out = String::new();
    let mut last_was_space = true;
    for ch in value.chars().flat_map(char::to_lowercase) {
        if ch.is_alphanumeric() {
            out.push(ch);
            last_was_space = false;
        } else if !last_was_space {
            out.push(' ');
            last_was_space = true;
        }
    }
    out.trim().to_string()
}

fn adjusted_total_length_s(score: &OsuProxyScore) -> Option<f64> {
    let mut length = score.beatmap_total_length_s?;
    if !length.is_finite() || length < 0.0 {
        return None;
    }
    let mods: Vec<String> = score
        .mods
        .iter()
        .map(|value| value.to_ascii_uppercase())
        .collect();
    if mods
        .iter()
        .any(|mod_name| mod_name == "DT" || mod_name == "NC")
    {
        length /= 1.5;
    } else if mods
        .iter()
        .any(|mod_name| mod_name == "HT" || mod_name == "DC")
    {
        length /= 0.75;
    }
    Some(length)
}

fn clamp_clip_time(value: f64, pending: &OsuPendingEnrichment) -> f64 {
    if !pending.clip_duration_s.is_finite() || pending.clip_duration_s <= 0.0 {
        return value.max(0.0);
    }
    value.max(0.0).min(pending.clip_duration_s)
}

fn unix_to_rfc3339(value: i64) -> String {
    let timestamp = UNIX_EPOCH + Duration::from_secs(value.max(0) as u64);
    DateTime::<Utc>::from(timestamp).to_rfc3339()
}

#[cfg(test)]
mod tests;
