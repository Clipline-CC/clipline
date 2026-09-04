//! On-disk cloud clip cache: layout, markers, leases, and eviction.
use super::*;

pub(crate) fn cloud_clip_cache_path(
    cloud: &CloudSettings,
    remote_clip_id: &str,
    asset: &str,
    extension: &str,
    version: Option<u64>,
) -> Result<PathBuf, String> {
    let file_name = cache_identity::cloud_cache_file_name(
        remote_clip_id,
        asset,
        extension,
        version.unwrap_or(0),
    )?;
    Ok(cloud_clip_cache_dir(cloud)?.join(file_name))
}

pub(crate) fn cloud_clip_cache_dir(cloud: &CloudSettings) -> Result<PathBuf, String> {
    Ok(cloud_clip_cache_root_dir().join(cloud_cache_namespace(cloud)?))
}

pub(crate) fn cloud_clip_cache_root_dir() -> PathBuf {
    crate::settings::persistence::local_cache_base().join("cloud-cache")
}

pub(crate) fn prepare_cloud_cache_root() -> Result<PathBuf, String> {
    let root = cloud_clip_cache_root_dir();
    migrate_legacy_cloud_cache(&legacy_cloud_clip_cache_root_dir(), &root)?;
    std::fs::create_dir_all(&root).map_err(|error| format!("create cloud cache: {error}"))?;
    Ok(root)
}

pub(crate) fn cloud_cache_namespace(cloud: &CloudSettings) -> Result<String, String> {
    let base =
        clipline_cloud_api::validate_cloud_host(&cloud.host_url, true).map_err(cloud_error)?;
    let account = cloud
        .connected_user_id
        .as_deref()
        .or(cloud.connected_username.as_deref())
        .or(cloud.credential_target.as_deref())
        .unwrap_or("anonymous")
        .trim();
    Ok(cache_identity::cloud_cache_namespace(
        base.as_str(),
        account,
    ))
}

pub(crate) fn cached_asset_matches(path: &Path, expected_size_bytes: Option<i64>) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() || meta.len() == 0 {
        return false;
    }
    if cloud_cache_marker_matches(path, meta.len()) {
        return true;
    }
    match expected_size_bytes {
        Some(expected) if expected > 0 => meta.len() == expected as u64,
        _ => true,
    }
}

pub(crate) fn cloud_cache_marker_path(path: &Path) -> PathBuf {
    let extension = path.extension().and_then(|ext| ext.to_str()).unwrap_or("");
    if extension.is_empty() {
        path.with_extension("ok")
    } else {
        path.with_extension(format!("{extension}.ok"))
    }
}

pub(crate) fn cloud_cache_marker_matches(path: &Path, size_bytes: u64) -> bool {
    std::fs::read_to_string(cloud_cache_marker_path(path))
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        == Some(size_bytes)
}

pub(crate) async fn write_cloud_cache_marker(path: &Path, size_bytes: u64) -> Result<(), String> {
    tokio::fs::write(cloud_cache_marker_path(path), size_bytes.to_string())
        .await
        .map_err(|e| format!("write cloud cache marker: {e}"))
}

pub(crate) fn cloud_clip_cache_tmp_path(target: &Path) -> Result<PathBuf, String> {
    let file_name = target
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "cloud cache target has no filename".to_string())?;
    let count = CLOUD_CACHE_TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    Ok(target.with_file_name(format!("{file_name}.{}.{}.tmp", std::process::id(), count)))
}

pub(crate) fn cloud_media_cache_max_bytes(expected_size_bytes: Option<i64>) -> u64 {
    expected_size_bytes
        .filter(|bytes| *bytes > 0)
        .map(|bytes| {
            (bytes as u64)
                .saturating_mul(2)
                .saturating_add(CLOUD_MEDIA_SIZE_SLACK_BYTES)
        })
        .unwrap_or(CLOUD_MEDIA_FALLBACK_MAX_BYTES)
        .min(CLOUD_MEDIA_HARD_MAX_BYTES)
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct CloudCachePruneReport {
    evicted_entries: usize,
    freed_bytes: u64,
    remaining_bytes: u64,
}

pub(crate) struct CloudCacheEntry {
    path: PathBuf,
    marker: Option<PathBuf>,
    bytes: u64,
    modified: std::time::SystemTime,
}

pub(crate) fn cloud_cache_lock() -> &'static Mutex<()> {
    CLOUD_CACHE_LOCK.get_or_init(|| Mutex::new(()))
}

pub(crate) fn cloud_cache_leases() -> &'static Mutex<BTreeMap<PathBuf, Instant>> {
    CLOUD_CACHE_LEASES.get_or_init(|| Mutex::new(BTreeMap::new()))
}

pub(crate) fn lease_cloud_cache_path(path: &Path) {
    if let Ok(mut leases) = cloud_cache_leases().lock() {
        let now = Instant::now();
        leases.retain(|_, expires| *expires > now);
        leases.insert(path.to_path_buf(), now + CLOUD_CACHE_PLAYBACK_LEASE);
    }
}

pub(crate) fn leased_cloud_cache_paths() -> Vec<PathBuf> {
    let Ok(mut leases) = cloud_cache_leases().lock() else {
        return Vec::new();
    };
    let now = Instant::now();
    leases.retain(|_, expires| *expires > now);
    leases.keys().cloned().collect()
}

pub(crate) fn prune_cloud_cache_for_download(
    root: &Path,
    additional_bytes: u64,
    additionally_protected: &[PathBuf],
) -> Result<CloudCachePruneReport, String> {
    let _guard = cloud_cache_lock()
        .lock()
        .map_err(|_| "cloud cache lock is poisoned".to_string())?;
    let mut protected = leased_cloud_cache_paths();
    protected.extend_from_slice(additionally_protected);
    let free = cloud_cache_available_space(root)?;
    enforce_cloud_cache_limits(
        root,
        CLOUD_CACHE_MAX_AGE,
        CLOUD_CACHE_QUOTA_BYTES,
        free,
        CLOUD_CACHE_FREE_SPACE_FLOOR_BYTES.saturating_add(additional_bytes),
        additional_bytes,
        &protected,
    )
}

pub(crate) fn enforce_cloud_cache_limits(
    root: &Path,
    max_age: Duration,
    quota_bytes: u64,
    available_bytes: u64,
    free_space_floor_bytes: u64,
    additional_bytes: u64,
    protected: &[PathBuf],
) -> Result<CloudCachePruneReport, String> {
    let now = std::time::SystemTime::now();
    let mut entries = Vec::new();
    collect_cloud_cache_entries(root, now, &mut entries)?;
    entries.sort_by_key(|entry| entry.modified);
    let total = entries
        .iter()
        .fold(0_u64, |sum, entry| sum.saturating_add(entry.bytes));
    let required_for_quota = total
        .saturating_add(additional_bytes)
        .saturating_sub(quota_bytes);
    let required_for_free_space = free_space_floor_bytes.saturating_sub(available_bytes);
    let required = required_for_quota.max(required_for_free_space);
    let mut report = CloudCachePruneReport::default();

    for entry in entries {
        let is_protected = protected.iter().any(|path| {
            path == &entry.path || entry.marker.as_ref().is_some_and(|marker| marker == path)
        });
        if is_protected {
            continue;
        }
        let old = now
            .duration_since(entry.modified)
            .ok()
            .is_some_and(|age| age >= max_age);
        if !old && report.freed_bytes >= required {
            continue;
        }
        if std::fs::remove_file(&entry.path).is_err() {
            continue;
        }
        if let Some(marker) = entry.marker {
            let _ = std::fs::remove_file(marker);
        }
        report.evicted_entries += 1;
        report.freed_bytes = report.freed_bytes.saturating_add(entry.bytes);
    }

    report.remaining_bytes = total.saturating_sub(report.freed_bytes);
    let quota_satisfied = report.remaining_bytes.saturating_add(additional_bytes) <= quota_bytes;
    let free_space_satisfied =
        available_bytes.saturating_add(report.freed_bytes) >= free_space_floor_bytes;
    if !quota_satisfied || !free_space_satisfied {
        return Err(format!(
            "cloud cache cannot reserve {:.1} MB without evicting active media or crossing its disk limits",
            additional_bytes as f64 / (1024.0 * 1024.0)
        ));
    }
    Ok(report)
}

pub(crate) fn collect_cloud_cache_entries(
    directory: &Path,
    now: std::time::SystemTime,
    entries: &mut Vec<CloudCacheEntry>,
) -> Result<(), String> {
    let read_dir = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("read cloud cache {directory:?}: {error}")),
    };
    for entry in read_dir.flatten() {
        let path = entry.path();
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if metadata.is_dir() {
            if !metadata_is_link(&metadata) {
                collect_cloud_cache_entries(&path, now, entries)?;
            }
            continue;
        }
        if !metadata.is_file() || metadata_is_link(&metadata) {
            continue;
        }
        let modified = metadata.modified().unwrap_or(std::time::UNIX_EPOCH);
        if is_owned_cloud_cache_temp(&path) {
            let stale = now
                .duration_since(modified)
                .ok()
                .is_some_and(|age| age >= CLOUD_CACHE_TEMP_MAX_AGE);
            if stale {
                let _ = std::fs::remove_file(path);
            }
            continue;
        }
        let is_marker = path.extension().and_then(|ext| ext.to_str()) == Some("ok");
        if is_marker {
            let asset = path.with_extension("");
            if asset.is_file() {
                continue;
            }
            entries.push(CloudCacheEntry {
                path,
                marker: None,
                bytes: metadata.len(),
                modified,
            });
            continue;
        }
        let marker = cloud_cache_marker_path(&path);
        let marker_bytes = std::fs::metadata(&marker)
            .ok()
            .filter(|metadata| metadata.is_file())
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        entries.push(CloudCacheEntry {
            path,
            marker: (marker_bytes > 0).then_some(marker),
            bytes: metadata.len().saturating_add(marker_bytes),
            modified,
        });
    }
    Ok(())
}

pub(crate) fn is_owned_cloud_cache_temp(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let mut parts = name.rsplit('.');
    parts.next() == Some("tmp")
        && parts
            .next()
            .is_some_and(|part| part.bytes().all(|byte| byte.is_ascii_digit()))
        && parts
            .next()
            .is_some_and(|part| part.bytes().all(|byte| byte.is_ascii_digit()))
        && parts.next().is_some()
}

pub(crate) fn metadata_is_link(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_type().is_symlink()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

pub(crate) fn touch_cloud_cache_entry(path: &Path) -> Result<(), String> {
    let now = std::time::SystemTime::now();
    std::fs::File::options()
        .write(true)
        .open(path)
        .and_then(|file| file.set_modified(now))
        .map_err(|error| format!("refresh cloud cache recency: {error}"))?;
    let marker = cloud_cache_marker_path(path);
    if marker.exists() {
        let _ = std::fs::File::options()
            .write(true)
            .open(marker)
            .and_then(|file| file.set_modified(now));
    }
    Ok(())
}

pub(crate) fn cloud_cache_available_space(path: &Path) -> Result<u64, String> {
    crate::windows::available_space_bytes(path, "read cloud cache free space")
}

#[cfg(test)]
mod tests {
    use super::*;
    use clipline_test_utils::TestDir;

    #[test]
    fn owned_cloud_cache_temp_cleans_only_while_armed() {
        let dir = TestDir::new("clipline-cloud", "cloud-cache-temp-guard");
        let abandoned = dir.path().join("abandoned.tmp");
        let published = dir.path().join("published.tmp");
        std::fs::write(&abandoned, b"partial").unwrap();
        std::fs::write(&published, b"complete").unwrap();

        drop(OwnedCloudCacheTemp::new(abandoned.clone()));
        let mut owner = OwnedCloudCacheTemp::new(published.clone());
        owner.disarm();
        drop(owner);

        assert!(!abandoned.exists());
        assert!(published.exists());
    }

    #[test]
    fn cloud_cache_prunes_lru_pairs_but_preserves_leased_entries() {
        let dir = TestDir::new("clipline-cloud", "cloud-cache-lru");
        let account = dir.path().join("account");
        std::fs::create_dir_all(&account).unwrap();
        let oldest = account.join("old.mp4");
        let newer = account.join("new.mp4");
        let leased = account.join("playing.mp4");
        let now = std::time::SystemTime::now();
        for (path, age) in [(&oldest, 30), (&newer, 20), (&leased, 10)] {
            std::fs::write(path, [0_u8; 8]).unwrap();
            std::fs::write(cloud_cache_marker_path(path), b"8").unwrap();
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(now - Duration::from_secs(age))
                .unwrap();
        }

        let report = enforce_cloud_cache_limits(
            dir.path(),
            Duration::from_secs(365 * 24 * 60 * 60),
            18,
            u64::MAX,
            0,
            0,
            std::slice::from_ref(&leased),
        )
        .unwrap();

        assert_eq!(report.evicted_entries, 1);
        assert!(!oldest.exists());
        assert!(!cloud_cache_marker_path(&oldest).exists());
        assert!(newer.exists());
        assert!(leased.exists());
    }

    #[test]
    fn cloud_cache_prunes_only_stale_owned_temps() {
        let dir = TestDir::new("clipline-cloud", "cloud-cache-temp-ownership");
        let stale = dir.path().join("media.mp4.123.1.tmp");
        let active = dir.path().join("media.mp4.123.2.tmp");
        let unrelated = dir.path().join("editor.tmp");
        for path in [&stale, &active, &unrelated] {
            std::fs::write(path, b"tmp").unwrap();
        }
        std::fs::File::options()
            .write(true)
            .open(&stale)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(1))
            .unwrap();

        enforce_cloud_cache_limits(
            dir.path(),
            CLOUD_CACHE_MAX_AGE,
            u64::MAX,
            u64::MAX,
            0,
            0,
            &[],
        )
        .unwrap();

        assert!(!stale.exists());
        assert!(active.exists());
        assert!(unrelated.exists());
    }

    #[test]
    fn cloud_cache_refuses_capacity_when_every_candidate_is_leased() {
        let dir = TestDir::new("clipline-cloud", "cloud-cache-leased-capacity");
        let leased = dir.path().join("playing.mp4");
        std::fs::write(&leased, [0_u8; 8]).unwrap();

        let error = enforce_cloud_cache_limits(
            dir.path(),
            CLOUD_CACHE_MAX_AGE,
            4,
            u64::MAX,
            0,
            0,
            std::slice::from_ref(&leased),
        )
        .unwrap_err();

        assert!(error.contains("active media"), "{error}");
        assert!(leased.exists());
    }

    #[test]
    fn cloud_media_size_hint_is_clamped_to_hard_limit() {
        assert_eq!(
            cloud_media_cache_max_bytes(Some(i64::MAX)),
            CLOUD_MEDIA_HARD_MAX_BYTES
        );
        assert_eq!(
            cloud_media_cache_max_bytes(Some(1)),
            CLOUD_MEDIA_SIZE_SLACK_BYTES + 2
        );
    }

}