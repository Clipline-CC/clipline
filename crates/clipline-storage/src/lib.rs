//! Filesystem storage management for saved clips.

mod empty_sessions;
mod sessions;

pub use empty_sessions::{
    remove_emptied_session_dir, remove_emptied_session_dir_after_clip, sweep_emptied_session_dirs,
};
pub use sessions::{is_session_dir_name, session_label, SessionTracker};

use std::fs;
use std::io::{self, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

pub const REPLAY_CACHE_RUN_PREFIX: &str = "clipline-replay-cache-";
pub const REPLAY_CACHE_OWNER_FILE: &str = ".clipline-run.json";

pub fn replay_cache_run_identity(name: &str) -> Option<(u128, u32)> {
    let suffix = name.strip_prefix(REPLAY_CACHE_RUN_PREFIX)?;
    let mut parts = suffix.split('-');
    let created_at = parts.next()?;
    let pid = parts.next()?;
    let attempt = parts.next()?;
    if parts.next().is_some()
        || [created_at, pid, attempt]
            .iter()
            .any(|part| part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()))
    {
        return None;
    }
    let created_at = created_at.parse().ok()?;
    let pid = pid.parse().ok()?;
    attempt.parse::<u32>().ok()?;
    Some((created_at, pid))
}

pub fn is_replay_cache_run_name(name: &str) -> bool {
    replay_cache_run_identity(name).is_some()
}

pub fn replay_cache_owner_identity(process_instance_id: &str) -> Option<(u32, u64)> {
    let (pid, creation_time) = process_instance_id.split_once(':')?;
    if [pid, creation_time]
        .iter()
        .any(|part| part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()))
    {
        return None;
    }
    Some((pid.parse().ok()?, creation_time.parse().ok()?))
}

/// Serializes emptied-session cleanup with session attribution writes.
/// ponytail: process-wide lock; split per session only if contention is measured.
static SESSION_MUTATION_LOCK: Mutex<()> = Mutex::new(());

pub(crate) fn lock_session_mutations() -> std::sync::MutexGuard<'static, ()> {
    SESSION_MUTATION_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageStatus {
    pub clip_count: usize,
    pub total_bytes: u64,
    pub quota_bytes: Option<u64>,
}

impl StorageStatus {
    pub fn is_over_quota(&self) -> bool {
        self.quota_bytes
            .is_some_and(|quota| self.total_bytes > quota)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GcReport {
    pub deleted_clips: usize,
    pub freed_bytes: u64,
    pub cleanup_errors: Vec<String>,
    pub status: StorageStatus,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClipGcPolicy {
    pub protected: bool,
    pub priority: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordingRecoveryReport {
    pub recovered: Vec<PathBuf>,
    pub deleted_empty: usize,
}

pub fn storage_status(dir: &Path, quota_bytes: Option<u64>) -> io::Result<StorageStatus> {
    let clips = inventory(dir, None)?;
    Ok(status_from_clips(&clips, quota_bytes))
}

/// Return the metadata sidecar that proves Clipline owns `path`.
///
/// Recording paths use the marker belonging to their eventual final MP4 so
/// the same proof survives recovery and finalization.
pub fn clip_ownership_marker_path(path: &Path) -> io::Result<PathBuf> {
    let clip = if is_recording_mp4(path) {
        recording_final_path(path)
            .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "invalid recording name"))?
    } else {
        path.to_path_buf()
    };
    if !is_mp4(&clip) {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "clip ownership markers require an MP4 path",
        ));
    }
    Ok(clip_sidecar_path(&clip, CLIP_OWNERSHIP_MARKER_SUFFIX))
}

/// Atomically create a valid empty Clipline metadata document for a new clip.
/// Returns `true` when this call created the marker and `false` when a regular
/// marker file already existed. Existing metadata is never overwritten.
pub fn ensure_clip_owned(path: &Path) -> io::Result<bool> {
    let marker = clip_ownership_marker_path(path)?;
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&marker)
    {
        Ok(mut file) => {
            if let Err(error) = file.write_all(b"{}") {
                drop(file);
                let _ = fs::remove_file(&marker);
                return Err(error);
            }
            Ok(true)
        }
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {
            if fs::metadata(&marker)?.is_file() {
                Ok(false)
            } else {
                Err(io::Error::new(
                    ErrorKind::AlreadyExists,
                    format!("clip ownership marker is not a file: {marker:?}"),
                ))
            }
        }
        Err(error) => Err(error),
    }
}

/// Create a session folder and its ownership marker while emptied-session
/// cleanup is excluded. The marker is the first visible proof of an in-progress
/// replay save.
pub fn ensure_session_clip_owned(path: &Path) -> io::Result<bool> {
    let marker = clip_ownership_marker_path(path)?;
    let parent = marker
        .parent()
        .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "clip path has no parent"))?;
    let _guard = lock_session_mutations();
    fs::create_dir_all(parent)?;
    ensure_clip_owned(path)
}

/// Reserve the `.mp4.recording` file while emptied-session cleanup is excluded.
/// Once this returns, the recording file itself keeps the folder alive.
pub fn reserve_session_recording_file(path: &Path) -> io::Result<fs::File> {
    clip_ownership_marker_path(path)?;
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "recording path has no parent"))?;
    let _guard = lock_session_mutations();
    fs::create_dir_all(parent)?;
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}

/// Write Clipline's session attribution file under the same lock used by
/// emptied-session cleanup. Returns `false` when an existing file was kept.
pub fn write_session_metadata(
    session_dir: &Path,
    bytes: &[u8],
    replace_existing: bool,
) -> io::Result<bool> {
    let _guard = lock_session_mutations();
    let path = session_dir.join(SESSION_META_FILE);
    if !replace_existing && path.exists() {
        return Ok(false);
    }
    fs::write(path, bytes)?;
    Ok(true)
}

pub fn remove_clip_ownership_marker(path: &Path) -> io::Result<()> {
    remove_file_if_exists(&clip_ownership_marker_path(path)?)
}

pub fn recover_recording_files(dir: &Path) -> io::Result<RecordingRecoveryReport> {
    let mut report = RecordingRecoveryReport {
        recovered: Vec::new(),
        deleted_empty: 0,
    };
    visit_media_dirs(dir, |media_dir| {
        for entry in fs::read_dir(media_dir)? {
            let entry = entry?;
            let path = entry.path();
            if !is_recording_mp4(&path) {
                continue;
            }
            if !is_managed_clip(&path) {
                continue;
            }
            let meta = entry.metadata()?;
            if !meta.is_file() {
                continue;
            }
            let old_marker = clip_ownership_marker_path(&path)?;
            if !old_marker.is_file() {
                ensure_clip_owned(&path)?;
            }
            if meta.len() == 0 {
                remove_file_if_exists(&path)?;
                remove_clip_ownership_marker(&path)?;
                report.deleted_empty += 1;
                continue;
            }
            let final_path = recording_final_path(&path)
                .map(|candidate| unique_recovered_path(&candidate, &old_marker))
                .ok_or_else(|| io::Error::new(ErrorKind::InvalidData, "invalid recording name"))?;
            let final_marker = clip_ownership_marker_path(&final_path)?;
            fs::rename(&path, &final_path)?;
            if old_marker != final_marker {
                if let Err(marker_error) = fs::rename(&old_marker, &final_marker) {
                    if let Err(rollback_error) = fs::rename(&final_path, &path) {
                        return Err(io::Error::new(
                            marker_error.kind(),
                            format!(
                                "move recovery marker {old_marker:?} to {final_marker:?}: \
                                 {marker_error}; restore recording {final_path:?} to {path:?}: \
                                 {rollback_error}"
                            ),
                        ));
                    }
                    return Err(marker_error);
                }
            }
            report.recovered.push(final_path);
        }
        Ok(())
    })?;
    Ok(report)
}

/// Delete every Clipline-owned saved or in-progress clip below `dir` while
/// preserving unrelated files and the media root itself.
pub fn delete_all_managed_media(dir: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(dir)?;
    if is_link_or_reparse_point(&metadata) {
        return Err(io::Error::new(
            ErrorKind::PermissionDenied,
            "refusing to delete media through a link or reparse point",
        ));
    }

    let mut clips = Vec::new();
    let mut first_error = None;
    if let Err(error) = collect_clips(dir, None, &mut clips) {
        first_error.get_or_insert(error);
    }
    match fs::read_dir(dir) {
        Ok(entries) => {
            for entry in entries {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(error) => {
                        first_error.get_or_insert(error);
                        continue;
                    }
                };
                let path = entry.path();
                let metadata = match fs::symlink_metadata(&path) {
                    Ok(metadata) => metadata,
                    Err(error) => {
                        first_error.get_or_insert(error);
                        continue;
                    }
                };
                if metadata.is_dir() && !is_link_or_reparse_point(&metadata) {
                    if let Err(error) = collect_clips(&path, None, &mut clips) {
                        first_error.get_or_insert(error);
                    }
                }
            }
        }
        Err(error) => {
            first_error.get_or_insert(error);
        }
    }

    for mut clip in clips {
        if clip.recording {
            let sidecar_clip =
                recording_final_path(&clip.path).unwrap_or_else(|| clip.path.clone());
            match clip_sidecars(&sidecar_clip) {
                Ok((sidecars, sidecar_bytes)) => {
                    clip.sidecars = sidecars;
                    clip.sidecar_bytes = sidecar_bytes;
                }
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
        if let Err(error) = delete_inventoried_clip(&clip, dir) {
            first_error.get_or_insert(error);
        }
    }
    if let Err(error) = sweep_emptied_session_dirs(dir) {
        first_error.get_or_insert(error);
    }
    first_error.map_or(Ok(()), Err)
}

pub fn enforce_quota(
    dir: &Path,
    quota_bytes: Option<u64>,
    protect: Option<&Path>,
) -> io::Result<GcReport> {
    enforce_quota_with_policy(dir, quota_bytes, protect, |_| ClipGcPolicy::default())
}

/// Enforces the clip quota while letting the caller order deletion by clip
/// policy and protect additional managed files that are temporarily immutable
/// (active upload sources, favorites). Protected bytes still count toward the
/// quota; collection skips them and continues with the next deletable clip.
///
/// `policy` is evaluated once per clip. Lower priorities are deleted first,
/// and within the same priority the oldest clip goes first. Storage stays
/// neutral about what protection and priority mean.
pub fn enforce_quota_with_policy(
    dir: &Path,
    quota_bytes: Option<u64>,
    protect: Option<&Path>,
    policy: impl Fn(&Path) -> ClipGcPolicy,
) -> io::Result<GcReport> {
    enforce_quota_with_policy_and_cleanup(
        dir,
        quota_bytes,
        protect,
        policy,
        remove_emptied_session_dir_after_clip,
    )
}

fn enforce_quota_with_policy_and_cleanup(
    dir: &Path,
    quota_bytes: Option<u64>,
    protect: Option<&Path>,
    policy: impl Fn(&Path) -> ClipGcPolicy,
    cleanup: impl Fn(&Path, &Path) -> io::Result<bool>,
) -> io::Result<GcReport> {
    let Some(quota) = quota_bytes else {
        return Ok(GcReport {
            deleted_clips: 0,
            freed_bytes: 0,
            cleanup_errors: Vec::new(),
            status: storage_status(dir, quota_bytes)?,
        });
    };

    let clips = inventory(dir, protect)?;
    let mut total_bytes = clips.iter().map(ClipFile::total_bytes).sum::<u64>();
    let mut deleted_clips = 0usize;
    let mut freed_bytes = 0u64;
    let mut cleanup_errors = Vec::new();

    if total_bytes <= quota {
        return Ok(GcReport {
            deleted_clips,
            freed_bytes,
            cleanup_errors,
            status: status_from_clips(&clips, quota_bytes),
        });
    }

    // Decorate once because app policy reads clip sidecars.
    let mut clips = clips
        .into_iter()
        .map(|clip| (policy(&clip.path), clip))
        .collect::<Vec<_>>();

    let undeletable_bytes = clips
        .iter()
        .filter(|(policy, clip)| !clip.can_delete(protect, policy.protected))
        .map(|(_, clip)| clip.total_bytes())
        .sum::<u64>();
    if undeletable_bytes > quota {
        let clips = clips.into_iter().map(|(_, clip)| clip).collect::<Vec<_>>();
        return Ok(GcReport {
            deleted_clips,
            freed_bytes,
            cleanup_errors,
            status: status_from_clips(&clips, quota_bytes),
        });
    }

    clips.sort_by(|(policy_a, a), (policy_b, b)| {
        policy_a
            .priority
            .cmp(&policy_b.priority)
            .then_with(|| a.modified.cmp(&b.modified))
            .then_with(|| a.path.file_name().cmp(&b.path.file_name()))
    });

    for (policy, clip) in clips {
        if total_bytes <= quota {
            break;
        }
        if !clip.can_delete(protect, policy.protected) {
            continue;
        }

        let clip_bytes = clip.total_bytes();
        match delete_inventoried_clip_with(&clip, dir, &cleanup)? {
            DeletedClip::Removed { cleanup_error } => {
                total_bytes = total_bytes.saturating_sub(clip_bytes);
                freed_bytes += clip_bytes;
                deleted_clips += 1;
                if let Some(error) = cleanup_error {
                    cleanup_errors.push(error);
                }
            }
            // The file is already gone from this tree (rename/delete race or a
            // prior collector). Drop its bytes from the running total so we do
            // not keep deleting the next-oldest clip against a stale sum.
            DeletedClip::AlreadyGone => {
                total_bytes = total_bytes.saturating_sub(clip_bytes);
            }
            DeletedClip::Skipped => {}
        }
    }

    Ok(GcReport {
        deleted_clips,
        freed_bytes,
        cleanup_errors,
        status: status_from_clips(&inventory(dir, protect)?, quota_bytes),
    })
}

#[derive(Debug, Clone)]
struct ClipFile {
    path: PathBuf,
    /// Files that live and die with the clip: markers, clip metadata, pending
    /// osu! enrichment, and the cached poster frame. Each is removed alongside
    /// the clip during quota GC so a leftover never keeps an emptied session
    /// folder alive.
    sidecars: Vec<PathBuf>,
    mp4_bytes: u64,
    sidecar_bytes: u64,
    modified: SystemTime,
    recording: bool,
}

impl ClipFile {
    fn total_bytes(&self) -> u64 {
        self.mp4_bytes + self.sidecar_bytes
    }

    fn can_delete(&self, protect: Option<&Path>, additionally_protected: bool) -> bool {
        !self.recording
            && !protect.is_some_and(|protected| same_path(&self.path, protected))
            && !additionally_protected
    }
}

/// Clips live at the root (legacy) or one level down in session folders.
fn inventory(dir: &Path, include: Option<&Path>) -> io::Result<Vec<ClipFile>> {
    let mut clips = Vec::new();
    visit_media_dirs(dir, |media_dir| {
        collect_clips(media_dir, include, &mut clips)
    })?;
    Ok(clips)
}

fn collect_clips(dir: &Path, include: Option<&Path>, clips: &mut Vec<ClipFile>) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let recording = is_recording_mp4(&path);
        if !is_mp4(&path) && !recording {
            continue;
        }
        if !is_managed_clip(&path) && !include.is_some_and(|candidate| same_path(&path, candidate))
        {
            continue;
        }
        let meta = entry.metadata()?;
        if !meta.is_file() {
            continue;
        }
        let (sidecars, sidecar_bytes) = if recording {
            (Vec::new(), 0)
        } else {
            clip_sidecars(&path)?
        };
        clips.push(ClipFile {
            path,
            sidecars,
            mp4_bytes: meta.len(),
            sidecar_bytes,
            modified: meta.modified().unwrap_or(UNIX_EPOCH),
            recording,
        });
    }
    Ok(())
}

fn status_from_clips(clips: &[ClipFile], quota_bytes: Option<u64>) -> StorageStatus {
    StorageStatus {
        clip_count: clips.iter().filter(|clip| !clip.recording).count(),
        total_bytes: clips.iter().map(ClipFile::total_bytes).sum(),
        quota_bytes,
    }
}

fn is_mp4(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("mp4"))
}

fn is_recording_mp4(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.to_ascii_lowercase().ends_with(".mp4.recording"))
}

fn recording_final_path(path: &Path) -> Option<PathBuf> {
    let name = path.file_name()?.to_str()?;
    const SUFFIX: &str = ".recording";
    let split = name.len().checked_sub(SUFFIX.len())?;
    let suffix = name.get(split..)?;
    if !suffix.eq_ignore_ascii_case(SUFFIX) {
        return None;
    }
    let final_name = name.get(..split)?;
    Some(path.with_file_name(final_name))
}

fn is_managed_clip(path: &Path) -> bool {
    let Ok(marker) = clip_ownership_marker_path(path) else {
        return false;
    };
    if marker.is_file() {
        return true;
    }
    // New recordings are identified by their ownership marker. Pre-marker
    // releases can be adopted only through Clipline's generated filename.
    if is_recording_mp4(path) {
        return is_legacy_generated_clip(path);
    }
    // Conservative legacy signals. Poster files are deliberately excluded:
    // merely previewing an unrelated MP4 can create one.
    clip_sidecar_path(path, MARKERS_SUFFIX).is_file()
        || clip_sidecar_path(path, OSU_ENRICHMENT_SUFFIX).is_file()
        || is_legacy_generated_clip(path)
}

pub fn is_clip_owned(path: &Path) -> bool {
    is_managed_clip(path)
}

fn is_legacy_generated_clip(path: &Path) -> bool {
    let candidate = if is_recording_mp4(path) {
        let Some(final_path) = recording_final_path(path) else {
            return false;
        };
        final_path
    } else {
        path.to_path_buf()
    };
    let Some(stem) = candidate.file_stem().and_then(|stem| stem.to_str()) else {
        return false;
    };
    let Some(generated) = stem
        .strip_prefix("clip_")
        .or_else(|| stem.strip_prefix("session_"))
    else {
        return false;
    };
    let mut parts = generated.split('_');
    let Some(timestamp) = parts.next() else {
        return false;
    };
    if !(9..=20).contains(&timestamp.len()) || !timestamp.bytes().all(|byte| byte.is_ascii_digit())
    {
        return false;
    }
    match (parts.next(), parts.next()) {
        (None, None) => true,
        (Some(attempt), None) => {
            !attempt.is_empty() && attempt.bytes().all(|byte| byte.is_ascii_digit())
        }
        _ => false,
    }
}

fn unique_recovered_path(candidate: &Path, current_marker: &Path) -> PathBuf {
    if recovery_destination_available(candidate, current_marker) {
        return candidate.to_path_buf();
    }
    let parent = candidate.parent().unwrap_or_else(|| Path::new(""));
    let stem = candidate
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("session");
    for attempt in 0u32..1024 {
        let name = if attempt == 0 {
            format!("{stem}_recovered.mp4")
        } else {
            format!("{stem}_recovered_{attempt}.mp4")
        };
        let recovered = parent.join(name);
        if recovery_destination_available(&recovered, current_marker) {
            return recovered;
        }
    }
    parent.join(format!(
        "{stem}_recovered_{}.mp4",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ))
}

fn recovery_destination_available(path: &Path, current_marker: &Path) -> bool {
    !path.exists()
        && clip_ownership_marker_path(path)
            .is_ok_and(|marker| marker == current_marker || !marker.exists())
}

pub(crate) const SESSION_META_FILE: &str = "clipline-session.json";
pub const MARKERS_SUFFIX: &str = ".markers.json";
pub const CLIP_OWNERSHIP_MARKER_SUFFIX: &str = ".clipline.json";
pub const FAVORITE_MARKER_SUFFIX: &str = ".clipline-favorite";
pub const OSU_ENRICHMENT_SUFFIX: &str = ".osu-enrichment.json";
pub const POSTER_SUFFIX: &str = ".poster.jpg";

/// Sidecar suffixes paired with a clip stem (`clip.mp4` → `clip.markers.json`).
/// Leftover-folder cleanup uses this same table; anything else is unrecognized.
pub const CLIP_SIDECAR_SUFFIXES: [&str; 5] = [
    MARKERS_SUFFIX,
    CLIP_OWNERSHIP_MARKER_SUFFIX,
    FAVORITE_MARKER_SUFFIX,
    OSU_ENRICHMENT_SUFFIX,
    POSTER_SUFFIX,
];

#[derive(Debug, Clone, PartialEq, Eq)]
enum DeletedClip {
    Removed {
        cleanup_error: Option<String>,
    },
    /// Inventoried MP4 vanished before we could delete it.
    AlreadyGone,
    /// Still present but protected (upload lease / active mutation).
    Skipped,
}

fn delete_inventoried_clip(clip: &ClipFile, media_root: &Path) -> io::Result<DeletedClip> {
    delete_inventoried_clip_with(clip, media_root, remove_emptied_session_dir_after_clip)
}

fn delete_inventoried_clip_with(
    clip: &ClipFile,
    media_root: &Path,
    cleanup: impl Fn(&Path, &Path) -> io::Result<bool>,
) -> io::Result<DeletedClip> {
    // Never delete through a directory symlink/junction that escaped the
    // configured media root (or any path that no longer resolves under it).
    if !is_within_media_root(&clip.path, media_root) {
        return Ok(DeletedClip::Skipped);
    }

    match fs::remove_file(&clip.path) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {
            // Another task already moved or deleted the MP4 (rename/delete).
            // Do not touch the inventoried sidecars — they may still belong
            // to the renamed clip, and the other task owns their cleanup.
            return Ok(DeletedClip::AlreadyGone);
        }
        Err(error) => return Err(error),
    }

    // The MP4 is gone; clip-attached sidecars are best-effort so a transient
    // sidecar error cannot abort collection of remaining over-quota clips.
    for sidecar in &clip.sidecars {
        let _ = remove_file_if_exists(sidecar);
    }
    // The primary deletion succeeded. Session cleanup is diagnostic only: it
    // must not turn a removed clip into a failed deletion or abort quota GC.
    let cleanup_error = clip.path.parent().and_then(|parent| {
        cleanup(parent, media_root)
            .err()
            .map(|error| error.to_string())
    });
    Ok(DeletedClip::Removed { cleanup_error })
}

fn is_within_media_root(path: &Path, media_root: &Path) -> bool {
    let Ok(root) = media_root.canonicalize() else {
        return false;
    };
    if let Ok(path) = path.canonicalize() {
        return path.starts_with(&root);
    }
    // The inventoried MP4 may already be gone (rename/delete race). Fall back
    // to the parent directory so containment still holds for AlreadyGone.
    let Some(parent) = path.parent() else {
        return false;
    };
    match parent.canonicalize() {
        Ok(parent) => parent.starts_with(&root) || parent == root,
        Err(_) => false,
    }
}

pub(crate) fn is_link_or_reparse_point(metadata: &fs::Metadata) -> bool {
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

pub fn clip_sidecar_path(clip: &Path, suffix: &str) -> PathBuf {
    clip.with_extension(suffix.trim_start_matches('.'))
}

/// The sidecar files present beside a clip (markers, clip metadata, pending osu!
/// enrichment, and cached poster) and their combined size. A zero-byte sidecar
/// that exists is still tracked so it gets cleaned up with the clip.
fn clip_sidecars(clip: &Path) -> io::Result<(Vec<PathBuf>, u64)> {
    let mut sidecars = Vec::new();
    let mut bytes = 0u64;
    for suffix in &CLIP_SIDECAR_SUFFIXES {
        let candidate = clip_sidecar_path(clip, suffix);
        let len = optional_file_len(&candidate)?;
        if len > 0 || candidate.exists() {
            bytes += len;
            sidecars.push(candidate);
        }
    }
    Ok((sidecars, bytes))
}

fn optional_file_len(path: &Path) -> io::Result<u64> {
    match fs::metadata(path) {
        Ok(meta) if meta.is_file() => Ok(meta.len()),
        Ok(_) => Ok(0),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(0),
        Err(e) => Err(e),
    }
}

pub(crate) fn remove_file_if_exists(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

fn same_path(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

fn visit_media_dirs(dir: &Path, mut f: impl FnMut(&Path) -> io::Result<()>) -> io::Result<()> {
    f(dir)?;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        // symlink_metadata does not follow links, so a child junction/symlink
        // into an external tree cannot be entered for inventory or GC.
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !metadata.is_dir() || is_link_or_reparse_point(&metadata) {
            continue;
        }
        f(&path)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clipline_test_utils::TestDir;
    use std::sync::mpsc;
    use std::time::Duration;

    fn tick_mtime() {
        std::thread::sleep(Duration::from_millis(20));
    }

    #[test]
    fn replay_reservation_waits_for_cleanup_lock_before_creating_the_folder() {
        let dir = TestDir::new("clipline-storage", "session-reservation-lock");
        let replay = dir.path().join("2026-08-30 01-00/clip_1.mp4");
        let guard = lock_session_mutations();
        let worker_path = replay.clone();
        let (tx, rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            tx.send(ensure_session_clip_owned(&worker_path)).unwrap();
        });

        assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());
        assert!(!replay.parent().unwrap().exists());
        drop(guard);
        assert!(rx.recv_timeout(Duration::from_secs(2)).unwrap().unwrap());
        worker.join().unwrap();
        assert!(clip_ownership_marker_path(&replay).unwrap().is_file());
    }

    #[test]
    fn full_session_reservation_waits_for_cleanup_lock_before_creating_the_folder() {
        let dir = TestDir::new("clipline-storage", "recording-reservation-lock");
        let recording = dir.path().join("2026-08-30 01-01/session_1.mp4.recording");
        let guard = lock_session_mutations();
        let worker_path = recording.clone();
        let (tx, rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            tx.send(reserve_session_recording_file(&worker_path))
                .unwrap();
        });

        assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());
        assert!(!recording.parent().unwrap().exists());
        drop(guard);
        let file = rx.recv_timeout(Duration::from_secs(2)).unwrap().unwrap();
        drop(file);
        worker.join().unwrap();
        assert!(recording.is_file());
    }

    fn mark_owned(path: &Path) {
        std::fs::write(clip_ownership_marker_path(path).unwrap(), b"").unwrap();
    }

    fn write_owned(dir: &TestDir, relative: &str, bytes: usize) -> PathBuf {
        let path = dir.write(relative, bytes);
        mark_owned(&path);
        path
    }

    #[test]
    fn replay_cache_run_names_require_three_numeric_components() {
        assert!(is_replay_cache_run_name("clipline-replay-cache-123-456-0"));
        assert_eq!(
            replay_cache_run_identity("clipline-replay-cache-123-456-0"),
            Some((123, 456))
        );
        assert!(!is_replay_cache_run_name("clipline-replay-cache-backup"));
        assert!(!is_replay_cache_run_name(
            "clipline-replay-cache-123-456-0-extra"
        ));
        assert_eq!(replay_cache_owner_identity("456:789"), Some((456, 789)));
        assert_eq!(replay_cache_owner_identity("456:not-a-time"), None);
        assert_eq!(
            replay_cache_owner_identity("456:18446744073709551616"),
            None
        );
    }

    #[test]
    fn status_counts_clip_metadata_and_other_sidecars() {
        let dir = TestDir::new("clipline-storage", "status-counts");
        dir.write("a.mp4", 10);
        dir.write("a.markers.json", 3);
        dir.write("a.clipline.json", 5);
        write_owned(&dir, "b.mp4", 7);

        let status = storage_status(dir.path(), Some(100)).unwrap();

        assert_eq!(status.clip_count, 2);
        assert_eq!(status.total_bytes, 25);
        assert_eq!(status.quota_bytes, Some(100));
        assert!(!status.is_over_quota());
    }

    #[test]
    fn status_counts_recording_bytes_without_counting_a_clip() {
        let dir = TestDir::new("clipline-storage", "status-recording");
        write_owned(&dir, "saved.mp4", 10);
        let recording = dir.write("session.mp4.recording", 90);
        mark_owned(&recording);

        let status = storage_status(dir.path(), Some(100)).unwrap();

        assert_eq!(status.clip_count, 1);
        assert_eq!(status.total_bytes, 100);
    }

    #[test]
    fn quota_status_is_observational_even_when_usage_exceeds_the_limit() {
        let dir = TestDir::new("clipline-storage", "status-never-deletes");
        let old = write_owned(&dir, "old.mp4", 10);
        let markers = dir.write("old.markers.json", 3);
        let recording = dir.write("session.mp4.recording", 7);
        mark_owned(&recording);

        let status = storage_status(dir.path(), Some(1)).unwrap();

        assert!(status.is_over_quota());
        assert_eq!(std::fs::read(&old).unwrap().len(), 10);
        assert_eq!(std::fs::read(&markers).unwrap().len(), 3);
        assert_eq!(std::fs::read(&recording).unwrap().len(), 7);
    }

    #[test]
    fn inventory_ignores_non_mp4_files() {
        let dir = TestDir::new("clipline-storage", "ignore-non-mp4");
        dir.write("notes.txt", 99);
        write_owned(&dir, "clip.mp4", 4);

        let status = storage_status(dir.path(), None).unwrap();

        assert_eq!(status.clip_count, 1);
        assert_eq!(status.total_bytes, 4);
    }

    #[test]
    fn status_ignores_unmarked_mp4_files_in_root_and_child_directories() {
        let dir = TestDir::new("clipline-storage", "ignore-unowned-mp4");
        dir.write("unrelated.mp4", 90);
        dir.write("Movies/also-unrelated.mp4", 80);
        dir.write("2026-07-18 12-00/owned.mp4", 10);
        dir.write("2026-07-18 12-00/owned.clipline.json", 2);

        let status = storage_status(dir.path(), None).unwrap();

        assert_eq!(status.clip_count, 1);
        assert_eq!(status.total_bytes, 12);
    }

    #[test]
    fn status_counts_unmarked_legacy_clipline_filenames() {
        let dir = TestDir::new("clipline-storage", "legacy-generated-status");
        dir.write("clip_1784525638.mp4", 10);
        dir.write("2026-07-20 01-31/session_1784525639_1.mp4", 12);
        dir.write("ordinary.mp4", 90);

        let status = storage_status(dir.path(), None).unwrap();

        assert_eq!(status.clip_count, 2);
        assert_eq!(status.total_bytes, 22);
    }

    #[test]
    fn enforce_quota_never_deletes_unmarked_mp4_files() {
        let dir = TestDir::new("clipline-storage", "preserve-unowned-mp4");
        let unrelated = dir.write("unrelated.mp4", 90);
        let nested_unrelated = dir.write("Movies/also-unrelated.mp4", 80);
        let owned = dir.write("2026-07-18 12-00/owned.mp4", 10);
        let owned_marker = dir.write("2026-07-18 12-00/owned.clipline.json", 2);

        let report = enforce_quota(dir.path(), Some(0), None).unwrap();

        assert_eq!(report.deleted_clips, 1);
        assert!(unrelated.exists());
        assert!(nested_unrelated.exists());
        assert!(!owned.exists());
        assert!(!owned_marker.exists());
        assert_eq!(report.status.total_bytes, 0);
    }

    #[test]
    fn enforce_quota_deletes_unmarked_legacy_clipline_filenames() {
        let dir = TestDir::new("clipline-storage", "legacy-generated-quota");
        let legacy = dir.write("clip_1784525638.mp4", 10);
        let unrelated = dir.write("ordinary.mp4", 90);

        let report = enforce_quota(dir.path(), Some(0), None).unwrap();

        assert_eq!(report.deleted_clips, 1);
        assert!(!legacy.exists());
        assert!(unrelated.exists());
    }

    #[test]
    fn enforce_quota_counts_an_explicitly_protected_new_clip() {
        let dir = TestDir::new("clipline-storage", "protect-new-unmarked");
        dir.write("unrelated.mp4", 90);
        let fresh = dir.write("2026-07-18 12-00/fresh.mp4", 10);

        let report = enforce_quota(dir.path(), Some(5), Some(&fresh)).unwrap();

        assert_eq!(report.deleted_clips, 0);
        assert_eq!(report.status.clip_count, 1);
        assert_eq!(report.status.total_bytes, 10);
        assert!(report.status.is_over_quota());
        assert!(fresh.exists());
    }

    #[test]
    fn enforce_quota_deletes_oldest_until_under_budget() {
        let dir = TestDir::new("clipline-storage", "oldest-first");
        let a = write_owned(&dir, "a.mp4", 10);
        tick_mtime();
        let b = write_owned(&dir, "b.mp4", 10);
        tick_mtime();
        let c = write_owned(&dir, "c.mp4", 10);

        let report = enforce_quota(dir.path(), Some(15), None).unwrap();

        assert_eq!(report.deleted_clips, 2);
        assert_eq!(report.freed_bytes, 20);
        assert!(!a.exists());
        assert!(!b.exists());
        assert!(c.exists());
        assert_eq!(report.status.total_bytes, 10);
    }

    #[test]
    fn enforce_quota_skips_additionally_protected_uploads_and_keeps_collecting() {
        let dir = TestDir::new("clipline-storage", "protect-active-upload");
        let uploading = write_owned(&dir, "uploading.mp4", 10);
        tick_mtime();
        let next_oldest = write_owned(&dir, "next-oldest.mp4", 10);
        tick_mtime();
        let newest = write_owned(&dir, "newest.mp4", 10);

        let report = enforce_quota_with_policy(dir.path(), Some(20), None, |path| ClipGcPolicy {
            protected: same_path(path, &uploading),
            priority: 0,
        })
        .unwrap();

        assert_eq!(report.deleted_clips, 1);
        assert!(uploading.exists(), "an active upload is immutable");
        assert!(
            !next_oldest.exists(),
            "GC must continue to the next deletable clip"
        );
        assert!(newest.exists());
        assert_eq!(report.status.total_bytes, 20);
    }

    fn kind_priority(name: &str) -> u8 {
        // Sessions drain first, then replays, then trims (the app maps the
        // clip kind onto this order; here the file name stands in for it).
        if name.starts_with("session-") {
            0
        } else if name.starts_with("replay-") {
            1
        } else {
            2
        }
    }

    #[test]
    fn enforce_quota_with_policy_deletes_by_priority_before_age() {
        let dir = TestDir::new("clipline-storage", "policy-priority");
        // Oldest clip is a trim (lowest deletion priority): age must lose to
        // kind priority when the quota frees only part of the library.
        let trim = write_owned(&dir, "trim-old.mp4", 10);
        tick_mtime();
        let replay = write_owned(&dir, "replay-mid.mp4", 10);
        tick_mtime();
        let session = write_owned(&dir, "session-new.mp4", 10);

        let report = enforce_quota_with_policy(dir.path(), Some(10), None, |path| ClipGcPolicy {
            protected: false,
            priority: kind_priority(path.file_name().and_then(|n| n.to_str()).unwrap_or("")),
        })
        .unwrap();

        assert_eq!(report.deleted_clips, 2);
        assert!(
            !session.exists(),
            "sessions must drain before replays/trims"
        );
        assert!(!replay.exists(), "replays must drain before trims");
        assert!(
            trim.exists(),
            "the oldest clip can survive when its kind is low priority"
        );
        assert_eq!(report.status.total_bytes, 10);
    }

    #[test]
    fn enforce_quota_with_policy_deletes_oldest_within_a_priority() {
        let dir = TestDir::new("clipline-storage", "policy-within-kind");
        let oldest = write_owned(&dir, "replay-oldest.mp4", 10);
        tick_mtime();
        let older = write_owned(&dir, "replay-old.mp4", 10);
        tick_mtime();
        let newer = write_owned(&dir, "replay-new.mp4", 10);

        let report = enforce_quota_with_policy(dir.path(), Some(10), None, |path| ClipGcPolicy {
            protected: false,
            priority: kind_priority(path.file_name().and_then(|n| n.to_str()).unwrap_or("")),
        })
        .unwrap();

        assert_eq!(report.deleted_clips, 2);
        assert!(!oldest.exists());
        assert!(!older.exists());
        assert!(newer.exists());
    }

    #[test]
    fn enforce_quota_with_policy_skips_protected_high_priority_clips() {
        let dir = TestDir::new("clipline-storage", "policy-protected");
        let favorite = write_owned(&dir, "session-favorite.mp4", 10);
        tick_mtime();
        let replay = write_owned(&dir, "replay.mp4", 10);
        tick_mtime();
        let trim = write_owned(&dir, "trim.mp4", 10);

        let report = enforce_quota_with_policy(dir.path(), Some(10), None, |path| ClipGcPolicy {
            protected: same_path(path, &favorite),
            priority: kind_priority(path.file_name().and_then(|n| n.to_str()).unwrap_or("")),
        })
        .unwrap();

        assert_eq!(report.deleted_clips, 2);
        assert!(
            favorite.exists(),
            "protected clips must survive even at high priority"
        );
        assert!(!replay.exists());
        assert!(!trim.exists());
        assert_eq!(report.status.total_bytes, 10);
    }

    #[test]
    fn enforce_quota_under_budget_skips_policy_callbacks() {
        let dir = TestDir::new("clipline-storage", "policy-under-budget");
        write_owned(&dir, "clip.mp4", 10);
        let protection_checks = std::cell::Cell::new(0usize);
        let priority_checks = std::cell::Cell::new(0usize);

        let report = enforce_quota_with_policy(dir.path(), Some(100), None, |_| {
            protection_checks.set(protection_checks.get() + 1);
            priority_checks.set(priority_checks.get() + 1);
            ClipGcPolicy::default()
        })
        .unwrap();

        assert_eq!(report.deleted_clips, 0);
        assert_eq!(protection_checks.get(), 0);
        assert_eq!(priority_checks.get(), 0);
    }

    #[test]
    fn enforce_quota_evaluates_policy_once_per_clip() {
        let dir = TestDir::new("clipline-storage", "policy-once");
        write_owned(&dir, "old.mp4", 10);
        tick_mtime();
        write_owned(&dir, "new.mp4", 10);
        let checks = std::cell::Cell::new(0usize);

        let report = enforce_quota_with_policy(dir.path(), Some(10), None, |_| {
            checks.set(checks.get() + 1);
            ClipGcPolicy::default()
        })
        .unwrap();

        assert_eq!(report.deleted_clips, 1);
        assert_eq!(checks.get(), 2, "policy must be computed once per clip");
    }

    #[test]
    fn enforce_quota_reports_cleanup_error_after_counting_deleted_clip() {
        let dir = TestDir::new("clipline-storage", "quota-cleanup-error");
        let old = write_owned(&dir, "2026-08-30 01-00/old.mp4", 40);
        tick_mtime();
        let keep = write_owned(&dir, "keep.mp4", 10);

        let report = enforce_quota_with_policy_and_cleanup(
            dir.path(),
            Some(20),
            None,
            |_| ClipGcPolicy::default(),
            |_, _| Err(io::Error::other("simulated cleanup failure")),
        )
        .unwrap();

        assert_eq!(report.deleted_clips, 1);
        assert_eq!(report.freed_bytes, 40);
        assert_eq!(report.cleanup_errors.len(), 1);
        assert!(!old.exists());
        assert!(keep.exists());
    }

    #[test]
    fn enforce_quota_deletes_marker_sidecar_with_clip() {
        let dir = TestDir::new("clipline-storage", "sidecar-delete");
        let old = dir.write("old.mp4", 10);
        let sidecar = dir.write("old.markers.json", 2);
        tick_mtime();
        let keep = write_owned(&dir, "keep.mp4", 10);

        let report = enforce_quota(dir.path(), Some(10), None).unwrap();

        assert_eq!(report.deleted_clips, 1);
        assert_eq!(report.freed_bytes, 12);
        assert!(!old.exists());
        assert!(!sidecar.exists());
        assert!(keep.exists());
        assert_eq!(report.status.total_bytes, 10);
    }

    #[test]
    fn enforce_quota_deletes_poster_sidecar_with_clip() {
        let dir = TestDir::new("clipline-storage", "poster-delete");
        let old = dir.write("old.mp4", 10);
        let markers = dir.write("old.markers.json", 2);
        let poster = dir.write("old.poster.jpg", 4);
        tick_mtime();
        let keep = write_owned(&dir, "keep.mp4", 10);

        let report = enforce_quota(dir.path(), Some(10), None).unwrap();

        assert_eq!(report.deleted_clips, 1);
        assert_eq!(report.freed_bytes, 16);
        assert!(!old.exists());
        assert!(!markers.exists());
        assert!(!poster.exists());
        assert!(keep.exists());
        assert_eq!(report.status.total_bytes, 10);
    }

    #[test]
    fn enforce_quota_deletes_osu_pending_sidecar_with_clip() {
        let dir = TestDir::new("clipline-storage", "osu-pending-delete");
        let old = dir.write("old.mp4", 10);
        let pending = dir.write("old.osu-enrichment.json", 6);
        tick_mtime();
        let keep = write_owned(&dir, "keep.mp4", 10);

        let report = enforce_quota(dir.path(), Some(10), None).unwrap();

        assert_eq!(report.deleted_clips, 1);
        assert_eq!(report.freed_bytes, 16);
        assert!(!old.exists());
        assert!(!pending.exists());
        assert!(keep.exists());
        assert_eq!(report.status.total_bytes, 10);
    }

    #[test]
    fn enforce_quota_deletes_clip_metadata_sidecar_with_clip() {
        let dir = TestDir::new("clipline-storage", "clip-metadata-delete");
        let old = dir.write("old.mp4", 10);
        let metadata = dir.write("old.clipline.json", 6);
        tick_mtime();
        let keep = write_owned(&dir, "keep.mp4", 10);

        let report = enforce_quota(dir.path(), Some(10), None).unwrap();

        assert_eq!(report.deleted_clips, 1);
        assert_eq!(report.freed_bytes, 16);
        assert!(!old.exists());
        assert!(!metadata.exists());
        assert!(keep.exists());
        assert_eq!(report.status.total_bytes, 10);
    }

    #[test]
    fn enforce_quota_leaves_library_when_protected_clip_alone_exceeds_budget() {
        let dir = TestDir::new("clipline-storage", "protect-fresh");
        let old = write_owned(&dir, "old.mp4", 10);
        tick_mtime();
        let fresh = dir.write("fresh.mp4", 20);

        let report = enforce_quota(dir.path(), Some(15), Some(&fresh)).unwrap();

        assert_eq!(report.deleted_clips, 0);
        assert_eq!(report.freed_bytes, 0);
        assert!(old.exists());
        assert!(fresh.exists());
        assert_eq!(report.status.total_bytes, 30);
        assert!(report.status.is_over_quota());
    }

    #[test]
    fn enforce_quota_counts_active_recording_but_never_deletes_it() {
        let dir = TestDir::new("clipline-storage", "recording-quota");
        let old = write_owned(&dir, "old.mp4", 10);
        tick_mtime();
        let recording = dir.write("session.mp4.recording", 12);
        mark_owned(&recording);
        tick_mtime();
        let keep = write_owned(&dir, "keep.mp4", 5);

        let report = enforce_quota(dir.path(), Some(20), None).unwrap();

        assert_eq!(report.deleted_clips, 1);
        assert!(!old.exists());
        assert!(recording.exists());
        assert!(keep.exists());
        assert_eq!(report.status.clip_count, 1);
        assert_eq!(report.status.total_bytes, 17);
    }

    #[test]
    fn recover_recording_files_renames_non_empty_and_deletes_empty() {
        let dir = TestDir::new("clipline-storage", "recording-recovery");
        let recording = dir.write("2026-06-13 15-04/session_1.mp4.recording", 10);
        let empty = dir.write("empty.mp4.recording", 0);
        mark_owned(&recording);
        mark_owned(&empty);

        let report = recover_recording_files(dir.path()).unwrap();

        assert_eq!(report.deleted_empty, 1);
        assert!(!recording.exists());
        assert!(!empty.exists());
        assert_eq!(report.recovered.len(), 1);
        assert_eq!(
            report.recovered[0]
                .file_name()
                .and_then(|name| name.to_str()),
            Some("session_1.mp4")
        );
        assert!(report.recovered[0].exists());
    }

    #[test]
    fn recovery_ignores_unmarked_recording_files() {
        let dir = TestDir::new("clipline-storage", "ignore-unowned-recording");
        let unrelated = dir.write("unrelated.mp4.recording", 10);
        let owned = dir.write("2026-07-18 12-00/session_1.mp4.recording", 10);
        dir.write("2026-07-18 12-00/session_1.clipline.json", 2);

        let report = recover_recording_files(dir.path()).unwrap();

        assert!(unrelated.exists());
        assert!(!owned.exists());
        assert_eq!(report.recovered.len(), 1);
        assert_eq!(
            report.recovered[0]
                .file_name()
                .and_then(|name| name.to_str()),
            Some("session_1.mp4")
        );
    }

    #[test]
    fn recovery_adopts_unmarked_legacy_clipline_recording() {
        let dir = TestDir::new("clipline-storage", "legacy-recording-recovery");
        let recording = dir.write("2026-07-20 01-31/session_1784525638.mp4.recording", 10);

        let report = recover_recording_files(dir.path()).unwrap();

        let recovered = dir.path().join("2026-07-20 01-31/session_1784525638.mp4");
        assert!(!recording.exists());
        assert_eq!(report.recovered, vec![recovered.clone()]);
        assert!(recovered.exists());
        assert!(recovered.with_extension("clipline.json").is_file());
    }

    #[test]
    fn recovery_handles_mixed_case_recording_suffixes() {
        let dir = TestDir::new("clipline-storage", "mixed-case-recording");
        let recording = dir.write("Session.MP4.RECORDING", 10);
        dir.write("Session.clipline.json", 2);

        let report = recover_recording_files(dir.path()).unwrap();

        assert!(!recording.exists());
        assert_eq!(report.recovered, vec![dir.path().join("Session.MP4")]);
        assert!(report.recovered[0].exists());
    }

    #[test]
    fn recovery_moves_ownership_marker_to_a_unique_destination() {
        let dir = TestDir::new("clipline-storage", "recovery-marker-collision");
        let recording = dir.write("session.mp4.recording", 10);
        mark_owned(&recording);
        dir.write("session.mp4", 5);

        let report = recover_recording_files(dir.path()).unwrap();

        let recovered = dir.path().join("session_recovered.mp4");
        assert_eq!(report.recovered, vec![recovered.clone()]);
        assert!(recovered.exists());
        assert!(recovered.with_extension("clipline.json").exists());
        assert!(!dir.path().join("session.clipline.json").exists());
    }

    #[test]
    fn status_counts_clips_inside_session_folders() {
        let dir = TestDir::new("clipline-storage", "session-status");
        write_owned(&dir, "legacy.mp4", 10);
        dir.write("2026-06-12 14-30/clip.mp4", 7);
        dir.write("2026-06-12 14-30/clip.markers.json", 3);

        let status = storage_status(dir.path(), Some(100)).unwrap();

        assert_eq!(status.clip_count, 2);
        assert_eq!(status.total_bytes, 20);
    }

    #[test]
    fn enforce_quota_crosses_folders_and_removes_emptied_session_dirs() {
        let dir = TestDir::new("clipline-storage", "session-gc");
        let old = dir.write("2026-06-11 09-00/old.mp4", 10);
        let old_sidecar = dir.write("2026-06-11 09-00/old.markers.json", 2);
        let old_poster = dir.write("2026-06-11 09-00/old.poster.jpg", 4);
        let old_metadata = dir.write("2026-06-11 09-00/old.clipline.json", 0);
        tick_mtime();
        let legacy = write_owned(&dir, "legacy.mp4", 10);
        tick_mtime();
        let fresh = write_owned(&dir, "2026-06-12 14-30/fresh.mp4", 10);

        let report = enforce_quota(dir.path(), Some(20), None).unwrap();

        assert_eq!(report.deleted_clips, 1);
        assert_eq!(report.freed_bytes, 16);
        assert!(!old.exists());
        assert!(!old_sidecar.exists());
        assert!(!old_poster.exists());
        assert!(!old_metadata.exists());
        assert!(
            !old.parent().unwrap().exists(),
            "emptied session folder must be removed even with a poster sidecar"
        );
        assert!(legacy.exists());
        assert!(fresh.exists());
    }

    #[test]
    fn enforce_quota_keeps_session_dirs_that_still_hold_clips() {
        let dir = TestDir::new("clipline-storage", "session-keep");
        let old = write_owned(&dir, "2026-06-12 14-30/old.mp4", 10);
        tick_mtime();
        let new = write_owned(&dir, "2026-06-12 14-30/new.mp4", 10);

        let report = enforce_quota(dir.path(), Some(10), None).unwrap();

        assert_eq!(report.deleted_clips, 1);
        assert!(!old.exists());
        assert!(new.exists());
        assert!(new.parent().unwrap().exists());
    }

    #[test]
    fn delete_inventoried_clip_skips_sidecars_when_mp4_already_gone() {
        let dir = TestDir::new("clipline-storage", "gc-rename-race");
        let old = write_owned(&dir, "old.mp4", 40);
        let old_meta = dir.path().join("old.clipline.json");
        let old_markers = dir.write("old.markers.json", 3);
        let clip = ClipFile {
            path: old.clone(),
            sidecars: vec![old_meta.clone(), old_markers.clone()],
            mp4_bytes: 40,
            sidecar_bytes: 3,
            modified: UNIX_EPOCH,
            recording: false,
        };

        // Concurrent rename already moved the MP4; inventoried sidecar paths
        // must be left alone so the renamer can finish moving them.
        std::fs::rename(&old, dir.path().join("renamed.mp4")).unwrap();

        let outcome = delete_inventoried_clip(&clip, dir.path()).unwrap();

        assert_eq!(outcome, DeletedClip::AlreadyGone);
        assert!(old_meta.exists());
        assert!(old_markers.exists());
    }

    #[test]
    fn deleted_clip_reports_session_cleanup_error_without_becoming_a_failure() {
        let dir = TestDir::new("clipline-storage", "gc-cleanup-error");
        let clip_path = write_owned(&dir, "2026-08-30 01-00/old.mp4", 40);
        let marker = clip_ownership_marker_path(&clip_path).unwrap();
        let clip = ClipFile {
            path: clip_path.clone(),
            sidecars: vec![marker],
            mp4_bytes: 40,
            sidecar_bytes: 2,
            modified: UNIX_EPOCH,
            recording: false,
        };

        let outcome = delete_inventoried_clip_with(&clip, dir.path(), |_, _| {
            Err(io::Error::other("simulated cleanup failure"))
        })
        .unwrap();

        let DeletedClip::Removed { cleanup_error } = outcome else {
            panic!("the MP4 deletion must remain successful");
        };
        assert!(!clip_path.exists());
        assert!(cleanup_error
            .as_deref()
            .is_some_and(|error| error.contains("simulated cleanup failure")));
    }

    #[test]
    fn enforce_quota_removes_session_metadata_with_emptied_folder() {
        let dir = TestDir::new("clipline-storage", "session-meta-gc");
        let old = dir.write("2026-06-11 09-00/old.mp4", 30);
        let _ = dir.write("2026-06-11 09-00/old.clipline.json", 0);
        let session_meta = dir.write("2026-06-11 09-00/clipline-session.json", 12);
        tick_mtime();
        let keep = write_owned(&dir, "keep.mp4", 10);

        let report = enforce_quota(dir.path(), Some(20), None).unwrap();

        assert_eq!(report.deleted_clips, 1);
        assert!(!old.exists());
        assert!(!session_meta.exists());
        assert!(
            !old.parent().unwrap().exists(),
            "emptied session folder must disappear with its session metadata"
        );
        assert!(keep.exists());
    }

    #[test]
    fn enforce_quota_removes_orphaned_sidecars_with_emptied_folder() {
        let dir = TestDir::new("clipline-storage", "session-orphan-sidecar-gc");
        let old = write_owned(&dir, "2026-06-11 09-00/old.mp4", 30);
        let orphan = dir.write("2026-06-11 09-00/gone.poster.jpg", 7);
        let session_meta = dir.write("2026-06-11 09-00/clipline-session.json", 12);
        tick_mtime();
        let keep = write_owned(&dir, "keep.mp4", 10);

        let report = enforce_quota(dir.path(), Some(20), None).unwrap();

        assert_eq!(report.deleted_clips, 1);
        assert!(!old.exists());
        assert!(!orphan.exists());
        assert!(!session_meta.exists());
        assert!(
            !old.parent().unwrap().exists(),
            "orphaned leftover metadata must not keep the emptied session folder"
        );
        assert!(keep.exists());
    }

    #[test]
    fn delete_all_managed_media_removes_owned_clips_recordings_and_sidecars_only() {
        let dir = TestDir::new("clipline-storage", "delete-all-managed");
        let saved = write_owned(&dir, "saved.mp4", 10);
        let saved_markers = dir.write("saved.markers.json", 2);
        let saved_osu = dir.write("saved.osu-enrichment.json", 3);
        let saved_poster = dir.write("saved.poster.jpg", 4);
        let recording = dir.write("2026-08-16 12-00/active.mp4.recording", 20);
        mark_owned(&recording);
        let recording_markers = dir.write("2026-08-16 12-00/active.markers.json", 2);
        let recording_osu = dir.write("2026-08-16 12-00/active.osu-enrichment.json", 3);
        let recording_poster = dir.write("2026-08-16 12-00/active.poster.jpg", 4);
        let session = dir.write("2026-08-16 12-00/clipline-session.json", 5);
        let legacy = dir.write("clip_1786900000.mp4", 7);
        let legacy_recording = dir.write("session_1786900001_1.mp4.recording", 8);
        let foreign = dir.write("foreign.mp4", 30);
        let foreign_poster = dir.write("foreign.poster.jpg", 6);

        delete_all_managed_media(dir.path()).unwrap();

        for removed in [
            saved,
            saved_markers,
            saved_osu,
            saved_poster,
            recording,
            recording_markers,
            recording_osu,
            recording_poster,
            session,
            legacy,
            legacy_recording,
        ] {
            assert!(
                !removed.exists(),
                "managed file was left behind: {removed:?}"
            );
        }
        assert!(foreign.exists(), "unmarked MP4s are user files");
        assert!(
            foreign_poster.exists(),
            "a poster alone does not prove Clipline ownership"
        );
        assert!(dir.path().exists(), "the media root belongs to the caller");
    }

    #[test]
    fn delete_all_managed_media_does_not_follow_linked_session_directories() {
        let root = TestDir::new("clipline-storage", "delete-all-symlink-root");
        let outside = TestDir::new("clipline-storage", "delete-all-symlink-outside");
        let external = write_owned(&outside, "external.mp4", 90);
        let link = root.path().join("linked-session");
        let linked = {
            #[cfg(windows)]
            {
                std::os::windows::fs::symlink_dir(outside.path(), &link)
            }
            #[cfg(unix)]
            {
                std::os::unix::fs::symlink(outside.path(), &link)
            }
        };
        if let Err(error) = linked {
            eprintln!("skipping symlink containment test: {error}");
            return;
        }

        delete_all_managed_media(root.path()).unwrap();

        assert!(external.exists());
    }

    #[cfg(unix)]
    #[test]
    fn delete_all_managed_media_continues_past_an_unreadable_session_directory() {
        use std::os::unix::fs::PermissionsExt;

        let root = TestDir::new("clipline-storage", "delete-all-unreadable-session");
        let owned = write_owned(&root, "owned.mp4", 10);
        let unreadable = root.path().join("unreadable-session");
        fs::create_dir_all(&unreadable).unwrap();
        fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o000)).unwrap();

        let result = delete_all_managed_media(root.path());

        fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(result.is_err(), "the partial failure remains observable");
        assert!(
            !owned.exists(),
            "accessible managed clips are still deleted"
        );
    }

    #[test]
    fn enforce_quota_ignores_symlinked_child_directories() {
        let root = TestDir::new("clipline-storage", "symlink-root");
        let outside = TestDir::new("clipline-storage", "symlink-outside");
        let external = write_owned(&outside, "external.mp4", 90);
        let link = root.path().join("linked-session");
        let linked = {
            #[cfg(windows)]
            {
                std::os::windows::fs::symlink_dir(outside.path(), &link)
            }
            #[cfg(unix)]
            {
                std::os::unix::fs::symlink(outside.path(), &link)
            }
        };
        if let Err(error) = linked {
            eprintln!("skipping symlink containment test: {error}");
            return;
        }
        let keep = write_owned(&root, "keep.mp4", 10);

        let report = enforce_quota(root.path(), Some(20), None).unwrap();

        assert_eq!(report.deleted_clips, 0);
        assert_eq!(report.status.total_bytes, 10);
        assert!(
            external.exists(),
            "quota GC must not delete managed clips through a linked child directory"
        );
        assert!(keep.exists());
    }

    #[test]
    fn enforce_quota_skips_clips_outside_canonical_media_root() {
        let root = TestDir::new("clipline-storage", "containment-root");
        let outside = TestDir::new("clipline-storage", "containment-outside");
        let external = write_owned(&outside, "external.mp4", 40);
        let clip = ClipFile {
            path: external.clone(),
            sidecars: Vec::new(),
            mp4_bytes: 40,
            sidecar_bytes: 0,
            modified: UNIX_EPOCH,
            recording: false,
        };

        let outcome = delete_inventoried_clip(&clip, root.path()).unwrap();

        assert_eq!(outcome, DeletedClip::Skipped);
        assert!(external.exists());
    }

    #[test]
    fn disabled_quota_does_not_delete() {
        let dir = TestDir::new("clipline-storage", "disabled");
        let clip = write_owned(&dir, "clip.mp4", 10);

        let report = enforce_quota(dir.path(), None, None).unwrap();

        assert_eq!(report.deleted_clips, 0);
        assert_eq!(report.freed_bytes, 0);
        assert!(clip.exists());
        assert_eq!(report.status.total_bytes, 10);
    }
}
