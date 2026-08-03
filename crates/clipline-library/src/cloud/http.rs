//! Bounded, redirect-free HTTP primitives for Clipline Cloud.
//!
//! The first-party API client currently carries an AGPL workspace license, so
//! the MIT/Apache desktop does not link it here. These helpers keep the small
//! desktop transport on reviewed `reqwest` + rustls while preserving the
//! existing response and error-body ceilings.

use std::time::Duration;

use bytes::Bytes;
use futures_util::StreamExt;
use reqwest::{redirect::Policy, Client, Response, StatusCode};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::cache::{
    CancellationProbe, CloudAssetRequest, CloudCacheError, DownloadPort, DownloadReceipt,
    DownloadSink, DownloadStatus,
};
use super::ports::{
    AvatarTransportResult, CloudCredential, CloudRequestFence, CloudTransport,
    CloudTransportFuture, PortError,
};
use super::{
    CloudClipSummary, CloudListTransportRequest, CloudListTransportResponse, CloudProfileTransport,
    CloudServiceAccount, CloudWorkToken, CLOUD_AVATAR_MAX_BYTES,
};

pub const MAX_CLOUD_CONTROL_JSON_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_CLOUD_ERROR_BODY_BYTES: usize = 64 * 1024;
pub const CLOUD_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
pub const CLOUD_CONTROL_READ_TIMEOUT: Duration = Duration::from_secs(15);
pub const CLOUD_CONTROL_TOTAL_TIMEOUT: Duration = Duration::from_secs(30);
pub const CLOUD_STREAM_READ_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Error)]
pub enum CloudHttpError {
    #[error("build cloud HTTP client: {0}")]
    Client(#[source] reqwest::Error),
    #[error("cloud host is invalid: {0}")]
    InvalidHost(String),
    #[error("{context} was canceled")]
    Canceled { context: String },
    #[error("{context} is {actual} bytes; maximum is {maximum}")]
    TooLarge {
        context: String,
        actual: u64,
        maximum: usize,
    },
    #[error("read {context}: {source}")]
    Read {
        context: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("decode {context}: {source}")]
    Json {
        context: String,
        #[source]
        source: serde_json::Error,
    },
}

pub fn control_client() -> Result<Client, CloudHttpError> {
    Client::builder()
        .redirect(Policy::none())
        .connect_timeout(CLOUD_CONNECT_TIMEOUT)
        .read_timeout(CLOUD_CONTROL_READ_TIMEOUT)
        .timeout(CLOUD_CONTROL_TOTAL_TIMEOUT)
        .build()
        .map_err(CloudHttpError::Client)
}

pub fn stream_client() -> Result<Client, CloudHttpError> {
    Client::builder()
        .redirect(Policy::none())
        .connect_timeout(CLOUD_CONNECT_TIMEOUT)
        .read_timeout(CLOUD_STREAM_READ_TIMEOUT)
        .build()
        .map_err(CloudHttpError::Client)
}

/// Blocking, bounded asset transport intended for [`super::cache::CloudCache`]
/// workers. It uses rustls, rejects redirects, checks cancellation between
/// chunks, and never buffers the asset body in memory.
#[derive(Clone)]
pub struct ReqwestAssetDownload {
    client: Client,
    api_base: reqwest::Url,
    bearer_token: String,
}

impl ReqwestAssetDownload {
    pub fn new(host_url: &str, bearer_token: impl Into<String>) -> Result<Self, CloudHttpError> {
        Ok(Self {
            client: stream_client()?,
            api_base: validated_api_base(host_url)?,
            bearer_token: bearer_token.into(),
        })
    }

    async fn download_async(
        &self,
        request: &CloudAssetRequest,
        sink: &mut DownloadSink<'_>,
        cancellation: &dyn CancellationProbe,
    ) -> Result<DownloadReceipt, CloudCacheError> {
        if cancellation.is_cancelled() {
            return Err(CloudCacheError::Canceled);
        }
        let relative = format!(
            "api/v1/clips/{}/{}",
            request.asset.remote_clip_id(),
            request.asset.kind().label()
        );
        let url = self.api_base.join(&relative).map_err(|error| {
            CloudCacheError::Download(format!("build cloud asset URL: {error}"))
        })?;
        let response = self
            .client
            .get(url)
            .bearer_auth(&self.bearer_token)
            .send()
            .await
            .map_err(|error| CloudCacheError::Download(format!("request cloud asset: {error}")))?;
        let status = response.status();
        if status == StatusCode::NOT_FOUND {
            return Ok(DownloadReceipt {
                status: DownloadStatus::Missing,
                advertised_size_bytes: response.content_length(),
            });
        }
        if !status.is_success() {
            let message = match response_bytes_limited(
                response,
                MAX_CLOUD_ERROR_BODY_BYTES,
                "cloud asset error",
                || cancellation.is_cancelled(),
            )
            .await
            {
                Ok(bytes) => {
                    let text = String::from_utf8_lossy(&bytes);
                    let text = text.trim();
                    if text.is_empty() {
                        "empty error body".into()
                    } else {
                        text.to_owned()
                    }
                }
                Err(CloudHttpError::TooLarge { .. }) => {
                    format!("response body exceeded {MAX_CLOUD_ERROR_BODY_BYTES} bytes")
                }
                Err(error) => error.to_string(),
            };
            return Err(CloudCacheError::Download(format!(
                "cloud asset returned {status}: {message}"
            )));
        }
        let advertised_size_bytes = response.content_length();
        if advertised_size_bytes.is_some_and(|bytes| bytes > request.hard_limit_bytes()) {
            return Err(CloudCacheError::TooLarge {
                limit: request.hard_limit_bytes(),
            });
        }
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            if cancellation.is_cancelled() {
                return Err(CloudCacheError::Canceled);
            }
            let chunk = chunk.map_err(|error| {
                CloudCacheError::Download(format!("read cloud asset response: {error}"))
            })?;
            sink.write_chunk(&chunk)?;
        }
        Ok(DownloadReceipt {
            status: DownloadStatus::Found,
            advertised_size_bytes,
        })
    }
}

impl DownloadPort for ReqwestAssetDownload {
    fn download(
        &self,
        request: &CloudAssetRequest,
        sink: &mut DownloadSink<'_>,
        cancellation: &dyn CancellationProbe,
    ) -> Result<DownloadReceipt, CloudCacheError> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| {
                CloudCacheError::Internal(format!("create cloud asset runtime: {error}"))
            })?;
        runtime.block_on(self.download_async(request, sink, cancellation))
    }
}

fn validated_api_base(host_url: &str) -> Result<reqwest::Url, CloudHttpError> {
    let mut base = reqwest::Url::parse(host_url.trim())
        .map_err(|error| CloudHttpError::InvalidHost(error.to_string()))?;
    if !matches!(base.scheme(), "http" | "https")
        || base.host_str().is_none()
        || !base.username().is_empty()
        || base.password().is_some()
        || base.query().is_some()
        || base.fragment().is_some()
    {
        return Err(CloudHttpError::InvalidHost(
            "only credential-free http(s) base URLs are accepted".into(),
        ));
    }
    if base.scheme() == "http" && !super::url_host_is_local(&base) {
        return Err(CloudHttpError::InvalidHost(
            "plain HTTP cloud host is not local".into(),
        ));
    }
    if !base.path().ends_with('/') {
        base.set_path(&format!("{}/", base.path()));
    }
    Ok(base)
}

pub async fn response_bytes_limited(
    response: Response,
    maximum: usize,
    context: impl Into<String>,
    is_canceled: impl Fn() -> bool,
) -> Result<Vec<u8>, CloudHttpError> {
    let context = context.into();
    if let Some(actual) = response.content_length() {
        if actual > maximum as u64 {
            return Err(CloudHttpError::TooLarge {
                context,
                actual,
                maximum,
            });
        }
    }

    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(
            response
                .content_length()
                .unwrap_or_default()
                .min(maximum as u64) as usize,
        )
        .map_err(|_| CloudHttpError::TooLarge {
            context: context.clone(),
            actual: maximum.saturating_add(1) as u64,
            maximum,
        })?;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        if is_canceled() {
            return Err(CloudHttpError::Canceled { context });
        }
        let chunk = chunk.map_err(|source| CloudHttpError::Read {
            context: context.clone(),
            source,
        })?;
        extend_checked(&mut bytes, &chunk, maximum, &context)?;
    }
    Ok(bytes)
}

fn extend_checked(
    destination: &mut Vec<u8>,
    chunk: &Bytes,
    maximum: usize,
    context: &str,
) -> Result<(), CloudHttpError> {
    let actual =
        destination
            .len()
            .checked_add(chunk.len())
            .ok_or_else(|| CloudHttpError::TooLarge {
                context: context.to_owned(),
                actual: u64::MAX,
                maximum,
            })?;
    if actual > maximum {
        return Err(CloudHttpError::TooLarge {
            context: context.to_owned(),
            actual: actual as u64,
            maximum,
        });
    }
    destination
        .try_reserve_exact(chunk.len())
        .map_err(|_| CloudHttpError::TooLarge {
            context: context.to_owned(),
            actual: actual as u64,
            maximum,
        })?;
    destination.extend_from_slice(chunk);
    Ok(())
}

pub async fn bounded_json<T: DeserializeOwned>(
    response: Response,
    context: impl Into<String>,
    is_canceled: impl Fn() -> bool,
) -> Result<T, CloudHttpError> {
    let context = context.into();
    let bytes = response_bytes_limited(
        response,
        MAX_CLOUD_CONTROL_JSON_BYTES,
        context.clone(),
        is_canceled,
    )
    .await?;
    serde_json::from_slice(&bytes).map_err(|source| CloudHttpError::Json { context, source })
}

pub async fn bounded_error_message(response: Response, context: &str) -> String {
    let status = response.status();
    match response_bytes_limited(response, MAX_CLOUD_ERROR_BODY_BYTES, context, || false).await {
        Ok(bytes) => {
            let text = String::from_utf8_lossy(&bytes);
            let text = text.trim();
            if text.is_empty() {
                status.to_string()
            } else {
                text.to_owned()
            }
        }
        Err(CloudHttpError::TooLarge { .. }) => {
            format!("{status}; response body exceeded {MAX_CLOUD_ERROR_BODY_BYTES} bytes")
        }
        Err(error) => format!("{status}; {error}"),
    }
}

#[must_use]
pub fn successful_or_missing(status: StatusCode, missing_ok: bool) -> bool {
    status.is_success() || (missing_ok && status == StatusCode::NOT_FOUND)
}

/// Reviewed rustls transport used by both the compatibility adapter and Slint.
/// It deliberately implements only the bounded read-only Task-6 surface.
#[derive(Clone)]
pub struct ReqwestCloudTransport {
    control: Client,
    stream: Client,
}

impl ReqwestCloudTransport {
    pub fn new() -> Result<Self, CloudHttpError> {
        Ok(Self {
            control: control_client()?,
            stream: stream_client()?,
        })
    }
}

#[derive(Debug, Serialize)]
struct ListQuery<'a> {
    sort: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    game: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_type: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    visibility: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    from: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    to: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    min_duration_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_duration_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    min_size_bytes: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_size_bytes: Option<i64>,
    #[serde(rename = "q", skip_serializing_if = "Option::is_none")]
    query: Option<&'a str>,
    page: u32,
    page_size: u16,
}

impl<'a> From<&'a CloudListTransportRequest> for ListQuery<'a> {
    fn from(request: &'a CloudListTransportRequest) -> Self {
        Self {
            sort: &request.query.sort,
            game: request.query.game.as_deref(),
            source_type: request.query.source_type.as_deref(),
            visibility: request.query.visibility.as_deref(),
            status: request.query.status.as_deref(),
            from: request.query.from.as_deref(),
            to: request.query.to.as_deref(),
            min_duration_ms: request.query.min_duration_ms,
            max_duration_ms: request.query.max_duration_ms,
            min_size_bytes: request.query.min_size_bytes,
            max_size_bytes: request.query.max_size_bytes,
            query: request.query.query.as_deref(),
            page: request.page,
            page_size: request.page_size,
        }
    }
}

#[derive(Debug, Deserialize)]
struct ListResponse {
    page: i64,
    page_size: i64,
    clips: Vec<ClipResponse>,
}

#[derive(Debug, Deserialize)]
struct ClipResponse {
    id: String,
    #[serde(default)]
    client_clip_id: Option<String>,
    title: String,
    #[serde(default)]
    source_type: Option<String>,
    #[serde(default)]
    uploaded_at: Option<String>,
    #[serde(default)]
    duration_ms: Option<i64>,
    #[serde(default)]
    file_size_bytes: Option<i64>,
    visibility: String,
    status: String,
    #[serde(default)]
    public_url: Option<String>,
    updated_at: String,
}

#[derive(Debug, Deserialize)]
struct MeResponse {
    user: UserResponse,
}

#[derive(Debug, Deserialize)]
struct UserResponse {
    id: String,
    username: String,
    #[serde(default)]
    display_name: Option<String>,
}

impl CloudTransport for ReqwestCloudTransport {
    fn list<'a>(
        &'a self,
        account: &'a CloudServiceAccount,
        credential: &'a CloudCredential,
        request: &'a CloudListTransportRequest,
        cancellation: &'a dyn CloudRequestFence,
        token: &'a CloudWorkToken,
    ) -> CloudTransportFuture<'a, CloudListTransportResponse> {
        Box::pin(async move {
            let url = api_url(account, "api/v1/clips")?;
            let send = self
                .control
                .get(url)
                .bearer_auth(credential.expose())
                .query(&ListQuery::from(request))
                .send();
            let response = tokio::select! {
                result = send => result,
                _ = cancellation.cancelled(token) => return Err(PortError::canceled()),
            }
            .map_err(|error| PortError::new(format!("list cloud clips: {error}")))?;
            if !response.status().is_success() {
                let status = response.status();
                let message = bounded_error_message(response, "list cloud clips").await;
                return Err(PortError::new(format!(
                    "list cloud clips failed with {status}: {message}"
                )));
            }
            let response: ListResponse = bounded_json(response, "list cloud clips", || {
                !cancellation.is_current(token)
            })
            .await
            .map_err(map_http_port_error)?;
            let page = u32::try_from(response.page)
                .map_err(|_| PortError::new("cloud list returned an invalid page"))?;
            let page_size = u16::try_from(response.page_size)
                .map_err(|_| PortError::new("cloud list returned an invalid page size"))?;
            let clips = response
                .clips
                .into_iter()
                .map(map_clip_response)
                .collect::<Result<Vec<_>, _>>()?;
            Ok(CloudListTransportResponse {
                page,
                page_size,
                clips,
            })
        })
    }

    fn profile<'a>(
        &'a self,
        account: &'a CloudServiceAccount,
        credential: &'a CloudCredential,
        cancellation: &'a dyn CloudRequestFence,
        token: &'a CloudWorkToken,
    ) -> CloudTransportFuture<'a, CloudProfileTransport> {
        Box::pin(async move {
            let send = self
                .control
                .get(api_url(account, "api/v1/me")?)
                .bearer_auth(credential.expose())
                .send();
            let response = tokio::select! {
                result = send => result,
                _ = cancellation.cancelled(token) => return Err(PortError::canceled()),
            }
            .map_err(|error| PortError::new(format!("get cloud profile: {error}")))?;
            if !response.status().is_success() {
                let status = response.status();
                let message = bounded_error_message(response, "get cloud profile").await;
                return Err(PortError::new(format!(
                    "get cloud profile failed with {status}: {message}"
                )));
            }
            let response: MeResponse = bounded_json(response, "get cloud profile", || {
                !cancellation.is_current(token)
            })
            .await
            .map_err(map_http_port_error)?;
            Ok(CloudProfileTransport {
                user_id: response.user.id,
                username: response.user.username,
                display_name: response.user.display_name,
            })
        })
    }

    fn avatar<'a>(
        &'a self,
        account: &'a CloudServiceAccount,
        credential: &'a CloudCredential,
        etag: Option<&'a str>,
        cancellation: &'a dyn CloudRequestFence,
        token: &'a CloudWorkToken,
    ) -> CloudTransportFuture<'a, AvatarTransportResult> {
        Box::pin(async move {
            let mut request = self
                .stream
                .get(api_url(account, "api/v1/me/avatar")?)
                .bearer_auth(credential.expose());
            if let Some(etag) = etag {
                request = request.header(reqwest::header::IF_NONE_MATCH, etag);
            }
            let response = tokio::select! {
                result = request.send() => result,
                _ = cancellation.cancelled(token) => return Err(PortError::canceled()),
            }
            .map_err(|error| PortError::new(format!("download cloud avatar: {error}")))?;
            match response.status() {
                StatusCode::NOT_FOUND => Ok(AvatarTransportResult::Missing),
                StatusCode::NOT_MODIFIED => Ok(AvatarTransportResult::NotModified),
                status if status.is_success() => {
                    let content_type = header_text(&response, reqwest::header::CONTENT_TYPE);
                    let etag = header_text(&response, reqwest::header::ETAG);
                    let bytes = response_bytes_limited(
                        response,
                        CLOUD_AVATAR_MAX_BYTES,
                        "cloud avatar",
                        || !cancellation.is_current(token),
                    )
                    .await
                    .map_err(map_http_port_error)?;
                    Ok(AvatarTransportResult::Fresh {
                        content_type,
                        etag,
                        bytes,
                    })
                }
                status => {
                    let message = bounded_error_message(response, "cloud avatar").await;
                    Err(PortError::new(format!(
                        "download cloud avatar failed with {status}: {message}"
                    )))
                }
            }
        })
    }
}

fn api_url(account: &CloudServiceAccount, path: &str) -> Result<reqwest::Url, PortError> {
    let mut base = reqwest::Url::parse(&account.snapshot.host_url)
        .map_err(|error| PortError::new(format!("cloud host is invalid: {error}")))?;
    if !matches!(base.scheme(), "http" | "https")
        || base.host_str().is_none()
        || !base.username().is_empty()
        || base.password().is_some()
        || base.query().is_some()
        || base.fragment().is_some()
    {
        return Err(PortError::new("cloud host is invalid"));
    }
    if base.scheme() == "http" && !super::url_host_is_local(&base) {
        return Err(PortError::new("plain HTTP cloud host is not local"));
    }
    if !base.path().ends_with('/') {
        base.set_path(&format!("{}/", base.path()));
    }
    base.join(path)
        .map_err(|error| PortError::new(format!("cloud API URL is invalid: {error}")))
}

fn map_clip_response(clip: ClipResponse) -> Result<CloudClipSummary, PortError> {
    Ok(CloudClipSummary {
        remote_clip_id: clip.id,
        local_clip_id: clip.client_clip_id,
        title: clip.title,
        public_url: clip.public_url,
        visibility: clip.visibility,
        status: clip.status,
        updated_at_unix: parse_timestamp(&clip.updated_at)?,
        uploaded_at_unix: clip
            .uploaded_at
            .as_deref()
            .map(parse_timestamp)
            .transpose()?,
        duration_ms: clip.duration_ms,
        file_size_bytes: clip.file_size_bytes,
        source_type: clip.source_type,
    })
}

fn parse_timestamp(value: &str) -> Result<u64, PortError> {
    let timestamp = chrono::DateTime::parse_from_rfc3339(value)
        .map_err(|error| PortError::new(format!("cloud timestamp is invalid: {error}")))?
        .timestamp();
    Ok(u64::try_from(timestamp).unwrap_or_default())
}

fn header_text(response: &Response, name: reqwest::header::HeaderName) -> Option<String> {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

fn map_http_port_error(error: CloudHttpError) -> PortError {
    if matches!(error, CloudHttpError::Canceled { .. }) {
        PortError::canceled()
    } else {
        PortError::new(error.to_string())
    }
}
