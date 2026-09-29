use super::*;

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct AudioPreviewPruneReport {
    removed_files: usize,
    removed_bytes: u64,
    reusable_bytes: u64,
}

pub(crate) fn is_audio_preview_mp4(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("audio-preview-") && name.ends_with(".mp4"))
}

pub(crate) fn is_audio_preview_partial(path: &Path) -> bool {
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

pub(crate) fn audio_preview_path_is_protected(path: &Path, protected: &[PathBuf]) -> bool {
    protected.iter().any(|candidate| {
        path == candidate
            || std::fs::canonicalize(path)
                .ok()
                .zip(std::fs::canonicalize(candidate).ok())
                .is_some_and(|(left, right)| left == right)
    })
}

pub(crate) fn prune_audio_preview_cache(
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

pub(crate) fn touch_audio_preview(path: &Path) -> Result<(), String> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use clipline_test_utils::TestDir;
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
}
