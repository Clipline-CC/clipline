//! Clip naming, sidecar, and media-tree filesystem helpers.
use std::fs;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
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

pub(crate) fn is_mp4(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("mp4"))
}

pub(crate) fn is_recording_mp4(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.to_ascii_lowercase().ends_with(".mp4.recording"))
}

pub(crate) fn recording_final_path(path: &Path) -> Option<PathBuf> {
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
pub(crate) fn clip_sidecars(clip: &Path) -> io::Result<(Vec<PathBuf>, u64)> {
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

pub(crate) fn optional_file_len(path: &Path) -> io::Result<u64> {
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

pub(crate) fn same_path(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

pub(crate) fn visit_media_dirs(dir: &Path, mut f: impl FnMut(&Path) -> io::Result<()>) -> io::Result<()> {
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
