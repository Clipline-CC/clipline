//! Cloud connection state, credentials, and bounded API request helpers.
use super::*;

#[tauri::command]
pub fn cloud_status(state: tauri::State<RuntimeState>) -> CloudConnectionStatus {
    if let Err(error) = reconcile_cloud_credential_cleanup(&state) {
        tracing::warn!(event = "cloud_pending_credential_reconcile_failed", error = %error);
    }
    let settings = state.settings();
    connection_status(&settings.cloud)
}

#[tauri::command]
pub async fn cloud_connect(
    state: tauri::State<'_, RuntimeState>,
    request: CloudConnectRequest,
) -> Result<CloudConnectionStatus, String> {
    let visibility = request
        .default_visibility
        .as_deref()
        .map(normalize_cloud_visibility)
        .unwrap_or_else(|| "private".to_string());
    let device_name = request
        .device_name
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(DEFAULT_DEVICE_NAME)
        .to_string();

    let base_url = clipline_cloud_api::validate_cloud_host(
        request.host_url.trim(),
        request.plain_http_confirmed,
    )
    .map_err(cloud_error)?;
    let discovery: DiscoveryResponse = bounded_cloud_json(
        cloud_request(
            &base_url,
            None,
            reqwest::Method::GET,
            ".well-known/clipline-cloud",
        )?,
        "discover Clipline Cloud",
    )
    .await
    .map_err(cloud_error)?;
    clipline_cloud_api::ensure_compatible_discovery(&discovery).map_err(cloud_error)?;
    let device_token: CreateDeviceTokenResponse = bounded_cloud_json(
        cloud_request(
            &base_url,
            None,
            reqwest::Method::POST,
            "api/v1/auth/device-token",
        )?
        .json(&CreateDeviceTokenRequest {
            username: request.username.trim().to_string(),
            password: request.password,
            name: device_name,
        }),
        "create cloud device token",
    )
    .await
    .map_err(cloud_error)?;
    let me: MeResponse = bounded_cloud_json(
        cloud_request(
            &base_url,
            Some(&device_token.token),
            reqwest::Method::GET,
            "api/v1/auth/me",
        )?,
        "load connected cloud identity",
    )
    .await
    .map_err(cloud_error)?;

    let host_url = base_url.as_str().trim_end_matches('/').to_string();
    let public_url = discovery
        .public_url
        .trim()
        .trim_end_matches('/')
        .to_string();
    let target = credential_target(&host_url, &me.user.id);
    let old_target = state.settings().cloud.credential_target;
    let previous_target_secret = read_credential(&target).ok();
    let settings = crate::credential_transaction::write_then_persist(
        &target,
        &me.user.username,
        &device_token.token,
        previous_target_secret.as_deref(),
        write_credential,
        delete_credential_if_present,
        || {
            state.update_cloud(|cloud| {
                let identity_changed = cloud.host_url != host_url
                    || cloud.connected_user_id.as_deref() != Some(me.user.id.as_str());
                cloud.host_url = host_url.clone();
                cloud.public_url = Some(public_url.clone());
                cloud.connected_user_id = Some(me.user.id.clone());
                cloud.connected_username = Some(me.user.username.clone());
                cloud.connected_display_name = me
                    .user
                    .display_name
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string);
                cloud.credential_target = Some(target.clone());
                cloud.default_visibility = visibility.clone();
                if let Some(old) = old_target.as_deref().filter(|old| *old != target) {
                    cloud.credential_cleanup_targets.push(old.to_string());
                }
                if identity_changed {
                    cloud.uploads.clear();
                }
            })
        },
    )?;
    if let Err(error) = reconcile_cloud_credential_cleanup(&state) {
        tracing::warn!(event = "cloud_old_credential_reconcile_failed", error = %error);
    }

    Ok(connection_status(&settings.cloud))
}

#[tauri::command]
pub fn cloud_disconnect(
    state: tauri::State<RuntimeState>,
) -> Result<CloudConnectionStatus, String> {
    let old_target = state.settings().cloud.credential_target;
    let settings = state.update_cloud(|cloud| {
        cloud.connected_user_id = None;
        cloud.connected_username = None;
        cloud.connected_display_name = None;
        if let Some(target) = old_target.clone() {
            cloud.credential_cleanup_targets.push(target);
        }
    })?;
    if let Err(error) = reconcile_cloud_credential_cleanup(&state) {
        tracing::warn!(event = "cloud_disconnected_credential_reconcile_failed", error = %error);
    }
    Ok(connection_status(&settings.cloud))
}

pub(crate) fn reconcile_cloud_credential_cleanup(state: &RuntimeState) -> Result<(), String> {
    let targets = state.settings().cloud.credential_cleanup_targets;
    if targets.is_empty() {
        return Ok(());
    }
    let report =
        crate::credential_transaction::cleanup_targets(targets, delete_credential_if_present);
    let deleted = report.deleted;
    if !deleted.is_empty() {
        state.update_cloud(|cloud| {
            cloud
                .credential_cleanup_targets
                .retain(|target| !deleted.contains(target));
            if cloud.connected_user_id.is_none()
                && cloud
                    .credential_target
                    .as_ref()
                    .is_some_and(|target| deleted.contains(target))
            {
                cloud.credential_target = None;
            }
        })?;
    }
    if report.failures.is_empty() {
        Ok(())
    } else {
        Err(report.failures.join(", "))
    }
}

pub(crate) fn connection_status(cloud: &CloudSettings) -> CloudConnectionStatus {
    let token_present = cloud
        .credential_target
        .as_deref()
        .is_some_and(|target| read_credential(target).is_ok());
    CloudConnectionStatus {
        connected: cloud.connected() && token_present,
        token_present,
        host_url: cloud.host_url.clone(),
        public_url: cloud.public_url.clone(),
        username: cloud.connected_username.clone(),
        display_name: cloud.connected_display_name.clone(),
        user_id: cloud.connected_user_id.clone(),
        default_visibility: cloud.default_visibility.clone(),
        delete_local_after_upload: cloud.delete_local_after_upload,
        auto_upload_rules: cloud.auto_upload_rules,
    }
}

pub(crate) fn connected_client(cloud: &CloudSettings, token: &str) -> Result<CloudClient, String> {
    if !cloud.connected() {
        return Err("connect to Clipline Cloud first".into());
    }
    let base_url =
        clipline_cloud_api::validate_cloud_host(&cloud.host_url, true).map_err(cloud_error)?;
    Ok(CloudClient::with_device_token(base_url, token))
}

pub(crate) fn cloud_request(
    base_url: &reqwest::Url,
    token: Option<&str>,
    method: reqwest::Method,
    path: &str,
) -> Result<reqwest::RequestBuilder, String> {
    let url = base_url
        .join(path.trim_start_matches('/'))
        .map_err(|error| format!("build cloud request URL: {error}"))?;
    let request = crate::bounded_http::control_client()?.request(method, url);
    Ok(match token {
        Some(token) => request.bearer_auth(token),
        None => request,
    })
}

pub(crate) fn cloud_clip_request(
    base_url: &reqwest::Url,
    token: &str,
    method: reqwest::Method,
    clip_id: &str,
    suffix: Option<&str>,
) -> Result<reqwest::RequestBuilder, String> {
    let mut url = base_url
        .join("api/v1/clips/")
        .map_err(|error| format!("build cloud clip URL: {error}"))?;
    {
        let mut segments = url
            .path_segments_mut()
            .map_err(|_| "build cloud clip URL path".to_string())?;
        segments.pop_if_empty().push(clip_id);
        if let Some(suffix) = suffix {
            segments.push(suffix);
        }
    }
    Ok(crate::bounded_http::control_client()?
        .request(method, url)
        .bearer_auth(token))
}

pub(crate) async fn bounded_cloud_json<T: DeserializeOwned>(
    request: reqwest::RequestBuilder,
    context: &str,
) -> Result<T, CloudApiError> {
    let response = request.send().await?;
    let status = response.status();
    if !status.is_success() {
        let message = crate::bounded_http::response_error_message(response, status, context).await;
        return Err(CloudApiError::Api { status, message });
    }
    crate::bounded_http::response_json_limited(
        response,
        crate::bounded_http::CONTROL_JSON_MAX_BYTES,
        context,
    )
    .await
    .map_err(|message| CloudApiError::Api { status, message })
}

pub(crate) async fn bounded_cloud_get_clip(
    client: &CloudClient,
    token: &str,
    clip_id: &str,
) -> Result<ClipDetailResponse, CloudApiError> {
    let request = cloud_clip_request(
        client.base_url(),
        token,
        reqwest::Method::GET,
        clip_id,
        None,
    )
    .map_err(CloudApiError::InvalidUpload)?;
    bounded_cloud_json(request, "get cloud clip").await
}

pub(crate) async fn update_cloud_clip_visibility(
    client: &CloudClient,
    token: &str,
    clip_id: &str,
    visibility: &str,
) -> Result<ClipDetailResponse, CloudApiError> {
    let request = cloud_clip_request(
        client.base_url(),
        token,
        reqwest::Method::POST,
        clip_id,
        Some("visibility"),
    )
    .map_err(CloudApiError::InvalidUpload)?
    .json(&UpdateVisibilityRequest {
        visibility: visibility.to_string(),
    });
    let updated: ClipDetailResponse =
        bounded_cloud_json(request, "update cloud clip visibility").await?;
    match bounded_cloud_get_clip(client, token, clip_id).await {
        Ok(refreshed) => Ok(refreshed),
        Err(error) => {
            tracing::warn!(
                event = "cloud_visibility_refresh_failed",
                clip_id,
                error = %error,
                "visibility changed, but refreshing the canonical public URL failed"
            );
            if updated.visibility != "private" && updated.public_url.is_none() {
                Err(CloudApiError::InvalidUpload(format!(
                    "visibility changed, but refreshing the canonical public URL failed: {error}"
                )))
            } else {
                Ok(updated)
            }
        }
    }
}

pub(crate) fn credential_target(host_url: &str, user_id: &str) -> String {
    format!("Clipline Cloud:{host_url}:{user_id}")
}

pub(crate) fn write_credential(target: &str, username: &str, token: &str) -> Result<(), String> {
    CLOUD_CREDENTIALS.write(target, username, token)
}

pub(crate) fn read_credential(target: &str) -> Result<String, String> {
    CLOUD_CREDENTIALS.read(target)
}

pub(crate) fn delete_credential_if_present(target: &str) -> Result<(), String> {
    CLOUD_CREDENTIALS.delete_if_present(target)
}

pub(crate) fn cloud_error(error: CloudApiError) -> String {
    error.to_string()
}

pub(crate) fn cloud_error_is_not_found(error: &CloudApiError) -> bool {
    matches!(error, CloudApiError::Api { status, .. } if status.as_u16() == 404)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_target_includes_server_and_user() {
        assert_eq!(
            credential_target("https://clips.example.com", "user_1"),
            "Clipline Cloud:https://clips.example.com:user_1"
        );
    }

    #[test]
    fn cloud_connection_status_includes_display_name() {
        let cloud = CloudSettings {
            connected_display_name: Some("Dain".into()),
            connected_username: Some("dain98".into()),
            ..CloudSettings::default()
        };

        let status = connection_status(&cloud);

        assert_eq!(status.display_name.as_deref(), Some("Dain"));
        assert_eq!(status.username.as_deref(), Some("dain98"));
    }

}