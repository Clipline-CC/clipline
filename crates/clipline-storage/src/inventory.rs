//! Clip inventory, deletion, and storage accounting.

use std::fs;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::empty_sessions::remove_emptied_session_dir_after_clip;
use crate::files::{
    clip_sidecars, is_mp4, is_recording_mp4, remove_file_if_exists, same_path, visit_media_dirs,
};
use crate::ownership::is_managed_clip;

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

pub fn storage_status(dir: &Path, quota_bytes: Option<u64>) -> io::Result<StorageStatus> {
    let clips = inventory(dir, None)?;
    Ok(status_from_clips(&clips, quota_bytes))
}

#[derive(Debug, Clone)]
pub(crate) struct ClipFile {
    pub(crate) path: PathBuf,
    /// Files that live and die with the clip: markers, clip metadata, pending
    /// osu! enrichment, and the cached poster frame. Each is removed alongside
    /// the clip during quota GC so a leftover never keeps an emptied session
    /// folder alive.
    pub(crate) sidecars: Vec<PathBuf>,
    pub(crate) mp4_bytes: u64,
    pub(crate) sidecar_bytes: u64,
    pub(crate) modified: SystemTime,
    pub(crate) recording: bool,
}

impl ClipFile {
    pub(crate) fn total_bytes(&self) -> u64 {
        self.mp4_bytes + self.sidecar_bytes
    }

    pub(crate) fn can_delete(&self, protect: Option<&Path>, additionally_protected: bool) -> bool {
        !self.recording
            && !protect.is_some_and(|protected| same_path(&self.path, protected))
            && !additionally_protected
    }
}

/// Clips live at the root (legacy) or one level down in session folders.
pub(crate) fn inventory(dir: &Path, include: Option<&Path>) -> io::Result<Vec<ClipFile>> {
    let mut clips = Vec::new();
    visit_media_dirs(dir, |media_dir| {
        collect_clips(media_dir, include, &mut clips)
    })?;
    Ok(clips)
}

pub(crate) fn collect_clips(dir: &Path, include: Option<&Path>, clips: &mut Vec<ClipFile>) -> io::Result<()> {
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

pub(crate) fn status_from_clips(clips: &[ClipFile], quota_bytes: Option<u64>) -> StorageStatus {
    StorageStatus {
        clip_count: clips.iter().filter(|clip| !clip.recording).count(),
        total_bytes: clips.iter().map(ClipFile::total_bytes).sum(),
        quota_bytes,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DeletedClip {
    Removed {
        cleanup_error: Option<String>,
    },
    /// Inventoried MP4 vanished before we could delete it.
    AlreadyGone,
    /// Still present but protected (upload lease / active mutation).
    Skipped,
}

pub(crate) fn delete_inventoried_clip(clip: &ClipFile, media_root: &Path) -> io::Result<DeletedClip> {
    delete_inventoried_clip_with(clip, media_root, remove_emptied_session_dir_after_clip)
}

pub(crate) fn delete_inventoried_clip_with(
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

pub(crate) fn is_within_media_root(path: &Path, media_root: &Path) -> bool {
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

#[cfg(test)]
mod tests {
    use super::*;
    use clipline_test_utils::TestDir;
    use crate::ownership::clip_ownership_marker_path;

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

fn mark_owned(path: &Path) {
    std::fs::write(clip_ownership_marker_path(path).unwrap(), b"").unwrap();
}

fn write_owned(dir: &TestDir, relative: &str, bytes: usize) -> PathBuf {
    let path = dir.write(relative, bytes);
    mark_owned(&path);
    path
}
}
