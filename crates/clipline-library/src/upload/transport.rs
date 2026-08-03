//! Bounded, permissively licensed Clipline Cloud upload transport.
//!
//! The caller owns and retains the original [`crate::UploadSourceLease`]. This
//! transport receives only the prepared payload path and never reacquires a
//! lease against that path, which is essential for selected-audio uploads.

use std::collections::{hash_map::DefaultHasher, BTreeSet};
use std::future::Future;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::{header, StatusCode};
use serde::de::DeserializeOwned;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;

use crate::cloud::http::{
    control_client, response_bytes_limited, stream_client, MAX_CLOUD_CONTROL_JSON_BYTES,
    MAX_CLOUD_ERROR_BODY_BYTES,
};
use crate::cloud::ports::CloudCredential;
use crate::cloud::protocol::{
    validate_discovery, CloudApiBase, CloudProtocolError, CreateUploadRequest,
    CreateUploadResponse, DirectPartUploadAckRequest, DirectPartUploadUrlResponse,
    DiscoveryResponse, PartUploadResponse, UploadProgressResponse, UPLOAD_PART_SHA256_HEADER,
};

pub const MAX_CONCURRENT_UPLOADS: usize = 2;
pub const MAX_UPLOAD_PART_BYTES: u64 = 64 * 1024 * 1024;
pub const UPLOAD_PUT_MAX_ATTEMPTS: usize = 3;
pub const UPLOAD_CONTROL_JSON_MAX_BYTES: usize = MAX_CLOUD_CONTROL_JSON_BYTES;
pub const UPLOAD_ERROR_BODY_MAX_BYTES: usize = MAX_CLOUD_ERROR_BODY_BYTES;

const RETRY_BACKOFF_BASE: Duration = Duration::from_millis(250);
const RETRY_BACKOFF_MAX: Duration = Duration::from_secs(30);
const MIN_UPLOAD_THROUGHPUT_BYTES_PER_SECOND: u64 = 256 * 1024;
const MIN_UPLOAD_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_UPLOAD_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);

static UPLOAD_PERMITS: tokio::sync::Semaphore =
    tokio::sync::Semaphore::const_new(MAX_CONCURRENT_UPLOADS);

#[derive(Debug, Default)]
struct CancellationInner {
    canceled: AtomicBool,
    notify: tokio::sync::Notify,
}

/// Explicit durable-upload cancellation, independent of any window lifetime.
#[derive(Debug, Clone, Default)]
pub struct UploadCancellation {
    inner: Arc<CancellationInner>,
}

impl UploadCancellation {
    pub fn cancel(&self) {
        if !self.inner.canceled.swap(true, Ordering::AcqRel) {
            self.inner.notify.notify_waiters();
        }
    }

    #[must_use]
    pub fn is_canceled(&self) -> bool {
        self.inner.canceled.load(Ordering::Acquire)
    }

    pub fn check(&self) -> Result<(), CloudProtocolError> {
        if self.is_canceled() {
            Err(CloudProtocolError::Canceled)
        } else {
            Ok(())
        }
    }

    pub async fn cancelled(&self) {
        loop {
            let notified = self.inner.notify.notified();
            if self.is_canceled() {
                return;
            }
            notified.await;
        }
    }
}

/// Three redirect-free clients: bearer control, bearer streaming, and object
/// streaming. Object requests never receive the Cloud credential.
#[derive(Clone)]
pub struct ReqwestUploadTransport {
    control: reqwest::Client,
    authenticated_stream: reqwest::Client,
    object_stream: reqwest::Client,
}

impl std::fmt::Debug for ReqwestUploadTransport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ReqwestUploadTransport")
            .finish_non_exhaustive()
    }
}

impl ReqwestUploadTransport {
    pub fn new() -> Result<Self, CloudProtocolError> {
        Ok(Self {
            control: control_client()
                .map_err(|error| CloudProtocolError::Http(error.to_string()))?,
            authenticated_stream: stream_client()
                .map_err(|error| CloudProtocolError::Http(error.to_string()))?,
            object_stream: stream_client()
                .map_err(|error| CloudProtocolError::Http(error.to_string()))?,
        })
    }

    /// Upload an already-prepared MP4 payload while the caller retains the
    /// original source lease.
    #[allow(clippy::too_many_arguments)]
    pub async fn upload_file_with_progress<F>(
        &self,
        api: &CloudApiBase,
        credential: &CloudCredential,
        request: &CreateUploadRequest,
        description: Option<&str>,
        payload: &Path,
        cancellation: &UploadCancellation,
        mut on_progress: F,
    ) -> Result<UploadProgressResponse, CloudProtocolError>
    where
        F: FnMut(&UploadProgressResponse),
    {
        cancellation.check()?;
        let permit = tokio::select! {
            _ = cancellation.cancelled() => return Err(CloudProtocolError::Canceled),
            permit = UPLOAD_PERMITS.acquire() => permit.map_err(|_| {
                CloudProtocolError::InvalidUpload(
                    "cloud upload concurrency limiter is closed".into(),
                )
            })?,
        };
        let _permit = permit;
        validate_upload_request_matches_file(request, payload, cancellation).await?;

        let direct_available = match self.discover_direct_s3(api, cancellation).await {
            Ok(available) => available,
            Err(CloudProtocolError::Canceled) => return Err(CloudProtocolError::Canceled),
            Err(_) => false,
        };
        let upload = self
            .create_upload(api, credential, request, description, cancellation)
            .await?;
        match self
            .upload_existing(
                api,
                credential,
                &upload,
                payload,
                direct_available,
                cancellation,
                &mut on_progress,
            )
            .await
        {
            Ok(progress) => Ok(progress),
            Err(DirectUploadError::Fallback(_)) => {
                cancellation.check()?;
                let replacement = self
                    .create_upload(api, credential, request, description, cancellation)
                    .await?;
                self.upload_existing(
                    api,
                    credential,
                    &replacement,
                    payload,
                    false,
                    cancellation,
                    &mut on_progress,
                )
                .await
                .map_err(DirectUploadError::into_protocol)
            }
            Err(error) => Err(error.into_protocol()),
        }
    }

    async fn discover_direct_s3(
        &self,
        api: &CloudApiBase,
        cancellation: &UploadCancellation,
    ) -> Result<bool, CloudProtocolError> {
        let url = api.api_url(".well-known/clipline-cloud")?;
        let response = send_request(self.control.get(url), cancellation).await?;
        let discovery: DiscoveryResponse =
            parse_json_response(response, "cloud discovery", cancellation).await?;
        validate_discovery(&discovery)?;
        Ok(discovery.features.direct_s3_upload)
    }

    async fn create_upload(
        &self,
        api: &CloudApiBase,
        credential: &CloudCredential,
        request: &CreateUploadRequest,
        description: Option<&str>,
        cancellation: &UploadCancellation,
    ) -> Result<CreateUploadResponse, CloudProtocolError> {
        let body = create_upload_body(request, description)?;
        let response = send_request(
            self.control
                .post(api.api_url("api/v1/uploads")?)
                .bearer_auth(credential.expose())
                .json(&body),
            cancellation,
        )
        .await?;
        parse_json_response(response, "create cloud upload", cancellation).await
    }

    #[allow(clippy::too_many_arguments)]
    async fn upload_existing<F>(
        &self,
        api: &CloudApiBase,
        credential: &CloudCredential,
        upload: &CreateUploadResponse,
        payload: &Path,
        direct_available: bool,
        cancellation: &UploadCancellation,
        on_progress: &mut F,
    ) -> Result<UploadProgressResponse, DirectUploadError>
    where
        F: FnMut(&UploadProgressResponse),
    {
        match upload.mode.as_str() {
            "single_put" => self
                .upload_single(api, credential, upload, payload, cancellation, on_progress)
                .await
                .map_err(DirectUploadError::Protocol),
            "chunked" => {
                let progress = self
                    .get_progress(api, credential, &upload.upload_id, cancellation)
                    .await
                    .map_err(DirectUploadError::Protocol)?;
                on_progress(&progress);
                let templates = upload
                    .direct_part_presign_url_template
                    .as_deref()
                    .zip(upload.direct_part_ack_url_template.as_deref());
                if !direct_available || templates.is_none() {
                    return self
                        .upload_chunked_proxy(
                            api,
                            credential,
                            upload,
                            payload,
                            progress,
                            cancellation,
                            on_progress,
                        )
                        .await
                        .map_err(DirectUploadError::Protocol);
                }
                let (presign, ack) = templates.expect("both direct templates were checked");
                self.upload_chunked_direct(
                    api,
                    credential,
                    upload,
                    payload,
                    progress,
                    DirectTemplates { presign, ack },
                    cancellation,
                    on_progress,
                )
                .await
            }
            other => Err(DirectUploadError::Protocol(
                CloudProtocolError::InvalidUpload(format!(
                    "server returned unsupported upload mode {other:?}"
                )),
            )),
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn upload_single<F>(
        &self,
        api: &CloudApiBase,
        credential: &CloudCredential,
        upload: &CreateUploadResponse,
        payload: &Path,
        cancellation: &UploadCancellation,
        on_progress: &mut F,
    ) -> Result<UploadProgressResponse, CloudProtocolError>
    where
        F: FnMut(&UploadProgressResponse),
    {
        let progress = self
            .get_progress(api, credential, &upload.upload_id, cancellation)
            .await?;
        if progress.status == "completed" {
            on_progress(&progress);
            return Ok(progress);
        }
        let template = upload.single_put_url.as_deref().ok_or_else(|| {
            CloudProtocolError::InvalidUpload("single_put upload omitted its content URL".into())
        })?;
        let url = api.authenticated_upload_url(template, 0)?;
        let (file, file_size) = open_payload(payload).await?;
        let response = send_request(
            self.authenticated_stream
                .put(url)
                .bearer_auth(credential.expose())
                .header(header::CONTENT_LENGTH, file_size)
                .header(header::CONTENT_TYPE, "video/mp4")
                .body(reqwest::Body::wrap_stream(ReaderStream::new(file)))
                .timeout(upload_timeout(file_size)),
            cancellation,
        )
        .await?;
        let progress = parse_json_response(response, "single upload", cancellation).await?;
        on_progress(&progress);
        Ok(progress)
    }

    #[allow(clippy::too_many_arguments)]
    async fn upload_chunked_proxy<F>(
        &self,
        api: &CloudApiBase,
        credential: &CloudCredential,
        upload: &CreateUploadResponse,
        payload: &Path,
        progress: UploadProgressResponse,
        cancellation: &UploadCancellation,
        on_progress: &mut F,
    ) -> Result<UploadProgressResponse, CloudProtocolError>
    where
        F: FnMut(&UploadProgressResponse),
    {
        let file_size = payload_size(payload).await?;
        validate_missing_parts(&progress.missing_parts, file_size, upload.part_size_bytes)?;
        for part_number in progress.missing_parts {
            cancellation.check()?;
            let part = prepare_upload_part(
                payload,
                file_size,
                upload.part_size_bytes,
                part_number,
                cancellation,
            )
            .await?;
            self.put_proxy_part(
                api,
                credential,
                &upload.upload_id,
                part_number,
                payload,
                &part,
                cancellation,
            )
            .await?;
            let progress = self
                .get_progress(api, credential, &upload.upload_id, cancellation)
                .await?;
            on_progress(&progress);
        }
        let progress = self
            .complete(api, credential, &upload.upload_id, cancellation)
            .await?;
        on_progress(&progress);
        Ok(progress)
    }

    #[allow(clippy::too_many_arguments)]
    async fn upload_chunked_direct<F>(
        &self,
        api: &CloudApiBase,
        credential: &CloudCredential,
        upload: &CreateUploadResponse,
        payload: &Path,
        progress: UploadProgressResponse,
        templates: DirectTemplates<'_>,
        cancellation: &UploadCancellation,
        on_progress: &mut F,
    ) -> Result<UploadProgressResponse, DirectUploadError>
    where
        F: FnMut(&UploadProgressResponse),
    {
        let file_size = payload_size(payload)
            .await
            .map_err(DirectUploadError::Protocol)?;
        validate_missing_parts(&progress.missing_parts, file_size, upload.part_size_bytes)
            .map_err(DirectUploadError::Protocol)?;
        for part_number in progress.missing_parts {
            cancellation.check().map_err(DirectUploadError::Protocol)?;
            let part = prepare_upload_part(
                payload,
                file_size,
                upload.part_size_bytes,
                part_number,
                cancellation,
            )
            .await
            .map_err(DirectUploadError::Protocol)?;
            self.upload_direct_part(
                api,
                credential,
                upload,
                part_number,
                payload,
                &part,
                templates,
                cancellation,
            )
            .await?;
            let progress = self
                .get_progress(api, credential, &upload.upload_id, cancellation)
                .await
                .map_err(DirectUploadError::Protocol)?;
            on_progress(&progress);
        }
        let progress = self
            .complete(api, credential, &upload.upload_id, cancellation)
            .await
            .map_err(DirectUploadError::Protocol)?;
        on_progress(&progress);
        Ok(progress)
    }

    #[allow(clippy::too_many_arguments)]
    async fn upload_direct_part(
        &self,
        api: &CloudApiBase,
        credential: &CloudCredential,
        upload: &CreateUploadResponse,
        part_number: u16,
        payload: &Path,
        part: &PreparedUploadPart,
        templates: DirectTemplates<'_>,
        cancellation: &UploadCancellation,
    ) -> Result<PartUploadResponse, DirectUploadError> {
        let mut last_error = None;
        for attempt in 1..=UPLOAD_PUT_MAX_ATTEMPTS {
            cancellation.check().map_err(DirectUploadError::Protocol)?;
            let presign = self
                .request_presign(
                    api,
                    credential,
                    templates.presign,
                    part_number,
                    cancellation,
                )
                .await?;
            validate_presign(upload, part_number, part.slice.length, &presign)?;
            match self
                .put_presigned_part(&presign, payload, part.slice, cancellation)
                .await
            {
                Ok(etag) => {
                    return self
                        .ack_direct_part(
                            api,
                            credential,
                            templates.ack,
                            part_number,
                            &DirectPartUploadAckRequest {
                                size_bytes: part.slice.length,
                                checksum_sha256: part.checksum_sha256.clone(),
                                etag,
                            },
                            cancellation,
                        )
                        .await;
                }
                Err(DirectPutError::Retryable {
                    message,
                    retry_after,
                }) => {
                    last_error = Some(message);
                    if attempt < UPLOAD_PUT_MAX_ATTEMPTS {
                        cancelable_sleep(
                            retry_delay(&upload.upload_id, part_number, attempt, retry_after),
                            cancellation,
                        )
                        .await
                        .map_err(DirectUploadError::Protocol)?;
                    }
                }
                Err(DirectPutError::Fallback(message)) => {
                    return Err(DirectUploadError::Fallback(message));
                }
                Err(DirectPutError::Terminal(error)) => {
                    return Err(DirectUploadError::Protocol(error));
                }
            }
        }
        Err(DirectUploadError::Protocol(
            CloudProtocolError::InvalidUpload(format!(
                "direct S3 PUT for part {part_number} failed after refreshing presign: {}",
                last_error.unwrap_or_else(|| "unknown error".into())
            )),
        ))
    }

    async fn request_presign(
        &self,
        api: &CloudApiBase,
        credential: &CloudCredential,
        template: &str,
        part_number: u16,
        cancellation: &UploadCancellation,
    ) -> Result<DirectPartUploadUrlResponse, DirectUploadError> {
        let url = api
            .authenticated_upload_url(template, part_number)
            .map_err(DirectUploadError::Protocol)?;
        let response = send_request(
            self.control.post(url).bearer_auth(credential.expose()),
            cancellation,
        )
        .await
        .map_err(classify_direct_control)?;
        parse_json_response(response, "direct upload presign", cancellation)
            .await
            .map_err(classify_direct_control)
    }

    async fn ack_direct_part(
        &self,
        api: &CloudApiBase,
        credential: &CloudCredential,
        template: &str,
        part_number: u16,
        ack: &DirectPartUploadAckRequest,
        cancellation: &UploadCancellation,
    ) -> Result<PartUploadResponse, DirectUploadError> {
        let url = api
            .authenticated_upload_url(template, part_number)
            .map_err(DirectUploadError::Protocol)?;
        let response = send_request(
            self.control
                .post(url)
                .bearer_auth(credential.expose())
                .json(ack),
            cancellation,
        )
        .await
        .map_err(classify_direct_control)?;
        parse_json_response(response, "direct upload acknowledgement", cancellation)
            .await
            .map_err(classify_direct_control)
    }

    async fn put_presigned_part(
        &self,
        presign: &DirectPartUploadUrlResponse,
        payload: &Path,
        slice: FileSlice,
        cancellation: &UploadCancellation,
    ) -> Result<String, DirectPutError> {
        let body = part_request_body(payload, slice)
            .await
            .map_err(DirectPutError::Terminal)?;
        let mut request = self
            .object_stream
            .put(&presign.url)
            .header(header::CONTENT_LENGTH, slice.length)
            .body(body)
            .timeout(upload_timeout(slice.length));
        for prescribed in &presign.headers {
            let name =
                header::HeaderName::from_bytes(prescribed.name.as_bytes()).map_err(|error| {
                    DirectPutError::Fallback(format!(
                        "direct S3 presign returned invalid header name {:?}: {error}",
                        prescribed.name
                    ))
                })?;
            let value = header::HeaderValue::from_str(&prescribed.value).map_err(|error| {
                DirectPutError::Fallback(format!(
                    "direct S3 presign returned invalid header value for {:?}: {error}",
                    prescribed.name
                ))
            })?;
            request = request.header(name, value);
        }
        let response = tokio::select! {
            _ = cancellation.cancelled() => {
                return Err(DirectPutError::Terminal(CloudProtocolError::Canceled));
            }
            result = request.send() => result.map_err(classify_direct_put_transport)?,
        };
        let status = response.status();
        if !status.is_success() {
            let message = format!("direct S3 PUT failed with {status}");
            if retryable_direct_status(status) {
                let retry_after = response
                    .headers()
                    .get(header::RETRY_AFTER)
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| parse_retry_after(value, SystemTime::now()));
                return Err(DirectPutError::Retryable {
                    message,
                    retry_after,
                });
            }
            return Err(DirectPutError::Fallback(message));
        }
        response
            .headers()
            .get(header::ETAG)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| {
                DirectPutError::Terminal(CloudProtocolError::InvalidUpload(
                    "direct S3 upload did not return an ETag for the uploaded part".into(),
                ))
            })
    }

    async fn get_progress(
        &self,
        api: &CloudApiBase,
        credential: &CloudCredential,
        upload_id: &str,
        cancellation: &UploadCancellation,
    ) -> Result<UploadProgressResponse, CloudProtocolError> {
        let response = send_request(
            self.control
                .get(api.upload_control_url(upload_id, None)?)
                .bearer_auth(credential.expose()),
            cancellation,
        )
        .await?;
        parse_json_response(response, "cloud upload progress", cancellation).await
    }

    async fn complete(
        &self,
        api: &CloudApiBase,
        credential: &CloudCredential,
        upload_id: &str,
        cancellation: &UploadCancellation,
    ) -> Result<UploadProgressResponse, CloudProtocolError> {
        let response = send_request(
            self.control
                .post(api.upload_control_url(upload_id, Some("complete"))?)
                .bearer_auth(credential.expose())
                .json(&serde_json::json!({})),
            cancellation,
        )
        .await?;
        parse_json_response(response, "complete cloud upload", cancellation).await
    }

    #[allow(clippy::too_many_arguments)]
    async fn put_proxy_part(
        &self,
        api: &CloudApiBase,
        credential: &CloudCredential,
        upload_id: &str,
        part_number: u16,
        payload: &Path,
        part: &PreparedUploadPart,
        cancellation: &UploadCancellation,
    ) -> Result<PartUploadResponse, CloudProtocolError> {
        let mut url = api.upload_control_url(upload_id, Some("parts"))?;
        url.path_segments_mut()
            .map_err(|_| CloudProtocolError::InvalidUpload("build cloud upload part URL".into()))?
            .push(&part_number.to_string());
        for attempt in 1..=UPLOAD_PUT_MAX_ATTEMPTS {
            cancellation.check()?;
            let body = part_request_body(payload, part.slice).await?;
            let request = self
                .authenticated_stream
                .put(url.clone())
                .bearer_auth(credential.expose())
                .header(header::CONTENT_TYPE, "video/mp4")
                .header(header::CONTENT_LENGTH, part.slice.length)
                .header(UPLOAD_PART_SHA256_HEADER, &part.checksum_sha256)
                .body(body)
                .timeout(upload_timeout(part.slice.length));
            let response = tokio::select! {
                _ = cancellation.cancelled() => return Err(CloudProtocolError::Canceled),
                response = request.send() => response,
            };
            match response {
                Ok(response)
                    if attempt < UPLOAD_PUT_MAX_ATTEMPTS
                        && retryable_proxy_status(response.status()) =>
                {
                    let retry_after = response
                        .headers()
                        .get(header::RETRY_AFTER)
                        .and_then(|value| value.to_str().ok())
                        .and_then(|value| parse_retry_after(value, SystemTime::now()));
                    cancelable_sleep(
                        retry_delay(upload_id, part_number, attempt, retry_after),
                        cancellation,
                    )
                    .await?;
                }
                Ok(response) => {
                    return parse_json_response(response, "proxy upload part", cancellation).await;
                }
                Err(_) if attempt < UPLOAD_PUT_MAX_ATTEMPTS => {
                    cancelable_sleep(
                        retry_delay(upload_id, part_number, attempt, None),
                        cancellation,
                    )
                    .await?;
                }
                Err(error) => return Err(CloudProtocolError::Http(error.to_string())),
            }
        }
        unreachable!("proxy upload attempt loop returns on its final attempt")
    }
}

fn create_upload_body(
    request: &CreateUploadRequest,
    description: Option<&str>,
) -> Result<Value, CloudProtocolError> {
    let mut body = serde_json::to_value(request).map_err(|error| {
        CloudProtocolError::InvalidUpload(format!("serialize upload request: {error}"))
    })?;
    let Value::Object(ref mut fields) = body else {
        return Err(CloudProtocolError::InvalidUpload(
            "upload request did not serialize to an object".into(),
        ));
    };
    fields.remove("markers");
    fields.remove("description");
    if let Some(description) = description.map(str::trim).filter(|value| !value.is_empty()) {
        fields.insert("description".into(), Value::String(description.into()));
    }
    Ok(body)
}

async fn send_request(
    request: reqwest::RequestBuilder,
    cancellation: &UploadCancellation,
) -> Result<reqwest::Response, CloudProtocolError> {
    tokio::select! {
        _ = cancellation.cancelled() => Err(CloudProtocolError::Canceled),
        result = request.send() => result.map_err(|error| CloudProtocolError::Http(error.to_string())),
    }
}

async fn parse_json_response<T: DeserializeOwned>(
    response: reqwest::Response,
    context: &str,
    cancellation: &UploadCancellation,
) -> Result<T, CloudProtocolError> {
    let status = response.status();
    let maximum = if status.is_success() {
        UPLOAD_CONTROL_JSON_MAX_BYTES
    } else {
        UPLOAD_ERROR_BODY_MAX_BYTES
    };
    let bytes = cancel_future(
        response_bytes_limited(response, maximum, context, || cancellation.is_canceled()),
        cancellation,
    )
    .await?
    .map_err(|error| CloudProtocolError::InvalidUpload(error.to_string()))?;
    if !status.is_success() {
        let message = serde_json::from_slice::<ErrorBody>(&bytes)
            .map(|body| body.error)
            .unwrap_or_else(|_| status.to_string());
        return Err(CloudProtocolError::Api { status, message });
    }
    serde_json::from_slice(&bytes).map_err(|error| CloudProtocolError::Api {
        status,
        message: format!("parse upload response: {error}"),
    })
}

async fn cancel_future<F, T>(
    future: F,
    cancellation: &UploadCancellation,
) -> Result<T, CloudProtocolError>
where
    F: Future<Output = T>,
{
    tokio::select! {
        _ = cancellation.cancelled() => Err(CloudProtocolError::Canceled),
        output = future => Ok(output),
    }
}

async fn cancelable_sleep(
    duration: Duration,
    cancellation: &UploadCancellation,
) -> Result<(), CloudProtocolError> {
    tokio::select! {
        _ = cancellation.cancelled() => Err(CloudProtocolError::Canceled),
        () = tokio::time::sleep(duration) => Ok(()),
    }
}

async fn validate_upload_request_matches_file(
    request: &CreateUploadRequest,
    payload: &Path,
    cancellation: &UploadCancellation,
) -> Result<(), CloudProtocolError> {
    let size = payload_size(payload).await?;
    if request.file_size_bytes != size {
        return Err(CloudProtocolError::InvalidUpload(format!(
            "file_size_bytes is {}, but file has {size} bytes",
            request.file_size_bytes
        )));
    }
    let checksum = sha256_file(payload, cancellation).await?;
    if !request.checksum_sha256.eq_ignore_ascii_case(&checksum) {
        return Err(CloudProtocolError::InvalidUpload(
            "checksum_sha256 does not match the upload file".into(),
        ));
    }
    Ok(())
}

pub async fn sha256_upload_file(
    payload: &Path,
    cancellation: &UploadCancellation,
) -> Result<String, CloudProtocolError> {
    sha256_file(payload, cancellation).await
}

async fn sha256_file(
    payload: &Path,
    cancellation: &UploadCancellation,
) -> Result<String, CloudProtocolError> {
    let mut file = tokio::fs::File::open(payload)
        .await
        .map_err(|error| file_error("open upload for hashing", payload, error))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        cancellation.check()?;
        let read = tokio::select! {
            _ = cancellation.cancelled() => return Err(CloudProtocolError::Canceled),
            result = file.read(&mut buffer) => result,
        }
        .map_err(|error| file_error("hash upload", payload, error))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

async fn payload_size(payload: &Path) -> Result<u64, CloudProtocolError> {
    tokio::fs::metadata(payload)
        .await
        .map(|metadata| metadata.len())
        .map_err(|error| file_error("read upload metadata", payload, error))
}

async fn open_payload(payload: &Path) -> Result<(tokio::fs::File, u64), CloudProtocolError> {
    let file = tokio::fs::File::open(payload)
        .await
        .map_err(|error| file_error("open upload", payload, error))?;
    let size = file
        .metadata()
        .await
        .map_err(|error| file_error("read upload metadata", payload, error))?
        .len();
    Ok((file, size))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileSlice {
    offset: u64,
    length: u64,
}

#[derive(Debug, PartialEq, Eq)]
struct PreparedUploadPart {
    slice: FileSlice,
    checksum_sha256: String,
}

async fn prepare_upload_part(
    payload: &Path,
    file_size: u64,
    part_size: u64,
    part_number: u16,
    cancellation: &UploadCancellation,
) -> Result<PreparedUploadPart, CloudProtocolError> {
    let slice = part_slice_for(file_size, part_size, part_number)?;
    let checksum_sha256 = sha256_file_slice(payload, slice, cancellation).await?;
    Ok(PreparedUploadPart {
        slice,
        checksum_sha256,
    })
}

fn part_slice_for(
    file_size: u64,
    part_size: u64,
    part_number: u16,
) -> Result<FileSlice, CloudProtocolError> {
    validate_part_size(part_size)?;
    if part_number == 0 {
        return Err(CloudProtocolError::InvalidUpload(
            "part numbers start at 1".into(),
        ));
    }
    let offset = u64::from(part_number - 1)
        .checked_mul(part_size)
        .ok_or_else(|| CloudProtocolError::InvalidUpload("part offset overflowed".into()))?;
    if offset >= file_size {
        return Err(CloudProtocolError::InvalidUpload(format!(
            "part {part_number} starts beyond the upload file"
        )));
    }
    Ok(FileSlice {
        offset,
        length: part_size.min(file_size - offset),
    })
}

fn validate_part_size(part_size: u64) -> Result<(), CloudProtocolError> {
    if part_size == 0 {
        return Err(CloudProtocolError::InvalidUpload(
            "part size must be positive".into(),
        ));
    }
    if part_size > MAX_UPLOAD_PART_BYTES {
        return Err(CloudProtocolError::InvalidUpload(format!(
            "server part size {part_size} exceeds the {MAX_UPLOAD_PART_BYTES} byte client limit"
        )));
    }
    Ok(())
}

fn validate_missing_parts(
    missing_parts: &[u16],
    file_size: u64,
    part_size: u64,
) -> Result<(), CloudProtocolError> {
    validate_part_size(part_size)?;
    let total_parts = file_size.div_ceil(part_size);
    if total_parts > u64::from(u16::MAX) {
        return Err(CloudProtocolError::InvalidUpload(format!(
            "upload requires {total_parts} parts, exceeding the protocol limit"
        )));
    }
    let mut seen = BTreeSet::new();
    for &part_number in missing_parts {
        if part_number == 0 {
            return Err(CloudProtocolError::InvalidUpload(
                "part numbers start at 1".into(),
            ));
        }
        if u64::from(part_number) > total_parts {
            return Err(CloudProtocolError::InvalidUpload(format!(
                "part {part_number} starts beyond the upload file"
            )));
        }
        if !seen.insert(part_number) {
            return Err(CloudProtocolError::InvalidUpload(format!(
                "server returned duplicate part {part_number}"
            )));
        }
    }
    Ok(())
}

async fn open_part_reader(
    payload: &Path,
    slice: FileSlice,
) -> Result<tokio::io::Take<tokio::fs::File>, CloudProtocolError> {
    let mut file = tokio::fs::File::open(payload)
        .await
        .map_err(|error| file_error("open upload part", payload, error))?;
    file.seek(std::io::SeekFrom::Start(slice.offset))
        .await
        .map_err(|error| file_error("seek upload part", payload, error))?;
    Ok(file.take(slice.length))
}

async fn sha256_file_slice(
    payload: &Path,
    slice: FileSlice,
    cancellation: &UploadCancellation,
) -> Result<String, CloudProtocolError> {
    let mut reader = open_part_reader(payload, slice).await?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    let mut total = 0_u64;
    loop {
        cancellation.check()?;
        let read = tokio::select! {
            _ = cancellation.cancelled() => return Err(CloudProtocolError::Canceled),
            result = reader.read(&mut buffer) => result,
        }
        .map_err(|error| file_error("hash upload part", payload, error))?;
        if read == 0 {
            break;
        }
        total = total.saturating_add(read as u64);
        hasher.update(&buffer[..read]);
    }
    if total != slice.length {
        return Err(CloudProtocolError::InvalidUpload(format!(
            "hash upload part {payload:?}: expected {} bytes at offset {}, found {total}",
            slice.length, slice.offset
        )));
    }
    Ok(format!("{:x}", hasher.finalize()))
}

async fn part_request_body(
    payload: &Path,
    slice: FileSlice,
) -> Result<reqwest::Body, CloudProtocolError> {
    Ok(reqwest::Body::wrap_stream(ReaderStream::new(
        open_part_reader(payload, slice).await?,
    )))
}

fn validate_presign(
    upload: &CreateUploadResponse,
    part_number: u16,
    part_length: u64,
    presign: &DirectPartUploadUrlResponse,
) -> Result<(), DirectUploadError> {
    if presign.upload_id != upload.upload_id || presign.part_number != part_number {
        return Err(DirectUploadError::Fallback(
            "direct S3 presign response did not match the requested part".into(),
        ));
    }
    if !presign.method.eq_ignore_ascii_case("PUT") {
        return Err(DirectUploadError::Fallback(format!(
            "direct S3 presign returned unsupported method {:?}",
            presign.method
        )));
    }
    if presign.expected_size_bytes != part_length {
        return Err(DirectUploadError::Fallback(format!(
            "direct S3 presign expected {} bytes for part {part_number}, but the client has {part_length}",
            presign.expected_size_bytes
        )));
    }
    Ok(())
}

fn classify_direct_control(error: CloudProtocolError) -> DirectUploadError {
    match error {
        CloudProtocolError::Api { status, message } if status == StatusCode::CONFLICT => {
            DirectUploadError::Protocol(CloudProtocolError::Api {
                status,
                message: format!(
                    "direct S3 part acknowledgement conflicted with existing metadata: {message}. Retry the upload from the beginning."
                ),
            })
        }
        CloudProtocolError::Api { status, message }
            if matches!(status.as_u16(), 404 | 405 | 410 | 501 | 503) =>
        {
            DirectUploadError::Fallback(format!(
                "direct S3 control endpoint is unavailable ({status}): {message}"
            ))
        }
        other => DirectUploadError::Protocol(other),
    }
}

fn classify_direct_put_transport(error: reqwest::Error) -> DirectPutError {
    let message = format!("direct S3 PUT request failed: {error}");
    if error.is_builder() || error.is_redirect() {
        DirectPutError::Fallback(message)
    } else {
        DirectPutError::Retryable {
            message,
            retry_after: None,
        }
    }
}

fn retryable_direct_status(status: StatusCode) -> bool {
    status == StatusCode::FORBIDDEN
        || status == StatusCode::REQUEST_TIMEOUT
        || status == StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error()
}

fn retryable_proxy_status(status: StatusCode) -> bool {
    status == StatusCode::REQUEST_TIMEOUT
        || status == StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error()
}

fn parse_retry_after(value: &str, now: SystemTime) -> Option<Duration> {
    let value = value.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let timestamp = chrono::DateTime::parse_from_rfc2822(value)
        .ok()?
        .timestamp();
    let timestamp = u64::try_from(timestamp).ok()?;
    let target = UNIX_EPOCH.checked_add(Duration::from_secs(timestamp))?;
    Some(target.duration_since(now).unwrap_or(Duration::ZERO))
}

fn retry_delay(
    upload_id: &str,
    part_number: u16,
    failed_attempt: usize,
    retry_after: Option<Duration>,
) -> Duration {
    let exponent = u32::try_from(failed_attempt.saturating_sub(1).min(16)).unwrap_or(16);
    let exponential = RETRY_BACKOFF_BASE.saturating_mul(1_u32 << exponent);
    let jitter_window_ms = u64::try_from((exponential / 2).as_millis()).unwrap_or(u64::MAX);
    let mut hasher = DefaultHasher::new();
    upload_id.hash(&mut hasher);
    part_number.hash(&mut hasher);
    failed_attempt.hash(&mut hasher);
    let jitter = hasher.finish() % jitter_window_ms.saturating_add(1);
    exponential
        .saturating_add(Duration::from_millis(jitter))
        .max(retry_after.unwrap_or(Duration::ZERO))
        .min(RETRY_BACKOFF_MAX)
}

fn upload_timeout(size_bytes: u64) -> Duration {
    let transfer_seconds = size_bytes.saturating_add(MIN_UPLOAD_THROUGHPUT_BYTES_PER_SECOND - 1)
        / MIN_UPLOAD_THROUGHPUT_BYTES_PER_SECOND;
    MIN_UPLOAD_TIMEOUT
        .saturating_add(Duration::from_secs(transfer_seconds))
        .min(MAX_UPLOAD_TIMEOUT)
}

fn file_error(action: &str, path: &Path, error: std::io::Error) -> CloudProtocolError {
    CloudProtocolError::InvalidUpload(format!("{action} {path:?}: {error}"))
}

#[derive(Clone, Copy)]
struct DirectTemplates<'a> {
    presign: &'a str,
    ack: &'a str,
}

#[derive(Debug)]
enum DirectUploadError {
    Fallback(String),
    Protocol(CloudProtocolError),
}

impl DirectUploadError {
    fn into_protocol(self) -> CloudProtocolError {
        match self {
            Self::Fallback(message) => CloudProtocolError::InvalidUpload(message),
            Self::Protocol(error) => error,
        }
    }
}

#[derive(Debug)]
enum DirectPutError {
    Retryable {
        message: String,
        retry_after: Option<Duration>,
    },
    Fallback(String),
    Terminal(CloudProtocolError),
}

#[derive(serde::Deserialize)]
struct ErrorBody {
    error: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_delay_is_deterministic_exponential_and_bounded() {
        let first = retry_delay("upload-1", 7, 1, None);
        assert_eq!(first, retry_delay("upload-1", 7, 1, None));
        assert!(retry_delay("upload-1", 7, 2, None) > first);
        assert_eq!(
            retry_delay("upload-1", 7, 1, Some(Duration::from_secs(3))),
            Duration::from_secs(3)
        );
        assert_eq!(
            retry_delay("upload-1", 7, 1, Some(Duration::from_secs(3_600))),
            RETRY_BACKOFF_MAX
        );
    }

    #[test]
    fn part_bounds_and_resumable_work_are_fail_closed() {
        assert!(part_slice_for(MAX_UPLOAD_PART_BYTES, MAX_UPLOAD_PART_BYTES, 1).is_ok());
        assert!(part_slice_for(1, MAX_UPLOAD_PART_BYTES + 1, 1).is_err());
        assert!(validate_missing_parts(&[3, 1], 7, 3).is_ok());
        assert!(validate_missing_parts(&[], 7, 3).is_ok());
        assert!(validate_missing_parts(&[0], 7, 3).is_err());
        assert!(validate_missing_parts(&[1, 1], 7, 3).is_err());
        assert!(validate_missing_parts(&[4], 7, 3).is_err());
    }
}
