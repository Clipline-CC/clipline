//! Quota enforcement over the clip inventory.
use std::io;
use std::path::Path;

use crate::empty_sessions::remove_emptied_session_dir_after_clip;
use crate::inventory::{ClipFile, DeletedClip, StorageStatus, delete_inventoried_clip_with, inventory, status_from_clips};
use crate::storage_status;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GcReport {
    pub deleted_clips: usize,
    pub freed_bytes: u64,
    pub cleanup_errors: Vec<String>,
    pub status: StorageStatus,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClipGcPolicy {
    pub protected: bool,
    pub priority: u8,
}

pub fn enforce_quota(
    dir: &Path,
    quota_bytes: Option<u64>,
    protect: Option<&Path>,
) -> io::Result<GcReport> {
    enforce_quota_with_policy(dir, quota_bytes, protect, |_| ClipGcPolicy::default())
}

/// Enforces the clip quota while letting the caller order deletion by clip
/// policy and protect additional managed files that are temporarily immutable
/// (active upload sources, favorites). Protected bytes still count toward the
/// quota; collection skips them and continues with the next deletable clip.
///
/// `policy` is evaluated once per clip. Lower priorities are deleted first,
/// and within the same priority the oldest clip goes first. Storage stays
/// neutral about what protection and priority mean.
pub fn enforce_quota_with_policy(
    dir: &Path,
    quota_bytes: Option<u64>,
    protect: Option<&Path>,
    policy: impl Fn(&Path) -> ClipGcPolicy,
) -> io::Result<GcReport> {
    enforce_quota_with_policy_and_cleanup(
        dir,
        quota_bytes,
        protect,
        policy,
        remove_emptied_session_dir_after_clip,
    )
}

fn enforce_quota_with_policy_and_cleanup(
    dir: &Path,
    quota_bytes: Option<u64>,
    protect: Option<&Path>,
    policy: impl Fn(&Path) -> ClipGcPolicy,
    cleanup: impl Fn(&Path, &Path) -> io::Result<bool>,
) -> io::Result<GcReport> {
    let Some(quota) = quota_bytes else {
        return Ok(GcReport {
            deleted_clips: 0,
            freed_bytes: 0,
            cleanup_errors: Vec::new(),
            status: storage_status(dir, quota_bytes)?,
        });
    };

    let clips = inventory(dir, protect)?;
    let mut total_bytes = clips.iter().map(ClipFile::total_bytes).sum::<u64>();
    let mut deleted_clips = 0usize;
    let mut freed_bytes = 0u64;
    let mut cleanup_errors = Vec::new();

    if total_bytes <= quota {
        return Ok(GcReport {
            deleted_clips,
            freed_bytes,
            cleanup_errors,
            status: status_from_clips(&clips, quota_bytes),
        });
    }

    // Decorate once because app policy reads clip sidecars.
    let mut clips = clips
        .into_iter()
        .map(|clip| (policy(&clip.path), clip))
        .collect::<Vec<_>>();

    let undeletable_bytes = clips
        .iter()
        .filter(|(policy, clip)| !clip.can_delete(protect, policy.protected))
        .map(|(_, clip)| clip.total_bytes())
        .sum::<u64>();
    if undeletable_bytes > quota {
        let clips = clips.into_iter().map(|(_, clip)| clip).collect::<Vec<_>>();
        return Ok(GcReport {
            deleted_clips,
            freed_bytes,
            cleanup_errors,
            status: status_from_clips(&clips, quota_bytes),
        });
    }

    clips.sort_by(|(policy_a, a), (policy_b, b)| {
        policy_a
            .priority
            .cmp(&policy_b.priority)
            .then_with(|| a.modified.cmp(&b.modified))
            .then_with(|| a.path.file_name().cmp(&b.path.file_name()))
    });

    for (policy, clip) in clips {
        if total_bytes <= quota {
            break;
        }
        if !clip.can_delete(protect, policy.protected) {
            continue;
        }

        let clip_bytes = clip.total_bytes();
        match delete_inventoried_clip_with(&clip, dir, &cleanup)? {
            DeletedClip::Removed { cleanup_error } => {
                total_bytes = total_bytes.saturating_sub(clip_bytes);
                freed_bytes += clip_bytes;
                deleted_clips += 1;
                if let Some(error) = cleanup_error {
                    cleanup_errors.push(error);
                }
            }
            // The file is already gone from this tree (rename/delete race or a
            // prior collector). Drop its bytes from the running total so we do
            // not keep deleting the next-oldest clip against a stale sum.
            DeletedClip::AlreadyGone => {
                total_bytes = total_bytes.saturating_sub(clip_bytes);
            }
            DeletedClip::Skipped => {}
        }
    }

    Ok(GcReport {
        deleted_clips,
        freed_bytes,
        cleanup_errors,
        status: status_from_clips(&inventory(dir, protect)?, quota_bytes),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use clipline_test_utils::TestDir;
    use std::path::PathBuf;
    use crate::files::same_path;
    use crate::ownership::clip_ownership_marker_path;
    use std::time::Duration;

fn tick_mtime() {
    std::thread::sleep(Duration::from_millis(20));
}

fn mark_owned(path: &Path) {
    std::fs::write(clip_ownership_marker_path(path).unwrap(), b"").unwrap();
}

fn write_owned(dir: &TestDir, relative: &str, bytes: usize) -> PathBuf {
    let path = dir.write(relative, bytes);
    mark_owned(&path);
    path
}

#[test]
fn enforce_quota_never_deletes_unmarked_mp4_files() {
    let dir = TestDir::new("clipline-storage", "preserve-unowned-mp4");
    let unrelated = dir.write("unrelated.mp4", 90);
    let nested_unrelated = dir.write("Movies/also-unrelated.mp4", 80);
    let owned = dir.write("2026-07-18 12-00/owned.mp4", 10);
    let owned_marker = dir.write("2026-07-18 12-00/owned.clipline.json", 2);

    let report = enforce_quota(dir.path(), Some(0), None).unwrap();

    assert_eq!(report.deleted_clips, 1);
    assert!(unrelated.exists());
    assert!(nested_unrelated.exists());
    assert!(!owned.exists());
    assert!(!owned_marker.exists());
    assert_eq!(report.status.total_bytes, 0);
}

#[test]
fn enforce_quota_deletes_unmarked_legacy_clipline_filenames() {
    let dir = TestDir::new("clipline-storage", "legacy-generated-quota");
    let legacy = dir.write("clip_1784525638.mp4", 10);
    let unrelated = dir.write("ordinary.mp4", 90);

    let report = enforce_quota(dir.path(), Some(0), None).unwrap();

    assert_eq!(report.deleted_clips, 1);
    assert!(!legacy.exists());
    assert!(unrelated.exists());
}

#[test]
fn enforce_quota_counts_an_explicitly_protected_new_clip() {
    let dir = TestDir::new("clipline-storage", "protect-new-unmarked");
    dir.write("unrelated.mp4", 90);
    let fresh = dir.write("2026-07-18 12-00/fresh.mp4", 10);

    let report = enforce_quota(dir.path(), Some(5), Some(&fresh)).unwrap();

    assert_eq!(report.deleted_clips, 0);
    assert_eq!(report.status.clip_count, 1);
    assert_eq!(report.status.total_bytes, 10);
    assert!(report.status.is_over_quota());
    assert!(fresh.exists());
}

#[test]
fn enforce_quota_deletes_oldest_until_under_budget() {
    let dir = TestDir::new("clipline-storage", "oldest-first");
    let a = write_owned(&dir, "a.mp4", 10);
    tick_mtime();
    let b = write_owned(&dir, "b.mp4", 10);
    tick_mtime();
    let c = write_owned(&dir, "c.mp4", 10);

    let report = enforce_quota(dir.path(), Some(15), None).unwrap();

    assert_eq!(report.deleted_clips, 2);
    assert_eq!(report.freed_bytes, 20);
    assert!(!a.exists());
    assert!(!b.exists());
    assert!(c.exists());
    assert_eq!(report.status.total_bytes, 10);
}

#[test]
fn enforce_quota_skips_additionally_protected_uploads_and_keeps_collecting() {
    let dir = TestDir::new("clipline-storage", "protect-active-upload");
    let uploading = write_owned(&dir, "uploading.mp4", 10);
    tick_mtime();
    let next_oldest = write_owned(&dir, "next-oldest.mp4", 10);
    tick_mtime();
    let newest = write_owned(&dir, "newest.mp4", 10);

    let report = enforce_quota_with_policy(dir.path(), Some(20), None, |path| ClipGcPolicy {
        protected: same_path(path, &uploading),
        priority: 0,
    })
    .unwrap();

    assert_eq!(report.deleted_clips, 1);
    assert!(uploading.exists(), "an active upload is immutable");
    assert!(
        !next_oldest.exists(),
        "GC must continue to the next deletable clip"
    );
    assert!(newest.exists());
    assert_eq!(report.status.total_bytes, 20);
}

fn kind_priority(name: &str) -> u8 {
    // Sessions drain first, then replays, then trims (the app maps the
    // clip kind onto this order; here the file name stands in for it).
    if name.starts_with("session-") {
        0
    } else if name.starts_with("replay-") {
        1
    } else {
        2
    }
}

#[test]
fn enforce_quota_with_policy_deletes_by_priority_before_age() {
    let dir = TestDir::new("clipline-storage", "policy-priority");
    // Oldest clip is a trim (lowest deletion priority): age must lose to
    // kind priority when the quota frees only part of the library.
    let trim = write_owned(&dir, "trim-old.mp4", 10);
    tick_mtime();
    let replay = write_owned(&dir, "replay-mid.mp4", 10);
    tick_mtime();
    let session = write_owned(&dir, "session-new.mp4", 10);

    let report = enforce_quota_with_policy(dir.path(), Some(10), None, |path| ClipGcPolicy {
        protected: false,
        priority: kind_priority(path.file_name().and_then(|n| n.to_str()).unwrap_or("")),
    })
    .unwrap();

    assert_eq!(report.deleted_clips, 2);
    assert!(
        !session.exists(),
        "sessions must drain before replays/trims"
    );
    assert!(!replay.exists(), "replays must drain before trims");
    assert!(
        trim.exists(),
        "the oldest clip can survive when its kind is low priority"
    );
    assert_eq!(report.status.total_bytes, 10);
}

#[test]
fn enforce_quota_with_policy_deletes_oldest_within_a_priority() {
    let dir = TestDir::new("clipline-storage", "policy-within-kind");
    let oldest = write_owned(&dir, "replay-oldest.mp4", 10);
    tick_mtime();
    let older = write_owned(&dir, "replay-old.mp4", 10);
    tick_mtime();
    let newer = write_owned(&dir, "replay-new.mp4", 10);

    let report = enforce_quota_with_policy(dir.path(), Some(10), None, |path| ClipGcPolicy {
        protected: false,
        priority: kind_priority(path.file_name().and_then(|n| n.to_str()).unwrap_or("")),
    })
    .unwrap();

    assert_eq!(report.deleted_clips, 2);
    assert!(!oldest.exists());
    assert!(!older.exists());
    assert!(newer.exists());
}

#[test]
fn enforce_quota_with_policy_skips_protected_high_priority_clips() {
    let dir = TestDir::new("clipline-storage", "policy-protected");
    let favorite = write_owned(&dir, "session-favorite.mp4", 10);
    tick_mtime();
    let replay = write_owned(&dir, "replay.mp4", 10);
    tick_mtime();
    let trim = write_owned(&dir, "trim.mp4", 10);

    let report = enforce_quota_with_policy(dir.path(), Some(10), None, |path| ClipGcPolicy {
        protected: same_path(path, &favorite),
        priority: kind_priority(path.file_name().and_then(|n| n.to_str()).unwrap_or("")),
    })
    .unwrap();

    assert_eq!(report.deleted_clips, 2);
    assert!(
        favorite.exists(),
        "protected clips must survive even at high priority"
    );
    assert!(!replay.exists());
    assert!(!trim.exists());
    assert_eq!(report.status.total_bytes, 10);
}

#[test]
fn enforce_quota_under_budget_skips_policy_callbacks() {
    let dir = TestDir::new("clipline-storage", "policy-under-budget");
    write_owned(&dir, "clip.mp4", 10);
    let protection_checks = std::cell::Cell::new(0usize);
    let priority_checks = std::cell::Cell::new(0usize);

    let report = enforce_quota_with_policy(dir.path(), Some(100), None, |_| {
        protection_checks.set(protection_checks.get() + 1);
        priority_checks.set(priority_checks.get() + 1);
        ClipGcPolicy::default()
    })
    .unwrap();

    assert_eq!(report.deleted_clips, 0);
    assert_eq!(protection_checks.get(), 0);
    assert_eq!(priority_checks.get(), 0);
}

#[test]
fn enforce_quota_evaluates_policy_once_per_clip() {
    let dir = TestDir::new("clipline-storage", "policy-once");
    write_owned(&dir, "old.mp4", 10);
    tick_mtime();
    write_owned(&dir, "new.mp4", 10);
    let checks = std::cell::Cell::new(0usize);

    let report = enforce_quota_with_policy(dir.path(), Some(10), None, |_| {
        checks.set(checks.get() + 1);
        ClipGcPolicy::default()
    })
    .unwrap();

    assert_eq!(report.deleted_clips, 1);
    assert_eq!(checks.get(), 2, "policy must be computed once per clip");
}

#[test]
fn enforce_quota_reports_cleanup_error_after_counting_deleted_clip() {
    let dir = TestDir::new("clipline-storage", "quota-cleanup-error");
    let old = write_owned(&dir, "2026-08-30 01-00/old.mp4", 40);
    tick_mtime();
    let keep = write_owned(&dir, "keep.mp4", 10);

    let report = enforce_quota_with_policy_and_cleanup(
        dir.path(),
        Some(20),
        None,
        |_| ClipGcPolicy::default(),
        |_, _| Err(io::Error::other("simulated cleanup failure")),
    )
    .unwrap();

    assert_eq!(report.deleted_clips, 1);
    assert_eq!(report.freed_bytes, 40);
    assert_eq!(report.cleanup_errors.len(), 1);
    assert!(!old.exists());
    assert!(keep.exists());
}

#[test]
fn enforce_quota_deletes_marker_sidecar_with_clip() {
    let dir = TestDir::new("clipline-storage", "sidecar-delete");
    let old = dir.write("old.mp4", 10);
    let sidecar = dir.write("old.markers.json", 2);
    tick_mtime();
    let keep = write_owned(&dir, "keep.mp4", 10);

    let report = enforce_quota(dir.path(), Some(10), None).unwrap();

    assert_eq!(report.deleted_clips, 1);
    assert_eq!(report.freed_bytes, 12);
    assert!(!old.exists());
    assert!(!sidecar.exists());
    assert!(keep.exists());
    assert_eq!(report.status.total_bytes, 10);
}

#[test]
fn enforce_quota_deletes_poster_sidecar_with_clip() {
    let dir = TestDir::new("clipline-storage", "poster-delete");
    let old = dir.write("old.mp4", 10);
    let markers = dir.write("old.markers.json", 2);
    let poster = dir.write("old.poster.jpg", 4);
    tick_mtime();
    let keep = write_owned(&dir, "keep.mp4", 10);

    let report = enforce_quota(dir.path(), Some(10), None).unwrap();

    assert_eq!(report.deleted_clips, 1);
    assert_eq!(report.freed_bytes, 16);
    assert!(!old.exists());
    assert!(!markers.exists());
    assert!(!poster.exists());
    assert!(keep.exists());
    assert_eq!(report.status.total_bytes, 10);
}

#[test]
fn enforce_quota_deletes_osu_pending_sidecar_with_clip() {
    let dir = TestDir::new("clipline-storage", "osu-pending-delete");
    let old = dir.write("old.mp4", 10);
    let pending = dir.write("old.osu-enrichment.json", 6);
    tick_mtime();
    let keep = write_owned(&dir, "keep.mp4", 10);

    let report = enforce_quota(dir.path(), Some(10), None).unwrap();

    assert_eq!(report.deleted_clips, 1);
    assert_eq!(report.freed_bytes, 16);
    assert!(!old.exists());
    assert!(!pending.exists());
    assert!(keep.exists());
    assert_eq!(report.status.total_bytes, 10);
}

#[test]
fn enforce_quota_deletes_clip_metadata_sidecar_with_clip() {
    let dir = TestDir::new("clipline-storage", "clip-metadata-delete");
    let old = dir.write("old.mp4", 10);
    let metadata = dir.write("old.clipline.json", 6);
    tick_mtime();
    let keep = write_owned(&dir, "keep.mp4", 10);

    let report = enforce_quota(dir.path(), Some(10), None).unwrap();

    assert_eq!(report.deleted_clips, 1);
    assert_eq!(report.freed_bytes, 16);
    assert!(!old.exists());
    assert!(!metadata.exists());
    assert!(keep.exists());
    assert_eq!(report.status.total_bytes, 10);
}

#[test]
fn enforce_quota_leaves_library_when_protected_clip_alone_exceeds_budget() {
    let dir = TestDir::new("clipline-storage", "protect-fresh");
    let old = write_owned(&dir, "old.mp4", 10);
    tick_mtime();
    let fresh = dir.write("fresh.mp4", 20);

    let report = enforce_quota(dir.path(), Some(15), Some(&fresh)).unwrap();

    assert_eq!(report.deleted_clips, 0);
    assert_eq!(report.freed_bytes, 0);
    assert!(old.exists());
    assert!(fresh.exists());
    assert_eq!(report.status.total_bytes, 30);
    assert!(report.status.is_over_quota());
}

#[test]
fn enforce_quota_counts_active_recording_but_never_deletes_it() {
    let dir = TestDir::new("clipline-storage", "recording-quota");
    let old = write_owned(&dir, "old.mp4", 10);
    tick_mtime();
    let recording = dir.write("session.mp4.recording", 12);
    mark_owned(&recording);
    tick_mtime();
    let keep = write_owned(&dir, "keep.mp4", 5);

    let report = enforce_quota(dir.path(), Some(20), None).unwrap();

    assert_eq!(report.deleted_clips, 1);
    assert!(!old.exists());
    assert!(recording.exists());
    assert!(keep.exists());
    assert_eq!(report.status.clip_count, 1);
    assert_eq!(report.status.total_bytes, 17);
}

#[test]
fn enforce_quota_crosses_folders_and_removes_emptied_session_dirs() {
    let dir = TestDir::new("clipline-storage", "session-gc");
    let old = dir.write("2026-06-11 09-00/old.mp4", 10);
    let old_sidecar = dir.write("2026-06-11 09-00/old.markers.json", 2);
    let old_poster = dir.write("2026-06-11 09-00/old.poster.jpg", 4);
    let old_metadata = dir.write("2026-06-11 09-00/old.clipline.json", 0);
    tick_mtime();
    let legacy = write_owned(&dir, "legacy.mp4", 10);
    tick_mtime();
    let fresh = write_owned(&dir, "2026-06-12 14-30/fresh.mp4", 10);

    let report = enforce_quota(dir.path(), Some(20), None).unwrap();

    assert_eq!(report.deleted_clips, 1);
    assert_eq!(report.freed_bytes, 16);
    assert!(!old.exists());
    assert!(!old_sidecar.exists());
    assert!(!old_poster.exists());
    assert!(!old_metadata.exists());
    assert!(
        !old.parent().unwrap().exists(),
        "emptied session folder must be removed even with a poster sidecar"
    );
    assert!(legacy.exists());
    assert!(fresh.exists());
}

#[test]
fn enforce_quota_keeps_session_dirs_that_still_hold_clips() {
    let dir = TestDir::new("clipline-storage", "session-keep");
    let old = write_owned(&dir, "2026-06-12 14-30/old.mp4", 10);
    tick_mtime();
    let new = write_owned(&dir, "2026-06-12 14-30/new.mp4", 10);

    let report = enforce_quota(dir.path(), Some(10), None).unwrap();

    assert_eq!(report.deleted_clips, 1);
    assert!(!old.exists());
    assert!(new.exists());
    assert!(new.parent().unwrap().exists());
}

#[test]
fn enforce_quota_removes_session_metadata_with_emptied_folder() {
    let dir = TestDir::new("clipline-storage", "session-meta-gc");
    let old = dir.write("2026-06-11 09-00/old.mp4", 30);
    let _ = dir.write("2026-06-11 09-00/old.clipline.json", 0);
    let session_meta = dir.write("2026-06-11 09-00/clipline-session.json", 12);
    tick_mtime();
    let keep = write_owned(&dir, "keep.mp4", 10);

    let report = enforce_quota(dir.path(), Some(20), None).unwrap();

    assert_eq!(report.deleted_clips, 1);
    assert!(!old.exists());
    assert!(!session_meta.exists());
    assert!(
        !old.parent().unwrap().exists(),
        "emptied session folder must disappear with its session metadata"
    );
    assert!(keep.exists());
}

#[test]
fn enforce_quota_removes_orphaned_sidecars_with_emptied_folder() {
    let dir = TestDir::new("clipline-storage", "session-orphan-sidecar-gc");
    let old = write_owned(&dir, "2026-06-11 09-00/old.mp4", 30);
    let orphan = dir.write("2026-06-11 09-00/gone.poster.jpg", 7);
    let session_meta = dir.write("2026-06-11 09-00/clipline-session.json", 12);
    tick_mtime();
    let keep = write_owned(&dir, "keep.mp4", 10);

    let report = enforce_quota(dir.path(), Some(20), None).unwrap();

    assert_eq!(report.deleted_clips, 1);
    assert!(!old.exists());
    assert!(!orphan.exists());
    assert!(!session_meta.exists());
    assert!(
        !old.parent().unwrap().exists(),
        "orphaned leftover metadata must not keep the emptied session folder"
    );
    assert!(keep.exists());
}

#[test]
fn enforce_quota_ignores_symlinked_child_directories() {
    let root = TestDir::new("clipline-storage", "symlink-root");
    let outside = TestDir::new("clipline-storage", "symlink-outside");
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
    let keep = write_owned(&root, "keep.mp4", 10);

    let report = enforce_quota(root.path(), Some(20), None).unwrap();

    assert_eq!(report.deleted_clips, 0);
    assert_eq!(report.status.total_bytes, 10);
    assert!(
        external.exists(),
        "quota GC must not delete managed clips through a linked child directory"
    );
    assert!(keep.exists());
}

#[test]
fn disabled_quota_does_not_delete() {
    let dir = TestDir::new("clipline-storage", "disabled");
    let clip = write_owned(&dir, "clip.mp4", 10);

    let report = enforce_quota(dir.path(), None, None).unwrap();

    assert_eq!(report.deleted_clips, 0);
    assert_eq!(report.freed_bytes, 0);
    assert!(clip.exists());
    assert_eq!(report.status.total_bytes, 10);
}
}
