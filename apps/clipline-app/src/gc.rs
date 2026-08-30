//! The app's quota-GC policy: what auto-delete protects and in what order
//! clips are removed. All quota-GC call sites go through this module so the
//! recorder, replay saves, and the manual recheck share one policy.

use std::io;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use crate::library;

/// App-owned serialization for GC, clip mutations, and upload lease starts.
/// Lock order is clip mutation -> upload sources -> session mutation.
/// ponytail: process-wide lock; split per media root only if contention is measured.
static CLIP_MUTATION_LOCK: Mutex<()> = Mutex::new(());

pub(crate) fn lock_clip_mutations() -> MutexGuard<'static, ()> {
    CLIP_MUTATION_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Quota GC deletes oldest clips within a kind, but drains kinds in order:
/// sessions first, then replays, then trims. Lower keys are deleted first.
pub(crate) fn clip_gc_priority(path: &Path) -> u8 {
    match library::clip_kind_for_path(path).as_str() {
        "session" => 0,
        "replay" => 1,
        _ => 2,
    }
}

fn clip_gc_policy(path: &Path) -> clipline_storage::ClipGcPolicy {
    clipline_storage::ClipGcPolicy {
        protected: crate::cloud_upload::is_active_upload_source(path)
            || library::is_favorite_clip(path),
        priority: clip_gc_priority(path),
    }
}

/// Active upload sources and favorites are never deleted, and kinds drain in
/// priority order (sessions → replays → trims).
pub(crate) fn enforce_quota_with_clip_policy(
    dir: &Path,
    quota_bytes: Option<u64>,
    protect: Option<&Path>,
) -> io::Result<clipline_storage::GcReport> {
    enforce_quota_with_clip_policy_using(dir, quota_bytes, protect, clip_gc_policy)
}

fn enforce_quota_with_clip_policy_using(
    dir: &Path,
    quota_bytes: Option<u64>,
    protect: Option<&Path>,
    policy: impl Fn(&Path) -> clipline_storage::ClipGcPolicy,
) -> io::Result<clipline_storage::GcReport> {
    let _guard = lock_clip_mutations();
    let report = clipline_storage::enforce_quota_with_policy(dir, quota_bytes, protect, policy)?;
    for error in &report.cleanup_errors {
        tracing::warn!(event = "storage_session_cleanup_failed", error);
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clipline_test_utils::TestDir;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn clip_gc_priority_orders_sessions_before_replays_before_trims() {
        assert_eq!(clip_gc_priority(Path::new("session_1781377615.mp4")), 0);
        assert_eq!(clip_gc_priority(Path::new("clip_1784525638.mp4")), 1);
        assert_eq!(
            clip_gc_priority(Path::new("clip_1_trim_001000_002000.mp4")),
            2
        );
    }

    #[test]
    fn policy_protects_favorites_and_orders_kinds() {
        let dir = TestDir::new("clipline-gc", "policy-favorites");
        // Oldest clip is a trim (lowest deletion priority); the newest clip is
        // a favorited session. With room for only one clip after GC, the
        // favorite must survive and the trim must go last.
        let trim = dir.path().join("clip_1_trim_001000_002000.mp4");
        std::fs::write(&trim, [0; 100]).unwrap();
        clipline_storage::ensure_clip_owned(&trim).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        let replay = dir.path().join("clip_1784525638.mp4");
        std::fs::write(&replay, [0; 100]).unwrap();
        clipline_storage::ensure_clip_owned(&replay).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        let session = dir.path().join("session_1784525639.mp4");
        std::fs::write(&session, [0; 100]).unwrap();
        clipline_storage::ensure_clip_owned(&session).unwrap();
        library::set_clip_favorite_impl(&session, true).unwrap();

        // Quota leaves room for one clip (sidecars count toward usage).
        let report = enforce_quota_with_clip_policy(dir.path(), Some(200), None).unwrap();

        assert_eq!(report.deleted_clips, 2);
        assert!(session.exists(), "favorites must never be auto-deleted");
        assert!(!replay.exists(), "replays must drain before trims");
        assert!(!trim.exists());
        assert_eq!(report.status.clip_count, 1);
    }

    #[test]
    fn policy_does_not_delete_other_clips_when_favorites_alone_exceed_quota() {
        let dir = TestDir::new("clipline-gc", "favorite-over-quota");
        let favorite = dir.path().join("favorite.mp4");
        std::fs::write(&favorite, [0; 200]).unwrap();
        clipline_storage::ensure_clip_owned(&favorite).unwrap();
        library::set_clip_favorite_impl(&favorite, true).unwrap();
        let other = dir.path().join("other.mp4");
        std::fs::write(&other, [0; 100]).unwrap();
        clipline_storage::ensure_clip_owned(&other).unwrap();

        let report = enforce_quota_with_clip_policy(dir.path(), Some(150), None).unwrap();

        assert_eq!(report.deleted_clips, 0);
        assert!(favorite.exists());
        assert!(
            other.exists(),
            "futile deletion cannot make room below quota"
        );
    }

    #[test]
    fn policy_drains_kinds_in_priority_order() {
        let dir = TestDir::new("clipline-gc", "policy-order");
        // Newest clip is a session; oldest is a trim. Sessions must drain
        // first even when they are newer than every replay and trim.
        let trim = dir.path().join("clip_1_trim_001000_002000.mp4");
        std::fs::write(&trim, [0; 100]).unwrap();
        clipline_storage::ensure_clip_owned(&trim).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        let replay = dir.path().join("clip_1784525638.mp4");
        std::fs::write(&replay, [0; 100]).unwrap();
        clipline_storage::ensure_clip_owned(&replay).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        let session = dir.path().join("session_1784525639.mp4");
        std::fs::write(&session, [0; 100]).unwrap();
        clipline_storage::ensure_clip_owned(&session).unwrap();

        let report = enforce_quota_with_clip_policy(dir.path(), Some(150), None).unwrap();

        assert_eq!(report.deleted_clips, 2);
        assert!(
            !session.exists(),
            "sessions must drain before replays and trims"
        );
        assert!(!replay.exists());
        assert!(
            trim.exists(),
            "trims must be the last kind auto-delete touches"
        );
        assert_eq!(report.status.clip_count, 1);
    }

    #[test]
    fn favorite_started_after_gc_check_cannot_succeed_then_be_deleted() {
        let dir = TestDir::new("clipline-gc", "favorite-race");
        let clip = dir.path().join("vacation.mp4");
        std::fs::write(&clip, [0; 100]).unwrap();
        clipline_storage::ensure_clip_owned(&clip).unwrap();

        let (checked_tx, checked_rx) = mpsc::channel();
        let (continue_tx, continue_rx) = mpsc::channel();
        let gc_root = dir.path().to_path_buf();
        let gc = std::thread::spawn(move || {
            enforce_quota_with_clip_policy_using(&gc_root, Some(0), None, |path| {
                let policy = clipline_storage::ClipGcPolicy {
                    protected: library::is_favorite_clip(path),
                    priority: 0,
                };
                checked_tx.send(()).unwrap();
                continue_rx.recv().unwrap();
                policy
            })
            .unwrap()
        });

        checked_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let favorite_clip = clip.clone();
        let (favorite_tx, favorite_rx) = mpsc::channel();
        std::thread::spawn(move || {
            favorite_tx
                .send(library::set_clip_favorite_impl(&favorite_clip, true))
                .unwrap();
        });
        assert!(
            favorite_rx
                .recv_timeout(Duration::from_millis(100))
                .is_err(),
            "favorite mutation must wait while production GC owns the clip lock"
        );
        continue_tx.send(()).unwrap();
        gc.join().unwrap();
        let favorite_result = favorite_rx.recv_timeout(Duration::from_secs(2)).unwrap();

        assert!(favorite_result.is_err());
        assert!(!clip.exists());
    }
}
