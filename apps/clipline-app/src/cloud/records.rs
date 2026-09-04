//! Local upload-record lookup, replacement, and post-upload cleanup.
use super::*;

pub(crate) fn existing_retry_status(cloud: &CloudSettings, local_clip_id: &str, path: &str) -> String {
    let existing = cloud.uploads.get(local_clip_id).or_else(|| {
        cloud
            .uploads
            .values()
            .filter(|record| clip_paths_equal(&record.path, path))
            .max_by_key(|record| record.updated_at_unix)
    });
    match existing.map(|record| record.upload_status.as_str()) {
        Some("failed") => "retrying".to_string(),
        Some("uploading") | Some("queued") | Some("processing") => "retrying".to_string(),
        _ => "queued".to_string(),
    }
}

pub(crate) fn windows_clip_path_key(path: &str) -> Option<String> {
    let mut normalized = path.trim().replace('/', "\\");
    let lower = normalized.to_ascii_lowercase();
    if lower.starts_with(r"\\?\unc\") {
        normalized = format!(r"\\{}", &normalized[8..]);
    } else if lower.starts_with(r"\\?\") {
        normalized = normalized[4..].to_string();
    }
    let bytes = normalized.as_bytes();
    let drive_path =
        bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'\\';
    if !drive_path && !normalized.starts_with(r"\\") {
        return None;
    }
    Some(normalized.to_ascii_lowercase())
}

pub(crate) fn clip_paths_equal(left: &str, right: &str) -> bool {
    if left == right {
        return true;
    }
    matches!(
        (windows_clip_path_key(left), windows_clip_path_key(right)),
        (Some(left), Some(right)) if left == right
    )
}

pub(crate) fn existing_uploaded_record(
    cloud: &CloudSettings,
    local_clip_id: Option<&str>,
    path: &str,
) -> Option<CloudUploadRecord> {
    let uploaded = |record: &&CloudUploadRecord| {
        record.remote_clip_id.is_some() && record.upload_status.starts_with("uploaded_")
    };
    if let Some(local_clip_id) = local_clip_id {
        return cloud.uploads.get(local_clip_id).filter(uploaded).cloned();
    }
    cloud
        .uploads
        .values()
        .filter(uploaded)
        .filter(|record| clip_paths_equal(&record.path, path))
        .max_by_key(|record| record.updated_at_unix)
        .cloned()
}

pub(crate) fn cloud_record_for_path(cloud: &CloudSettings, path: &str) -> Option<CloudUploadRecord> {
    cloud
        .uploads
        .values()
        .filter(|record| clip_paths_equal(&record.path, path))
        .max_by_key(|record| record.updated_at_unix)
        .cloned()
}

pub(crate) fn replace_upload_record(cloud: &mut CloudSettings, record: CloudUploadRecord) {
    cloud.uploads.retain(|key, existing| {
        key == &record.local_clip_id || !clip_paths_equal(&existing.path, &record.path)
    });
    cloud.uploads.insert(record.local_clip_id.clone(), record);
}

pub(crate) fn remove_upload_record(cloud: &mut CloudSettings, record: &CloudUploadRecord) {
    cloud.uploads.retain(|key, existing| {
        key != &record.local_clip_id && !clip_paths_equal(&existing.path, &record.path)
    });
}

pub(crate) fn persist_record(state: &RuntimeState, record: &CloudUploadRecord) -> Result<(), String> {
    state.update_cloud(|cloud| {
        replace_upload_record(cloud, record.clone());
    })?;
    Ok(())
}

pub(crate) fn delete_uploaded_local_files(target: &Path, media_root: &Path) -> std::io::Result<()> {
    crate::library::groups::recover_group_order_transaction(media_root)
        .map_err(std::io::Error::other)?;
    let _guard = crate::gc::lock_clip_mutations();
    std::fs::remove_file(target).map_err(|error| {
        std::io::Error::new(
            error.kind(),
            format!("delete uploaded local clip {target:?}: {error}"),
        )
    })?;
    // Sidecars may not exist — ignore missing-file errors.
    let mut first_error = None;
    for sidecar in crate::library::clip_sidecar_paths(target) {
        if let Err(error) = std::fs::remove_file(&sidecar) {
            if error.kind() != std::io::ErrorKind::NotFound && first_error.is_none() {
                first_error = Some(std::io::Error::new(
                    error.kind(),
                    format!("delete uploaded clip sidecar {sidecar:?}: {error}"),
                ));
            }
        }
    }
    if let Some(parent) = target.parent() {
        if let Err(error) =
            clipline_storage::remove_emptied_session_dir_after_clip(parent, media_root)
        {
            first_error.get_or_insert(error);
        }
    }
    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clipline_test_utils::TestDir;

    #[test]
    fn upload_record_supersedes_older_record_for_same_path() {
        let mut cloud = CloudSettings::default();
        cloud.uploads.insert(
            "old".into(),
            upload_record("old", "D:\\Videos\\clip.mp4", "failed", 10),
        );
        cloud.uploads.insert(
            "other".into(),
            upload_record("other", "D:\\Videos\\other.mp4", "uploaded_public", 11),
        );

        let newer = upload_record("new", "D:\\Videos\\clip.mp4", "queued", 12);
        replace_upload_record(&mut cloud, newer.clone());

        assert!(!cloud.uploads.contains_key("old"));
        assert_eq!(cloud.uploads.get("new"), Some(&newer));
        assert_eq!(
            cloud
                .uploads
                .get("other")
                .map(|record| record.path.as_str()),
            Some("D:\\Videos\\other.mp4")
        );
    }

    #[test]
    fn existing_retry_status_uses_same_path_when_audio_selection_changed() {
        let mut cloud = CloudSettings::default();
        cloud.uploads.insert(
            "old".into(),
            upload_record("old", "D:\\Videos\\clip.mp4", "failed", 10),
        );

        assert_eq!(
            existing_retry_status(&cloud, "new", "D:\\Videos\\clip.mp4"),
            "retrying"
        );
        assert_eq!(
            existing_retry_status(&cloud, "new", "D:\\Videos\\other.mp4"),
            "queued"
        );
    }

    #[test]
    fn legacy_windows_canonical_paths_match_library_paths() {
        assert!(clip_paths_equal(
            r"\\?\D:\Videos\Clipline\clip.mp4",
            r"D:\Videos\Clipline\clip.mp4"
        ));
        assert!(clip_paths_equal(
            r"d:/videos/clipline/CLIP.mp4",
            r"D:\Videos\Clipline\clip.mp4"
        ));
        assert!(!clip_paths_equal("/Clips/clip.mp4", "/clips/clip.mp4"));
    }

    #[test]
    fn uploaded_record_lookup_blocks_legacy_path_reupload() {
        let mut cloud = CloudSettings::default();
        let mut record = upload_record(
            "legacy",
            r"\\?\D:\Videos\Clipline\clip.mp4",
            "uploaded_public",
            10,
        );
        record.remote_clip_id = Some("remote-1".into());
        record.remote_url = Some("https://clips.example.com/c/c_existing".into());
        cloud.uploads.insert("legacy".into(), record.clone());

        assert_eq!(
            existing_uploaded_record(&cloud, None, r"D:\Videos\Clipline\clip.mp4"),
            Some(record.clone())
        );
        assert_eq!(
            existing_uploaded_record(
                &cloud,
                Some("different-payload-hash"),
                r"D:\Videos\Clipline\clip.mp4"
            ),
            None,
            "a changed payload at the same path must not reuse an older upload"
        );
    }

    #[test]
    fn uploaded_private_record_without_share_url_blocks_reupload() {
        let mut cloud = CloudSettings::default();
        let mut record = upload_record(
            "private-local",
            r"D:\Videos\Clipline\private.mp4",
            "uploaded_private",
            10,
        );
        record.remote_clip_id = Some("private-remote".into());
        cloud.uploads.insert("private-local".into(), record.clone());

        assert_eq!(
            existing_uploaded_record(
                &cloud,
                Some("private-local"),
                r"D:\Videos\Clipline\private.mp4"
            ),
            Some(record)
        );
    }

    #[test]
    fn delete_uploaded_local_files_removes_poster_sidecar() {
        let dir = test_dir("cloud-delete");
        let clip = dir.join("clip.mp4");
        let markers = clip.with_extension("markers.json");
        let metadata = clip.with_extension("clipline.json");
        let pending_osu = clip.with_extension("osu-enrichment.json");
        let poster = crate::poster::poster_path(&clip);
        std::fs::write(&clip, b"mp4").unwrap();
        std::fs::write(&markers, b"{}").unwrap();
        std::fs::write(&metadata, b"{}").unwrap();
        std::fs::write(&pending_osu, b"{}").unwrap();
        std::fs::write(&poster, b"jpg").unwrap();

        delete_uploaded_local_files(&clip, &dir).unwrap();

        assert!(!clip.exists());
        assert!(!markers.exists());
        assert!(!metadata.exists());
        assert!(!pending_osu.exists());
        assert!(!poster.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn delete_uploaded_local_files_removes_emptied_session_folder() {
        let dir = TestDir::new("clipline-cloud", "delete-empty-session");
        let media = dir.path().join("media");
        let session = media.join("2026-06-12 19-15");
        let clip = session.join("clip.mp4");
        std::fs::create_dir_all(&session).unwrap();
        std::fs::write(&clip, b"mp4").unwrap();
        std::fs::write(clip.with_extension("clipline.json"), b"{}").unwrap();
        std::fs::write(session.join("clipline-session.json"), b"{}").unwrap();

        delete_uploaded_local_files(&clip, &media).unwrap();

        assert!(!clip.exists());
        assert!(
            !session.exists(),
            "emptied session folder should be removed after delete-local-on-upload"
        );
        assert!(media.exists());
    }

    #[test]
    fn local_cleanup_preserves_sidecars_when_primary_deletion_fails() {
        let dir = TestDir::new("clipline-cloud", "delete-primary-first");
        let clip = dir.path().join("clip.mp4");
        let markers = clip.with_extension("markers.json");
        std::fs::create_dir(&clip).unwrap();
        std::fs::write(&markers, b"{}").unwrap();

        delete_uploaded_local_files(&clip, dir.path())
            .expect_err("a directory is not a removable MP4 file");

        assert!(clip.exists());
        assert!(markers.exists());
    }

    #[test]
    fn local_cleanup_reports_sidecar_failure_after_primary_deletion() {
        let dir = TestDir::new("clipline-cloud", "delete-sidecar-error");
        let clip = dir.path().join("clip.mp4");
        let markers = clip.with_extension("markers.json");
        std::fs::write(&clip, b"mp4").unwrap();
        std::fs::create_dir(&markers).unwrap();

        let error = delete_uploaded_local_files(&clip, dir.path())
            .expect_err("sidecar directory must fail");

        assert!(!clip.exists(), "primary deletion happens before sidecars");
        assert!(markers.exists());
        assert!(error.to_string().contains("sidecar"), "{error}");
    }

}