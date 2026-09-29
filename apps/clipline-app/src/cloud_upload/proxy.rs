//! Server-proxy chunked upload path.
use super::*;

pub(crate) async fn upload_chunked_proxy<F>(
    transport: UploadTransport<'_>,
    upload: &CreateUploadResponse,
    path: &Path,
    progress: UploadProgressResponse,
    on_progress: &mut F,
) -> CloudApiResult<UploadProgressResponse>
where
    F: FnMut(&UploadProgressResponse),
{
    let file_size = tokio::fs::metadata(path)
        .await
        .map_err(|error| upload_file_error("read upload metadata", path, error))?
        .len();
    validate_missing_parts(&progress.missing_parts, file_size, upload.part_size_bytes)?;
    for part_number in progress.missing_parts {
        let part =
            prepare_upload_part(path, file_size, upload.part_size_bytes, part_number).await?;
        put_proxy_part(
            transport.client,
            transport.authenticated_stream,
            transport.device_token,
            &upload.upload_id,
            part_number,
            path,
            &part,
        )
        .await?;
        let progress = get_upload_progress(
            transport.client,
            transport.authenticated_control,
            transport.device_token,
            &upload.upload_id,
        )
        .await?;
        on_progress(&progress);
    }
    let progress = complete_upload(
        transport.client,
        transport.authenticated_control,
        transport.device_token,
        &upload.upload_id,
    )
    .await?;
    on_progress(&progress);
    Ok(progress)
}

pub(crate) async fn put_proxy_part(
    client: &CloudClient,
    http: &reqwest::Client,
    device_token: &str,
    upload_id: &str,
    part_number: u16,
    path: &Path,
    part: &PreparedUploadPart,
) -> CloudApiResult<PartUploadResponse> {
    let mut url = upload_control_url(client, upload_id, Some("parts"))?;
    url.path_segments_mut()
        .map_err(|_| CloudApiError::InvalidUpload("build cloud upload part URL".to_string()))?
        .push(&part_number.to_string());
    for attempt in 1..=PROXY_PUT_MAX_ATTEMPTS {
        let body = part_request_body(path, part.slice).await?;
        let response = http
            .put(url.clone())
            .bearer_auth(device_token)
            .header(header::CONTENT_TYPE, "video/mp4")
            .header(header::CONTENT_LENGTH, part.slice.length)
            .header("x-clipline-part-sha256", &part.checksum_sha256)
            .body(body)
            .timeout(crate::bounded_http::upload_timeout(part.slice.length))
            .send()
            .await;
        match response {
            Ok(response)
                if attempt < PROXY_PUT_MAX_ATTEMPTS
                    && is_retryable_proxy_put_status(response.status()) =>
            {
                let retry_after = response
                    .headers()
                    .get(header::RETRY_AFTER)
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| parse_retry_after(value, SystemTime::now()));
                tokio::time::sleep(direct_put_retry_delay(
                    upload_id,
                    part_number,
                    attempt,
                    retry_after,
                ))
                .await;
            }
            Ok(response) => return parse_json_response(response).await,
            Err(_) if attempt < PROXY_PUT_MAX_ATTEMPTS => {
                tokio::time::sleep(direct_put_retry_delay(
                    upload_id,
                    part_number,
                    attempt,
                    None,
                ))
                .await;
            }
            Err(error) => return Err(error.into()),
        }
    }
    unreachable!("proxy upload attempt loop always returns on its final attempt")
}
