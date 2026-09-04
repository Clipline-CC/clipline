use super::*;

#[tauri::command]
pub fn delete_clip(path: String, settings: tauri::State<StorageSettings>) -> Result<(), String> {
    let target = validate_clip_path(&settings, &path)?;
    let media_root = settings.clips_dir()?;
    remove_clip_files(&target, &media_root)
}

/// Marks or unmarks a clip as a favorite. Favorites are never auto-deleted by
/// quota GC, and the Library's Favorites chip isolates them.
#[tauri::command]
pub async fn set_clip_favorite(
    path: String,
    favorite: bool,
    settings: tauri::State<'_, StorageSettings>,
) -> Result<SetClipFavoriteInfo, String> {
    let target = validate_clip_path(&settings, &path)?;
    tauri::async_runtime::spawn_blocking(move || set_clip_favorite_impl(&target, favorite))
        .await
        .map_err(|error| format!("favorite clip task: {error}"))?
}

pub(crate) fn set_clip_favorite_impl(
    target: &Path,
    favorite: bool,
) -> Result<SetClipFavoriteInfo, String> {
    let _guard = crate::gc::lock_clip_mutations();
    if !target.is_file() {
        return Err("clip no longer exists".into());
    }
    let marker = favorite_marker_path(target);
    if favorite {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&marker)
        {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && marker.is_file() => {
            }
            Err(error) => return Err(format!("favorite clip {target:?}: {error}")),
        }
    } else if let Err(error) = std::fs::remove_file(&marker) {
        if error.kind() != std::io::ErrorKind::NotFound {
            return Err(format!("unfavorite clip {target:?}: {error}"));
        }
    }
    Ok(SetClipFavoriteInfo {
        path: target.display().to_string(),
        favorite,
    })
}

/// Every file that lives and dies with a clip: the MP4 plus one sidecar per
/// storage-recognized suffix. `with_extension` replaces the extension, so each
/// suffix is written relative to the clip stem.
/// A bulk-delete result: the paths that were removed and the (path, reason)
/// pairs that could not be. Surface `failed` to the UI so partial success is
/// visible rather than silently swallowed.
#[derive(serde::Serialize)]
pub struct DeletedClipsReport {
    pub deleted: Vec<String>,
    pub failed: Vec<(String, String)>,
}

/// Testable core of [`delete_clips`]: deletes each already-validated clip plus
/// its sidecars (best effort), recording any removal failures. `failed`
/// carries inputs that already failed validation so the caller's report stays
/// complete in one place.
/// Testable core of [`delete_clips`]: deletes each already-validated clip plus
/// its sidecars (best effort), recording any removal failures. `failed`
/// carries inputs that already failed validation so the caller's report stays
/// complete in one place.
pub(crate) fn delete_clips_impl(
    media_root: PathBuf,
    validated: Vec<(String, PathBuf)>,
    mut failed: Vec<(String, String)>,
) -> DeletedClipsReport {
    let mut deleted = Vec::new();
    for (path, target) in validated {
        match remove_clip_files(&target, &media_root) {
            Ok(_) => deleted.push(path),
            Err(e) => failed.push((path, e.to_string())),
        }
    }
    DeletedClipsReport { deleted, failed }
}

/// Delete many clips in one round trip. Validation runs up front while the
/// `StorageSettings` borrow is live; owned `PathBuf`s then move into a single
/// blocking task so the UI does not pay N async hops.
#[tauri::command]
pub async fn delete_clips(
    paths: Vec<String>,
    settings: tauri::State<'_, StorageSettings>,
) -> Result<DeletedClipsReport, String> {
    let mut validated: Vec<(String, PathBuf)> = Vec::with_capacity(paths.len());
    let mut failed: Vec<(String, String)> = Vec::new();
    let media_root = settings.clips_dir()?;
    for path in paths {
        match validate_clip_path(&settings, &path) {
            Ok(target) => validated.push((path, target)),
            Err(e) => failed.push((path, e)),
        }
    }
    let result = tauri::async_runtime::spawn_blocking(move || {
        delete_clips_impl(media_root, validated, failed)
    })
    .await
    .map_err(|e| format!("delete clips task: {e}"))?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clipline_test_utils::TestDir;
        #[test]
        fn set_clip_favorite_impl_sets_and_clears_the_flag() {
            let dir = TestDir::new("clipline-library", "set-favorite");
            let clip = dir.path().join("clip.mp4");
            touch_mp4(&clip);
            clipline_storage::ensure_clip_owned(&clip).unwrap();
            let ownership = std::fs::read(clip_metadata_path(&clip)).unwrap();

            let result = set_clip_favorite_impl(&clip, true).unwrap();
            assert!(result.favorite);
            assert_eq!(result.path, clip.display().to_string());
            assert!(is_favorite_clip(&clip));
            assert_eq!(std::fs::read(clip_metadata_path(&clip)).unwrap(), ownership);

            let result = set_clip_favorite_impl(&clip, false).unwrap();
            assert!(!result.favorite);
            assert!(!is_favorite_clip(&clip));
        }
        #[test]
        fn favorite_only_metadata_never_adopts_an_imported_mp4() {
            let dir = TestDir::new("clipline-library", "favorite-imported");
            let clip = dir.path().join("vacation.mp4");
            touch_mp4(&clip);

            set_clip_favorite_impl(&clip, true).unwrap();
            assert!(is_favorite_clip(&clip));
            assert!(
                !clip_metadata_path(&clip).exists(),
                "favorite state must not reuse the ownership metadata sidecar"
            );
            assert_eq!(
                clipline_storage::storage_status(dir.path(), Some(0))
                    .unwrap()
                    .clip_count,
                0
            );

            set_clip_favorite_impl(&clip, false).unwrap();
            assert_eq!(
                clipline_storage::storage_status(dir.path(), Some(0))
                    .unwrap()
                    .clip_count,
                0
            );
            assert!(clip.exists());

            rename_clip_title(
                clip.clone(),
                clip.display().to_string(),
                "Imported clip".into(),
            )
            .unwrap();
            assert_eq!(
                clipline_storage::storage_status(dir.path(), Some(0))
                    .unwrap()
                    .clip_count,
                1,
                "editing the title keeps the existing explicit adoption behavior"
            );
        }
        #[test]
        fn delete_clips_impl_handles_partial_success_and_sidecars() {
            let dir = TestDir::new("clipline-library", "delete-clips-impl");
            let root = dir.path().join("media");
            std::fs::create_dir_all(&root).unwrap();

            // Two real clips, each with all four sidecars.
            let a = root.join("a.mp4");
            let b = root.join("b.mp4");
            touch_mp4(&a);
            touch_mp4(&b);
            std::fs::write(a.with_extension("markers.json"), b"{}").unwrap();
            std::fs::write(b.with_extension("markers.json"), b"{}").unwrap();
            std::fs::write(clip_metadata_path(&a), b"{}").unwrap();
            std::fs::write(clip_metadata_path(&b), b"{}").unwrap();
            std::fs::write(a.with_extension("osu-enrichment.json"), b"{}").unwrap();
            std::fs::write(b.with_extension("osu-enrichment.json"), b"{}").unwrap();
            std::fs::write(crate::poster::poster_path(&a), b"poster").unwrap();
            std::fs::write(crate::poster::poster_path(&b), b"poster").unwrap();

            // A third clip that should be left untouched (not in the deleted set).
            let c = root.join("c.mp4");
            touch_mp4(&c);
            std::fs::write(c.with_extension("markers.json"), b"{}").unwrap();
            std::fs::write(clip_metadata_path(&c), b"{}").unwrap();
            std::fs::write(c.with_extension("osu-enrichment.json"), b"{}").unwrap();

            let validated = vec![
                (a.to_str().unwrap().to_string(), a.clone()),
                (b.to_str().unwrap().to_string(), b.clone()),
            ];
            // One path already failed validation upstream — passed through as failed.
            let failed_in = vec![("bogus".to_string(), "refused".to_string())];

            let report = delete_clips_impl(root.clone(), validated, failed_in);

            assert_eq!(report.deleted.len(), 2);
            assert_eq!(report.failed.len(), 1);
            assert_eq!(report.failed[0].0, "bogus");
            assert!(!a.exists(), "a.mp4 should be removed");
            assert!(!b.exists(), "b.mp4 should be removed");
            assert!(
                !a.with_extension("markers.json").exists(),
                "a.mp4 markers sidecar should be removed"
            );
            assert!(
                !crate::poster::poster_path(&b).exists(),
                "b.mp4 poster should be removed"
            );
            assert!(
                !clip_metadata_path(&a).exists(),
                "a.mp4 clip metadata should be removed"
            );
            assert!(
                !clip_metadata_path(&b).exists(),
                "b.mp4 clip metadata should be removed"
            );
            assert!(
                !a.with_extension("osu-enrichment.json").exists(),
                "a.mp4 pending osu! sidecar should be removed"
            );
            assert!(
                !b.with_extension("osu-enrichment.json").exists(),
                "b.mp4 pending osu! sidecar should be removed"
            );
            assert!(c.exists(), "c.mp4 must be left untouched");
            assert!(
                c.with_extension("markers.json").exists(),
                "c.mp4 markers sidecar must be left untouched"
            );
            assert!(
                clip_metadata_path(&c).exists(),
                "c.mp4 clip metadata must be left untouched"
            );
            assert!(
                c.with_extension("osu-enrichment.json").exists(),
                "c.mp4 pending osu! sidecar must be left untouched"
            );
        }
}
