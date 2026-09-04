//! Cloud clip asset download, cache-backed open, and asset URLs.
use super::*;

#[tauri::command]
pub async fn cloud_clip_thumbnail<R: Runtime>(
    app: AppHandle<R>,
    state: tauri::State<'_, RuntimeState>,
    request: CloudClipAssetRequest,
) -> Result<Option<String>, String> {
    let (cloud, token) = cloud_asset_context(&state)?;
    let Some(path) = download_cloud_asset_to_cache(
        &cloud,
        &token,
        CloudAssetDownload {
            remote_clip_id: &request.remote_clip_id,
            asset: "thumbnail",
            extension: "jpg",
            version: request.updated_at_unix,
            expected_size_bytes: None,
            max_size_bytes: CLOUD_THUMBNAIL_MAX_BYTES,
            missing_ok: true,
        },
    )
    .await?
    else {
        return Ok(None);
    };
    allow_cloud_cache_asset(&app, &path)?;
    Ok(Some(path.display().to_string()))
}

#[tauri::command]
pub async fn cache_cloud_clip_media<R: Runtime>(
    app: AppHandle<R>,
    state: tauri::State<'_, RuntimeState>,
    request: CloudClipAssetRequest,
) -> Result<CachedCloudClip, String> {
    let (cloud, token) = cloud_asset_context(&state)?;
    let path = download_cloud_asset_to_cache(
        &cloud,
        &token,
        CloudAssetDownload {
            remote_clip_id: &request.remote_clip_id,
            asset: "media",
            extension: "mp4",
            version: request.updated_at_unix,
            expected_size_bytes: request.file_size_bytes,
            max_size_bytes: cloud_media_cache_max_bytes(request.file_size_bytes),
            missing_ok: false,
        },
    )
    .await?
    .ok_or_else(|| "cloud clip media is not available".to_string())?;
    allow_cloud_cache_asset(&app, &path)?;
    cached_cloud_clip_from_path(&path, &request)
}

#[tauri::command]
pub fn open_cloud_clip(
    state: tauri::State<RuntimeState>,
    remote_clip_id: String,
) -> Result<(), String> {
    let cloud = state.settings().cloud;
    let url = cloud_owner_clip_page_url(&cloud, &remote_clip_id)?;
    open_cloud_url(url.as_str(), "cloud clip page")
}

pub(crate) fn open_cloud_url(url: &str, context: &str) -> Result<(), String> {
    crate::windows::open_with_shell(std::ffi::OsStr::new(url), context)
}

pub(crate) fn cloud_asset_context(
    state: &tauri::State<'_, RuntimeState>,
) -> Result<(CloudSettings, String), String> {
    let cloud = state.settings().cloud;
    let token_target = cloud
        .credential_target
        .as_deref()
        .ok_or_else(|| "Clipline Cloud is not connected".to_string())?;
    let token = read_credential(token_target)?;
    Ok((cloud, token))
}


pub(crate) async fn download_cloud_asset_to_cache(
    cloud: &CloudSettings,
    token: &str,
    request: CloudAssetDownload<'_>,
) -> Result<Option<PathBuf>, String> {
    let cache_root = prepare_cloud_cache_root()?;
    let target = cloud_clip_cache_path(
        cloud,
        request.remote_clip_id,
        request.asset,
        request.extension,
        request.version,
    )?;
    lease_cloud_cache_path(&target);
    prune_cloud_cache_for_download(&cache_root, 0, std::slice::from_ref(&target))?;
    if cached_asset_matches(&target, request.expected_size_bytes) {
        touch_cloud_cache_entry(&target)?;
        return Ok(Some(target));
    }
    let url = cloud_clip_asset_url(cloud, request.remote_clip_id, request.asset)?;
    if let Some(parent) = target.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("create cloud cache: {e}"))?;
    }

    let response = crate::bounded_http::authenticated_stream_client()?
        .get(url)
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| format!("download cloud {}: {e}", request.asset))?;
    let status = response.status();
    if status == reqwest::StatusCode::NOT_FOUND && request.missing_ok {
        return Ok(None);
    }
    if !status.is_success() {
        let message = crate::bounded_http::response_error_message(
            response,
            status,
            &format!("cloud {}", request.asset),
        )
        .await;
        return Err(format!(
            "download cloud {} failed with {status}: {message}",
            request.asset
        ));
    }
    if response
        .content_length()
        .is_some_and(|length| length > request.max_size_bytes)
    {
        return Err(format!(
            "download cloud {} is too large (limit {:.1} MB)",
            request.asset,
            request.max_size_bytes as f64 / (1024.0 * 1024.0)
        ));
    }
    let reservation = response
        .content_length()
        .unwrap_or(request.max_size_bytes)
        .min(request.max_size_bytes);
    prune_cloud_cache_for_download(&cache_root, reservation, std::slice::from_ref(&target))?;

    let tmp = cloud_clip_cache_tmp_path(&target)?;
    let mut response = response;
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .await
        .map_err(|e| format!("create cloud cache file: {e}"))?;
    let mut tmp_owner = OwnedCloudCacheTemp::new(tmp.clone());
    let mut written = 0_u64;
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| format!("read cloud {}: {e}", request.asset))?
    {
        written += chunk.len() as u64;
        if written > request.max_size_bytes {
            return Err(format!(
                "download cloud {} is too large (limit {:.1} MB)",
                request.asset,
                request.max_size_bytes as f64 / (1024.0 * 1024.0)
            ));
        }
        file.write_all(&chunk)
            .await
            .map_err(|e| format!("write cloud cache file: {e}"))?;
    }
    file.flush()
        .await
        .map_err(|e| format!("flush cloud cache file: {e}"))?;
    drop(file);
    if written == 0 {
        return Err(format!(
            "download cloud {} returned an empty body",
            request.asset
        ));
    }

    {
        let _cache_guard = cloud_cache_lock()
            .lock()
            .map_err(|_| "cloud cache lock is poisoned".to_string())?;
        if target.exists() && !cached_asset_matches(&target, request.expected_size_bytes) {
            let _ = std::fs::remove_file(cloud_cache_marker_path(&target));
            let _ = std::fs::remove_file(&target);
        }
        let mut protected = leased_cloud_cache_paths();
        protected.push(target.clone());
        protected.push(tmp.clone());
        let free = cloud_cache_available_space(&cache_root)?;
        enforce_cloud_cache_limits(
            &cache_root,
            CLOUD_CACHE_MAX_AGE,
            CLOUD_CACHE_QUOTA_BYTES,
            free,
            CLOUD_CACHE_FREE_SPACE_FLOOR_BYTES,
            written,
            &protected,
        )?;
    }

    match tokio::fs::rename(&tmp, &target).await {
        Ok(()) => {
            tmp_owner.disarm();
            write_cloud_cache_marker(&target, written).await?;
            touch_cloud_cache_entry(&target)?;
            lease_cloud_cache_path(&target);
            Ok(Some(target))
        }
        Err(error) if target.exists() => {
            if cached_asset_matches(&target, request.expected_size_bytes) {
                touch_cloud_cache_entry(&target)?;
                Ok(Some(target))
            } else {
                Err(format!("finalize cloud cache file: {error}"))
            }
        }
        Err(error) => Err(format!("finalize cloud cache file: {error}")),
    }
}

pub(crate) fn cloud_clip_asset_url(
    cloud: &CloudSettings,
    remote_clip_id: &str,
    asset: &str,
) -> Result<reqwest::Url, String> {
    let remote_clip_id = validate_cloud_cache_component(remote_clip_id, "remote clip id")?;
    let asset = validate_cloud_cache_component(asset, "cloud asset")?;
    let base =
        clipline_cloud_api::validate_cloud_host(&cloud.host_url, true).map_err(cloud_error)?;
    base.join(&format!("api/v1/clips/{remote_clip_id}/{asset}"))
        .map_err(|e| format!("cloud asset URL is invalid: {e}"))
}

pub(crate) fn allow_cloud_cache_asset<R: Runtime>(app: &AppHandle<R>, path: &Path) -> Result<(), String> {
    let cache_dir = prepare_cloud_cache_root()?;
    let canonical_dir = cache_dir
        .canonicalize()
        .map_err(|e| format!("canonicalize cloud cache {cache_dir:?}: {e}"))?;
    let canonical_path = path
        .canonicalize()
        .map_err(|e| format!("canonicalize cloud cache asset {path:?}: {e}"))?;
    if !canonical_path.starts_with(&canonical_dir) {
        return Err(format!(
            "cloud cache asset {canonical_path:?} escaped cache {canonical_dir:?}"
        ));
    }
    app.asset_protocol_scope()
        .allow_file(&canonical_path)
        .map_err(|e| format!("scope cloud cache asset for playback: {e}"))
}

pub(crate) fn cached_cloud_clip_from_path(
    path: &Path,
    request: &CloudClipAssetRequest,
) -> Result<CachedCloudClip, String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("read cached cloud clip: {e}"))?;
    let modified_unix = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or_else(unix_now);
    let title = request
        .title
        .as_deref()
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .unwrap_or("Cloud clip");
    let name = if title.to_ascii_lowercase().ends_with(".mp4") {
        title.to_string()
    } else {
        format!("{title}.mp4")
    };
    Ok(CachedCloudClip {
        path: path.display().to_string(),
        name,
        size_mb: meta.len() as f64 / (1024.0 * 1024.0),
        modified_unix,
        duration_s: request
            .duration_ms
            .filter(|duration| *duration >= 0)
            .map(|duration| duration as f64 / 1000.0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use clipline_test_utils::TestDir;

    #[test]
    fn cloud_clip_asset_url_uses_api_host_and_safe_clip_ids() {
        let cloud = CloudSettings {
            host_url: "https://clips.example.com/base".into(),
            ..CloudSettings::default()
        };
        let url = cloud_clip_asset_url(&cloud, "remote-1_ABC", "media").expect("asset URL");
        assert_eq!(
            url.as_str(),
            "https://clips.example.com/base/api/v1/clips/remote-1_ABC/media"
        );
        assert!(cloud_clip_asset_url(&cloud, "../escape", "media").is_err());
        assert!(cloud_clip_asset_url(&cloud, "remote/escape", "thumbnail").is_err());
    }

    #[test]
    fn cached_asset_marker_accepts_actual_download_size() {
        let dir = TestDir::new("clipline-cloud", "cached-asset-marker");
        let asset = dir.path().join("remote-media-42.mp4");
        std::fs::write(&asset, b"served bytes").unwrap();
        std::fs::write(cloud_cache_marker_path(&asset), b"12").unwrap();

        assert!(
            cached_asset_matches(&asset, Some(999)),
            "a completed cloud-cache download should not be invalidated by a stale server size"
        );
    }

    #[test]
    fn cloud_clip_cache_path_is_namespaced_by_account() {
        let first = CloudSettings {
            host_url: "https://clips.example.com".into(),
            connected_user_id: Some("user-1".into()),
            ..CloudSettings::default()
        };
        let second = CloudSettings {
            host_url: "https://clips.example.com".into(),
            connected_user_id: Some("user-2".into()),
            ..CloudSettings::default()
        };

        let first_path =
            cloud_clip_cache_path(&first, "remote-1", "media", "mp4", Some(1)).unwrap();
        let second_path =
            cloud_clip_cache_path(&second, "remote-1", "media", "mp4", Some(1)).unwrap();

        assert_ne!(first_path.parent(), second_path.parent());
        assert_eq!(
            first_path.file_name().and_then(|name| name.to_str()),
            second_path.file_name().and_then(|name| name.to_str())
        );
    }

    #[test]
    fn cloud_clip_cache_path_keeps_remote_ids_inside_cache() {
        let cloud = CloudSettings {
            host_url: "https://clips.example.com".into(),
            connected_user_id: Some("user-1".into()),
            ..CloudSettings::default()
        };
        let path = cloud_clip_cache_path(&cloud, "remote-1_ABC", "media", "mp4", Some(42))
            .expect("cache path");
        assert!(path.ends_with("remote-1_ABC-media-42.mp4"));
        assert!(cloud_clip_cache_path(&cloud, "../escape", "media", "mp4", None).is_err());
        assert!(cloud_clip_cache_path(&cloud, "remote-1", "../asset", "mp4", None).is_err());
    }

}