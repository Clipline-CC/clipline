//! Remove session folders that no longer hold any real media.
//!
//! Protocol: classify the folder, delete leftover husks, re-check (a
//! concurrent save can land media after the first inventory), read the session
//! metadata into memory, `remove_dir`, and restore the file from memory only
//! when removal fails *and* the file is still absent. Nothing is renamed to a
//! sibling, so no cleanup pass can be wedged by stale debris, and a fresh
//! `clipline-session.json` written mid-cleanup is never clobbered.

use std::fs;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};

use crate::sessions::is_session_dir_name;
use crate::{
    is_link_or_reparse_point, remove_file_if_exists, CLIP_OWNERSHIP_MARKER_SUFFIX,
    CLIP_SIDECAR_SUFFIXES, SESSION_META_FILE,
};

/// Remove a session folder when it is a direct child of `media_root`, uses a
/// recorder session name, and no longer holds anything except Clipline leftover
/// metadata (or is empty).
///
/// Videos, in-progress recordings, in-progress ownership markers, screenshots,
/// nested directories, temps, and any unrecognized file keep the folder. The
/// media root itself is never removed.
pub fn remove_emptied_session_dir(session_dir: &Path, media_root: &Path) -> io::Result<bool> {
    let _guard = crate::lock_session_mutations();
    remove_emptied_session_dir_with(session_dir, media_root, |dir| {
        fs::remove_dir(dir).map(|_| ())
    })
}

/// Same protocol with an injected `remove_dir`, so tests can simulate
/// mid-cleanup races (a save landing between inventory and removal).
pub(crate) fn remove_emptied_session_dir_with(
    session_dir: &Path,
    media_root: &Path,
    remove_dir: impl Fn(&Path) -> io::Result<()>,
) -> io::Result<bool> {
    let Some(name) = session_dir.file_name().and_then(|name| name.to_str()) else {
        return Ok(false);
    };
    if !is_session_dir_name(name) {
        return Ok(false);
    }
    if !is_direct_session_child(session_dir, media_root)? {
        return Ok(false);
    }

    let Some(leftovers) = collect_disposable_leftovers(session_dir)? else {
        return Ok(false);
    };

    for leftover in leftovers.iter().filter(|leftover| {
        leftover.file_name().and_then(|name| name.to_str()) != Some(SESSION_META_FILE)
    }) {
        let _ = remove_file_if_exists(leftover);
    }

    // A concurrent save can land media after the first inventory. Re-check
    // before touching session metadata so game attribution survives.
    if collect_disposable_leftovers(session_dir)?.is_none() {
        return Ok(false);
    }

    let session_meta = session_dir.join(SESSION_META_FILE);
    let meta_bytes = read_session_meta(&session_meta)?;
    if meta_bytes.is_some() {
        // The folder must be empty for remove_dir; the bytes are already in
        // memory and go back only if removal fails (see below).
        remove_file_if_exists(&session_meta)?;
    }
    match remove_dir(session_dir) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(true),
        Err(_) => {
            // The folder survived (a save landed mid-cleanup); put the
            // metadata back unless a fresh one replaced it.
            if let Some(bytes) = meta_bytes.filter(|_| !session_meta.exists()) {
                let _ = fs::write(&session_meta, bytes);
            }
            Ok(false)
        }
    }
}

/// Remove every emptied session folder directly under `media_root`.
pub fn sweep_emptied_session_dirs(media_root: &Path) -> io::Result<usize> {
    let metadata = match fs::symlink_metadata(media_root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    };
    if !metadata.is_dir() || is_link_or_reparse_point(&metadata) {
        return Ok(0);
    }

    let mut removed = 0usize;
    for entry in fs::read_dir(media_root)? {
        let path = match entry {
            Ok(entry) => entry.path(),
            Err(_) => continue,
        };
        if remove_emptied_session_dir(&path, media_root).unwrap_or(false) {
            removed += 1;
        }
    }
    Ok(removed)
}

fn is_direct_session_child(session_dir: &Path, media_root: &Path) -> io::Result<bool> {
    let metadata = match fs::symlink_metadata(session_dir) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    if !metadata.is_dir() || is_link_or_reparse_point(&metadata) {
        return Ok(false);
    }
    let Ok(root) = media_root.canonicalize() else {
        return Ok(false);
    };
    let Ok(dir) = session_dir.canonicalize() else {
        return Ok(false);
    };
    Ok(dir.parent().is_some_and(|parent| parent == root))
}

fn collect_disposable_leftovers(session_dir: &Path) -> io::Result<Option<Vec<PathBuf>>> {
    let mut leftovers = Vec::new();
    for entry in fs::read_dir(session_dir)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if metadata.is_dir()
            || is_link_or_reparse_point(&metadata)
            || is_in_progress_ownership_marker(session_dir, &path)
            || !is_disposable_session_leftover(&path)
        {
            return Ok(None);
        }
        leftovers.push(path);
    }
    Ok(Some(leftovers))
}

/// `ensure_clip_owned` writes `clip.clipline.json` before the MP4 exists.
/// Treat that marker as live work, not a husk, so cleanup cannot drop it.
fn is_in_progress_ownership_marker(session_dir: &Path, path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let Some(stem) = strip_ascii_suffix(name, CLIP_OWNERSHIP_MARKER_SUFFIX) else {
        return false;
    };
    let mp4 = session_dir.join(format!("{stem}.mp4"));
    let recording = session_dir.join(format!("{stem}.mp4.recording"));
    !is_regular_file(&mp4) && !is_regular_file(&recording)
}

fn is_regular_file(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && !is_link_or_reparse_point(&metadata))
}

fn is_disposable_session_leftover(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    name.eq_ignore_ascii_case(SESSION_META_FILE) || is_clip_sidecar_filename(name)
}

/// The session metadata bytes, when it is a regular file. Absent, directories,
/// links, and reparse points all read as `None` — nothing here may follow a
/// link out of the media tree.
fn read_session_meta(session_meta: &Path) -> io::Result<Option<Vec<u8>>> {
    match fs::symlink_metadata(session_meta) {
        Ok(metadata) if metadata.is_file() && !is_link_or_reparse_point(&metadata) => {
            Ok(Some(fs::read(session_meta)?))
        }
        Ok(_) => Ok(None),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn is_clip_sidecar_filename(name: &str) -> bool {
    CLIP_SIDECAR_SUFFIXES
        .iter()
        .any(|suffix| strip_ascii_suffix(name, suffix).is_some())
}

fn strip_ascii_suffix<'a>(name: &'a str, suffix: &str) -> Option<&'a str> {
    let start = name.len().checked_sub(suffix.len())?;
    if start > 0 && name.get(start..)?.eq_ignore_ascii_case(suffix) {
        name.get(..start)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clip_ownership_marker_path;
    use clipline_test_utils::TestDir;
    use std::sync::mpsc;

    fn mark_owned(path: &Path) {
        std::fs::write(clip_ownership_marker_path(path).unwrap(), b"").unwrap();
    }

    fn write_owned(dir: &TestDir, relative: &str, bytes: usize) -> PathBuf {
        let path = dir.write(relative, bytes);
        mark_owned(&path);
        path
    }

    #[test]
    fn removes_metadata_only_folders() {
        let dir = TestDir::new("clipline-storage", "empty-session-metadata");
        let session = dir
            .write("2026-06-12 19-15/clipline-session.json", 12)
            .parent()
            .unwrap()
            .to_path_buf();
        dir.write("2026-06-12 19-15/clip_1781306146.poster.jpg", 4);
        dir.write("2026-06-12 19-15/clip_1781306146.markers.json", 6);
        let keep = write_owned(&dir, "2026-06-12 19-16/keep.mp4", 10);
        let screenshots = dir.write("Screenshots/shot.png", 20);
        let screenshot_poster = dir.write("Screenshots/shot.poster.jpg", 3);
        let notes = dir.write("2026-06-12 19-17/notes.txt", 5);

        assert!(remove_emptied_session_dir(&session, dir.path()).unwrap());
        assert!(!session.exists());
        assert!(keep.parent().unwrap().exists());
        assert!(screenshots.exists());
        assert!(screenshot_poster.exists());
        assert!(notes.exists());
        assert!(notes.parent().unwrap().exists());
        assert!(dir.path().exists());
    }

    #[test]
    fn failed_removal_restores_metadata_absent_at_removal_time() {
        let dir = TestDir::new("clipline-storage", "empty-session-race-absent");
        let session = dir.path().join("2026-06-12 19-15");
        std::fs::create_dir_all(&session).unwrap();
        std::fs::write(session.join("clip_1781306146.poster.jpg"), b"husk").unwrap();
        std::fs::write(session.join("clipline-session.json"), b"attribution").unwrap();

        // remove_dir fails as if a save landed mid-cleanup (the file is only
        // discovered by the kernel, so the pre-checks still passed).
        let result = remove_emptied_session_dir_with(&session, dir.path(), |_| {
            Err(io::Error::other("simulated mid-cleanup save"))
        })
        .unwrap();

        assert!(!result, "a failed removal is not a removal");
        assert!(session.exists());
        let restored = std::fs::read_to_string(session.join("clipline-session.json")).unwrap();
        assert_eq!(
            restored, "attribution",
            "session metadata must be written back so game attribution survives"
        );
    }

    #[test]
    fn failed_removal_never_overwrites_fresh_metadata() {
        let dir = TestDir::new("clipline-storage", "empty-session-race-fresh");
        let session = dir.path().join("2026-06-12 19-15");
        std::fs::create_dir_all(&session).unwrap();
        std::fs::write(session.join("clip_1781306146.poster.jpg"), b"husk").unwrap();
        std::fs::write(session.join("clipline-session.json"), b"stale").unwrap();

        // The recorder rewrites clipline-session.json after cleanup read the
        // stale bytes but before remove_dir fails. The stale copy is dropped.
        let result = remove_emptied_session_dir_with(&session, dir.path(), |_| {
            std::fs::write(session.join("clipline-session.json"), b"fresh").unwrap();
            Err(io::Error::other("simulated mid-cleanup save"))
        })
        .unwrap();

        assert!(!result);
        assert!(session.exists());
        let restored = std::fs::read_to_string(session.join("clipline-session.json")).unwrap();
        assert_eq!(
            restored, "fresh",
            "a freshly written clipline-session.json must never be clobbered"
        );
    }

    #[test]
    fn cleanup_holds_the_session_mutation_lock() {
        let dir = TestDir::new("clipline-storage", "empty-session-lock");
        let session = dir
            .write("2026-06-12 19-15/clipline-session.json", 12)
            .parent()
            .unwrap()
            .to_path_buf();
        let root = dir.path().to_path_buf();
        let guard = crate::lock_session_mutations();
        let (started_tx, started_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();
        let cleanup = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            result_tx
                .send(remove_emptied_session_dir(&session, &root))
                .unwrap();
        });

        started_rx.recv().unwrap();
        assert!(
            result_rx
                .recv_timeout(std::time::Duration::from_millis(100))
                .is_err(),
            "cleanup must wait for an active session metadata mutation"
        );
        drop(guard);
        assert!(result_rx.recv().unwrap().unwrap());
        cleanup.join().unwrap();
    }

    #[test]
    fn media_landing_after_inventory_aborts_cleanup() {
        let dir = TestDir::new("clipline-storage", "empty-session-race-recheck");
        let session = dir.path().join("2026-06-12 19-15");
        std::fs::create_dir_all(&session).unwrap();
        std::fs::write(session.join("clip_1781306146.poster.jpg"), b"husk").unwrap();
        std::fs::write(session.join("clipline-session.json"), b"attribution").unwrap();

        // The save lands between the first and second inventory, so the
        // re-check sees real media and cleanup aborts before removing
        // anything.
        let result = remove_emptied_session_dir_with(&session, dir.path(), |dir_path| {
            std::fs::write(dir_path.join("clip_1781306146.mp4"), b"new media").unwrap();
            mark_owned(&dir_path.join("clip_1781306146.mp4"));
            fs::remove_dir(dir_path)
        })
        .unwrap();

        assert!(!result);
        assert!(session.exists());
        assert!(session.join("clip_1781306146.mp4").exists());
        assert_eq!(
            std::fs::read_to_string(session.join("clipline-session.json")).unwrap(),
            "attribution"
        );
    }

    #[test]
    fn successful_removal_leaves_no_sibling_debris() {
        let dir = TestDir::new("clipline-storage", "empty-session-clean");
        let session = dir
            .write("2026-06-12 19-15/clipline-session.json", 12)
            .parent()
            .unwrap()
            .to_path_buf();
        dir.write("2026-06-12 19-15/clip_1781306146.poster.jpg", 4);

        assert!(remove_emptied_session_dir(&session, dir.path()).unwrap());
        assert!(!session.exists());
        let siblings: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            siblings,
            Vec::<String>::new(),
            "cleanup must not leave staging debris beside the session folder"
        );
    }

    #[test]
    fn keeps_in_progress_temp_and_recordings() {
        let dir = TestDir::new("clipline-storage", "empty-session-keep-media");
        let recording_session = dir
            .write("2026-06-12 19-15/session.mp4.recording", 8)
            .parent()
            .unwrap()
            .to_path_buf();
        dir.write("2026-06-12 19-15/clipline-session.json", 4);
        let tmp_session = dir
            .write("2026-06-12 19-16/clip_trim_pending_001.mp4.tmp", 8)
            .parent()
            .unwrap()
            .to_path_buf();

        assert!(!remove_emptied_session_dir(&recording_session, dir.path()).unwrap());
        assert!(recording_session.exists());
        assert!(!remove_emptied_session_dir(&tmp_session, dir.path()).unwrap());
        assert!(tmp_session.exists());
    }

    #[test]
    fn keeps_ownership_marker_without_mp4() {
        let dir = TestDir::new("clipline-storage", "empty-session-in-progress-marker");
        let session = dir
            .write("2026-06-12 19-15/clip.clipline.json", 8)
            .parent()
            .unwrap()
            .to_path_buf();
        dir.write("2026-06-12 19-15/clipline-session.json", 4);
        dir.write("2026-06-12 19-15/clip.poster.jpg", 3);

        assert!(!remove_emptied_session_dir(&session, dir.path()).unwrap());
        assert!(session.exists());
        assert!(session.join("clip.clipline.json").exists());
        assert!(session.join("clipline-session.json").exists());
        assert!(session.join("clip.poster.jpg").exists());
    }

    #[test]
    fn keeps_unrecognized_debug_sidecars() {
        let dir = TestDir::new("clipline-storage", "empty-session-unrecognized");
        let session = dir
            .write(
                "2026-06-12 19-15/session.markers.before-player-summary.json",
                6,
            )
            .parent()
            .unwrap()
            .to_path_buf();
        dir.write("2026-06-12 19-15/clipline-session.json", 4);

        assert!(!remove_emptied_session_dir(&session, dir.path()).unwrap());
        assert!(session.exists());
        assert!(session.join("clipline-session.json").exists());
    }

    #[test]
    fn unicode_filename_cannot_panic_suffix_classification() {
        let dir = TestDir::new("clipline-storage", "empty-session-unicode");
        let session = dir
            .write("2026-06-12 19-15/éaaaaaaaaaa", 1)
            .parent()
            .unwrap()
            .to_path_buf();

        assert!(!remove_emptied_session_dir(&session, dir.path()).unwrap());
        assert!(session.exists());
    }

    #[test]
    fn keeps_date_only_folders() {
        let dir = TestDir::new("clipline-storage", "empty-session-date-only");
        let leftover = dir
            .write("2026-08-16/clipline-session.json", 5)
            .parent()
            .unwrap()
            .to_path_buf();

        assert!(!remove_emptied_session_dir(&leftover, dir.path()).unwrap());
        assert!(leftover.exists());
        assert!(leftover.join("clipline-session.json").exists());
    }

    #[test]
    fn never_deletes_the_media_root() {
        let dir = TestDir::new("clipline-storage", "empty-session-root");
        dir.write("orphan.clipline.json", 4);
        dir.write("clipline-session.json", 4);

        assert!(!remove_emptied_session_dir(dir.path(), dir.path()).unwrap());
        assert!(dir.path().exists());
        assert!(dir.path().join("orphan.clipline.json").exists());
    }

    #[test]
    fn sweep_cleans_orphans_and_keeps_real_media() {
        let dir = TestDir::new("clipline-storage", "sweep-empty-sessions");
        let empty = dir.path().join("2026-06-13 02-31");
        fs::create_dir_all(&empty).unwrap();
        let leftover = dir
            .write("2026-06-12 19-15/clipline-session.json", 12)
            .parent()
            .unwrap()
            .to_path_buf();
        let keep = write_owned(&dir, "2026-06-12 19-16/keep.mp4", 10);
        dir.write("2026-06-12 19-16/clipline-session.json", 4);
        let screenshots = dir.write("Screenshots/shot.png", 20);
        let empty_screenshots = dir.path().join("EmptyScreenshots");
        fs::create_dir_all(&empty_screenshots).unwrap();
        let custom_leftover = dir.write("Exports/old.poster.jpg", 3);

        let removed = sweep_emptied_session_dirs(dir.path()).unwrap();

        assert_eq!(removed, 2);
        assert!(!empty.exists());
        assert!(!leftover.exists());
        assert!(keep.exists());
        assert!(keep.parent().unwrap().exists());
        assert!(screenshots.exists());
        assert!(
            empty_screenshots.exists(),
            "empty non-session folders must not be swept"
        );
        assert!(
            custom_leftover.exists(),
            "leftover sidecars in a non-session folder must not be swept"
        );
    }

    #[test]
    fn keeps_session_meta_when_folder_still_has_files() {
        let dir = TestDir::new("clipline-storage", "empty-session-keep-meta");
        let session = dir
            .write("2026-06-12 19-15/clipline-session.json", 12)
            .parent()
            .unwrap()
            .to_path_buf();
        let blocked = session.join("keep.txt");
        std::fs::write(&blocked, b"user file").unwrap();

        assert!(!remove_emptied_session_dir(&session, dir.path()).unwrap());
        assert!(session.exists());
        assert!(blocked.exists());
        assert!(
            session.join("clipline-session.json").exists(),
            "session metadata must survive when the folder cannot be removed"
        );
    }

    // Guard against the removed staging protocol returning: its sibling file
    // and any restore-rename must stay gone from this module.
    #[test]
    fn staging_protocol_is_gone() {
        let dir = TestDir::new("clipline-storage", "empty-session-no-staging");
        let session = dir
            .write("2026-06-12 19-15/clipline-session.json", 12)
            .parent()
            .unwrap()
            .to_path_buf();

        let (tx, rx) = mpsc::channel();
        let result = remove_emptied_session_dir_with(&session, dir.path(), move |dir_path| {
            tx.send(dir_path.to_path_buf()).unwrap();
            fs::remove_dir(dir_path)
        })
        .unwrap();

        assert!(result);
        assert_eq!(rx.try_recv().ok().as_deref(), Some(session.as_path()));
        assert!(!session.exists());
    }
}
