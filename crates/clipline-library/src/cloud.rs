//! Framework-neutral Clipline Cloud list, profile, and avatar service.

pub mod cache;
pub mod cache_identity;
pub mod http;
pub mod ports;
pub mod protocol;
pub mod settings;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    CatalogRevision, CloudAccountKey, CloudAccountSnapshot, CloudLibraryItem,
    CloudListPageCompletion, CloudPageNumber, CloudWorkToken, PayloadBoundsError,
    MAX_CATALOG_PAGE_ROWS, MAX_CATALOG_STRING_BYTES, MAX_CLOUD_INDEX_ROWS,
};
use ports::{
    AvatarTransportResult, CloudAccountPort, CloudCredential, CloudCredentialPort,
    CloudProfilePatch, CloudRequestFence, CloudTransport, PortError,
};

pub const CLOUD_PAGE_SIZE: usize = MAX_CATALOG_PAGE_ROWS;
pub const CLOUD_LEGACY_PAGE_SIZE: usize = 100;
pub const CLOUD_LEGACY_MAX_PAGES: u32 = 100;
pub const CLOUD_LEGACY_MAX_CLIPS: usize = 10_000;
pub const CLOUD_AVATAR_MAX_BYTES: usize = 2 * 1024 * 1024;

const _: () = assert!(CLOUD_PAGE_SIZE == 60);
const _: () = assert!(CLOUD_LEGACY_PAGE_SIZE * CLOUD_LEGACY_MAX_PAGES as usize == 10_000);
const _: () = assert!(CLOUD_LEGACY_MAX_CLIPS == MAX_CLOUD_INDEX_ROWS);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloudServiceAccount {
    pub snapshot: CloudAccountSnapshot,
    pub credential_target: Option<String>,
    /// Local paths keyed by the server's stable `client_clip_id`.
    pub local_paths_by_clip_id: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloudListQuery {
    pub sort: String,
    pub game: Option<String>,
    pub source_type: Option<String>,
    pub visibility: Option<String>,
    pub status: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub min_duration_ms: Option<i64>,
    pub max_duration_ms: Option<i64>,
    pub min_size_bytes: Option<i64>,
    pub max_size_bytes: Option<i64>,
    pub query: Option<String>,
}

impl Default for CloudListQuery {
    fn default() -> Self {
        Self {
            sort: "uploaded_at_desc".into(),
            game: None,
            source_type: None,
            visibility: None,
            status: None,
            from: None,
            to: None,
            min_duration_ms: None,
            max_duration_ms: None,
            min_size_bytes: None,
            max_size_bytes: None,
            query: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloudListTransportRequest {
    /// One-based server page.
    pub page: u32,
    pub page_size: u16,
    pub query: CloudListQuery,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CloudListTransportResponse {
    pub page: u32,
    pub page_size: u16,
    pub clips: Vec<CloudClipSummary>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CloudClipSummary {
    pub remote_clip_id: String,
    pub local_clip_id: Option<String>,
    pub title: String,
    pub public_url: Option<String>,
    pub visibility: String,
    pub status: String,
    pub updated_at_unix: u64,
    pub uploaded_at_unix: Option<u64>,
    pub duration_ms: Option<i64>,
    pub file_size_bytes: Option<i64>,
    pub source_type: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloudProfileTransport {
    pub user_id: String,
    pub username: String,
    pub display_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloudUserProfile {
    pub user_id: String,
    pub username: String,
    pub display_name: Option<String>,
    pub profile_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloudConnectionStatus {
    pub connected: bool,
    pub token_present: bool,
    pub host_url: String,
    pub public_url: Option<String>,
    pub username: Option<String>,
    pub display_name: Option<String>,
    pub user_id: Option<String>,
    pub default_visibility: String,
    pub delete_local_after_upload: bool,
    pub auto_upload_rules: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloudAvatar {
    pub content_type: String,
    pub etag: Option<String>,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloudBrowserEffect {
    pub url: String,
    pub context: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CloudLegacyListResult {
    pub clips: Vec<CloudLibraryItem>,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FencedCloudResult<T> {
    pub token: CloudWorkToken,
    pub value: T,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CloudServiceError {
    #[error("cloud work belongs to a stale window or request")]
    StaleWork,
    #[error("cloud account changed while work was in flight")]
    AccountChanged,
    #[error("connect to Clipline Cloud first")]
    NotConnected,
    #[error("Clipline Cloud credential is unavailable")]
    MissingCredential,
    #[error("invalid cloud request: {0}")]
    InvalidRequest(String),
    #[error("invalid cloud response: {0}")]
    InvalidResponse(String),
    #[error("cloud port failed: {0}")]
    Port(String),
}

#[derive(Clone)]
struct CachedAvatar {
    account_key: CloudAccountKey,
    account_generation: crate::CloudAccountGeneration,
    avatar: CloudAvatar,
}

pub struct CloudService {
    accounts: Arc<dyn CloudAccountPort>,
    credentials: Arc<dyn CloudCredentialPort>,
    transport: Arc<dyn CloudTransport>,
    avatar: Mutex<Option<CachedAvatar>>,
}

impl CloudService {
    #[must_use]
    pub fn new(
        accounts: Arc<dyn CloudAccountPort>,
        credentials: Arc<dyn CloudCredentialPort>,
        transport: Arc<dyn CloudTransport>,
    ) -> Self {
        Self {
            accounts,
            credentials,
            transport,
            avatar: Mutex::new(None),
        }
    }

    pub async fn list_page(
        &self,
        token: CloudWorkToken,
        fence: &dyn CloudRequestFence,
        revision: CatalogRevision,
        page: CloudPageNumber,
        query: CloudListQuery,
    ) -> Result<CloudListPageCompletion, CloudServiceError> {
        validate_query(&query)?;
        let (account, credential) = self.begin(&token, fence)?;
        let request = CloudListTransportRequest {
            page: page.get(),
            page_size: CLOUD_PAGE_SIZE as u16,
            query,
        };
        let response = self
            .transport
            .list(&account, &credential, &request, fence, &token)
            .await
            .map_err(map_port_error)?;
        self.ensure_current(&token, fence)?;
        let items = map_response(&account, &request, response)?;
        self.ensure_current(&token, fence)?;
        if items.is_empty() && page.get() > 1 {
            CloudListPageCompletion::past_end(token, revision, page, Vec::new())
                .map_err(map_payload_error)
        } else {
            CloudListPageCompletion::page(token, revision, page, items, Vec::new())
                .map_err(map_payload_error)
        }
    }

    pub fn status(
        &self,
        token: CloudWorkToken,
        fence: &dyn CloudRequestFence,
    ) -> Result<FencedCloudResult<CloudConnectionStatus>, CloudServiceError> {
        let account = self.ensure_current(&token, fence)?;
        validate_account_bounds(&account)?;
        let token_present = account
            .credential_target
            .as_deref()
            .is_some_and(|target| self.credentials.read(target).is_ok());
        let account = self.ensure_current(&token, fence)?;
        validate_account_bounds(&account)?;
        let snapshot = account.snapshot;
        Ok(FencedCloudResult {
            token,
            value: CloudConnectionStatus {
                connected: snapshot.connected && token_present,
                token_present,
                host_url: snapshot.host_url,
                public_url: snapshot.public_url,
                username: snapshot.username,
                display_name: snapshot.display_name,
                user_id: snapshot.user_id,
                default_visibility: snapshot.default_visibility,
                delete_local_after_upload: snapshot.delete_local_after_upload,
                auto_upload_rules: snapshot.auto_upload_rules,
            },
        })
    }

    pub async fn legacy_list(
        &self,
        token: CloudWorkToken,
        fence: &dyn CloudRequestFence,
    ) -> Result<FencedCloudResult<CloudLegacyListResult>, CloudServiceError> {
        let (account, credential) = self.begin(&token, fence)?;
        let mut clips = Vec::new();
        let mut remote_ids = BTreeSet::new();
        let mut truncated = false;
        for page in 1..=CLOUD_LEGACY_MAX_PAGES {
            let request = CloudListTransportRequest {
                page,
                page_size: CLOUD_LEGACY_PAGE_SIZE as u16,
                query: CloudListQuery::default(),
            };
            let response = self
                .transport
                .list(&account, &credential, &request, fence, &token)
                .await
                .map_err(map_port_error)?;
            self.ensure_current(&token, fence)?;
            let page_items = map_response(&account, &request, response)?;
            let full = page_items.len() == CLOUD_LEGACY_PAGE_SIZE;
            for clip in page_items {
                if remote_ids.insert(clip.remote_clip_id.clone()) {
                    clips.push(clip);
                }
            }
            if !full {
                break;
            }
            if page == CLOUD_LEGACY_MAX_PAGES || clips.len() >= CLOUD_LEGACY_MAX_CLIPS {
                truncated = true;
                break;
            }
        }
        self.ensure_current(&token, fence)?;
        Ok(FencedCloudResult {
            token,
            value: CloudLegacyListResult { clips, truncated },
        })
    }

    pub async fn profile(
        &self,
        token: CloudWorkToken,
        fence: &dyn CloudRequestFence,
    ) -> Result<FencedCloudResult<CloudUserProfile>, CloudServiceError> {
        let (account, credential) = self.begin(&token, fence)?;
        let response = self
            .transport
            .profile(&account, &credential, fence, &token)
            .await
            .map_err(map_port_error)?;
        self.ensure_current(&token, fence)?;
        validate_required("profile.user_id", &response.user_id, true)?;
        validate_required("profile.username", &response.username, true)?;
        if account.snapshot.user_id.as_deref() != Some(response.user_id.as_str()) {
            return Err(CloudServiceError::AccountChanged);
        }
        let display_name = normalize_optional(response.display_name, "profile.display_name")?;
        let patch = CloudProfilePatch {
            user_id: response.user_id.clone(),
            username: response.username.clone(),
            display_name: display_name.clone(),
        };
        let updated = self
            .accounts
            .apply_profile(&token.account_key, token.account_generation, patch)
            .map_err(map_port_error)?;
        ensure_account_matches(&updated, &token)?;
        self.ensure_current(&token, fence)?;
        let profile_url = profile_url(&updated, &response.username)?;
        self.ensure_current(&token, fence)?;
        Ok(FencedCloudResult {
            token,
            value: CloudUserProfile {
                user_id: response.user_id,
                username: response.username,
                display_name,
                profile_url,
            },
        })
    }

    pub async fn avatar(
        &self,
        token: CloudWorkToken,
        fence: &dyn CloudRequestFence,
    ) -> Result<FencedCloudResult<Option<CloudAvatar>>, CloudServiceError> {
        let (account, credential) = self.begin(&token, fence)?;
        let cached = self.cached_avatar(&token)?;
        let response = self
            .transport
            .avatar(
                &account,
                &credential,
                cached.as_ref().and_then(|avatar| avatar.etag.as_deref()),
                fence,
                &token,
            )
            .await
            .map_err(map_port_error)?;
        self.ensure_current(&token, fence)?;
        let value = match response {
            AvatarTransportResult::Missing => {
                self.store_avatar(None)?;
                None
            }
            AvatarTransportResult::NotModified => Some(cached.ok_or_else(|| {
                CloudServiceError::InvalidResponse(
                    "avatar returned not-modified without a cached value".into(),
                )
            })?),
            AvatarTransportResult::Fresh {
                content_type,
                etag,
                bytes,
            } => {
                let avatar = validate_avatar(content_type, etag, bytes)?;
                self.store_avatar(Some(CachedAvatar {
                    account_key: token.account_key.clone(),
                    account_generation: token.account_generation,
                    avatar: avatar.clone(),
                }))?;
                Some(avatar)
            }
        };
        self.ensure_current(&token, fence)?;
        Ok(FencedCloudResult { token, value })
    }

    pub fn open_profile_effect(
        &self,
        token: CloudWorkToken,
        fence: &dyn CloudRequestFence,
    ) -> Result<FencedCloudResult<CloudBrowserEffect>, CloudServiceError> {
        let account = self.ensure_current(&token, fence)?;
        let username =
            account.snapshot.username.as_deref().ok_or_else(|| {
                CloudServiceError::InvalidRequest("cloud username is unknown".into())
            })?;
        let effect = CloudBrowserEffect {
            url: profile_url(&account, username)?,
            context: "cloud user profile".into(),
        };
        self.ensure_current(&token, fence)?;
        Ok(FencedCloudResult {
            token,
            value: effect,
        })
    }

    pub fn open_clip_effect(
        &self,
        token: CloudWorkToken,
        fence: &dyn CloudRequestFence,
        remote_clip_id: &str,
    ) -> Result<FencedCloudResult<CloudBrowserEffect>, CloudServiceError> {
        let account = self.ensure_current(&token, fence)?;
        let remote_clip_id = validate_safe_component(remote_clip_id, "remote clip id")?;
        let effect = CloudBrowserEffect {
            url: append_segment(public_base(&account)?, "clip", remote_clip_id)?,
            context: "cloud clip page".into(),
        };
        self.ensure_current(&token, fence)?;
        Ok(FencedCloudResult {
            token,
            value: effect,
        })
    }

    fn begin(
        &self,
        token: &CloudWorkToken,
        fence: &dyn CloudRequestFence,
    ) -> Result<(CloudServiceAccount, CloudCredential), CloudServiceError> {
        let account = self.ensure_current(token, fence)?;
        if !account.snapshot.connected {
            return Err(CloudServiceError::NotConnected);
        }
        validate_account(&account)?;
        let target = account
            .credential_target
            .as_deref()
            .ok_or(CloudServiceError::MissingCredential)?;
        let credential = self.credentials.read(target).map_err(map_port_error)?;
        self.ensure_current(token, fence)?;
        Ok((account, credential))
    }

    fn ensure_current(
        &self,
        token: &CloudWorkToken,
        fence: &dyn CloudRequestFence,
    ) -> Result<CloudServiceAccount, CloudServiceError> {
        if !fence.is_current(token) {
            return Err(CloudServiceError::StaleWork);
        }
        let account = self.accounts.snapshot().map_err(map_port_error)?;
        ensure_account_matches(&account, token)?;
        Ok(account)
    }

    fn cached_avatar(
        &self,
        token: &CloudWorkToken,
    ) -> Result<Option<CloudAvatar>, CloudServiceError> {
        let cache = self
            .avatar
            .lock()
            .map_err(|_| CloudServiceError::Port("avatar cache lock poisoned".into()))?;
        Ok(cache
            .as_ref()
            .filter(|cached| {
                cached.account_key == token.account_key
                    && cached.account_generation == token.account_generation
            })
            .map(|cached| cached.avatar.clone()))
    }

    fn store_avatar(&self, avatar: Option<CachedAvatar>) -> Result<(), CloudServiceError> {
        *self
            .avatar
            .lock()
            .map_err(|_| CloudServiceError::Port("avatar cache lock poisoned".into()))? = avatar;
        Ok(())
    }
}

fn ensure_account_matches(
    account: &CloudServiceAccount,
    token: &CloudWorkToken,
) -> Result<(), CloudServiceError> {
    if account.snapshot.account_key != token.account_key
        || account.snapshot.generation != token.account_generation
    {
        Err(CloudServiceError::AccountChanged)
    } else {
        Ok(())
    }
}

fn validate_account(account: &CloudServiceAccount) -> Result<(), CloudServiceError> {
    validate_account_bounds(account)?;
    validate_required("account.host_url", &account.snapshot.host_url, true)?;
    Ok(())
}

fn validate_account_bounds(account: &CloudServiceAccount) -> Result<(), CloudServiceError> {
    validate_optional("account.host_url", Some(&account.snapshot.host_url))?;
    validate_optional("account.public_url", account.snapshot.public_url.as_deref())?;
    validate_optional("account.username", account.snapshot.username.as_deref())?;
    validate_optional(
        "account.display_name",
        account.snapshot.display_name.as_deref(),
    )?;
    validate_optional("account.user_id", account.snapshot.user_id.as_deref())?;
    validate_optional(
        "account.credential_target",
        account.credential_target.as_deref(),
    )?;
    validate_required(
        "account.default_visibility",
        &account.snapshot.default_visibility,
        true,
    )?;
    if !matches!(
        account.snapshot.default_visibility.as_str(),
        "private" | "public" | "unlisted"
    ) {
        return Err(CloudServiceError::InvalidResponse(
            "account default visibility is invalid".into(),
        ));
    }
    if account.local_paths_by_clip_id.len() > MAX_CLOUD_INDEX_ROWS {
        return Err(CloudServiceError::InvalidRequest(
            "account local-path index is too large".into(),
        ));
    }
    for (id, path) in &account.local_paths_by_clip_id {
        validate_required("account.local_clip_id", id, true)?;
        validate_required("account.local_path", path, true)?;
    }
    Ok(())
}

fn validate_query(query: &CloudListQuery) -> Result<(), CloudServiceError> {
    const SORTS: &[&str] = &[
        "recorded_at_desc",
        "recorded_at_asc",
        "uploaded_at_desc",
        "uploaded_at_asc",
        "duration_desc",
        "duration_asc",
        "size_desc",
        "size_asc",
        "title_asc",
        "title_desc",
        "created_at_desc",
        "created_at_asc",
        "updated_at_desc",
        "updated_at_asc",
    ];
    if !SORTS.contains(&query.sort.as_str()) {
        return Err(CloudServiceError::InvalidRequest(
            "unsupported cloud sort".into(),
        ));
    }
    for (field, value) in [
        ("query.game", query.game.as_deref()),
        ("query.source_type", query.source_type.as_deref()),
        ("query.from", query.from.as_deref()),
        ("query.to", query.to.as_deref()),
        ("query.q", query.query.as_deref()),
    ] {
        validate_optional(field, value)?;
    }
    if query
        .visibility
        .as_deref()
        .is_some_and(|value| !matches!(value, "private" | "public" | "unlisted"))
    {
        return Err(CloudServiceError::InvalidRequest(
            "unsupported cloud visibility".into(),
        ));
    }
    if query.status.as_deref().is_some_and(|value| {
        !matches!(
            value,
            "created" | "uploading" | "processing" | "ready" | "failed"
        )
    }) {
        return Err(CloudServiceError::InvalidRequest(
            "unsupported cloud status".into(),
        ));
    }
    validate_nonnegative_range(query.min_duration_ms, query.max_duration_ms, "duration")?;
    validate_nonnegative_range(query.min_size_bytes, query.max_size_bytes, "size")
}

fn validate_nonnegative_range(
    minimum: Option<i64>,
    maximum: Option<i64>,
    label: &str,
) -> Result<(), CloudServiceError> {
    if minimum.is_some_and(|value| value < 0) || maximum.is_some_and(|value| value < 0) {
        return Err(CloudServiceError::InvalidRequest(format!(
            "cloud {label} bounds must be non-negative"
        )));
    }
    if matches!((minimum, maximum), (Some(minimum), Some(maximum)) if minimum > maximum) {
        return Err(CloudServiceError::InvalidRequest(format!(
            "cloud {label} minimum exceeds maximum"
        )));
    }
    Ok(())
}

fn map_response(
    account: &CloudServiceAccount,
    request: &CloudListTransportRequest,
    response: CloudListTransportResponse,
) -> Result<Vec<CloudLibraryItem>, CloudServiceError> {
    if response.page != request.page || response.page_size != request.page_size {
        return Err(CloudServiceError::InvalidResponse(
            "server returned a different cloud page".into(),
        ));
    }
    if response.clips.len() > request.page_size as usize {
        return Err(CloudServiceError::InvalidResponse(
            "server returned too many cloud rows".into(),
        ));
    }
    response
        .clips
        .into_iter()
        .map(|clip| map_clip(account, clip))
        .collect()
}

fn map_clip(
    account: &CloudServiceAccount,
    clip: CloudClipSummary,
) -> Result<CloudLibraryItem, CloudServiceError> {
    validate_required("clip.id", &clip.remote_clip_id, true)?;
    validate_required("clip.title", &clip.title, true)?;
    validate_required("clip.visibility", &clip.visibility, true)?;
    validate_required("clip.status", &clip.status, true)?;
    validate_optional("clip.local_clip_id", clip.local_clip_id.as_deref())?;
    validate_optional("clip.public_url", clip.public_url.as_deref())?;
    validate_optional("clip.source_type", clip.source_type.as_deref())?;
    if clip.duration_ms.is_some_and(|value| value < 0)
        || clip.file_size_bytes.is_some_and(|value| value < 0)
    {
        return Err(CloudServiceError::InvalidResponse(
            "cloud clip has negative duration or size".into(),
        ));
    }
    let path = clip
        .local_clip_id
        .as_ref()
        .and_then(|id| account.local_paths_by_clip_id.get(id))
        .cloned()
        .unwrap_or_default();
    let remote_url = if clip.visibility == "private" {
        String::new()
    } else {
        clip.public_url.unwrap_or_default()
    };
    let upload_status = match clip.status.as_str() {
        "failed" => "failed",
        "ready" if clip.visibility == "private" => "uploaded_private",
        "ready" => "uploaded_public",
        _ => "uploaded_processing",
    }
    .to_string();
    Ok(CloudLibraryItem {
        remote_clip_id: clip.remote_clip_id,
        local_clip_id: clip.local_clip_id,
        path,
        title: clip.title,
        remote_url,
        visibility: clip.visibility,
        upload_status,
        updated_at_unix: clip.updated_at_unix,
        uploaded_at_unix: clip.uploaded_at_unix,
        duration_ms: clip.duration_ms,
        file_size_bytes: clip.file_size_bytes,
        source_type: clip.source_type,
    })
}

fn validate_avatar(
    content_type: Option<String>,
    etag: Option<String>,
    bytes: Vec<u8>,
) -> Result<CloudAvatar, CloudServiceError> {
    if bytes.is_empty() {
        return Err(CloudServiceError::InvalidResponse(
            "cloud avatar returned an empty body".into(),
        ));
    }
    if bytes.len() > CLOUD_AVATAR_MAX_BYTES {
        return Err(CloudServiceError::InvalidResponse(
            "cloud avatar is too large".into(),
        ));
    }
    let content_type = content_type
        .as_deref()
        .and_then(|value| value.split(';').next())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("image/jpeg")
        .to_ascii_lowercase();
    if !content_type.starts_with("image/") || content_type.len() > MAX_CATALOG_STRING_BYTES {
        return Err(CloudServiceError::InvalidResponse(
            "cloud avatar response is not a bounded image".into(),
        ));
    }
    validate_optional("avatar.etag", etag.as_deref())?;
    Ok(CloudAvatar {
        content_type,
        etag,
        bytes,
    })
}

fn profile_url(account: &CloudServiceAccount, username: &str) -> Result<String, CloudServiceError> {
    validate_required("profile.username", username, true)?;
    append_segment(public_base(account)?, "u", username)
}

fn public_base(account: &CloudServiceAccount) -> Result<&str, CloudServiceError> {
    let value = account
        .snapshot
        .public_url
        .as_deref()
        .unwrap_or(&account.snapshot.host_url);
    validate_url_base(value)?;
    Ok(value)
}

fn append_segment(base: &str, prefix: &str, value: &str) -> Result<String, CloudServiceError> {
    let mut url = parse_url_base(base)?;
    url.path_segments_mut()
        .map_err(|_| CloudServiceError::InvalidRequest("cloud URL cannot be a base".into()))?
        .pop_if_empty()
        .push(prefix)
        .push(value);
    Ok(url.to_string())
}

fn validate_url_base(value: &str) -> Result<(), CloudServiceError> {
    parse_url_base(value).map(|_| ())
}

fn parse_url_base(value: &str) -> Result<reqwest::Url, CloudServiceError> {
    validate_required("cloud.url", value, true)?;
    let url = reqwest::Url::parse(value)
        .map_err(|_| CloudServiceError::InvalidRequest("cloud URL is invalid".into()))?;
    if !matches!(url.scheme(), "https" | "http")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(CloudServiceError::InvalidRequest(
            "cloud URL is invalid".into(),
        ));
    }
    if url.scheme() == "http" && !url_host_is_local(&url) {
        return Err(CloudServiceError::InvalidRequest(
            "plain HTTP cloud URL is not local".into(),
        ));
    }
    Ok(url)
}

fn url_host_is_local(url: &reqwest::Url) -> bool {
    let raw_host = url.host_str().unwrap_or_default();
    let host = raw_host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(raw_host);
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    host.parse::<std::net::IpAddr>()
        .is_ok_and(|address| match address {
            std::net::IpAddr::V4(address) => address.is_loopback() || address.is_private(),
            std::net::IpAddr::V6(address) => address.is_loopback(),
        })
}

fn validate_safe_component<'a>(value: &'a str, label: &str) -> Result<&'a str, CloudServiceError> {
    let trimmed = value.trim();
    if trimmed.is_empty()
        || trimmed.len() > MAX_CATALOG_STRING_BYTES
        || !trimmed
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(CloudServiceError::InvalidRequest(format!(
            "{label} contains unsupported characters"
        )));
    }
    Ok(trimmed)
}

fn normalize_optional(
    value: Option<String>,
    field: &str,
) -> Result<Option<String>, CloudServiceError> {
    let value = value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    validate_optional(field, value.as_deref())?;
    Ok(value)
}

fn validate_required(field: &str, value: &str, nonempty: bool) -> Result<(), CloudServiceError> {
    if value.len() > MAX_CATALOG_STRING_BYTES || (nonempty && value.is_empty()) {
        Err(CloudServiceError::InvalidResponse(format!(
            "{field} is empty or too large"
        )))
    } else {
        Ok(())
    }
}

fn validate_optional(field: &str, value: Option<&str>) -> Result<(), CloudServiceError> {
    if value.is_some_and(|value| value.len() > MAX_CATALOG_STRING_BYTES) {
        Err(CloudServiceError::InvalidResponse(format!(
            "{field} is too large"
        )))
    } else {
        Ok(())
    }
}

fn map_port_error(error: PortError) -> CloudServiceError {
    if error.is_account_changed() {
        CloudServiceError::AccountChanged
    } else if error.is_canceled() {
        CloudServiceError::StaleWork
    } else {
        CloudServiceError::Port(error.to_string())
    }
}

fn map_payload_error(error: PayloadBoundsError) -> CloudServiceError {
    CloudServiceError::InvalidResponse(error.to_string())
}
