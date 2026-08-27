//! Remove session folders that no longer hold any real media.
//!
//! Protocol: classify the folder, delete leftover husks (including session
//! metadata), then `remove_dir`. If a concurrent save won, `remove_dir` fails
//! and the new files stay. There is no staging rename and no overwrite-restore.

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

    for leftover in leftovers {
        let _ = remove_file_if_exists(&leftover);
    }
    match fs::remove_dir(session_dir) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(true),
        Err(_) => Ok(false),
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

fn is_clip_sidecar_filename(name: &str) -> bool {
    CLIP_SIDECAR_SUFFIXES
        .iter()
        .any(|suffix| strip_ascii_suffix(name, suffix).is_some())
}

fn strip_ascii_suffix<'a>(name: &'a str, suffix: &str) -> Option<&'a str> {
    let start = name.len().checked_sub(suffix.len())?;
    if start > 0 && name[start..].eq_ignore_ascii_case(suffix) {
        Some(&name[..start])
    } else {
        None
    }
}
