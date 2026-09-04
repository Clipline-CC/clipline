//! Cloud library listing and per-clip status reconciliation.
use super::*;

#[tauri::command]
pub async fn list_cloud_clips(
    state: tauri::State<'_, RuntimeState>,
) -> Result<CloudLibraryListResult, String> {
    let settings = state.settings();
    let cloud = settings.cloud.clone();
    let token_target = cloud
        .credential_target
        .clone()
        .ok_or_else(|| "connect to Clipline Cloud first".to_string())?;
    let token = read_credential(&token_target)?;
    let client = connected_client(&cloud, &token)?;

    let mut page = 1;
    let mut clips = Vec::new();
    let mut remote_ids = BTreeSet::new();
    let mut truncated = false;
    while page <= CLOUD_LIBRARY_MAX_PAGES && clips.len() < CLOUD_LIBRARY_MAX_CLIPS {
        let request = ListClipsRequest {
            sort: Some("uploaded_at_desc".to_string()),
            page: Some(page),
            page_size: Some(CLOUD_LIBRARY_PAGE_SIZE),
            ..Default::default()
        };
        let response: clipline_cloud_api::ClipListResponse = bounded_cloud_json(
            cloud_request(
                client.base_url(),
                Some(&token),
                reqwest::Method::GET,
                "api/v1/clips",
            )?
            .query(&request),
            "list cloud clips",
        )
        .await
        .map_err(cloud_error)?;
        let clip_count = response.clips.len();
        for clip in response.clips {
            if !remote_ids.insert(clip.id.clone()) {
                continue;
            }
            let local_record = clip
                .client_clip_id
                .as_deref()
                .and_then(|local_clip_id| cloud.uploads.get(local_clip_id));
            clips.push(cloud_library_clip_from_summary(&clip, local_record));
            if clips.len() >= CLOUD_LIBRARY_MAX_CLIPS {
                truncated = true;
                break;
            }
        }
        if clip_count < CLOUD_LIBRARY_PAGE_SIZE as usize {
            break;
        }
        page += 1;
        if page > CLOUD_LIBRARY_MAX_PAGES {
            truncated = true;
        }
    }

    Ok(CloudLibraryListResult { clips, truncated })
}

#[tauri::command]
pub async fn sync_cloud_clip_status(
    state: tauri::State<'_, RuntimeState>,
    request: SyncCloudClipStatusRequest,
) -> Result<CloudClipStatusSyncResult, String> {
    let settings = state.settings();
    let cloud = settings.cloud.clone();
    let Some(record) = cloud_record_for_path(&cloud, &request.path) else {
        return Ok(CloudClipStatusSyncResult {
            path: request.path,
            record: None,
            removed: false,
        });
    };
    let Some(remote_clip_id) = record.remote_clip_id.clone() else {
        return Ok(CloudClipStatusSyncResult {
            path: request.path,
            record: Some(record),
            removed: false,
        });
    };
    let token_target = cloud
        .credential_target
        .clone()
        .ok_or_else(|| "connect to Clipline Cloud first".to_string())?;
    let token = read_credential(&token_target)?;
    let client = connected_client(&cloud, &token)?;

    match bounded_cloud_get_clip(&client, &token, &remote_clip_id).await {
        Ok(clip) => {
            let mut updated = record;
            apply_remote_clip_to_record(&mut updated, &clip);
            persist_record(&state, &updated)?;
            Ok(CloudClipStatusSyncResult {
                path: request.path,
                record: Some(updated),
                removed: false,
            })
        }
        Err(error) if cloud_error_is_not_found(&error) => match missing_remote_sync_action(&record)
        {
            MissingRemoteSyncAction::Keep => Ok(CloudClipStatusSyncResult {
                path: request.path,
                record: Some(record),
                removed: false,
            }),
            MissingRemoteSyncAction::ConfirmMissing => {
                let mut updated = record;
                mark_remote_not_found_once(&mut updated);
                persist_record(&state, &updated)?;
                Ok(CloudClipStatusSyncResult {
                    path: request.path,
                    record: Some(updated),
                    removed: false,
                })
            }
            MissingRemoteSyncAction::Remove => {
                state.update_cloud(|cloud| {
                    remove_upload_record(cloud, &record);
                })?;
                Ok(CloudClipStatusSyncResult {
                    path: request.path,
                    record: None,
                    removed: true,
                })
            }
        },
        Err(error) => Err(cloud_error(error)),
    }
}


pub(crate) fn apply_remote_clip_to_record(record: &mut CloudUploadRecord, clip: &ClipDetailResponse) {
    record.visibility = clip.visibility.clone();
    record.remote_clip_id = Some(clip.id.clone());
    record.remote_url = if clip.visibility == "private" {
        None
    } else {
        clip.public_url.clone()
    };
    record.upload_status = upload_status_for_remote_clip(clip);
    record.error = None;
    record.updated_at_unix = unix_now();
}

pub(crate) fn cloud_library_clip_from_summary(
    clip: &ClipSummaryResponse,
    local_record: Option<&CloudUploadRecord>,
) -> CloudLibraryClip {
    CloudLibraryClip {
        remote_clip_id: clip.id.clone(),
        local_clip_id: clip.client_clip_id.clone(),
        path: local_record
            .map(|record| record.path.clone())
            .unwrap_or_default(),
        title: clip.title.clone(),
        remote_url: if clip.visibility == "private" {
            String::new()
        } else {
            clip.public_url.clone().unwrap_or_default()
        },
        visibility: clip.visibility.clone(),
        upload_status: upload_status_for_summary_clip(clip),
        updated_at_unix: datetime_to_unix_seconds(clip.updated_at),
        uploaded_at_unix: clip.uploaded_at.map(datetime_to_unix_seconds),
        duration_ms: clip.duration_ms,
        file_size_bytes: clip.file_size_bytes,
        source_type: clip.source_type.clone(),
    }
}

pub(crate) fn upload_status_for_remote_clip(clip: &ClipDetailResponse) -> String {
    if clip.status != "ready" {
        "uploaded_processing".to_string()
    } else if clip.visibility == "private" {
        "uploaded_private".to_string()
    } else {
        "uploaded_public".to_string()
    }
}

pub(crate) fn upload_status_for_summary_clip(clip: &ClipSummaryResponse) -> String {
    match clip.status.as_str() {
        "failed" => "failed".to_string(),
        "ready" if clip.visibility == "private" => "uploaded_private".to_string(),
        "ready" => "uploaded_public".to_string(),
        _ => "uploaded_processing".to_string(),
    }
}

pub(crate) fn datetime_to_unix_seconds(value: DateTime<Utc>) -> u64 {
    value.timestamp().max(0) as u64
}

pub(crate) fn missing_remote_sync_action(record: &CloudUploadRecord) -> MissingRemoteSyncAction {
    if !record.upload_status.starts_with("uploaded_")
        || record.upload_status == "uploaded_processing"
    {
        return MissingRemoteSyncAction::Keep;
    }
    if record.error.as_deref() == Some(REMOTE_NOT_FOUND_SYNC_MARKER) {
        MissingRemoteSyncAction::Remove
    } else {
        MissingRemoteSyncAction::ConfirmMissing
    }
}

pub(crate) fn mark_remote_not_found_once(record: &mut CloudUploadRecord) {
    record.error = Some(REMOTE_NOT_FOUND_SYNC_MARKER.to_string());
    record.updated_at_unix = unix_now();
}

#[cfg(test)]
mod tests {
    use super::*;
    

    #[test]
    fn cloud_clip_detail_updates_record_visibility_status_and_url() {
        let mut record = upload_record("local", "D:\\Videos\\clip.mp4", "uploaded_public", 10);
        record.remote_clip_id = Some("remote-1".into());
        record.remote_url = Some("https://clips.example.com/old".into());

        apply_remote_clip_to_record(
            &mut record,
            &clip_detail(
                "remote-1",
                "unlisted",
                "ready",
                Some("https://share.example.com/c/1"),
            ),
        );

        assert_eq!(record.visibility, "unlisted");
        assert_eq!(record.upload_status, "uploaded_public");
        assert_eq!(
            record.remote_url.as_deref(),
            Some("https://share.example.com/c/1")
        );
        assert!(record.error.is_none());

        apply_remote_clip_to_record(
            &mut record,
            &clip_detail("remote-1", "private", "ready", None),
        );

        assert_eq!(record.visibility, "private");
        assert_eq!(record.upload_status, "uploaded_private");
        assert_eq!(
            record.remote_url, None,
            "private clip detail must clear a previously saved public share URL"
        );
    }

    #[test]
    fn private_cloud_summary_maps_to_library_clip_without_share_url() {
        let local = upload_record("local-1", "D:\\Videos\\known.mp4", "uploaded_public", 10);

        let entry = cloud_library_clip_from_summary(
            &clip_summary(
                "remote-1",
                Some("local-1"),
                "Server Title",
                "private",
                "ready",
                None,
            ),
            Some(&local),
        );

        assert_eq!(entry.remote_clip_id, "remote-1");
        assert_eq!(entry.local_clip_id.as_deref(), Some("local-1"));
        assert_eq!(entry.path, "D:\\Videos\\known.mp4");
        assert_eq!(entry.title, "Server Title");
        assert_eq!(entry.remote_url, "");
        assert_eq!(entry.visibility, "private");
        assert_eq!(entry.upload_status, "uploaded_private");
        assert_eq!(entry.source_type.as_deref(), Some("replay"));
        assert!(entry.updated_at_unix > 0);
    }

    #[test]
    fn missing_remote_clip_keeps_unconfirmed_and_processing_records() {
        assert_eq!(
            missing_remote_sync_action(&upload_record(
                "local",
                "D:\\Videos\\clip.mp4",
                "uploaded_public",
                10
            )),
            MissingRemoteSyncAction::ConfirmMissing
        );
        assert_eq!(
            missing_remote_sync_action(&upload_record(
                "local",
                "D:\\Videos\\clip.mp4",
                "uploaded_processing",
                10
            )),
            MissingRemoteSyncAction::Keep
        );
        assert_eq!(
            missing_remote_sync_action(&upload_record(
                "local",
                "D:\\Videos\\clip.mp4",
                "processing",
                10
            )),
            MissingRemoteSyncAction::Keep
        );
    }

    #[test]
    fn missing_remote_clip_requires_confirmation_before_removing_finalized_record() {
        let mut record = upload_record("local", "D:\\Videos\\clip.mp4", "uploaded_public", 10);

        assert_eq!(
            missing_remote_sync_action(&record),
            MissingRemoteSyncAction::ConfirmMissing
        );

        mark_remote_not_found_once(&mut record);

        assert_eq!(
            missing_remote_sync_action(&record),
            MissingRemoteSyncAction::Remove
        );
    }

}