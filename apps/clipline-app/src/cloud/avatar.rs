//! Cloud user profile and avatar fetch/cache.
use super::*;

#[tauri::command]
pub async fn cloud_user_avatar(
    state: tauri::State<'_, RuntimeState>,
) -> Result<Option<String>, String> {
    let (cloud, token) = cloud_asset_context(&state)?;
    let cache_key = cloud_user_avatar_cache_key(&cloud)?;
    let cached = cached_cloud_user_avatar(&cache_key);
    let url = cloud_user_avatar_url(&cloud)?;
    let mut request = crate::bounded_http::authenticated_stream_client()?
        .get(url)
        .bearer_auth(token);
    if let Some(etag) = cached.as_ref().and_then(|avatar| avatar.etag.as_deref()) {
        request = request.header(reqwest::header::IF_NONE_MATCH, etag);
    }

    let response = request
        .send()
        .await
        .map_err(|e| format!("download cloud avatar: {e}"))?;
    let status = response.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        clear_cached_cloud_user_avatar(&cache_key);
        return Ok(None);
    }
    if status == reqwest::StatusCode::NOT_MODIFIED {
        return Ok(cached.map(|avatar| avatar.data_url));
    }
    if !status.is_success() {
        let message =
            crate::bounded_http::response_error_message(response, status, "cloud avatar").await;
        return Err(format!(
            "download cloud avatar failed with {status}: {message}"
        ));
    }
    if response
        .content_length()
        .is_some_and(|length| length as usize > MAX_AVATAR_BYTES)
    {
        return Err("cloud avatar is too large".to_string());
    }

    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let etag = response
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let bytes =
        crate::bounded_http::response_bytes_limited(response, MAX_AVATAR_BYTES, "cloud avatar")
            .await?;
    let data_url = cloud_user_avatar_data_url(content_type.as_deref(), &bytes)?;
    store_cached_cloud_user_avatar(CachedCloudUserAvatar {
        key: cache_key,
        etag,
        data_url: data_url.clone(),
    });
    Ok(Some(data_url))
}

#[tauri::command]
pub async fn cloud_user_profile(
    state: tauri::State<'_, RuntimeState>,
) -> Result<CloudUserProfile, String> {
    let (cloud, token) = cloud_asset_context(&state)?;
    let client = connected_client(&cloud, &token)?;
    let response: MeResponse = bounded_cloud_json(
        cloud_request(
            client.base_url(),
            Some(&token),
            reqwest::Method::GET,
            "api/v1/auth/me",
        )?,
        "load cloud profile",
    )
    .await
    .map_err(cloud_error)?;
    let profile = cloud_user_profile_from_response(&cloud, &response.user)?;
    let profile_for_settings = profile.clone();
    let _settings = state.update_cloud(|cloud| {
        cloud.connected_user_id = Some(profile_for_settings.user_id.clone());
        cloud.connected_username = Some(profile_for_settings.username.clone());
        cloud.connected_display_name = profile_for_settings.display_name.clone();
    })?;
    Ok(profile)
}

#[tauri::command]
pub fn open_cloud_user_profile(state: tauri::State<RuntimeState>) -> Result<(), String> {
    let cloud = state.settings().cloud;
    let username = cloud
        .connected_username
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "Clipline Cloud username is unknown".to_string())?;
    let url = cloud_user_profile_url(&cloud, username)?;
    open_cloud_url(url.as_str(), "cloud user profile")
}


pub(crate) fn cloud_user_avatar_url(cloud: &CloudSettings) -> Result<reqwest::Url, String> {
    let base =
        clipline_cloud_api::validate_cloud_host(&cloud.host_url, true).map_err(cloud_error)?;
    base.join("api/v1/me/avatar")
        .map_err(|e| format!("cloud avatar URL is invalid: {e}"))
}

pub(crate) fn cloud_user_profile_from_response(
    cloud: &CloudSettings,
    user: &clipline_cloud_api::UserResponse,
) -> Result<CloudUserProfile, String> {
    let display_name = user
        .display_name
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    Ok(CloudUserProfile {
        user_id: user.id.clone(),
        username: user.username.clone(),
        display_name,
        profile_url: cloud_user_profile_url(cloud, &user.username)?.to_string(),
    })
}

pub(crate) fn cloud_user_profile_url(cloud: &CloudSettings, username: &str) -> Result<reqwest::Url, String> {
    let username = username.trim();
    if username.is_empty() {
        return Err("Clipline Cloud username is unknown".to_string());
    }
    let base = cloud.public_url.as_deref().unwrap_or(&cloud.host_url);
    let mut url = clipline_cloud_api::validate_cloud_host(base, true).map_err(cloud_error)?;
    url = url
        .join("u/")
        .map_err(|e| format!("cloud user profile URL is invalid: {e}"))?;
    url.path_segments_mut()
        .map_err(|_| "cloud user profile URL cannot be a base".to_string())?
        .pop_if_empty()
        .push(username);
    Ok(url)
}

pub(crate) fn cloud_user_avatar_cache_key(cloud: &CloudSettings) -> Result<String, String> {
    let base =
        clipline_cloud_api::validate_cloud_host(&cloud.host_url, true).map_err(cloud_error)?;
    let user_id = cloud
        .connected_user_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "Clipline Cloud user is unknown".to_string())?;
    Ok(format!("{}|{user_id}", base.as_str().trim_end_matches('/')))
}

pub(crate) fn cloud_user_avatar_data_url(content_type: Option<&str>, bytes: &[u8]) -> Result<String, String> {
    if bytes.is_empty() {
        return Err("cloud avatar returned an empty body".to_string());
    }
    if bytes.len() > MAX_AVATAR_BYTES {
        return Err("cloud avatar is too large".to_string());
    }
    let mime = content_type
        .and_then(|value| value.split(';').next())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("image/jpeg")
        .to_ascii_lowercase();
    if !mime.starts_with("image/") {
        return Err(format!("cloud avatar response is not an image: {mime}"));
    }
    Ok(format!(
        "data:{mime};base64,{}",
        general_purpose::STANDARD.encode(bytes)
    ))
}

pub(crate) fn cloud_user_avatar_cache() -> &'static Mutex<Option<CachedCloudUserAvatar>> {
    CLOUD_USER_AVATAR_CACHE.get_or_init(|| Mutex::new(None))
}

pub(crate) fn cached_cloud_user_avatar(key: &str) -> Option<CachedCloudUserAvatar> {
    cloud_user_avatar_cache()
        .lock()
        .ok()
        .and_then(|avatar| avatar.as_ref().filter(|cached| cached.key == key).cloned())
}

pub(crate) fn store_cached_cloud_user_avatar(avatar: CachedCloudUserAvatar) {
    if let Ok(mut cached) = cloud_user_avatar_cache().lock() {
        *cached = Some(avatar);
    }
}

pub(crate) fn clear_cached_cloud_user_avatar(key: &str) {
    if let Ok(mut cached) = cloud_user_avatar_cache().lock() {
        if cached.as_ref().is_some_and(|avatar| avatar.key == key) {
            *cached = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloud_user_avatar_data_url_requires_image_content_type() {
        assert_eq!(
            cloud_user_avatar_data_url(Some("image/png"), b"\x01\x02\x03").unwrap(),
            "data:image/png;base64,AQID"
        );
        assert!(
            cloud_user_avatar_data_url(Some("text/html"), b"<script>").is_err(),
            "avatar data URLs must only accept image responses"
        );
        assert!(
            cloud_user_avatar_data_url(Some("image/png"), b"").is_err(),
            "empty avatar bodies should not render as broken images"
        );
    }

    #[test]
    fn cloud_user_avatar_url_uses_api_host() {
        let cloud = CloudSettings {
            host_url: "https://clips.example.com/base".into(),
            ..CloudSettings::default()
        };
        let url = cloud_user_avatar_url(&cloud).expect("avatar URL");
        assert_eq!(
            url.as_str(),
            "https://clips.example.com/base/api/v1/me/avatar"
        );
    }

    #[test]
    fn cloud_user_profile_url_uses_public_url_and_escapes_username() {
        let cloud = CloudSettings {
            host_url: "https://api.example.com/base".into(),
            public_url: Some("https://clips.example.com/cloud".into()),
            ..CloudSettings::default()
        };
        let url = cloud_user_profile_url(&cloud, "Dain 98").expect("profile URL");
        assert_eq!(url.as_str(), "https://clips.example.com/cloud/u/Dain%2098");
    }

}