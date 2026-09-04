use super::*;

#[tauri::command]
pub async fn rename_clip(
    path: String,
    name: String,
    settings: tauri::State<'_, StorageSettings>,
    _state: tauri::State<'_, crate::app::RuntimeState>,
) -> Result<RenamedClipInfo, String> {
    let source = validate_clip_path(&settings, &path)?;
    let title = normalized_clip_title(&name)?;
    let old_path = path.clone();
    tauri::async_runtime::spawn_blocking(move || rename_clip_title(source, old_path, title))
        .await
        .map_err(|e| format!("rename clip task: {e}"))?
}

#[tauri::command]
pub async fn rename_clip_file(
    path: String,
    name: String,
    settings: tauri::State<'_, StorageSettings>,
    state: tauri::State<'_, crate::app::RuntimeState>,
) -> Result<RenamedClipInfo, String> {
    let source = validate_clip_path(&settings, &path)?;
    let target_name = normalized_clip_file_name(&name)?;
    let old_path = path.clone();
    let renamed = tauri::async_runtime::spawn_blocking(move || {
        rename_clip_files(source, old_path, target_name)
    })
    .await
    .map_err(|e| format!("rename clip task: {e}"))??;

    update_cloud_record_paths(&state, &path, &renamed.path);
    Ok(renamed)
}

pub(crate) fn rename_clip_title(
    source: PathBuf,
    old_path: String,
    title: String,
) -> Result<RenamedClipInfo, String> {
    let _guard = crate::gc::lock_clip_mutations();
    if !source.is_file() {
        return Err("clip no longer exists".into());
    }
    let mut metadata = read_clip_metadata(&source).unwrap_or_default();
    let kind = clip_kind_from_metadata(&source, &metadata).to_string();
    metadata.title = Some(title.clone());
    metadata.kind = Some(kind.clone());
    write_clip_metadata(&source, &metadata)?;
    let name = source
        .file_name()
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or_default();
    Ok(RenamedClipInfo {
        old_path: old_path.clone(),
        path: old_path,
        name,
        title: Some(title),
        kind,
    })
}

pub(crate) fn same_existing_path(first: &Path, second: &Path) -> bool {
    match (first.canonicalize(), second.canonicalize()) {
        (Ok(first), Ok(second)) => first == second,
        _ => first == second,
    }
}

struct PreparedOsuSidecarMove {
    source: PathBuf,
    target: PathBuf,
    staged: PathBuf,
    backup: PathBuf,
}

impl PreparedOsuSidecarMove {
    fn stage(source_clip: &Path, target_clip: &Path) -> Result<Option<Self>, String> {
        let source = crate::osu_enrichment::pending_path(source_clip);
        if !source.exists() {
            return Ok(None);
        }
        let target = crate::osu_enrichment::pending_path(target_clip);
        let target_is_source = same_existing_path(&target, &source);
        if target.exists() && !target_is_source {
            return Err("an osu! enrichment sidecar with that name already exists".into());
        }
        let bytes = std::fs::read(&source)
            .map_err(|error| format!("read osu! enrichment sidecar {source:?}: {error}"))?;
        let mut pending: crate::osu_enrichment::OsuPendingEnrichment =
            serde_json::from_slice(&bytes)
                .map_err(|error| format!("parse osu! enrichment sidecar {source:?}: {error}"))?;
        pending.clip_path = target_clip.display().to_string();
        let staged = target.with_extension("osu-enrichment.rename.tmp");
        let backup = source.with_extension("osu-enrichment.rename.backup");
        if staged.exists() {
            return Err(format!(
                "staged osu! enrichment path already exists: {staged:?}"
            ));
        }
        if backup.exists() {
            return Err(format!(
                "backup osu! enrichment path already exists: {backup:?}"
            ));
        }
        let json = serde_json::to_vec_pretty(&pending)
            .map_err(|error| format!("serialize osu! enrichment sidecar: {error}"))?;
        std::fs::write(&staged, json)
            .map_err(|error| format!("stage osu! enrichment sidecar {staged:?}: {error}"))?;
        Ok(Some(Self {
            source,
            target,
            staged,
            backup,
        }))
    }

    fn commit(&self) -> Result<(), String> {
        std::fs::rename(&self.source, &self.backup)
            .map_err(|error| format!("stage old osu! enrichment sidecar: {error}"))?;
        std::fs::rename(&self.staged, &self.target).map_err(|error| {
            let _ = std::fs::rename(&self.backup, &self.source);
            format!("install renamed osu! enrichment sidecar: {error}")
        })?;
        Ok(())
    }

    fn finish(&self) {
        let _ = std::fs::remove_file(&self.backup);
    }

    fn rollback(&self) {
        let _ = std::fs::remove_file(&self.target);
        if self.backup.exists() && !self.source.exists() {
            let _ = std::fs::rename(&self.backup, &self.source);
        }
        let _ = std::fs::remove_file(&self.staged);
    }
}

impl Drop for PreparedOsuSidecarMove {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.staged);
    }
}

pub(crate) fn rename_clip_files(
    source: PathBuf,
    old_path: String,
    target_name: String,
) -> Result<RenamedClipInfo, String> {
    let _guard = crate::gc::lock_clip_mutations();
    if let Some(error) = crate::cloud_upload::active_upload_source_error(&source) {
        return Err(error);
    }
    let parent = source
        .parent()
        .ok_or_else(|| "clip has no containing folder".to_string())?;
    let target = parent.join(&target_name);
    let source_metadata = clip_metadata_path(&source);
    let target_metadata = clip_metadata_path(&target);
    let metadata = read_clip_metadata(&source).unwrap_or_default();
    let title = clip_title_from_metadata(&metadata);
    let kind = clip_kind_from_metadata(&source, &metadata).to_string();

    let target_is_same_file = same_existing_path(&target, &source);
    if target.exists() && !target_is_same_file {
        return Err("a clip with that name already exists".into());
    }

    let source_markers =
        clipline_storage::clip_sidecar_path(&source, clipline_storage::MARKERS_SUFFIX);
    let target_markers =
        clipline_storage::clip_sidecar_path(&target, clipline_storage::MARKERS_SUFFIX);
    let target_markers_same_file = same_existing_path(&target_markers, &source_markers);
    if source_markers.exists() && target_markers.exists() && !target_markers_same_file {
        return Err("a marker sidecar with that name already exists".into());
    }

    let target_metadata_same_file = same_existing_path(&target_metadata, &source_metadata);
    if source_metadata.exists() && target_metadata.exists() && !target_metadata_same_file {
        return Err("a clip metadata sidecar with that name already exists".into());
    }

    let source_favorite = favorite_marker_path(&source);
    let target_favorite = favorite_marker_path(&target);
    let target_favorite_same_file = same_existing_path(&target_favorite, &source_favorite);
    if source_favorite.exists() && target_favorite.exists() && !target_favorite_same_file {
        return Err("a favorite marker with that name already exists".into());
    }

    let pending_osu_move = PreparedOsuSidecarMove::stage(&source, &target)?;

    if source != target {
        std::fs::rename(&source, &target).map_err(|error| {
            crate::cloud_upload::active_upload_source_error(&source)
                .unwrap_or_else(|| format!("rename clip: {error}"))
        })?;
    }
    if source_markers.exists() && source_markers != target_markers {
        if let Err(error) = std::fs::rename(&source_markers, &target_markers) {
            let _ = std::fs::rename(&target, &source);
            return Err(format!("rename clip markers: {error}"));
        }
    }
    let moved_metadata = source_metadata.exists() && source_metadata != target_metadata;
    if moved_metadata {
        if let Err(error) = std::fs::rename(&source_metadata, &target_metadata) {
            let _ = std::fs::rename(&target_markers, &source_markers);
            let _ = std::fs::rename(&target, &source);
            return Err(format!("rename clip metadata: {error}"));
        }
    }
    let moved_favorite = source_favorite.exists() && source_favorite != target_favorite;
    if moved_favorite {
        if let Err(error) = std::fs::rename(&source_favorite, &target_favorite) {
            rollback_renamed_clip_files(
                &source,
                &target,
                &source_markers,
                &target_markers,
                moved_metadata.then_some((source_metadata.as_path(), target_metadata.as_path())),
                None,
            );
            return Err(format!("rename favorite marker: {error}"));
        }
    }

    if let Some(pending) = &pending_osu_move {
        if let Err(error) = pending.commit() {
            rollback_renamed_clip_files(
                &source,
                &target,
                &source_markers,
                &target_markers,
                moved_metadata.then_some((source_metadata.as_path(), target_metadata.as_path())),
                moved_favorite.then_some((source_favorite.as_path(), target_favorite.as_path())),
            );
            return Err(error);
        }
    }

    let mut target_metadata_value = read_clip_metadata(&target).unwrap_or(metadata);
    target_metadata_value.title = title.clone();
    target_metadata_value.kind = Some(kind.clone());
    if let Err(error) = write_clip_metadata(&target, &target_metadata_value) {
        if let Some(pending) = &pending_osu_move {
            pending.rollback();
        }
        rollback_renamed_clip_files(
            &source,
            &target,
            &source_markers,
            &target_markers,
            moved_metadata.then_some((source_metadata.as_path(), target_metadata.as_path())),
            moved_favorite.then_some((source_favorite.as_path(), target_favorite.as_path())),
        );
        return Err(error);
    }
    if let Some(pending) = &pending_osu_move {
        pending.finish();
    }

    // The poster is a regenerable cache, not user data: move it alongside the
    // clip when we can, otherwise drop the stale one so it rebuilds on demand.
    let source_poster = crate::poster::poster_path(&source);
    if source_poster.exists() {
        let target_poster = crate::poster::poster_path(&target);
        if source_poster != target_poster
            && std::fs::rename(&source_poster, &target_poster).is_err()
        {
            let _ = std::fs::remove_file(&source_poster);
        }
    }

    let new_path = display_renamed_clip_path(&old_path, &target_name, parent);
    Ok(RenamedClipInfo {
        old_path,
        path: new_path,
        name: target_name,
        title,
        kind,
    })
}

pub(crate) fn rollback_renamed_clip_files(
    source: &Path,
    target: &Path,
    source_markers: &Path,
    target_markers: &Path,
    metadata: Option<(&Path, &Path)>,
    favorite: Option<(&Path, &Path)>,
) {
    if let Some((source_favorite, target_favorite)) = favorite {
        if target_favorite.exists() && source_favorite != target_favorite {
            let _ = std::fs::rename(target_favorite, source_favorite);
        }
    }
    if let Some((source_metadata, target_metadata)) = metadata {
        if target_metadata.exists() && source_metadata != target_metadata {
            let _ = std::fs::rename(target_metadata, source_metadata);
        }
    }
    if target_markers.exists() && source_markers != target_markers {
        let _ = std::fs::rename(target_markers, source_markers);
    }
    if target.exists() && source != target {
        let _ = std::fs::rename(target, source);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clipline_test_utils::TestDir;
        #[test]
        fn file_rename_adopts_a_favorite_only_imported_mp4() {
            let dir = TestDir::new("clipline-library", "rename-favorite-imported");
            let source = dir.path().join("vacation.mp4");
            let target = dir.path().join("Vacation renamed.mp4");
            touch_mp4(&source);
            set_clip_favorite_impl(&source, true).unwrap();
            assert_eq!(
                clipline_storage::storage_status(dir.path(), Some(0))
                    .unwrap()
                    .clip_count,
                0
            );

            rename_clip_files(
                source.clone(),
                source.display().to_string(),
                normalized_clip_file_name("Vacation renamed").unwrap(),
            )
            .unwrap();

            assert!(target.exists());
            assert!(is_favorite_clip(&target));
            assert!(!is_favorite_clip(&source));
            assert_eq!(
                clipline_storage::storage_status(dir.path(), Some(0))
                    .unwrap()
                    .clip_count,
                1,
                "editing the file name must remain an explicit adoption boundary"
            );
        }
        #[test]
        fn rename_clip_updates_title_metadata_without_moving_file() {
            let dir = TestDir::new("clipline-library", "rename-title-metadata");
            let root = dir.path().join("media");
            let clip = root.join("2026-07-02").join("session_123.mp4");
            touch_mp4(&clip);

            let result = rename_clip_title(
                clip.clone(),
                clip.display().to_string(),
                "Ranked win vs Lux".to_string(),
            )
            .unwrap();

            assert_eq!(result.old_path, result.path);
            assert_eq!(result.name, "session_123.mp4");
            assert_eq!(result.title.as_deref(), Some("Ranked win vs Lux"));
            assert_eq!(result.kind, "session");
            assert!(clip.exists(), "display title rename must not move the MP4");

            let clips = list_clips_from_dir(root).unwrap().clips;
            assert_eq!(clips.len(), 1);
            assert_eq!(clips[0].name, "session_123.mp4");
            assert_eq!(clips[0].title.as_deref(), Some("Ranked win vs Lux"));
            assert_eq!(clips[0].kind, "session");
        }
        #[test]
        fn rename_clip_file_preserves_kind_and_moves_sidecars() {
            let dir = TestDir::new("clipline-library", "rename-file-sidecars");
            let root = dir.path().join("media");
            let source = root.join("session_123.mp4");
            let target = root.join("Ranked win.mp4");
            touch_mp4(&source);
            std::fs::write(source.with_extension("markers.json"), b"{}").unwrap();
            std::fs::write(crate::poster::poster_path(&source), b"poster").unwrap();

            let result = rename_clip_files(
                source.clone(),
                source.display().to_string(),
                normalized_clip_file_name("Ranked win").unwrap(),
            )
            .unwrap();

            assert_eq!(result.old_path, source.display().to_string());
            assert_eq!(result.path, target.display().to_string());
            assert_eq!(result.name, "Ranked win.mp4");
            assert_eq!(result.title, None);
            assert_eq!(result.kind, "session");
            assert!(!source.exists(), "source MP4 should move");
            assert!(target.exists(), "target MP4 should exist");
            assert!(!source.with_extension("markers.json").exists());
            assert!(target.with_extension("markers.json").exists());
            assert!(!crate::poster::poster_path(&source).exists());
            assert!(crate::poster::poster_path(&target).exists());

            let clips = list_clips_from_dir(root).unwrap().clips;
            assert_eq!(clips.len(), 1);
            assert_eq!(clips[0].name, "Ranked win.mp4");
            assert_eq!(clips[0].title, None);
            assert_eq!(clips[0].kind, "session");
        }
        #[test]
        fn prepared_osu_sidecar_move_commit_then_rollback_restores_exact_source() {
            let dir = TestDir::new("clipline-library", "prepared-osu-rollback");
            let source_clip = dir.path().join("session_1.mp4");
            let target_clip = dir.path().join("Ranked win.mp4");
            let source = crate::osu_enrichment::pending_path(&source_clip);
            let target = crate::osu_enrichment::pending_path(&target_clip);
            let original = serde_json::to_vec(&pending_osu_enrichment(&source_clip)).unwrap();
            std::fs::write(&source, &original).unwrap();

            let prepared = PreparedOsuSidecarMove::stage(&source_clip, &target_clip)
                .unwrap()
                .expect("source pending sidecar should prepare a move");
            let staged = prepared.staged.clone();
            let backup = prepared.backup.clone();

            assert_eq!(std::fs::read(&source).unwrap(), original);
            assert!(staged.exists());
            assert!(!target.exists());
            assert!(!backup.exists());

            prepared.commit().unwrap();

            assert!(!source.exists());
            assert!(target.exists());
            assert!(!staged.exists());
            assert!(backup.exists());
            let moved: crate::osu_enrichment::OsuPendingEnrichment =
                serde_json::from_slice(&std::fs::read(&target).unwrap()).unwrap();
            assert_eq!(moved.clip_path, target_clip.display().to_string());

            prepared.rollback();

            assert!(source.exists());
            assert_eq!(std::fs::read(&source).unwrap(), original);
            assert!(!target.exists());
            assert!(!staged.exists());
            assert!(!backup.exists());
        }
        #[test]
        fn prepared_osu_sidecar_move_commit_then_finish_cleans_backup() {
            let dir = TestDir::new("clipline-library", "prepared-osu-finish");
            let source_clip = dir.path().join("session_1.mp4");
            let target_clip = dir.path().join("Ranked win.mp4");
            let source = crate::osu_enrichment::pending_path(&source_clip);
            let target = crate::osu_enrichment::pending_path(&target_clip);
            std::fs::write(
                &source,
                serde_json::to_vec_pretty(&pending_osu_enrichment(&source_clip)).unwrap(),
            )
            .unwrap();

            let prepared = PreparedOsuSidecarMove::stage(&source_clip, &target_clip)
                .unwrap()
                .expect("source pending sidecar should prepare a move");
            let staged = prepared.staged.clone();
            let backup = prepared.backup.clone();
            prepared.commit().unwrap();

            assert!(target.exists());
            assert!(backup.exists());
            prepared.finish();

            let moved: crate::osu_enrichment::OsuPendingEnrichment =
                serde_json::from_slice(&std::fs::read(&target).unwrap()).unwrap();
            assert_eq!(moved.clip_path, target_clip.display().to_string());
            assert!(!source.exists());
            assert!(!staged.exists());
            assert!(!backup.exists());
        }
        #[test]
        fn prepared_osu_sidecar_move_rejects_staging_path_collision_without_mutation() {
            let dir = TestDir::new("clipline-library", "prepared-osu-staged-collision");
            let source_clip = dir.path().join("session_1.mp4");
            let target_clip = dir.path().join("Ranked win.mp4");
            let source = crate::osu_enrichment::pending_path(&source_clip);
            let target = crate::osu_enrichment::pending_path(&target_clip);
            let staged = target.with_extension("osu-enrichment.rename.tmp");
            let backup = source.with_extension("osu-enrichment.rename.backup");
            let original = serde_json::to_vec(&pending_osu_enrichment(&source_clip)).unwrap();
            std::fs::write(&source, &original).unwrap();
            std::fs::write(&staged, b"occupied staged path").unwrap();

            let error = match PreparedOsuSidecarMove::stage(&source_clip, &target_clip) {
                Ok(_) => panic!("occupied staging path must stop preparation"),
                Err(error) => error,
            };

            assert!(error.contains("staged osu! enrichment path"), "{error}");
            assert_eq!(std::fs::read(&source).unwrap(), original);
            assert_eq!(std::fs::read(&staged).unwrap(), b"occupied staged path");
            assert!(!target.exists());
            assert!(!backup.exists());
        }
        #[test]
        fn prepared_osu_sidecar_move_rejects_backup_path_collision_without_mutation() {
            let dir = TestDir::new("clipline-library", "prepared-osu-backup-collision");
            let source_clip = dir.path().join("session_1.mp4");
            let target_clip = dir.path().join("Ranked win.mp4");
            let source = crate::osu_enrichment::pending_path(&source_clip);
            let target = crate::osu_enrichment::pending_path(&target_clip);
            let staged = target.with_extension("osu-enrichment.rename.tmp");
            let backup = source.with_extension("osu-enrichment.rename.backup");
            let original = serde_json::to_vec(&pending_osu_enrichment(&source_clip)).unwrap();
            std::fs::write(&source, &original).unwrap();
            std::fs::write(&backup, b"occupied backup path").unwrap();

            let error = match PreparedOsuSidecarMove::stage(&source_clip, &target_clip) {
                Ok(_) => panic!("occupied backup path must stop preparation"),
                Err(error) => error,
            };

            assert!(error.contains("backup osu! enrichment path"), "{error}");
            assert_eq!(std::fs::read(&source).unwrap(), original);
            assert_eq!(std::fs::read(&backup).unwrap(), b"occupied backup path");
            assert!(!target.exists());
            assert!(!staged.exists());
        }
        #[test]
        fn prepared_osu_sidecar_move_install_failure_restores_source_and_drop_cleans_stage() {
            let dir = TestDir::new("clipline-library", "prepared-osu-install-failure");
            let source_clip = dir.path().join("session_1.mp4");
            let target_clip = dir.path().join("Ranked win.mp4");
            let source = crate::osu_enrichment::pending_path(&source_clip);
            let target = crate::osu_enrichment::pending_path(&target_clip);
            let original = serde_json::to_vec(&pending_osu_enrichment(&source_clip)).unwrap();
            std::fs::write(&source, &original).unwrap();

            let prepared = PreparedOsuSidecarMove::stage(&source_clip, &target_clip)
                .unwrap()
                .expect("source pending sidecar should prepare a move");
            let staged = prepared.staged.clone();
            let backup = prepared.backup.clone();
            std::fs::create_dir(&target).unwrap();

            let error = prepared.commit().unwrap_err();

            assert!(
                error.contains("install renamed osu! enrichment sidecar"),
                "{error}"
            );
            assert_eq!(std::fs::read(&source).unwrap(), original);
            assert!(target.is_dir());
            assert!(staged.exists());
            assert!(!backup.exists());

            drop(prepared);

            assert_eq!(std::fs::read(&source).unwrap(), original);
            assert!(target.is_dir());
            assert!(!staged.exists());
            assert!(!backup.exists());
        }
        #[test]
        fn prepared_osu_sidecar_move_backup_rename_failure_preserves_source_and_drop_cleans_stage() {
            let dir = TestDir::new("clipline-library", "prepared-osu-backup-rename-failure");
            let source_clip = dir.path().join("session_1.mp4");
            let target_clip = dir.path().join("Ranked win.mp4");
            let source = crate::osu_enrichment::pending_path(&source_clip);
            let target = crate::osu_enrichment::pending_path(&target_clip);
            let original = serde_json::to_vec(&pending_osu_enrichment(&source_clip)).unwrap();
            std::fs::write(&source, &original).unwrap();

            let prepared = PreparedOsuSidecarMove::stage(&source_clip, &target_clip)
                .unwrap()
                .expect("source pending sidecar should prepare a move");
            let staged = prepared.staged.clone();
            let backup = prepared.backup.clone();
            std::fs::create_dir(&backup).unwrap();

            let error = prepared.commit().unwrap_err();

            assert!(
                error.contains("stage old osu! enrichment sidecar"),
                "{error}"
            );
            assert_eq!(std::fs::read(&source).unwrap(), original);
            assert!(!target.exists());
            assert!(staged.exists());
            assert!(backup.is_dir());

            drop(prepared);

            assert_eq!(std::fs::read(&source).unwrap(), original);
            assert!(!target.exists());
            assert!(!staged.exists());
            assert!(backup.is_dir());
            std::fs::remove_dir(&backup).unwrap();
        }
        #[test]
        fn prepared_osu_sidecar_move_drop_cleans_uncommitted_stage_without_mutation() {
            let dir = TestDir::new("clipline-library", "prepared-osu-drop");
            let source_clip = dir.path().join("session_1.mp4");
            let target_clip = dir.path().join("Ranked win.mp4");
            let source = crate::osu_enrichment::pending_path(&source_clip);
            let target = crate::osu_enrichment::pending_path(&target_clip);
            let original = serde_json::to_vec(&pending_osu_enrichment(&source_clip)).unwrap();
            std::fs::write(&source, &original).unwrap();

            let staged;
            let backup;
            {
                let prepared = PreparedOsuSidecarMove::stage(&source_clip, &target_clip)
                    .unwrap()
                    .expect("source pending sidecar should prepare a move");
                staged = prepared.staged.clone();
                backup = prepared.backup.clone();
                assert!(staged.exists());
                assert_eq!(std::fs::read(&source).unwrap(), original);
                assert!(!target.exists());
                assert!(!backup.exists());
            }

            assert_eq!(std::fs::read(&source).unwrap(), original);
            assert!(!target.exists());
            assert!(!staged.exists());
            assert!(!backup.exists());
        }
        #[test]
        fn rename_clip_file_moves_pending_osu_sidecar_and_rewrites_clip_path() {
            let dir = TestDir::new("clipline-library", "rename-osu-pending");
            let source = dir.path().join("session_1.mp4");
            let target = dir.path().join("Ranked win.mp4");
            touch_mp4(&source);
            std::fs::write(
                crate::osu_enrichment::pending_path(&source),
                serde_json::to_vec_pretty(&pending_osu_enrichment(&source)).unwrap(),
            )
            .unwrap();

            rename_clip_files(
                source.clone(),
                source.display().to_string(),
                normalized_clip_file_name("Ranked win").unwrap(),
            )
            .unwrap();

            assert!(!crate::osu_enrichment::pending_path(&source).exists());
            let moved: crate::osu_enrichment::OsuPendingEnrichment = serde_json::from_slice(
                &std::fs::read(crate::osu_enrichment::pending_path(&target)).unwrap(),
            )
            .unwrap();
            assert_eq!(moved.clip_path, target.display().to_string());
        }
        #[test]
        fn rename_clip_file_rejects_malformed_pending_osu_before_moving_mp4() {
            let dir = TestDir::new("clipline-library", "rename-osu-malformed");
            let source = dir.path().join("session_1.mp4");
            touch_mp4(&source);
            std::fs::write(crate::osu_enrichment::pending_path(&source), b"not json").unwrap();

            let error = match rename_clip_files(
                source.clone(),
                source.display().to_string(),
                normalized_clip_file_name("Ranked win").unwrap(),
            ) {
                Ok(_) => panic!("malformed pending enrichment must stop the rename"),
                Err(error) => error,
            };

            assert!(error.contains("osu! enrichment"), "{error}");
            assert!(source.exists());
            assert!(crate::osu_enrichment::pending_path(&source).exists());
            assert!(!dir.path().join("Ranked win.mp4").exists());
        }
        #[test]
        fn rename_clip_file_rejects_pending_osu_destination_collision() {
            let dir = TestDir::new("clipline-library", "rename-osu-collision");
            let source = dir.path().join("session_1.mp4");
            let target = dir.path().join("Ranked win.mp4");
            touch_mp4(&source);
            let source_pending = crate::osu_enrichment::pending_path(&source);
            let target_pending = crate::osu_enrichment::pending_path(&target);
            let original = serde_json::to_vec(&pending_osu_enrichment(&source)).unwrap();
            std::fs::write(&source_pending, &original).unwrap();
            std::fs::write(&target_pending, b"occupied").unwrap();

            let error = match rename_clip_files(
                source.clone(),
                source.display().to_string(),
                normalized_clip_file_name("Ranked win").unwrap(),
            ) {
                Ok(_) => panic!("pending enrichment destination collision must stop the rename"),
                Err(error) => error,
            };

            assert!(error.contains("osu! enrichment sidecar"), "{error}");
            assert!(source.exists());
            assert!(!target.exists());
            assert_eq!(std::fs::read(&source_pending).unwrap(), original);
            assert_eq!(std::fs::read(&target_pending).unwrap(), b"occupied");
            assert!(!target_pending
                .with_extension("osu-enrichment.rename.tmp")
                .exists());
            assert!(!source_pending
                .with_extension("osu-enrichment.rename.backup")
                .exists());
        }
        #[cfg(windows)]
        #[test]
        fn rename_clip_file_case_only_moves_mp4_and_rewrites_pending_osu_path() {
            let dir = TestDir::new("clipline-library", "rename-osu-case-only");
            let source = dir.path().join("session_1.mp4");
            let target = dir.path().join("Session_1.mp4");
            let source_markers = source.with_extension("markers.json");
            let target_markers = target.with_extension("markers.json");
            let source_metadata = clip_metadata_path(&source);
            let target_metadata = clip_metadata_path(&target);
            let marker_bytes = br#"{"marker":"case-only-marker"}"#;
            touch_mp4(&source);
            std::fs::write(&source_markers, marker_bytes).unwrap();
            write_clip_metadata(
                &source,
                &ClipMetadata {
                    title: Some("Case-only metadata".to_string()),
                    kind: Some("session".to_string()),
                    group: None,
                    source_group: None,
                    source_group_fingerprint: None,
                },
            )
            .unwrap();
            let metadata_bytes = std::fs::read(&source_metadata).unwrap();
            std::fs::write(
                crate::osu_enrichment::pending_path(&source),
                serde_json::to_vec_pretty(&pending_osu_enrichment(&source)).unwrap(),
            )
            .unwrap();
            let result = rename_clip_files(
                source.clone(),
                source.display().to_string(),
                normalized_clip_file_name("Session_1").unwrap(),
            )
            .unwrap();

            assert_eq!(result.path, target.display().to_string());
            let names: Vec<String> = std::fs::read_dir(dir.path())
                .unwrap()
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            assert!(names.iter().any(|name| name == "Session_1.mp4"));
            assert!(!names.iter().any(|name| name == "session_1.mp4"));
            assert!(names.iter().any(|name| name == "Session_1.markers.json"));
            assert!(!names.iter().any(|name| name == "session_1.markers.json"));
            assert!(names.iter().any(|name| name == "Session_1.clipline.json"));
            assert!(!names.iter().any(|name| name == "session_1.clipline.json"));
            assert!(names
                .iter()
                .any(|name| name == "Session_1.osu-enrichment.json"));
            assert!(!names
                .iter()
                .any(|name| name == "session_1.osu-enrichment.json"));
            assert_eq!(std::fs::read(&target_markers).unwrap(), marker_bytes);
            assert_eq!(std::fs::read(&target_metadata).unwrap(), metadata_bytes);

            let moved: crate::osu_enrichment::OsuPendingEnrichment = serde_json::from_slice(
                &std::fs::read(crate::osu_enrichment::pending_path(&target)).unwrap(),
            )
            .unwrap();
            assert_eq!(moved.clip_path, target.display().to_string());
            assert!(!crate::osu_enrichment::pending_path(&target)
                .with_extension("osu-enrichment.rename.tmp")
                .exists());
            assert!(!crate::osu_enrichment::pending_path(&source)
                .with_extension("osu-enrichment.rename.backup")
                .exists());
            assert!(!target_metadata.with_extension("clipline.json.tmp").exists());
            assert!(!names.iter().any(|name| name.contains(".rename.")));
        }
        #[test]
        fn rename_clip_file_rolls_back_when_final_metadata_write_fails() {
            let dir = TestDir::new("clipline-library", "rename-file-metadata-rollback");
            let root = dir.path().join("media");
            let source = root.join("session_123.mp4");
            let target = root.join("Ranked win.mp4");
            touch_mp4(&source);
            let original_pending = pending_osu_enrichment(&source);
            std::fs::write(
                crate::osu_enrichment::pending_path(&source),
                serde_json::to_vec_pretty(&original_pending).unwrap(),
            )
            .unwrap();
            std::fs::write(source.with_extension("markers.json"), b"{}").unwrap();
            std::fs::create_dir_all(clip_metadata_path(&target)).unwrap();

            let err = match rename_clip_files(
                source.clone(),
                source.display().to_string(),
                normalized_clip_file_name("Ranked win").unwrap(),
            ) {
                Ok(_) => panic!("metadata write failure should roll back moved clip files"),
                Err(error) => error,
            };

            assert!(
                err.contains("clip metadata"),
                "unexpected rename error: {err}"
            );
            assert!(source.exists(), "source MP4 should be restored");
            assert!(source.with_extension("markers.json").exists());
            assert!(!target.exists(), "target MP4 should be rolled back");
            assert!(!target.with_extension("markers.json").exists());
            assert!(crate::osu_enrichment::pending_path(&source).exists());
            assert!(!crate::osu_enrichment::pending_path(&target).exists());
            let restored: crate::osu_enrichment::OsuPendingEnrichment = serde_json::from_slice(
                &std::fs::read(crate::osu_enrichment::pending_path(&source)).unwrap(),
            )
            .unwrap();
            assert_eq!(restored.clip_path, source.display().to_string());
        }
}
