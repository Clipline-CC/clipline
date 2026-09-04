//! Filesystem storage management for saved clips.

mod empty_sessions;
mod files;
mod inventory;
mod ownership;
mod quota;
mod recovery;
mod replay_cache;
mod sessions;

pub use empty_sessions::{
    remove_emptied_session_dir, remove_emptied_session_dir_after_clip, sweep_emptied_session_dirs,
};
pub use files::{
    CLIP_OWNERSHIP_MARKER_SUFFIX, CLIP_SIDECAR_SUFFIXES, FAVORITE_MARKER_SUFFIX, MARKERS_SUFFIX,
    OSU_ENRICHMENT_SUFFIX, POSTER_SUFFIX, clip_sidecar_path,
};
pub(crate) use files::{is_link_or_reparse_point, remove_file_if_exists};
pub use inventory::{StorageStatus, storage_status};
pub use ownership::{
    clip_ownership_marker_path, ensure_clip_owned, ensure_session_clip_owned, is_clip_owned,
    remove_clip_ownership_marker, reserve_session_recording_file, write_session_metadata,
};
pub(crate) use ownership::SESSION_META_FILE;
pub use quota::{ClipGcPolicy, GcReport, enforce_quota, enforce_quota_with_policy};
pub use recovery::{RecordingRecoveryReport, delete_all_managed_media, recover_recording_files};
pub use replay_cache::{
    REPLAY_CACHE_OWNER_FILE, REPLAY_CACHE_RUN_PREFIX, is_replay_cache_run_name,
    replay_cache_owner_identity, replay_cache_run_identity,
};
pub use sessions::{is_session_dir_name, session_label, SessionTracker};

use std::sync::Mutex;

/// Serializes emptied-session cleanup with session attribution writes.
/// ponytail: process-wide lock; split per session only if contention is measured.
static SESSION_MUTATION_LOCK: Mutex<()> = Mutex::new(());

pub(crate) fn lock_session_mutations() -> std::sync::MutexGuard<'static, ()> {
    SESSION_MUTATION_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
