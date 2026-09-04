//! Create-upload request construction and file validation.
use super::*;

pub(crate) async fn create_upload(
    client: &CloudClient,
    http: &reqwest::Client,
    device_token: &str,
    request: &CreateUploadRequest,
    description: Option<&str>,
) -> CloudApiResult<CreateUploadResponse> {
    let body = create_upload_body(request, description)?;

    let url = client.base_url().join("api/v1/uploads")?;
    let response = http
        .post(url)
        .bearer_auth(device_token)
        .json(&body)
        .send()
        .await?;
    let status = response.status();
    if !status.is_success() {
        let message = response
            .json::<ErrorResponse>()
            .await
            .map(|body| body.error)
            .unwrap_or_else(|_| status.to_string());
        return Err(CloudApiError::Api { status, message });
    }
    Ok(response.json::<CreateUploadResponse>().await?)
}

pub(crate) fn create_upload_body(
    request: &CreateUploadRequest,
    description: Option<&str>,
) -> CloudApiResult<Value> {
    let mut body = serde_json::to_value(request)
        .map_err(|e| CloudApiError::InvalidUpload(format!("serialize upload request: {e}")))?;
    let Value::Object(ref mut map) = body else {
        return Err(CloudApiError::InvalidUpload(
            "upload request did not serialize to an object".to_string(),
        ));
    };
    map.remove("markers");
    map.remove("description");
    if let Some(description) = normalized_description(description) {
        map.insert(
            "description".to_string(),
            Value::String(description.to_string()),
        );
    }
    Ok(body)
}

pub(crate) fn normalized_description(description: Option<&str>) -> Option<&str> {
    description.map(str::trim).filter(|value| !value.is_empty())
}

pub(crate) async fn validate_upload_request_matches_file(
    request: &CreateUploadRequest,
    path: &Path,
) -> CloudApiResult<()> {
    let file_size = tokio::fs::metadata(path)
        .await
        .map_err(|error| upload_file_error("read upload metadata", path, error))?
        .len();
    if request.file_size_bytes != file_size {
        return Err(CloudApiError::InvalidUpload(format!(
            "file_size_bytes is {}, but file has {} bytes",
            request.file_size_bytes, file_size
        )));
    }
    let checksum = sha256_file(path).await?;
    if request.checksum_sha256.to_ascii_lowercase() != checksum {
        return Err(CloudApiError::InvalidUpload(
            "checksum_sha256 does not match the upload file".to_string(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clipline_cloud_api::sha256_hex;
    use httpmock::prelude::*;
    use serde_json::json;

    #[test]
    fn create_upload_body_includes_description_and_omits_markers() {
        let bytes = b"abc";
        let mut request = upload_request(bytes);
        request.markers = Some(vec![clipline_cloud_api::types::CreateMarkerRequest {
            kind: "ChampionKill".to_string(),
            label: Some("kill".to_string()),
            timestamp_ms: 1200,
            metadata: Some(json!({ "deprecated": true })),
        }]);

        let body = create_upload_body(&request, Some("  Useful context  ")).unwrap();

        assert_eq!(body["description"], "Useful context");
        assert_eq!(body["checksum_sha256"], sha256_hex(bytes));
        assert!(body.get("markers").is_none());
    }

    #[test]
    fn create_upload_body_omits_blank_description() {
        let body = create_upload_body(&upload_request(b"abc"), Some(" \t\n ")).unwrap();

        assert!(body.get("description").is_none());
        assert!(body.get("markers").is_none());
    }

    #[tokio::test]
    async fn authenticated_create_upload_does_not_follow_redirects() {
        let cloud = MockServer::start();
        let target = MockServer::start();
        let redirected = target.mock(|when, then| {
            when.method(GET).path("/stolen");
            then.status(400)
                .json_body(json!({ "error": "reached target" }));
        });
        cloud.mock(|when, then| {
            when.method(POST).path("/api/v1/uploads");
            then.status(302)
                .header("Location", format!("{}/stolen", target.base_url()));
        });
        let client = test_client(&cloud);
        let http = crate::bounded_http::control_client().unwrap();

        let error = create_upload(&client, http, TOKEN, &upload_request(b"abc"), None)
            .await
            .expect_err("redirect must not be followed");

        assert!(error.to_string().contains("302"), "{error}");
        redirected.assert_hits(0);
    }

}