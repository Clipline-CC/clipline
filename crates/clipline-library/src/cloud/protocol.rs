//! Permissively licensed Clipline Cloud wire contract used by desktop adapters.
//!
//! This module intentionally implements only Clipline's bounded first-party
//! protocol surface. It does not depend on or copy the server's AGPL client.

use std::net::IpAddr;

use chrono::{DateTime, Utc};
use reqwest::{StatusCode, Url};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const EXPECTED_DISCOVERY_NAME: &str = "Clipline Cloud";
pub const SUPPORTED_API_VERSION: &str = "v1";
pub const UPLOAD_PART_SHA256_HEADER: &str = "x-clipline-part-sha256";

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CloudProtocolError {
    #[error("cloud host URL is invalid: {0}")]
    InvalidUrl(String),
    #[error("cloud host URL must use https or http")]
    UnsupportedScheme,
    #[error("plain HTTP requires explicit user confirmation because credentials are sent once")]
    PlainHttpRequiresConfirmation,
    #[error("plain HTTP is only allowed for localhost, loopback, or RFC1918 IPv4 addresses")]
    PlainHttpPublicHost,
    #[error("server did not identify as Clipline Cloud")]
    InvalidDiscovery,
    #[error("unsupported Clipline Cloud API version {0}")]
    UnsupportedApiVersion(String),
    #[error("request failed with {status}: {message}")]
    Api { status: StatusCode, message: String },
    #[error("HTTP client error: {0}")]
    Http(String),
    #[error("request body is inconsistent with declared upload metadata: {0}")]
    InvalidUpload(String),
    #[error("cloud protocol work was canceled")]
    Canceled,
}

impl CloudProtocolError {
    #[must_use]
    pub const fn is_not_found(&self) -> bool {
        matches!(
            self,
            Self::Api {
                status: StatusCode::NOT_FOUND,
                ..
            }
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloudApiBase {
    url: Url,
}

impl CloudApiBase {
    pub fn parse(input: &str, plain_http_confirmed: bool) -> Result<Self, CloudProtocolError> {
        let mut url = Url::parse(input.trim())
            .map_err(|error| CloudProtocolError::InvalidUrl(error.to_string()))?;
        if url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(CloudProtocolError::InvalidUrl(
                "credential-free base URL without query or fragment required".into(),
            ));
        }
        match url.scheme() {
            "https" => {}
            "http" => {
                if !host_is_local(&url) {
                    return Err(CloudProtocolError::PlainHttpPublicHost);
                }
                if !plain_http_confirmed {
                    return Err(CloudProtocolError::PlainHttpRequiresConfirmation);
                }
            }
            _ => return Err(CloudProtocolError::UnsupportedScheme),
        }
        if !url.path().ends_with('/') {
            url.set_path(&format!("{}/", url.path()));
        }
        Ok(Self { url })
    }

    #[must_use]
    pub fn as_url(&self) -> &Url {
        &self.url
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        self.url.as_str()
    }

    pub fn api_url(&self, relative_path: &str) -> Result<Url, CloudProtocolError> {
        let candidate = relative_path.trim();
        if candidate.starts_with("//")
            || candidate.contains(['\\', '?', '#'])
            || Url::parse(candidate).is_ok()
        {
            return Err(CloudProtocolError::InvalidUpload(
                "cloud API path must be relative and contain no authority, query, or fragment"
                    .into(),
            ));
        }
        let relative_path = candidate.trim_start_matches('/');
        if relative_path.is_empty() {
            return Err(CloudProtocolError::InvalidUpload(
                "cloud API path must not be empty".into(),
            ));
        }
        if relative_path
            .split('/')
            .any(|segment| segment == "." || segment == "..")
        {
            return Err(CloudProtocolError::InvalidUpload(
                "cloud API path must not contain traversal segments".into(),
            ));
        }
        self.url
            .join(relative_path)
            .map_err(|error| CloudProtocolError::InvalidUrl(error.to_string()))
    }

    pub fn upload_control_url(
        &self,
        upload_id: &str,
        suffix: Option<&str>,
    ) -> Result<Url, CloudProtocolError> {
        self.resource_url("api/v1/uploads", upload_id, suffix)
    }

    pub fn clip_url(&self, clip_id: &str, suffix: Option<&str>) -> Result<Url, CloudProtocolError> {
        self.resource_url("api/v1/clips", clip_id, suffix)
    }

    pub fn authenticated_upload_url(
        &self,
        template: &str,
        part_number: u16,
    ) -> Result<Url, CloudProtocolError> {
        let replaced = template.replace("{part_number}", &part_number.to_string());
        if replaced.contains('{') || replaced.contains('}') {
            return Err(CloudProtocolError::InvalidUpload(
                "authenticated upload URL contains an unknown placeholder".into(),
            ));
        }
        let url = Url::parse(&replaced)
            .or_else(|_| self.url.join(replaced.trim_start_matches('/')))
            .map_err(|error| CloudProtocolError::InvalidUrl(error.to_string()))?;
        if url.origin() != self.url.origin() {
            return Err(CloudProtocolError::InvalidUpload(format!(
                "authenticated upload URL must use the configured cloud origin: {url}"
            )));
        }
        if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
            return Err(CloudProtocolError::InvalidUpload(
                "authenticated upload URL contains credentials or a fragment".into(),
            ));
        }
        Ok(url)
    }

    fn resource_url(
        &self,
        collection: &str,
        resource_id: &str,
        suffix: Option<&str>,
    ) -> Result<Url, CloudProtocolError> {
        validate_path_segment(resource_id, "resource id")?;
        if let Some(suffix) = suffix {
            validate_path_segment(suffix, "resource suffix")?;
        }
        let mut url = self.api_url(&format!("{collection}/"))?;
        let mut segments = url.path_segments_mut().map_err(|_| {
            CloudProtocolError::InvalidUpload("cloud URL cannot own path segments".into())
        })?;
        segments.pop_if_empty().push(resource_id);
        if let Some(suffix) = suffix {
            segments.push(suffix);
        }
        drop(segments);
        Ok(url)
    }
}

pub fn validate_discovery(discovery: &DiscoveryResponse) -> Result<(), CloudProtocolError> {
    if discovery.name != EXPECTED_DISCOVERY_NAME {
        return Err(CloudProtocolError::InvalidDiscovery);
    }
    if discovery.api_version != SUPPORTED_API_VERSION {
        return Err(CloudProtocolError::UnsupportedApiVersion(
            discovery.api_version.clone(),
        ));
    }
    Ok(())
}

#[must_use]
pub fn sha256_hex(bytes: impl AsRef<[u8]>) -> String {
    format!("{:x}", Sha256::digest(bytes.as_ref()))
}

fn host_is_local(url: &Url) -> bool {
    let raw = url.host_str().unwrap_or_default();
    if raw.eq_ignore_ascii_case("localhost") {
        return true;
    }
    let host = raw
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(raw);
    host.parse::<IpAddr>().is_ok_and(|address| match address {
        IpAddr::V4(address) => address.is_loopback() || address.is_private(),
        IpAddr::V6(address) => address.is_loopback(),
    })
}

fn validate_path_segment(value: &str, context: &str) -> Result<(), CloudProtocolError> {
    if value.is_empty()
        || value.len() > 256
        || value == "."
        || value == ".."
        || value.contains(['/', '\\', '?', '#'])
    {
        return Err(CloudProtocolError::InvalidUpload(format!(
            "{context} is invalid"
        )));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveryResponse {
    pub name: String,
    pub api_version: String,
    pub server_version: String,
    pub min_client_version: String,
    pub public_url: String,
    pub features: DiscoveryFeatures,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveryFeatures {
    pub single_put_upload: bool,
    pub chunked_upload: bool,
    #[serde(default)]
    pub direct_s3_upload: bool,
    pub public_sharing: bool,
    pub clip_markers: bool,
    pub max_upload_size_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateDeviceTokenRequest {
    pub username: String,
    pub password: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateDeviceTokenResponse {
    pub token: String,
    pub device_token: DeviceTokenResponse,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceTokenResponse {
    pub id: String,
    pub name: String,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
    pub revoked_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeResponse {
    pub user: UserResponse,
    pub auth_kind: String,
    pub csrf_token: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserResponse {
    pub id: String,
    pub username: String,
    pub display_name: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub bio: Option<String>,
    #[serde(default)]
    pub avatar_url: Option<String>,
    pub role: String,
    pub is_disabled: bool,
    #[serde(default)]
    pub storage_bytes: u64,
    #[serde(default)]
    pub storage_quota_bytes: Option<u64>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub last_login_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateUploadRequest {
    pub client_clip_id: Option<String>,
    pub title: String,
    pub description: Option<String>,
    pub game_name: Option<String>,
    pub game_id: Option<String>,
    pub game_executable: Option<String>,
    pub source_type: Option<String>,
    pub recorded_at: Option<DateTime<Utc>>,
    pub duration_ms: Option<i64>,
    pub file_size_bytes: u64,
    pub checksum_sha256: String,
    pub container: String,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub fps: Option<f64>,
    pub visibility: Option<String>,
    pub markers: Option<Vec<CreateMarkerRequest>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateMarkerRequest {
    pub kind: String,
    pub label: Option<String>,
    pub timestamp_ms: i64,
    pub metadata: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateUploadResponse {
    pub clip_id: String,
    pub upload_id: String,
    pub mode: String,
    pub part_size_bytes: u64,
    pub single_put_url: Option<String>,
    pub parts_url_template: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direct_part_presign_url_template: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direct_part_ack_url_template: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadProgressResponse {
    pub upload_id: String,
    pub clip_id: String,
    pub mode: String,
    pub status: String,
    pub file_size_bytes: u64,
    pub part_size_bytes: u64,
    pub received_size_bytes: u64,
    pub total_parts: u16,
    pub received_part_count: u16,
    pub missing_part_count: u16,
    pub next_part_number: Option<u16>,
    pub progress_basis_points: u16,
    pub failure_reason: Option<String>,
    pub recovery_action: Option<String>,
    pub expires_at: DateTime<Utc>,
    pub received_parts: Vec<u16>,
    pub missing_parts: Vec<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartUploadResponse {
    pub upload_id: String,
    pub part_number: u16,
    pub size_bytes: u64,
    pub checksum_sha256: String,
    pub etag: Option<String>,
    pub idempotent: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectUploadHeader {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectPartUploadUrlResponse {
    pub upload_id: String,
    pub part_number: u16,
    pub method: String,
    pub url: String,
    pub expires_at: DateTime<Utc>,
    pub expected_size_bytes: u64,
    pub headers: Vec<DirectUploadHeader>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectPartUploadAckRequest {
    pub size_bytes: u64,
    pub checksum_sha256: String,
    pub etag: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClipDetailResponse {
    pub id: String,
    pub client_clip_id: Option<String>,
    pub title: String,
    pub description: Option<String>,
    pub game_name: Option<String>,
    pub game_id: Option<String>,
    pub game_executable: Option<String>,
    pub source_type: Option<String>,
    pub recorded_at: Option<DateTime<Utc>>,
    pub uploaded_at: Option<DateTime<Utc>>,
    pub duration_ms: Option<i64>,
    pub file_size_bytes: Option<i64>,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub fps: Option<f64>,
    pub container: Option<String>,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub checksum_sha256: Option<String>,
    pub visibility: String,
    pub status: String,
    pub public_share_id: Option<String>,
    pub public_url: Option<String>,
    #[serde(default)]
    pub view_count: i64,
    pub markers: Vec<ClipMarkerResponse>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClipMarkerResponse {
    pub id: String,
    pub kind: String,
    pub label: Option<String>,
    pub timestamp_ms: i64,
    pub metadata: Option<serde_json::Value>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateVisibilityRequest {
    pub visibility: String,
}
