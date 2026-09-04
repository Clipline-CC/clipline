use super::*;

#[derive(Default, Debug, PartialEq, serde::Serialize, serde::Deserialize, Clone)]
pub(crate) struct ClipMetadata {
    #[serde(default)]
    pub(crate) title: Option<String>,
    #[serde(default)]
    pub(crate) kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) group: Option<ClipGroup>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) source_group: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) source_group_fingerprint: Option<String>,
}

pub(crate) const AUDIO_PREVIEW_CACHE_MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Every file that lives and dies with a clip: the MP4 plus one sidecar per
/// storage-recognized suffix. `with_extension` replaces the extension, so each
/// suffix is written relative to the clip stem.
pub(crate) fn clip_sidecar_paths(
    target: &Path,
) -> [PathBuf; clipline_storage::CLIP_SIDECAR_SUFFIXES.len()] {
    clipline_storage::CLIP_SIDECAR_SUFFIXES
        .map(|suffix| clipline_storage::clip_sidecar_path(target, suffix))
}

pub(crate) fn remove_clip_files(target: &Path, media_root: &Path) -> Result<(), String> {
    let _guard = crate::gc::lock_clip_mutations();
    if let Some(error) = crate::cloud_upload::active_upload_source_error(target) {
        return Err(error);
    }
    std::fs::remove_file(target).map_err(|error| {
        crate::cloud_upload::active_upload_source_error(target).unwrap_or_else(|| error.to_string())
    })?;
    for sidecar in clip_sidecar_paths(target) {
        let _ = std::fs::remove_file(sidecar);
    }
    if let Some(parent) = target.parent() {
        if let Err(error) = remove_emptied_session_dir_after_clip(parent, media_root) {
            tracing::warn!(
                event = "library_session_cleanup_failed",
                session_dir = ?parent,
                error = %error
            );
        }
    }
    Ok(())
}

pub(crate) fn validate_clip_path(
    settings: &StorageSettings,
    path: &str,
) -> Result<PathBuf, String> {
    let clips_dir = settings.clips_dir()?;
    groups::recover_group_order_transaction(&clips_dir)?;
    let dir = clips_dir
        .canonicalize()
        .map_err(|e| e.to_string())?;
    let target = Path::new(path).canonicalize().map_err(|e| e.to_string())?;
    // Legacy clips sit at the root; session clips one folder down.
    let parent_ok = target.parent() == Some(dir.as_path())
        || target.parent().and_then(Path::parent) == Some(dir.as_path());
    if !parent_ok || target.extension().and_then(|e| e.to_str()) != Some("mp4") {
        return Err("refusing to access a clip outside the clips directory".into());
    }
    Ok(target)
}

pub(crate) fn clip_metadata_path(path: &Path) -> PathBuf {
    clipline_storage::clip_sidecar_path(path, clipline_storage::CLIP_OWNERSHIP_MARKER_SUFFIX)
}

pub(crate) fn favorite_marker_path(path: &Path) -> PathBuf {
    clipline_storage::clip_sidecar_path(path, clipline_storage::FAVORITE_MARKER_SUFFIX)
}

pub(crate) fn read_clip_metadata(path: &Path) -> Option<ClipMetadata> {
    std::fs::read_to_string(clip_metadata_path(path))
        .ok()
        .and_then(|json| serde_json::from_str(&json).ok())
}

pub(crate) fn write_clip_metadata(path: &Path, metadata: &ClipMetadata) -> Result<(), String> {
    let target = clip_metadata_path(path);
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create clip metadata folder: {e}"))?;
    }
    let json =
        serde_json::to_vec_pretty(metadata).map_err(|e| format!("serialize clip metadata: {e}"))?;
    let tmp = target.with_extension("clipline.json.tmp");
    let result = (|| {
        let mut file = std::fs::File::create(&tmp)
            .map_err(|error| format!("create clip metadata: {error}"))?;
        file.write_all(&json)
            .map_err(|error| format!("write clip metadata: {error}"))?;
        file.sync_all()
            .map_err(|error| format!("sync clip metadata: {error}"))?;
        replace_clip_metadata(&tmp, &target)?;
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&target)
            .and_then(|file| file.sync_all())
            .map_err(|error| format!("sync replaced clip metadata: {error}"))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(tmp);
    }
    result
}

pub(crate) fn replace_clip_metadata(tmp: &Path, target: &Path) -> Result<(), String> {
    match crate::windows::replace_file(tmp, target) {
        Ok(()) => Ok(()),
        Err(error) if target.is_file() => replace_existing_clip_metadata(tmp, target, error),
        Err(error) => {
            let _ = std::fs::remove_file(tmp);
            Err(format!("replace clip metadata: {error}"))
        }
    }
}

pub(crate) fn replace_existing_clip_metadata(
    tmp: &Path,
    target: &Path,
    original_error: std::io::Error,
) -> Result<(), String> {
    let backup = target.with_extension(format!("json.{}.bak", std::process::id()));
    if backup.exists() {
        if let Err(error) = std::fs::remove_file(&backup) {
            let _ = std::fs::remove_file(tmp);
            return Err(format!(
                "replace clip metadata: {original_error}; remove stale clip metadata backup: {error}"
            ));
        }
    }
    if let Err(error) = crate::windows::replace_file(target, &backup) {
        let _ = std::fs::remove_file(tmp);
        return Err(format!(
            "replace clip metadata: {original_error}; backup existing clip metadata: {error}"
        ));
    }
    if let Err(error) = crate::windows::replace_file(tmp, target) {
        let _ = crate::windows::replace_file(&backup, target);
        let _ = std::fs::remove_file(tmp);
        return Err(format!("replace clip metadata: {error}"));
    }
    let _ = std::fs::remove_file(backup);
    Ok(())
}

pub(crate) fn clip_title_from_metadata(metadata: &ClipMetadata) -> Option<String> {
    metadata
        .title
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

pub(crate) fn clip_title_for_path(path: &Path) -> String {
    let metadata = read_clip_metadata(path).unwrap_or_default();
    clip_title_from_metadata(&metadata).unwrap_or_else(|| {
        path.file_stem()
            .or_else(|| path.file_name())
            .map(|value| value.to_string_lossy().to_string())
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "Clipline clip".to_string())
    })
}

pub(crate) fn clip_kind_from_metadata<'a>(path: &'a Path, metadata: &'a ClipMetadata) -> &'a str {
    metadata
        .kind
        .as_deref()
        .map(str::trim)
        .filter(|value| matches!(*value, "replay" | "session" | "trim" | "compilation"))
        .unwrap_or_else(|| inferred_clip_kind_for_path(path))
}

pub(crate) fn clip_kind_for_path(path: &Path) -> String {
    let metadata = read_clip_metadata(path).unwrap_or_default();
    clip_kind_from_metadata(path, &metadata).to_string()
}

pub(crate) fn is_favorite_clip(path: &Path) -> bool {
    favorite_marker_path(path).is_file()
}

pub(crate) fn display_renamed_clip_path(old_path: &str, name: &str, fallback_parent: &Path) -> String {
    Path::new(old_path)
        .parent()
        .map(|parent| parent.join(name))
        .unwrap_or_else(|| fallback_parent.join(name))
        .display()
        .to_string()
}

pub(crate) fn update_cloud_record_paths(state: &crate::app::RuntimeState, old_path: &str, new_path: &str) {
    if old_path == new_path {
        return;
    }
    if let Err(error) = state.update_cloud(|cloud| {
        for record in cloud.uploads.values_mut() {
            if record.path == old_path {
                record.path = new_path.to_string();
            }
        }
    }) {
        tracing::warn!(event = "renamed_clip_cloud_record_update_failed", error = %error);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clipline_test_utils::TestDir;
        #[test]
        fn remove_clip_files_deletes_clip_metadata_sidecar() {
            let dir = TestDir::new("clipline-library", "delete-clip-metadata");
            let clip = dir.path().join("clip.mp4");
            touch_mp4(&clip);
            std::fs::write(clip.with_extension("markers.json"), b"{}").unwrap();
            std::fs::write(clip_metadata_path(&clip), br#"{"title":"Old title"}"#).unwrap();
            set_clip_favorite_impl(&clip, true).unwrap();
            let favorite = favorite_marker_path(&clip);

            remove_clip_files(&clip, dir.path()).unwrap();

            assert!(!clip.exists());
            assert!(!clip.with_extension("markers.json").exists());
            assert!(!clip_metadata_path(&clip).exists());
            assert!(!favorite.exists());
        }
        #[test]
        fn remove_clip_files_removes_emptied_session_folder() {
            let dir = TestDir::new("clipline-library", "delete-empty-session");
            let media = dir.path().join("media");
            let session = media.join("2026-06-12 19-15");
            let clip = session.join("clip.mp4");
            touch_mp4(&clip);
            std::fs::write(clip.with_extension("markers.json"), b"{}").unwrap();
            std::fs::write(clip_metadata_path(&clip), b"{}").unwrap();
            std::fs::write(
                session.join("clipline-session.json"),
                b"{\"id\":\"league\"}",
            )
            .unwrap();
            let sibling = media.join("2026-06-12 19-16").join("keep.mp4");
            touch_mp4(&sibling);

            remove_clip_files(&clip, &media).unwrap();

            assert!(!clip.exists());
            assert!(
                !session.exists(),
                "emptied session folder should be removed"
            );
            assert!(sibling.exists());
            assert!(sibling.parent().unwrap().exists());
            assert!(media.exists(), "media root must stay");
        }
        #[test]
        fn remove_clip_files_keeps_session_folder_with_remaining_clip() {
            let dir = TestDir::new("clipline-library", "delete-keep-session");
            let media = dir.path().join("media");
            let session = media.join("2026-06-12 19-15");
            let gone = session.join("gone.mp4");
            let keep = session.join("keep.mp4");
            touch_mp4(&gone);
            touch_mp4(&keep);
            std::fs::write(session.join("clipline-session.json"), b"{}").unwrap();

            remove_clip_files(&gone, &media).unwrap();

            assert!(!gone.exists());
            assert!(keep.exists());
            assert!(session.exists());
            assert!(session.join("clipline-session.json").exists());
        }
        #[test]
        fn write_clip_metadata_replaces_existing_sidecar() {
            let dir = TestDir::new("clipline-library", "replace-clip-metadata");
            let clip = dir.path().join("clip.mp4");
            touch_mp4(&clip);

            write_clip_metadata(
                &clip,
                &ClipMetadata {
                    title: Some("First title".to_string()),
                    kind: Some("replay".to_string()),
                    group: None,
                    source_group: None,
                    source_group_fingerprint: None,
                },
            )
            .unwrap();
            write_clip_metadata(
                &clip,
                &ClipMetadata {
                    title: Some("Second title".to_string()),
                    kind: Some("session".to_string()),
                    group: None,
                    source_group: None,
                    source_group_fingerprint: None,
                },
            )
            .unwrap();

            let metadata = read_clip_metadata(&clip).unwrap();
            assert_eq!(metadata.title.as_deref(), Some("Second title"));
            assert_eq!(metadata.kind.as_deref(), Some("session"));
        }
        #[test]
        fn clip_metadata_round_trips_group_membership() {
            let dir = TestDir::new("clipline-library", "group-metadata");
            let clip = dir.path().join("clip.mp4");
            touch_mp4(&clip);

            write_clip_metadata(
                &clip,
                &ClipMetadata {
                    title: Some("Grouped clip".to_string()),
                    kind: Some("trim".to_string()),
                    group: Some(ClipGroup {
                        name: "Highlights".to_string(),
                        order: 2,
                    }),
                    source_group: Some("Highlights".to_string()),
                    source_group_fingerprint: Some("fingerprint".to_string()),
                },
            )
            .unwrap();

            let metadata = read_clip_metadata(&clip).unwrap();
            assert_eq!(metadata.group.as_ref().map(|group| group.name.as_str()), Some("Highlights"));
            assert_eq!(metadata.group.map(|group| group.order), Some(2));
            assert_eq!(metadata.source_group.as_deref(), Some("Highlights"));
            assert_eq!(
                metadata.source_group_fingerprint.as_deref(),
                Some("fingerprint")
            );
        }
        #[test]
        fn validate_clip_path_accepts_root_and_session_clips() {
            let dir = TestDir::new("clipline-library", "validate-accept");
            let root = dir.path().join("media");
            let settings = StorageSettings::new(None, root.clone());

            let legacy = root.join("clip.mp4");
            touch_mp4(&legacy);
            let session = root.join("2026-06-12").join("clip.mp4");
            touch_mp4(&session);

            assert!(validate_clip_path(&settings, legacy.to_str().unwrap()).is_ok());
            assert!(validate_clip_path(&settings, session.to_str().unwrap()).is_ok());
        }
        #[test]
        fn validate_clip_path_rejects_escapes_and_non_mp4() {
            let dir = TestDir::new("clipline-library", "validate-reject");
            let root = dir.path().join("media");
            std::fs::create_dir_all(&root).unwrap();
            let settings = StorageSettings::new(None, root.clone());

            // Two folders below the root — deeper than a session clip.
            let too_deep = root.join("a").join("b").join("clip.mp4");
            touch_mp4(&too_deep);
            assert!(validate_clip_path(&settings, too_deep.to_str().unwrap()).is_err());

            // A sibling directory outside the configured root.
            let outside = dir.path().join("elsewhere").join("clip.mp4");
            touch_mp4(&outside);
            assert!(validate_clip_path(&settings, outside.to_str().unwrap()).is_err());

            // Correct location, wrong extension.
            let not_mp4 = root.join("clip.txt");
            touch_mp4(&not_mp4);
            assert!(validate_clip_path(&settings, not_mp4.to_str().unwrap()).is_err());
        }
}
