//! Single-PUT upload path.
use super::*;

pub(crate) async fn upload_single<F>(
    client: &CloudClient,
    control_http: &reqwest::Client,
    stream_http: &reqwest::Client,
    device_token: &str,
    upload: &CreateUploadResponse,
    path: &Path,
    on_progress: &mut F,
) -> CloudApiResult<UploadProgressResponse>
where
    F: FnMut(&UploadProgressResponse),
{
    let progress =
        get_upload_progress(client, control_http, device_token, &upload.upload_id).await?;
    if progress.status == "completed" {
        on_progress(&progress);
        return Ok(progress);
    }
    let template = upload.single_put_url.as_deref().ok_or_else(|| {
        CloudApiError::InvalidUpload("single_put upload omitted its content URL".to_string())
    })?;
    let url = upload_url(client, template, 0)?;
    let file = tokio::fs::File::open(path)
        .await
        .map_err(|error| upload_file_error("open upload", path, error))?;
    let file_size = file
        .metadata()
        .await
        .map_err(|error| upload_file_error("read upload metadata", path, error))?
        .len();
    let response = stream_http
        .put(url)
        .bearer_auth(device_token)
        .header(header::CONTENT_LENGTH, file_size)
        .header(header::CONTENT_TYPE, "video/mp4")
        .body(reqwest::Body::wrap_stream(ReaderStream::new(file)))
        .timeout(crate::bounded_http::upload_timeout(file_size))
        .send()
        .await?;
    let progress = parse_json_response(response).await?;
    on_progress(&progress);
    Ok(progress)
}
