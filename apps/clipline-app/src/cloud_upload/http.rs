//! Authenticated upload HTTP helpers (URLs, progress, completion).
use super::*;

pub(crate) fn upload_url(client: &CloudClient, template: &str, part_number: u16) -> CloudApiResult<String> {
    let path = template.replace("{part_number}", &part_number.to_string());
    let url = reqwest::Url::parse(&path).or_else(|_| client.base_url().join(&path))?;
    if url.origin() != client.base_url().origin() {
        return Err(CloudApiError::InvalidUpload(format!(
            "authenticated upload URL must use the configured cloud origin: {url}"
        )));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(CloudApiError::InvalidUpload(
            "authenticated upload URL must not contain user credentials".to_string(),
        ));
    }
    Ok(url.to_string())
}

pub(crate) async fn discover_direct_s3(client: &CloudClient, http: &reqwest::Client) -> CloudApiResult<bool> {
    let url = client.base_url().join(".well-known/clipline-cloud")?;
    let response = http.get(url).send().await?;
    let discovery: DiscoveryResponse = parse_json_response(response).await?;
    clipline_cloud_api::ensure_compatible_discovery(&discovery)?;
    Ok(discovery.features.direct_s3_upload)
}

pub(crate) fn upload_control_url(
    client: &CloudClient,
    upload_id: &str,
    suffix: Option<&str>,
) -> CloudApiResult<reqwest::Url> {
    let mut url = client.base_url().join("api/v1/uploads/")?;
    {
        let mut segments = url.path_segments_mut().map_err(|_| {
            CloudApiError::InvalidUpload("build cloud upload control URL".to_string())
        })?;
        segments.pop_if_empty().push(upload_id);
        if let Some(suffix) = suffix {
            segments.push(suffix);
        }
    }
    Ok(url)
}

pub(crate) async fn get_upload_progress(
    client: &CloudClient,
    http: &reqwest::Client,
    device_token: &str,
    upload_id: &str,
) -> CloudApiResult<UploadProgressResponse> {
    let response = http
        .get(upload_control_url(client, upload_id, None)?)
        .bearer_auth(device_token)
        .send()
        .await?;
    parse_json_response(response).await
}

pub(crate) async fn complete_upload(
    client: &CloudClient,
    http: &reqwest::Client,
    device_token: &str,
    upload_id: &str,
) -> CloudApiResult<UploadProgressResponse> {
    let response = http
        .post(upload_control_url(client, upload_id, Some("complete"))?)
        .bearer_auth(device_token)
        .json(&serde_json::json!({}))
        .send()
        .await?;
    parse_json_response(response).await
}

pub(crate) async fn post_json_with_auth<T, B>(
    http: &reqwest::Client,
    url: String,
    device_token: &str,
    body: &B,
) -> CloudApiResult<T>
where
    T: serde::de::DeserializeOwned,
    B: serde::Serialize + ?Sized,
{
    let response = http
        .post(url)
        .bearer_auth(device_token)
        .json(body)
        .send()
        .await?;
    parse_json_response(response).await
}

pub(crate) async fn post_empty_with_auth<T>(
    http: &reqwest::Client,
    url: String,
    device_token: &str,
) -> CloudApiResult<T>
where
    T: serde::de::DeserializeOwned,
{
    let response = http.post(url).bearer_auth(device_token).send().await?;
    parse_json_response(response).await
}

pub(crate) async fn parse_json_response<T>(response: reqwest::Response) -> CloudApiResult<T>
where
    T: serde::de::DeserializeOwned,
{
    let status = response.status();
    let bytes = crate::bounded_http::response_bytes_limited(
        response,
        if status.is_success() {
            crate::bounded_http::CONTROL_JSON_MAX_BYTES
        } else {
            crate::bounded_http::ERROR_BODY_MAX_BYTES
        },
        "cloud upload control",
    )
    .await
    .map_err(CloudApiError::InvalidUpload)?;
    if !status.is_success() {
        let message = serde_json::from_slice::<ErrorResponse>(&bytes)
            .map(|body| body.error)
            .unwrap_or_else(|_| status.to_string());
        return Err(CloudApiError::Api { status, message });
    }
    serde_json::from_slice::<T>(&bytes).map_err(|error| CloudApiError::Api {
        status,
        message: format!("parse upload response: {error}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;

    #[test]
    fn authenticated_upload_urls_stay_on_the_configured_cloud_origin() {
        let cloud = MockServer::start();
        let other = MockServer::start();
        let client = test_client(&cloud);

        assert!(upload_url(&client, "/api/v1/uploads/u1/content", 0).is_ok());
        assert!(upload_url(
            &client,
            &format!("{}/api/v1/uploads/u1/content", cloud.base_url()),
            0,
        )
        .is_ok());
        assert!(upload_url(
            &client,
            &format!("{}/api/v1/uploads/u1/content", other.base_url()),
            0,
        )
        .is_err());
    }

    #[test]
    fn authenticated_upload_url_rejects_scheme_downgrade_and_port_change() {
        let client =
            CloudClient::with_device_token("https://cloud.example:8443/".parse().unwrap(), TOKEN);

        assert!(upload_url(
            &client,
            "http://cloud.example:8443/api/v1/uploads/u1/content",
            0,
        )
        .is_err());
        assert!(upload_url(
            &client,
            "https://cloud.example:9443/api/v1/uploads/u1/content",
            0,
        )
        .is_err());
    }

}