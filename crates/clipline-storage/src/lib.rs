//! Filesystem storage management for saved clips.

mod sessions;

pub use sessions::{session_label, SessionTracker};

use std::fs;
use std::io::{self, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

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
    Ok(clip.with_extension("clipline.json"))
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

#[derive(Debug, Clone)]
struct ClipFile {
    mp4_bytes: u64,
    sidecar_bytes: u64,
    recording: bool,
}

impl ClipFile {
    fn total_bytes(&self) -> u64 {
        self.mp4_bytes + self.sidecar_bytes
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
        let (_, sidecar_bytes) = if recording {
            (Vec::new(), 0)
        } else {
            clip_sidecars(&path)?
        };
        clips.push(ClipFile {
            mp4_bytes: meta.len(),
            sidecar_bytes,
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
    path.with_extension("markers.json").is_file()
        || path.with_extension("osu-enrichment.json").is_file()
        || is_legacy_generated_clip(path)
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

fn sidecar_path(path: &Path) -> PathBuf {
    path.with_extension("markers.json")
}

fn clip_metadata_path(path: &Path) -> PathBuf {
    path.with_extension("clipline.json")
}

fn poster_path(path: &Path) -> PathBuf {
    path.with_extension("poster.jpg")
}

fn osu_pending_path(path: &Path) -> PathBuf {
    path.with_extension("osu-enrichment.json")
}

/// The sidecar files present beside a clip (markers, clip metadata, pending osu!
/// enrichment, and cached poster) and their combined size. A zero-byte sidecar
/// that exists is still tracked so it gets cleaned up with the clip.
fn clip_sidecars(clip: &Path) -> io::Result<(Vec<PathBuf>, u64)> {
    let mut sidecars = Vec::new();
    let mut bytes = 0u64;
    for candidate in [
        sidecar_path(clip),
        clip_metadata_path(clip),
        osu_pending_path(clip),
        poster_path(clip),
    ] {
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

fn remove_file_if_exists(path: &Path) -> io::Result<()> {
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
        if entry.metadata()?.is_dir() {
            f(&entry.path())?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clipline_test_utils::TestDir;

    fn mark_owned(path: &Path) {
        std::fs::write(clip_ownership_marker_path(path).unwrap(), b"").unwrap();
    }

    fn write_owned(dir: &TestDir, relative: &str, bytes: usize) -> PathBuf {
        let path = dir.write(relative, bytes);
        mark_owned(&path);
        path
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
}
