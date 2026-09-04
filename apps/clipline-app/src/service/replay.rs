//! Storage quotas and replay-cache management.
use super::*;

pub const DEFAULT_DISK_QUOTA_BYTES: u64 = 10 * 1024 * 1024 * 1024;

pub(super) fn quota_would_be_exceeded(
    total_bytes: u64,
    quota_bytes: Option<u64>,
    required_bytes: u64,
) -> bool {
    quota_bytes.is_some_and(|quota| {
        total_bytes > quota || required_bytes > quota.saturating_sub(total_bytes)
    })
}

pub(super) fn storage_quota_event_for_usage(
    total_bytes: u64,
    quota_bytes: Option<u64>,
    required_bytes: u64,
) -> Option<Event> {
    quota_would_be_exceeded(total_bytes, quota_bytes, required_bytes).then(|| {
        Event::StorageQuotaFull {
            total_bytes,
            quota_bytes: quota_bytes.expect("a quota is present when capacity is exceeded"),
            required_bytes,
        }
    })
}

pub(super) fn storage_status_or_warn(clips_dir: &Path, quota_bytes: Option<u64>) -> Option<StorageStatus> {
    match storage_status(clips_dir, quota_bytes) {
        Ok(status) => Some(status),
        Err(error) => {
            tracing::warn!(
                event = "storage_quota_inspection_failed",
                path = ?clips_dir,
                error = %error,
            );
            None
        }
    }
}

/// Deletes oldest managed clips until `required_bytes` fit under the quota.
/// Active recordings and upload sources are never removed.
pub(super) fn make_room_for_quota(
    events: &Sender<Event>,
    clips_dir: &Path,
    quota_bytes: Option<u64>,
    required_bytes: u64,
    protect: Option<&Path>,
) -> Option<(StorageStatus, usize)> {
    let Some(quota) = quota_bytes else {
        return storage_status_or_warn(clips_dir, None).map(|status| (status, 0));
    };
    let before_bytes = storage_status_or_warn(clips_dir, quota_bytes).map(|status| status.total_bytes);
    let target = quota.saturating_sub(required_bytes);
    let mut deleted_clips = match crate::gc::enforce_quota_with_clip_policy(
        clips_dir,
        Some(target),
        protect,
    ) {
        Ok(report) => report.deleted_clips,
        Err(error) => {
            tracing::warn!(
                event = "storage_quota_auto_delete_failed",
                path = ?clips_dir,
                error = %error,
            );
            0
        }
    };
    let status = storage_status_or_warn(clips_dir, quota_bytes)?;
    // A collector can remove an MP4 and then fail on a later sidecar/scan.
    // Compare the post-error inventory so LibraryChanged still fires.
    if deleted_clips == 0
        && before_bytes.is_some_and(|before| status.total_bytes < before)
    {
        deleted_clips = 1;
    }
    if deleted_clips > 0 {
        let _ = events.send(Event::LibraryChanged);
    }
    Some((status, deleted_clips))
}

pub(super) fn storage_quota_full_event(
    events: &Sender<Event>,
    clips_dir: &Path,
    quota_bytes: Option<u64>,
    required_bytes: u64,
    auto_delete: bool,
) -> Option<Event> {
    let mut status = storage_status_or_warn(clips_dir, quota_bytes)?;
    if !quota_would_be_exceeded(status.total_bytes, quota_bytes, required_bytes) {
        return None;
    }
    if auto_delete {
        if let Some((cleaned, _)) =
            make_room_for_quota(events, clips_dir, quota_bytes, required_bytes, None)
        {
            status = cleaned;
        }
        if !quota_would_be_exceeded(status.total_bytes, quota_bytes, required_bytes) {
            return None;
        }
    }
    storage_quota_event_for_usage(status.total_bytes, quota_bytes, required_bytes)
}

pub(super) struct PreparedReplayStorage {
    pub(super) run_dir: Option<PathBuf>,
    pub(super) max_bytes: usize,
    pub(super) armed: bool,
}

impl PreparedReplayStorage {
    pub(super) fn memory(max_bytes: usize) -> Self {
        Self {
            run_dir: None,
            max_bytes,
            armed: false,
        }
    }

    pub(super) fn disk(run_dir: PathBuf, max_bytes: usize) -> Self {
        Self {
            run_dir: Some(run_dir),
            max_bytes,
            armed: true,
        }
    }

    pub(super) fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for PreparedReplayStorage {
    fn drop(&mut self) {
        if self.armed {
            if let Some(run_dir) = &self.run_dir {
                let _ = std::fs::remove_dir_all(run_dir);
            }
        }
    }
}

#[derive(serde::Deserialize, serde::Serialize)]
pub(super) struct ReplayCacheOwner {
    pub(super) process_instance_id: String,
    pub(super) created_at_unix: u64,
}

pub(super) fn prepare_replay_storage(opts: &ServiceOptions) -> Result<PreparedReplayStorage, String> {
    match &opts.replay_storage {
        ReplayStorageOptions::Memory => Ok(PreparedReplayStorage::memory(opts.buffer_bytes)),
        ReplayStorageOptions::Disk { dir, quota_bytes } => {
            if *quota_bytes < 256 * 1024 * 1024 {
                return Err("replay cache quota is too small".into());
            }
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("create replay cache folder {dir:?}: {e}"))?;
            ensure_replay_cache_free_space(opts)?;
            let now = SystemTime::now();
            let preserved_bytes =
                sweep_replay_cache_runs(dir, now, crate::windows::process_instance_id)?;
            let available_quota = quota_bytes.saturating_sub(preserved_bytes);
            if available_quota == 0 {
                return Err(format!(
                    "replay cache quota is already consumed by active or protected runs ({preserved_bytes} bytes)"
                ));
            }
            let current_process_instance_id =
                crate::windows::process_instance_id(std::process::id())?;
            let created_at = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default();
            let stamp = created_at.as_nanos();
            let run_dir = (0u32..1024)
                .find_map(|attempt| {
                    let candidate = dir.join(format!(
                        "{REPLAY_CACHE_RUN_PREFIX}{stamp}-{}-{attempt}",
                        std::process::id()
                    ));
                    match std::fs::create_dir(&candidate) {
                        Ok(()) => Some(Ok(candidate)),
                        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => None,
                        Err(e) => Some(Err(format!(
                            "create replay cache run folder {candidate:?}: {e}"
                        ))),
                    }
                })
                .unwrap_or_else(|| {
                    Err("create replay cache run folder: too many collisions".into())
                })?;
            let owner = ReplayCacheOwner {
                process_instance_id: current_process_instance_id,
                created_at_unix: created_at.as_secs(),
            };
            if let Err(error) = write_replay_cache_owner(&run_dir, &owner) {
                let _ = std::fs::remove_dir_all(&run_dir);
                return Err(error);
            }
            Ok(PreparedReplayStorage::disk(
                run_dir,
                usize::try_from(available_quota).unwrap_or(usize::MAX),
            ))
        }
    }
}

pub(super) fn write_replay_cache_owner(run_dir: &Path, owner: &ReplayCacheOwner) -> Result<(), String> {
    let path = run_dir.join(REPLAY_CACHE_OWNER_FILE);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|e| format!("create replay cache ownership record {path:?}: {e}"))?;
    serde_json::to_writer(&mut file, owner)
        .map_err(|e| format!("write replay cache ownership record {path:?}: {e}"))?;
    file.sync_all()
        .map_err(|e| format!("flush replay cache ownership record {path:?}: {e}"))
}

pub(super) fn sweep_replay_cache_runs(
    root: &Path,
    now: SystemTime,
    mut process_instance_id: impl FnMut(u32) -> Result<String, String>,
) -> Result<u64, String> {
    let entries =
        std::fs::read_dir(root).map_err(|e| format!("scan replay cache folder {root:?}: {e}"))?;
    let mut preserved_bytes = 0u64;
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !clipline_storage::is_replay_cache_run_name(name) {
            continue;
        }
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !metadata.is_dir() || is_link_or_reparse_point(&metadata) {
            continue;
        }

        let owner = std::fs::read(path.join(REPLAY_CACHE_OWNER_FILE))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<ReplayCacheOwner>(&bytes).ok());
        let definitively_stale = owner
            .as_ref()
            .and_then(|owner| {
                clipline_storage::replay_cache_owner_identity(&owner.process_instance_id)
                    .map(|(pid, _)| (owner, pid))
            })
            .and_then(|(owner, pid)| {
                process_instance_id(pid)
                    .ok()
                    .map(|current| current != owner.process_instance_id)
            });
        let old_enough = metadata
            .modified()
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age >= AMBIGUOUS_REPLAY_CACHE_MAX_AGE);
        let should_remove = definitively_stale.unwrap_or(old_enough);

        if should_remove && std::fs::remove_dir_all(&path).is_ok() {
            continue;
        }
        preserved_bytes = preserved_bytes.saturating_add(replay_cache_run_size(&path));
    }
    Ok(preserved_bytes)
}

pub(super) fn replay_cache_run_size(path: &Path) -> u64 {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    if is_link_or_reparse_point(&metadata) {
        return 0;
    }
    if metadata.is_file() {
        return metadata.len();
    }
    if !metadata.is_dir() {
        return 0;
    }
    std::fs::read_dir(path)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| replay_cache_run_size(&entry.path()))
                .fold(0u64, u64::saturating_add)
        })
        .unwrap_or(0)
}

pub(super) fn is_link_or_reparse_point(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_type().is_symlink()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

pub(super) fn ensure_replay_cache_free_space(opts: &ServiceOptions) -> Result<(), String> {
    let ReplayStorageOptions::Disk { dir, .. } = &opts.replay_storage else {
        return Ok(());
    };
    let free = available_space_bytes(dir)?;
    if free < LOW_REPLAY_CACHE_DISK_RESERVE_BYTES {
        return Err(format!(
            "replay cache disk is low: {} MiB free, need at least 2048 MiB",
            free / (1024 * 1024)
        ));
    }
    Ok(())
}

pub(super) fn available_space_bytes(path: &Path) -> Result<u64, String> {
    crate::windows::available_space_bytes(path, &format!("could not read free space for {path:?}"))
}
