//! `upload_clip_to_cloud` command orchestration and post-upload bookkeeping.
use super::*;

#[tauri::command]
pub async fn upload_clip_to_cloud<R: Runtime>(
    app: AppHandle<R>,
    state: tauri::State<'_, RuntimeState>,
    storage: tauri::State<'_, StorageSettings>,
    request: UploadClipCommandRequest,
) -> Result<CloudUploadResult, String> {
    let target = validate_clip_path(&storage, &request.path)?;
    let media_root = storage.media_dir();
    let settings = state.settings();
    let cloud = settings.cloud.clone();

    let meta = std::fs::metadata(&target).map_err(|e| format!("read clip metadata: {e}"))?;
    if meta.len() == 0 {
        return Err("clip file is empty".into());
    }
    let markers = crate::util::read_markers_raw(&target);
    let payload = upload_payload_for_audio_selection_from_path(
        &target,
        markers.as_ref(),
        request.audio_track_ids.as_deref(),
    )
    .await?;
    let payload_meta = tokio::fs::metadata(payload.path())
        .await
        .map_err(|e| format!("read upload payload metadata: {e}"))?;
    let payload_size = payload_meta.len();
    let checksum = crate::cloud_upload::sha256_file(payload.path())
        .await
        .map_err(cloud_error)?;
    let local_clip_id = local_clip_id(&target, &meta, &checksum)?;
    if let Some(record) = existing_uploaded_record(&cloud, Some(&local_clip_id), &request.path) {
        return Ok(CloudUploadResult {
            record,
            clip: None,
            local_deleted: false,
        });
    }
    let token_target = cloud
        .credential_target
        .clone()
        .ok_or_else(|| "connect to Clipline Cloud first".to_string())?;
    let token = read_credential(&token_target)?;
    let client = connected_client(&cloud, &token)?;
    let visibility = request
        .visibility
        .as_deref()
        .map(normalize_cloud_visibility)
        .unwrap_or_else(|| cloud.default_visibility.clone());
    let description = normalize_upload_description(request.description.as_deref());
    let mut record = CloudUploadRecord {
        local_clip_id: local_clip_id.clone(),
        // Store the path exactly as `list_clips` emits it (non-canonical), so the
        // UI can pair this record to its clip row by string equality. `target` is
        // the canonicalized form (`\\?\D:\…` on Windows) and is used only for I/O.
        path: request.path.clone(),
        remote_clip_id: None,
        remote_url: None,
        visibility: visibility.clone(),
        upload_status: existing_retry_status(&cloud, &local_clip_id, &request.path),
        error: None,
        updated_at_unix: unix_now(),
    };
    persist_record(&state, &record)?;
    emit_upload_progress(&app, &record, 0, payload_size, None);

    let upload_request = create_upload_request(UploadRequestInput {
        path: &target,
        meta: &meta,
        file_size_bytes: payload_size,
        duration_ms: clip_duration_ms_file(payload.path(), markers.as_ref()),
        checksum: &checksum,
        visibility: &visibility,
        markers: markers.as_ref(),
        client_clip_id: &local_clip_id,
        title: request.title.as_deref(),
    })?;
    let progress_path = request.path.clone();
    let upload_result = crate::cloud_upload::upload_mp4_file_with_progress(
        &client,
        &token,
        &upload_request,
        description.as_deref(),
        payload.path(),
        |progress| {
            let status = if progress.status == "completed" {
                "processing"
            } else {
                "uploading"
            };
            let event = CloudUploadProgressEvent {
                local_clip_id: local_clip_id.clone(),
                path: progress_path.clone(),
                upload_status: status.to_string(),
                received_size_bytes: progress.received_size_bytes,
                file_size_bytes: progress.file_size_bytes,
                remote_clip_id: Some(progress.clip_id.clone()),
                remote_url: None,
                error: None,
            };
            let _ = app.emit(CLOUD_UPLOAD_PROGRESS_EVENT, event);
        },
    )
    .await;

    let progress = match upload_result {
        Ok(progress) => progress,
        Err(error) => {
            record.upload_status = "failed".to_string();
            record.error = Some(cloud_error(error));
            record.updated_at_unix = unix_now();
            persist_record(&state, &record)?;
            emit_upload_progress(&app, &record, 0, payload_size, record.error.clone());
            return Ok(CloudUploadResult {
                record,
                clip: None,
                local_deleted: false,
            });
        }
    };

    record.remote_clip_id = Some(progress.clip_id.clone());
    record.remote_url = None;
    record.upload_status = "processing".to_string();
    record.error = None;
    record.updated_at_unix = unix_now();
    persist_record(&state, &record)?;
    emit_upload_progress(
        &app,
        &record,
        progress.received_size_bytes,
        progress.file_size_bytes,
        None,
    );

    let clip = match wait_for_ready_clip(&client, &token, &progress.clip_id).await {
        Ok(ReadyClipOutcome::Ready(clip)) => clip,
        Ok(ReadyClipOutcome::Failed(clip)) => {
            apply_remote_clip_to_record(&mut record, &clip);
            record.upload_status = "failed".to_string();
            record.error = Some(
                "cloud upload completed, but cloud media processing failed; the local clip was preserved"
                    .to_string(),
            );
            record.updated_at_unix = unix_now();
            persist_post_upload_record(&app, &state, &record, progress.file_size_bytes)?;
            return Ok(CloudUploadResult {
                record,
                clip: None,
                local_deleted: false,
            });
        }
        Ok(ReadyClipOutcome::TimedOut) => {
            mark_ready_timeout(&mut record);
            persist_post_upload_record(&app, &state, &record, progress.file_size_bytes)?;
            return Ok(CloudUploadResult {
                record,
                clip: None,
                local_deleted: false,
            });
        }
        Err(error) => {
            mark_post_upload_problem(
                &mut record,
                format!(
                    "cloud upload completed, but checking cloud processing failed: {}; the local clip was preserved",
                    cloud_error(error)
                ),
            );
            persist_post_upload_record(&app, &state, &record, progress.file_size_bytes)?;
            return Ok(CloudUploadResult {
                record,
                clip: None,
                local_deleted: false,
            });
        }
    };

    let clip = if visibility == "private" {
        clip
    } else {
        match update_cloud_clip_visibility(&client, &token, &clip.id, &visibility).await {
            Ok(updated) if updated.status == "ready" => updated,
            Ok(updated) => {
                apply_remote_clip_to_record(&mut record, &updated);
                mark_post_upload_problem(
                    &mut record,
                    format!(
                        "cloud upload completed, but visibility update returned status {:?}; the local clip was preserved",
                        updated.status
                    ),
                );
                persist_post_upload_record(&app, &state, &record, progress.file_size_bytes)?;
                return Ok(CloudUploadResult {
                    record,
                    clip: None,
                    local_deleted: false,
                });
            }
            Err(error) => {
                mark_post_upload_problem(
                    &mut record,
                    format!(
                        "cloud upload completed, but updating visibility failed: {}; the local clip was preserved",
                        cloud_error(error)
                    ),
                );
                persist_post_upload_record(&app, &state, &record, progress.file_size_bytes)?;
                return Ok(CloudUploadResult {
                    record,
                    clip: None,
                    local_deleted: false,
                });
            }
        }
    };

    apply_remote_clip_to_record(&mut record, &clip);
    persist_post_upload_record(&app, &state, &record, progress.file_size_bytes)?;

    if cloud.delete_local_after_upload {
        if let Err(error) = verify_ready_cloud_media(&cloud, &token, &clip.id).await {
            mark_post_upload_problem(
                &mut record,
                format!(
                    "cloud reported the upload ready, but its media could not be verified: {error}; the local clip was preserved"
                ),
            );
            persist_post_upload_record(&app, &state, &record, progress.file_size_bytes)?;
            return Ok(CloudUploadResult {
                record,
                clip: Some(clip),
                local_deleted: false,
            });
        }
        let cleanup_result = delete_uploaded_local_files(&target, &media_root);
        let local_deleted = cleanup_result.is_ok() || matches!(target.try_exists(), Ok(false));
        if let Err(error) = cleanup_result {
            record.error = Some(format!(
                "cloud upload is ready, but local cleanup failed: {error}"
            ));
            record.updated_at_unix = unix_now();
            persist_post_upload_record(&app, &state, &record, progress.file_size_bytes)?;
        }
        return Ok(CloudUploadResult {
            record,
            clip: Some(clip),
            local_deleted,
        });
    }

    Ok(CloudUploadResult {
        record,
        clip: Some(clip),
        local_deleted: false,
    })
}

pub(crate) fn mark_ready_timeout(record: &mut CloudUploadRecord) {
    record.upload_status = "uploaded_processing".to_string();
    record.error = Some(format!(
        "cloud upload completed, but cloud processing did not become ready within {} seconds; the local clip was preserved and a public share link will remain unavailable until a later status refresh",
        READY_POLL_ATTEMPTS as u64 * READY_POLL_DELAY.as_secs()
    ));
    record.updated_at_unix = unix_now();
}

pub(crate) fn mark_post_upload_problem(record: &mut CloudUploadRecord, message: String) {
    record.upload_status = "uploaded_processing".to_string();
    record.error = Some(message);
    record.updated_at_unix = unix_now();
}

pub(crate) fn persist_post_upload_record<R: Runtime>(
    app: &AppHandle<R>,
    state: &RuntimeState,
    record: &CloudUploadRecord,
    file_size_bytes: u64,
) -> Result<(), String> {
    persist_record(state, record)?;
    emit_upload_progress(
        app,
        record,
        file_size_bytes,
        file_size_bytes,
        record.error.clone(),
    );
    Ok(())
}

pub(crate) fn emit_upload_progress<R: Runtime>(
    app: &AppHandle<R>,
    record: &CloudUploadRecord,
    received_size_bytes: u64,
    file_size_bytes: u64,
    error: Option<String>,
) {
    let _ = app.emit(
        CLOUD_UPLOAD_PROGRESS_EVENT,
        CloudUploadProgressEvent {
            local_clip_id: record.local_clip_id.clone(),
            path: record.path.clone(),
            upload_status: record.upload_status.clone(),
            received_size_bytes,
            file_size_bytes,
            remote_clip_id: record.remote_clip_id.clone(),
            remote_url: record.remote_url.clone(),
            error,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use clipline_test_utils::TestDir;

    #[test]
    fn upload_metadata_uses_clip_title_and_kind_sidecar() {
        let dir = TestDir::new("clipline-cloud", "clip-metadata-upload");
        let clip = dir.path().join("Ranked win.mp4");
        std::fs::write(&clip, b"mp4").unwrap();
        std::fs::write(
            clip.with_extension("clipline.json"),
            r#"{"title":"Ranked win vs Lux","kind":"session"}"#,
        )
        .unwrap();

        assert_eq!(upload_title(None, &clip), "Ranked win vs Lux");
        assert_eq!(source_type(&clip), "session");

        std::fs::write(
            clip.with_extension("clipline.json"),
            r#"{"title":"Highlights compilation","kind":"compilation"}"#,
        )
        .unwrap();
        assert_eq!(upload_title(None, &clip), "Highlights compilation");
        assert_eq!(source_type(&clip), "compilation");
    }

    #[test]
    fn owned_upload_payload_is_removed_but_original_is_preserved() {
        let dir = TestDir::new("clipline-cloud", "upload-payload-ownership");
        let original = dir.path().join("original.mp4");
        let temporary = dir.path().join("temporary.mp4");
        std::fs::write(&original, b"original").unwrap();
        std::fs::write(&temporary, b"temporary").unwrap();

        drop(UploadPayload::original(&original));
        drop(UploadPayload::owned(temporary.clone()));

        assert!(original.exists());
        assert!(!temporary.exists());
    }

    #[tokio::test]
    async fn selected_audio_upload_uses_and_cleans_file_backed_payload() {
        let dir = TestDir::new("clipline-cloud", "selected-upload-payload");
        let source = dir.path().join("source.mp4");
        std::fs::write(&source, two_audio_mp4()).unwrap();
        let markers = audio_markers();
        let selected = vec!["microphone".to_string()];

        let payload =
            upload_payload_for_audio_selection_from_path(&source, Some(&markers), Some(&selected))
                .await
                .unwrap();
        let payload_path = payload.path().to_path_buf();
        let payload_bytes = std::fs::read(&payload_path).unwrap();

        assert_ne!(payload_path, source);
        assert_ne!(
            payload_path.parent(),
            source.parent(),
            "upload payloads must not keep a source session folder alive"
        );
        assert!(payload_bytes.windows(6).any(|window| window == b"V00000"));
        assert!(!payload_bytes.windows(6).any(|window| window == b"A00000"));
        assert!(payload_bytes.windows(6).any(|window| window == b"B00000"));
        drop(payload);
        assert!(!payload_path.exists());
        assert!(source.exists());
    }

    #[test]
    fn abandoned_upload_payload_prune_is_scoped_and_age_gated() {
        let dir = TestDir::new("clipline-cloud", "upload-payload-prune");
        let abandoned = dir.path().join("clipline-upload-1-1.mp4.tmp");
        let active = dir.path().join("clipline-upload-1-2.mp4.tmp");
        let unrelated = dir.path().join("editor.tmp");
        for path in [&abandoned, &active, &unrelated] {
            std::fs::write(path, b"temp").unwrap();
        }
        std::fs::File::options()
            .write(true)
            .open(&abandoned)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(1))
            .unwrap();

        prune_abandoned_upload_payloads(dir.path());

        assert!(!abandoned.exists());
        assert!(active.exists());
        assert!(unrelated.exists());
    }

    #[test]
    fn ready_timeout_keeps_remote_identity_without_fabricating_share_url() {
        let mut record = upload_record("local", "D:\\Videos\\clip.mp4", "processing", 10);
        record.remote_clip_id = Some("remote-1".into());

        mark_ready_timeout(&mut record);

        assert_eq!(record.upload_status, "uploaded_processing");
        assert_eq!(record.remote_clip_id.as_deref(), Some("remote-1"));
        assert_eq!(record.remote_url, None);
        assert!(
            record
                .error
                .as_deref()
                .is_some_and(|error| error.contains("processing") && !error.contains("retry the upload")),
            "timeout should explain that cloud processing is still pending without forcing a reupload"
        );
    }

    #[test]
    fn post_upload_problem_keeps_remote_identity_for_reconciliation() {
        let mut record = upload_record("local", "D:\\Videos\\clip.mp4", "processing", 10);
        record.remote_clip_id = Some("remote-1".into());
        record.remote_url = Some("https://clips.example.com/c/c_existing".into());

        mark_post_upload_problem(&mut record, "visibility update failed".into());

        assert_eq!(record.upload_status, "uploaded_processing");
        assert_eq!(record.remote_clip_id.as_deref(), Some("remote-1"));
        assert_eq!(
            record.remote_url.as_deref(),
            Some("https://clips.example.com/c/c_existing")
        );
        assert_eq!(record.error.as_deref(), Some("visibility update failed"));
    }

}