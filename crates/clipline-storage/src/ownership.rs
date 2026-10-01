//! Clip ownership markers and locked session reservations.
use std::fs;
use std::io::{self, ErrorKind, Write};
use std::path::{Path, PathBuf};

use crate::files::{
    CLIP_OWNERSHIP_MARKER_SUFFIX, OSU_ENRICHMENT_SUFFIX, clip_sidecar_path,
    is_mp4, is_recording_mp4, recording_final_path,
};
use crate::files::remove_file_if_exists;
use crate::lock_session_mutations;
pub(crate) const SESSION_META_FILE: &str = "clipline-session.json";

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
            let metadata = fs::symlink_metadata(&marker)?;
            if metadata.is_file() && !crate::files::is_link_or_reparse_point(&metadata) {
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
    create_session_directory(parent)?;
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
    create_session_directory(parent)?;
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
    validate_session_directory(session_dir)?;
    let path = session_dir.join(SESSION_META_FILE);
    match fs::symlink_metadata(&path) {
        Ok(metadata) if !metadata.is_file() || crate::files::is_link_or_reparse_point(&metadata) => {
            return Err(io::Error::new(ErrorKind::PermissionDenied, "refusing linked session metadata"));
        }
        Ok(_) if !replace_existing => return Ok(false),
        Ok(_) => {},
        Err(error) if error.kind() == ErrorKind::NotFound => {},
        Err(error) => return Err(error),
    }
    let mut file = fs::OpenOptions::new().write(true).create_new(!replace_existing)
        .create(replace_existing).truncate(replace_existing).open(path)?;
    file.write_all(bytes)?;
    Ok(true)
}

fn validate_session_directory(dir: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(dir)?;
    if !metadata.is_dir() || crate::files::is_link_or_reparse_point(&metadata) {
        return Err(io::Error::new(ErrorKind::PermissionDenied, "refusing linked session directory"));
    }
    Ok(())
}

fn create_session_directory(dir: &Path) -> io::Result<()> {
    match fs::create_dir(dir) {
        Ok(()) => {},
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {},
        Err(error) => return Err(error),
    }
    validate_session_directory(dir)
}

pub fn remove_clip_ownership_marker(path: &Path) -> io::Result<()> {
    remove_file_if_exists(&clip_ownership_marker_path(path)?)
}

pub(crate) fn is_managed_clip(path: &Path) -> bool {
    let Ok(marker) = clip_ownership_marker_path(path) else {
        return false;
    };
    let is_regular_proof = |proof: &Path| fs::symlink_metadata(proof)
        .is_ok_and(|metadata| metadata.is_file() && !crate::files::is_link_or_reparse_point(&metadata));
    if is_regular_proof(&marker) {
        return true;
    }
    // New recordings are identified by their ownership marker. Pre-marker
    // releases can be adopted only through Clipline's generated filename.
    if is_recording_mp4(path) {
        return is_legacy_generated_clip(path);
    }
    // Review can create posters and audio-selection markers for imported MP4s.
    // Neither proves ownership; explicit edits create the ownership marker.
    is_regular_proof(&clip_sidecar_path(path, OSU_ENRICHMENT_SUFFIX))
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

#[cfg(test)]
mod tests {
    use super::*;
    use clipline_test_utils::TestDir;
    use std::sync::mpsc;
    use std::time::Duration;

#[test]
fn security_scan_session_writes_refuse_linked_directories() {
    let root = TestDir::new("clipline-storage", "session-link-root");
    let outside = TestDir::new("clipline-storage", "session-link-target");
    let sentinel = outside.write(SESSION_META_FILE, 8);
    let original = fs::read(&sentinel).unwrap();
    let link = root.path().join("session");
    #[cfg(windows)]
    let status = std::process::Command::new("cmd").args(["/c", "mklink", "/J"])
        .arg(&link).arg(outside.path()).output().unwrap();
    #[cfg(windows)]
    assert!(status.status.success(), "{}", String::from_utf8_lossy(&status.stderr));
    #[cfg(unix)]
    std::os::unix::fs::symlink(outside.path(), &link).unwrap();
    assert!(ensure_session_clip_owned(&link.join("clip_1.mp4")).is_err());
    assert!(reserve_session_recording_file(&link.join("session_1.mp4.recording")).is_err());
    assert!(write_session_metadata(&link, b"replacement", true).is_err());
    assert_eq!(fs::read(sentinel).unwrap(), original);
    assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 1);
    fs::remove_dir(&link).or_else(|_| fs::remove_file(&link)).unwrap();
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
}
