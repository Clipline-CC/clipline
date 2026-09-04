//! Crash recovery and full media wipe.

use std::fs;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::empty_sessions::sweep_emptied_session_dirs;
use crate::files::{clip_sidecars, is_link_or_reparse_point, is_recording_mp4, recording_final_path, remove_file_if_exists, visit_media_dirs};
use crate::inventory::{collect_clips, delete_inventoried_clip};
use crate::ownership::{
    clip_ownership_marker_path, ensure_clip_owned, is_managed_clip, remove_clip_ownership_marker,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordingRecoveryReport {
    pub recovered: Vec<PathBuf>,
    pub deleted_empty: usize,
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

pub(crate) fn unique_recovered_path(candidate: &Path, current_marker: &Path) -> PathBuf {
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

pub(crate) fn recovery_destination_available(path: &Path, current_marker: &Path) -> bool {
    !path.exists()
        && clip_ownership_marker_path(path)
            .is_ok_and(|marker| marker == current_marker || !marker.exists())
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
}
