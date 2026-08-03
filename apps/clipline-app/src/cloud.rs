//! Clipline Cloud desktop integration: connection state, OS credential storage,
//! and per-clip uploads through the first-party API client.

#[path = "cloud/cache_identity.rs"]
mod cache_identity;
use cache_identity::validate_cloud_cache_component;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, UNIX_EPOCH};

use base64::{engine::general_purpose, Engine as _};
use chrono::{DateTime, Utc};
use clipline_cloud_api::types::{CreateDeviceTokenRequest, CreateDeviceTokenResponse};
use clipline_cloud_api::{
    sha256_hex, ClipDetailResponse, CloudApiError, CloudClient, CreateUploadRequest,
    DiscoveryResponse, MeResponse, UpdateVisibilityRequest,
};
use clipline_desktop::{CloudAccountScope, Generation, UiEvent, UiEventSink, WindowLifecycleMode};
use clipline_events::ClipMarkers;
use clipline_library::cache::{
    AccountPublicationGuard, AvailableSpacePort, CloudAssetRequest as SharedCloudAssetRequest,
    CloudCache, CloudCacheError, CloudCancellation, CloudMediaLease,
};
use clipline_library::cache_identity::{
    CloudAccountFence, CloudAssetKey, CloudAssetKind, CloudCacheNamespace,
};
use clipline_library::http::{ReqwestAssetDownload, ReqwestCloudTransport};
use clipline_library::ports::{
    CloudAccountPort, CloudCredential, CloudCredentialPort, CloudProfilePatch, CloudRequestFence,
    PortError,
};
use clipline_library::{
    account_key as shared_account_key, CloudAccountFields,
    CloudAccountGeneration as LibraryAccountGeneration, CloudAccountSnapshot, CloudBrowserEffect,
    CloudService, CloudServiceAccount, CloudUserProfile as SharedCloudUserProfile, CloudWorkToken,
    ForegroundGeneration, RequestGeneration, WindowAttachmentGeneration, WindowWorkToken,
};
use clipline_shell::windows::credential::CredentialStore;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, Runtime, Wry};

use crate::app::{RuntimeState, WindowLifecycleState};
use crate::library::{validate_clip_path, StorageSettings};
use crate::settings::{normalize_cloud_visibility, CloudSettings, CloudUploadRecord};
use crate::util::unix_now;

const DEFAULT_DEVICE_NAME: &str = "Clipline Desktop";
const READY_POLL_ATTEMPTS: usize = 30;
const READY_POLL_DELAY: Duration = Duration::from_secs(1);
const READY_MEDIA_PROBE_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const READY_MEDIA_PROBE_TIMEOUT: Duration = Duration::from_secs(15);
const REMOTE_NOT_FOUND_SYNC_MARKER: &str = "remote clip not found during status sync";
const UPLOAD_PAYLOAD_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);
const CLOUD_CREDENTIALS: CredentialStore = CredentialStore::new("cloud token");
static CLOUD_COMPAT_REQUEST_GENERATION: AtomicU64 = AtomicU64::new(0);
static CLOUD_MEDIA_LEASE_GENERATION: AtomicU64 = AtomicU64::new(0);
static UPLOAD_PAYLOAD_TMP_COUNTER: AtomicU64 = AtomicU64::new(0);
static SHARED_CLOUD_SERVICE: OnceLock<CloudService> = OnceLock::new();
static SHARED_CLOUD_MEDIA_LEASES: OnceLock<Mutex<HashMap<u64, CloudMediaLease>>> = OnceLock::new();

#[derive(Clone)]
struct RuntimeCloudAccountPort {
    app: AppHandle<Wry>,
}

impl RuntimeCloudAccountPort {
    fn current(&self) -> Result<CloudServiceAccount, PortError> {
        let state = self.app.state::<RuntimeState>();
        state
            .with_cloud_settings_exclusive(|cloud, generation| {
                service_account_from_settings(cloud, generation).map_err(|error| error.to_string())
            })
            .map_err(PortError::new)
    }
}

impl CloudAccountPort for RuntimeCloudAccountPort {
    fn snapshot(&self) -> Result<CloudServiceAccount, PortError> {
        self.current()
    }

    fn apply_profile(
        &self,
        expected_key: &clipline_library::CloudAccountKey,
        expected_generation: LibraryAccountGeneration,
        patch: CloudProfilePatch,
    ) -> Result<CloudServiceAccount, PortError> {
        let state = self.app.state::<RuntimeState>();
        let current = self.current()?;
        if &current.snapshot.account_key != expected_key
            || current.snapshot.generation != expected_generation
            || current.snapshot.user_id.as_deref() != Some(patch.user_id.as_str())
        {
            return Err(PortError::account_changed());
        }
        let mut cloud = state.cloud_settings_generation().map_err(PortError::new)?.0;
        cloud.connected_username = Some(patch.username);
        cloud.connected_display_name = patch.display_name;
        let settings = state
            .replace_cloud_profile_if_generation(expected_generation.get(), cloud)
            .map_err(|_| PortError::account_changed())?;
        service_account_from_settings(&settings.cloud, expected_generation.get())
    }
}

struct RuntimeCloudCredentialPort;

impl CloudCredentialPort for RuntimeCloudCredentialPort {
    fn read(&self, target: &str) -> Result<CloudCredential, PortError> {
        read_credential(target)
            .map(CloudCredential::new)
            .map_err(PortError::new)
    }
}

#[derive(Clone)]
struct ExactCloudFence {
    app: AppHandle<Wry>,
    token: CloudWorkToken,
}

impl CloudRequestFence for ExactCloudFence {
    fn is_current(&self, token: &CloudWorkToken) -> bool {
        if &self.token != token {
            return false;
        }
        let lifecycle = self.app.state::<WindowLifecycleState>().snapshot();
        lifecycle.mode == WindowLifecycleMode::Foreground
            && lifecycle.revision.get() == token.window.foreground.get()
    }
}

fn shared_cloud_service(app: &AppHandle<Wry>) -> Result<&'static CloudService, String> {
    if let Some(service) = SHARED_CLOUD_SERVICE.get() {
        return Ok(service);
    }
    let transport = ReqwestCloudTransport::new().map_err(|error| error.to_string())?;
    let service = CloudService::new(
        Arc::new(RuntimeCloudAccountPort { app: app.clone() }),
        Arc::new(RuntimeCloudCredentialPort),
        Arc::new(transport),
    );
    let _ = SHARED_CLOUD_SERVICE.set(service);
    SHARED_CLOUD_SERVICE
        .get()
        .ok_or_else(|| "initialize shared cloud service".to_string())
}

fn service_account_from_settings(
    cloud: &CloudSettings,
    generation: u64,
) -> Result<CloudServiceAccount, PortError> {
    let account_key = shared_account_key(&CloudAccountFields {
        host_url: cloud.host_url.clone(),
        connected_user_id: cloud.connected_user_id.clone().unwrap_or_default(),
        credential_target: cloud.credential_target.clone().unwrap_or_default(),
    })
    .map_err(|error| PortError::new(error.to_string()))?;
    Ok(CloudServiceAccount {
        snapshot: CloudAccountSnapshot {
            account_key,
            generation: LibraryAccountGeneration::new(generation),
            connected: cloud.connected(),
            host_url: cloud.host_url.clone(),
            public_url: cloud.public_url.clone(),
            username: cloud.connected_username.clone(),
            display_name: cloud.connected_display_name.clone(),
            user_id: cloud.connected_user_id.clone(),
            default_visibility: cloud.default_visibility.clone(),
            delete_local_after_upload: cloud.delete_local_after_upload,
            auto_upload_rules: cloud.auto_upload_rules,
        },
        credential_target: cloud.credential_target.clone(),
        local_paths_by_clip_id: cloud
            .uploads
            .values()
            .map(|record| (record.local_clip_id.clone(), record.path.clone()))
            .collect(),
    })
}

fn current_cloud_work(app: &AppHandle<Wry>) -> Result<(CloudWorkToken, ExactCloudFence), String> {
    let lifecycle = app.state::<WindowLifecycleState>().snapshot();
    if lifecycle.mode != WindowLifecycleMode::Foreground {
        return Err("cloud foreground work requires the main window".into());
    }
    let account = RuntimeCloudAccountPort { app: app.clone() }
        .snapshot()
        .map_err(|error| error.to_string())?;
    let request = CLOUD_COMPAT_REQUEST_GENERATION
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            current.checked_add(1)
        })
        .map_err(|_| "cloud compatibility request generation exhausted".to_string())?
        + 1;
    let token = CloudWorkToken {
        window: WindowWorkToken {
            attachment: WindowAttachmentGeneration::new(1),
            foreground: ForegroundGeneration::new(lifecycle.revision.get()),
            request: RequestGeneration::new(request),
        },
        account_key: account.snapshot.account_key,
        account_generation: account.snapshot.generation,
    };
    Ok((
        token.clone(),
        ExactCloudFence {
            app: app.clone(),
            token,
        },
    ))
}

#[derive(Debug, Deserialize)]
pub struct CloudConnectRequest {
    pub host_url: String,
    pub username: String,
    pub password: String,
    #[serde(default)]
    pub device_name: Option<String>,
    #[serde(default)]
    pub plain_http_confirmed: bool,
    #[serde(default)]
    pub default_visibility: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UploadClipCommandRequest {
    pub path: String,
    #[serde(default)]
    pub visibility: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default, rename = "audioTrackIds")]
    pub audio_track_ids: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
pub struct SyncCloudClipStatusRequest {
    pub path: String,
}

#[derive(Debug, Serialize)]
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

#[derive(Debug, Serialize, Clone)]
pub struct CloudUserProfile {
    pub user_id: String,
    pub username: String,
    pub display_name: Option<String>,
    pub profile_url: String,
}

impl From<SharedCloudUserProfile> for CloudUserProfile {
    fn from(profile: SharedCloudUserProfile) -> Self {
        Self {
            user_id: profile.user_id,
            username: profile.username,
            display_name: profile.display_name,
            profile_url: profile.profile_url,
        }
    }
}

pub type CloudUploadProgressEvent = clipline_desktop::CloudUploadProgress;

#[derive(Debug, Serialize)]
pub struct CloudUploadResult {
    pub record: CloudUploadRecord,
    pub clip: Option<ClipDetailResponse>,
    pub local_deleted: bool,
}

#[derive(Debug)]
enum ReadyClipOutcome {
    Ready(ClipDetailResponse),
    Failed(ClipDetailResponse),
    TimedOut,
}

#[derive(Debug, Serialize)]
pub struct CloudClipStatusSyncResult {
    pub path: String,
    pub record: Option<CloudUploadRecord>,
    pub removed: bool,
}

#[derive(Debug, Serialize, Clone, PartialEq)]
pub struct CloudLibraryClip {
    pub remote_clip_id: String,
    pub local_clip_id: Option<String>,
    pub path: String,
    pub title: String,
    pub remote_url: String,
    pub visibility: String,
    pub upload_status: String,
    pub updated_at_unix: u64,
    pub uploaded_at_unix: Option<u64>,
    pub duration_ms: Option<i64>,
    pub file_size_bytes: Option<i64>,
    pub source_type: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct CloudLibraryListResult {
    pub clips: Vec<CloudLibraryClip>,
    pub truncated: bool,
}

#[derive(Debug, Deserialize)]
pub struct CloudClipAssetRequest {
    pub remote_clip_id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub duration_ms: Option<i64>,
    #[serde(default)]
    pub file_size_bytes: Option<i64>,
    #[serde(default)]
    pub updated_at_unix: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct CachedCloudClip {
    pub path: String,
    pub name: String,
    pub size_mb: f64,
    pub modified_unix: u64,
    pub duration_s: Option<f64>,
    pub cloud_media_lease_id: u64,
}

#[tauri::command]
pub fn cloud_status(state: tauri::State<RuntimeState>) -> CloudConnectionStatus {
    if let Err(error) = reconcile_cloud_credential_cleanup(&state) {
        tracing::warn!(event = "cloud_pending_credential_reconcile_failed", error = %error);
    }
    let settings = state.settings();
    connection_status(&settings.cloud)
}

#[tauri::command]
pub async fn list_cloud_clips(app: AppHandle<Wry>) -> Result<CloudLibraryListResult, String> {
    let service = shared_cloud_service(&app)?;
    let (token, fence) = current_cloud_work(&app)?;
    let result = service
        .legacy_list(token, &fence)
        .await
        .map_err(|error| error.to_string())?;
    Ok(CloudLibraryListResult {
        clips: result
            .value
            .clips
            .into_iter()
            .map(|clip| CloudLibraryClip {
                remote_clip_id: clip.remote_clip_id,
                local_clip_id: clip.local_clip_id,
                path: clip.path,
                title: clip.title,
                remote_url: clip.remote_url,
                visibility: clip.visibility,
                upload_status: clip.upload_status,
                updated_at_unix: clip.updated_at_unix,
                uploaded_at_unix: clip.uploaded_at_unix,
                duration_ms: clip.duration_ms,
                file_size_bytes: clip.file_size_bytes,
                source_type: clip.source_type,
            })
            .collect(),
        truncated: result.value.truncated,
    })
}

#[tauri::command]
pub async fn cloud_clip_thumbnail(
    app: AppHandle<Wry>,
    request: CloudClipAssetRequest,
) -> Result<Option<String>, String> {
    let lifecycle = current_foreground_lifecycle(&app)?;
    let (cache, account) = shared_cloud_cache(&app, lifecycle)?;
    let asset_request = shared_cloud_asset_request(&account, &request, CloudAssetKind::Thumbnail)?;
    let worker_cache = Arc::clone(&cache);
    let cached = tokio::task::spawn_blocking(move || {
        worker_cache.get(asset_request, &CloudCancellation::default())
    })
    .await
    .map_err(|error| format!("cloud thumbnail worker failed: {error}"))?
    .map_err(|error| error.to_string())?;
    let Some(cached) = cached else {
        return Ok(None);
    };
    allow_cloud_cache_asset(&app, cached.path())?;
    Ok(Some(cloud_cache_display_path(cached.path())))
}

#[tauri::command]
pub async fn cache_cloud_clip_media(
    app: AppHandle<Wry>,
    request: CloudClipAssetRequest,
) -> Result<CachedCloudClip, String> {
    let lifecycle = current_foreground_lifecycle(&app)?;
    let (cache, account) = shared_cloud_cache(&app, lifecycle)?;
    let asset_request = shared_cloud_asset_request(&account, &request, CloudAssetKind::Media)?;
    let worker_cache = Arc::clone(&cache);
    let cached = tokio::task::spawn_blocking(move || {
        worker_cache.get(asset_request, &CloudCancellation::default())
    })
    .await
    .map_err(|error| format!("cloud media worker failed: {error}"))?
    .map_err(|error| error.to_string())?
    .ok_or_else(|| "cloud clip media is not available".to_string())?;
    allow_cloud_cache_asset(&app, cached.path())?;
    let lease = cache
        .accept_media(&account, cached)
        .map_err(|error| error.to_string())?;
    let path = lease.path().to_path_buf();
    let mut clip = cached_cloud_clip_from_path(&path, &request)?;
    let cloud_media_lease_id = register_cloud_media_lease(lease)?;
    clip.cloud_media_lease_id = cloud_media_lease_id;
    Ok(clip)
}

#[tauri::command]
pub fn release_cloud_media_lease(cloud_media_lease_id: u64) -> Result<(), String> {
    shared_cloud_media_leases()
        .lock()
        .map_err(|_| "cloud media lease lock poisoned".to_string())?
        .remove(&cloud_media_lease_id);
    Ok(())
}

pub(crate) fn release_all_cloud_media_leases() {
    if let Some(leases) = SHARED_CLOUD_MEDIA_LEASES.get() {
        leases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }
}

#[tauri::command]
pub async fn cloud_user_avatar(app: AppHandle<Wry>) -> Result<Option<String>, String> {
    let service = shared_cloud_service(&app)?;
    let (token, fence) = current_cloud_work(&app)?;
    let result = service
        .avatar(token, &fence)
        .await
        .map_err(|error| error.to_string())?;
    Ok(result
        .value
        .map(|avatar| cloud_user_avatar_data_url(&avatar.content_type, &avatar.bytes)))
}

#[tauri::command]
pub async fn cloud_user_profile(app: AppHandle<Wry>) -> Result<CloudUserProfile, String> {
    let service = shared_cloud_service(&app)?;
    let (token, fence) = current_cloud_work(&app)?;
    let result = service
        .profile(token, &fence)
        .await
        .map_err(|error| error.to_string())?;
    Ok(result.value.into())
}

#[tauri::command]
pub fn open_cloud_user_profile(app: AppHandle<Wry>) -> Result<(), String> {
    let service = shared_cloud_service(&app)?;
    let (token, fence) = current_cloud_work(&app)?;
    let effect = service
        .open_profile_effect(token, &fence)
        .map_err(|error| error.to_string())?;
    open_cloud_browser_effect(effect.value)
}

#[tauri::command]
pub fn open_cloud_clip(app: AppHandle<Wry>, remote_clip_id: String) -> Result<(), String> {
    let service = shared_cloud_service(&app)?;
    let (token, fence) = current_cloud_work(&app)?;
    let effect = service
        .open_clip_effect(token, &fence, &remote_clip_id)
        .map_err(|error| error.to_string())?;
    open_cloud_browser_effect(effect.value)
}

struct RuntimeCloudAvailableSpace;

impl AvailableSpacePort for RuntimeCloudAvailableSpace {
    fn available_bytes(&self, cache_root: &Path) -> Result<u64, CloudCacheError> {
        crate::windows::available_space_bytes(cache_root, "read shared cloud cache free space")
            .map_err(CloudCacheError::Io)
    }
}

#[derive(Clone)]
struct RuntimeCloudPublicationGuard {
    app: AppHandle<Wry>,
    lifecycle: clipline_desktop::WindowLifecycleSnapshot,
}

impl AccountPublicationGuard for RuntimeCloudPublicationGuard {
    fn is_current(&self, account: &CloudAccountFence) -> bool {
        if self.app.state::<WindowLifecycleState>().snapshot() != self.lifecycle {
            return false;
        }
        self.app
            .state::<RuntimeState>()
            .with_cloud_settings_exclusive(|cloud, generation| {
                Ok(cloud_cache_account_fence(cloud, generation)
                    .is_ok_and(|current| current == *account))
            })
            .unwrap_or(false)
    }

    fn publish_if_current(
        &self,
        account: &CloudAccountFence,
        publication: &mut dyn FnMut() -> Result<(), CloudCacheError>,
    ) -> Result<(), CloudCacheError> {
        if self.app.state::<WindowLifecycleState>().snapshot() != self.lifecycle {
            return Err(CloudCacheError::StaleAccount);
        }
        let mut publication_result = None;
        self.app
            .state::<RuntimeState>()
            .with_cloud_settings_exclusive(|cloud, generation| {
                let current = cloud_cache_account_fence(cloud, generation)
                    .map_err(|error| error.to_string())?;
                publication_result = Some(
                    if &current == account
                        && self.app.state::<WindowLifecycleState>().snapshot() == self.lifecycle
                    {
                        publication()
                    } else {
                        Err(CloudCacheError::StaleAccount)
                    },
                );
                Ok(())
            })
            .map_err(CloudCacheError::Io)?;
        publication_result.unwrap_or_else(|| {
            Err(CloudCacheError::Internal(
                "cloud account publication closure was not invoked".into(),
            ))
        })
    }
}

fn shared_cloud_cache(
    app: &AppHandle<Wry>,
    lifecycle: clipline_desktop::WindowLifecycleSnapshot,
) -> Result<(Arc<CloudCache>, CloudAccountFence), String> {
    if lifecycle.mode != WindowLifecycleMode::Foreground
        || app.state::<WindowLifecycleState>().snapshot() != lifecycle
    {
        return Err("cloud cache work requires the current foreground window".into());
    }
    let state = app.state::<RuntimeState>();
    let (cloud, generation) = state.cloud_settings_generation()?;
    let account = cloud_cache_account_fence(&cloud, generation)?;
    let credential_target = cloud
        .credential_target
        .as_deref()
        .ok_or_else(|| "Clipline Cloud is not connected".to_string())?;
    let credential = read_credential(credential_target)?;
    let root = prepare_cloud_cache_root()?;
    let download = ReqwestAssetDownload::new(&cloud.host_url, credential)
        .map_err(|error| error.to_string())?;
    let cache = CloudCache::open(
        root,
        Arc::new(download),
        Arc::new(RuntimeCloudAvailableSpace),
        Arc::new(RuntimeCloudPublicationGuard {
            app: app.clone(),
            lifecycle,
        }),
    )
    .map_err(|error| error.to_string())?;
    Ok((Arc::new(cache), account))
}

fn current_foreground_lifecycle(
    app: &AppHandle<Wry>,
) -> Result<clipline_desktop::WindowLifecycleSnapshot, String> {
    let lifecycle = app.state::<WindowLifecycleState>().snapshot();
    if lifecycle.mode != WindowLifecycleMode::Foreground {
        return Err("cloud foreground work requires the main window".into());
    }
    Ok(lifecycle)
}

fn cloud_cache_account_fence(
    cloud: &CloudSettings,
    generation: u64,
) -> Result<CloudAccountFence, String> {
    if !cloud.connected() {
        return Err("Clipline Cloud is not connected".into());
    }
    let service =
        service_account_from_settings(cloud, generation).map_err(|error| error.to_string())?;
    let stable_account = cloud
        .connected_user_id
        .as_deref()
        .or(cloud.connected_username.as_deref())
        .or(cloud.credential_target.as_deref())
        .ok_or_else(|| "Clipline Cloud account identity is unavailable".to_string())?;
    Ok(CloudAccountFence {
        account_key: service.snapshot.account_key,
        account_generation: service.snapshot.generation,
        cache_namespace: CloudCacheNamespace::derive(&cloud.host_url, stable_account)
            .map_err(|error| error.to_string())?,
    })
}

fn shared_cloud_asset_request(
    account: &CloudAccountFence,
    request: &CloudClipAssetRequest,
    kind: CloudAssetKind,
) -> Result<SharedCloudAssetRequest, String> {
    let expected_size_bytes = request
        .file_size_bytes
        .filter(|bytes| *bytes > 0)
        .map(u64::try_from)
        .transpose()
        .map_err(|_| "cloud asset size is invalid".to_string())?;
    Ok(SharedCloudAssetRequest {
        account: account.clone(),
        asset: CloudAssetKey::new(
            &request.remote_clip_id,
            kind,
            request.updated_at_unix.unwrap_or(0),
        )
        .map_err(|error| error.to_string())?,
        expected_size_bytes: (kind == CloudAssetKind::Media)
            .then_some(expected_size_bytes)
            .flatten(),
    })
}

fn shared_cloud_media_leases() -> &'static Mutex<HashMap<u64, CloudMediaLease>> {
    SHARED_CLOUD_MEDIA_LEASES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn register_cloud_media_lease(lease: CloudMediaLease) -> Result<u64, String> {
    let lease_id = CLOUD_MEDIA_LEASE_GENERATION
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            current.checked_add(1)
        })
        .map_err(|_| "cloud media lease generation exhausted".to_string())?
        + 1;
    shared_cloud_media_leases()
        .lock()
        .map_err(|_| "cloud media lease lock poisoned".to_string())?
        .insert(lease_id, lease);
    Ok(lease_id)
}

fn open_cloud_browser_effect(effect: CloudBrowserEffect) -> Result<(), String> {
    open_cloud_url(&effect.url, &effect.context)
}

fn open_cloud_url(url: &str, context: &str) -> Result<(), String> {
    clipline_shell::windows::shell_execute::open_browser_url(url, context)
        .map_err(|error| error.to_string())
}

fn cloud_clip_asset_url(
    cloud: &CloudSettings,
    remote_clip_id: &str,
    asset: &str,
) -> Result<reqwest::Url, String> {
    let remote_clip_id = validate_cloud_cache_component(remote_clip_id, "remote clip id")?;
    let asset = validate_cloud_cache_component(asset, "cloud asset")?;
    let base =
        clipline_cloud_api::validate_cloud_host(&cloud.host_url, true).map_err(cloud_error)?;
    base.join(&format!("api/v1/clips/{remote_clip_id}/{asset}"))
        .map_err(|e| format!("cloud asset URL is invalid: {e}"))
}

fn cloud_user_avatar_data_url(content_type: &str, bytes: &[u8]) -> String {
    format!(
        "data:{content_type};base64,{}",
        general_purpose::STANDARD.encode(bytes)
    )
}

fn cloud_clip_cache_root_dir() -> PathBuf {
    crate::settings::persistence::local_cache_base().join("cloud-cache")
}

fn legacy_cloud_clip_cache_root_dir() -> PathBuf {
    crate::settings::persistence::config_base().join("cloud-cache")
}

fn prepare_cloud_cache_root() -> Result<PathBuf, String> {
    let root = cloud_clip_cache_root_dir();
    migrate_legacy_cloud_cache(&legacy_cloud_clip_cache_root_dir(), &root)?;
    std::fs::create_dir_all(&root).map_err(|error| format!("create cloud cache: {error}"))?;
    Ok(root)
}

fn migrate_legacy_cloud_cache(legacy: &Path, local: &Path) -> Result<(), String> {
    if legacy == local || !legacy.exists() {
        return Ok(());
    }
    let metadata = std::fs::symlink_metadata(legacy)
        .map_err(|error| format!("inspect legacy cloud cache: {error}"))?;
    if !metadata.is_dir() || metadata_is_link(&metadata) {
        return Ok(());
    }
    std::fs::create_dir_all(local).map_err(|error| format!("create local cloud cache: {error}"))?;
    let entries =
        std::fs::read_dir(legacy).map_err(|error| format!("read legacy cloud cache: {error}"))?;
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let namespace = name.to_string_lossy();
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !metadata.is_dir()
            || metadata_is_link(&metadata)
            || namespace.len() != 16
            || !namespace.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            continue;
        }
        let destination = local.join(&name);
        if destination.exists() {
            continue;
        }
        std::fs::rename(&path, &destination)
            .map_err(|error| format!("migrate cloud cache namespace {namespace}: {error}"))?;
    }
    Ok(())
}

fn metadata_is_link(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_type().is_symlink()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

fn allow_cloud_cache_asset<R: Runtime>(app: &AppHandle<R>, path: &Path) -> Result<(), String> {
    let cache_dir = prepare_cloud_cache_root()?;
    let canonical_dir = cache_dir
        .canonicalize()
        .map_err(|e| format!("canonicalize cloud cache {cache_dir:?}: {e}"))?;
    let canonical_path = path
        .canonicalize()
        .map_err(|e| format!("canonicalize cloud cache asset {path:?}: {e}"))?;
    if !canonical_path.starts_with(&canonical_dir) {
        return Err(format!(
            "cloud cache asset {canonical_path:?} escaped cache {canonical_dir:?}"
        ));
    }
    app.asset_protocol_scope()
        .allow_file(&canonical_path)
        .map_err(|e| format!("scope cloud cache asset for playback: {e}"))
}

fn cached_cloud_clip_from_path(
    path: &Path,
    request: &CloudClipAssetRequest,
) -> Result<CachedCloudClip, String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("read cached cloud clip: {e}"))?;
    let modified_unix = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or_else(unix_now);
    let title = request
        .title
        .as_deref()
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .unwrap_or("Cloud clip");
    let name = if title.to_ascii_lowercase().ends_with(".mp4") {
        title.to_string()
    } else {
        format!("{title}.mp4")
    };
    Ok(CachedCloudClip {
        path: cloud_cache_display_path(path),
        name,
        size_mb: meta.len() as f64 / (1024.0 * 1024.0),
        modified_unix,
        duration_s: request
            .duration_ms
            .filter(|duration| *duration >= 0)
            .map(|duration| duration as f64 / 1000.0),
        cloud_media_lease_id: 0,
    })
}

fn cloud_cache_display_path(path: &Path) -> String {
    let display = path.to_string_lossy();
    let lowercase = display.to_ascii_lowercase();
    if lowercase.starts_with(r"\\?\unc\") {
        format!(r"\\{}", &display[8..])
    } else if lowercase.starts_with(r"\\?\") {
        display[4..].to_string()
    } else {
        display.into_owned()
    }
}

#[tauri::command]
pub async fn sync_cloud_clip_status(
    state: tauri::State<'_, RuntimeState>,
    request: SyncCloudClipStatusRequest,
) -> Result<CloudClipStatusSyncResult, String> {
    let settings = state.settings();
    let cloud = settings.cloud.clone();
    let Some(record) = cloud_record_for_path(&cloud, &request.path) else {
        return Ok(CloudClipStatusSyncResult {
            path: request.path,
            record: None,
            removed: false,
        });
    };
    let Some(remote_clip_id) = record.remote_clip_id.clone() else {
        return Ok(CloudClipStatusSyncResult {
            path: request.path,
            record: Some(record),
            removed: false,
        });
    };
    let token_target = cloud
        .credential_target
        .clone()
        .ok_or_else(|| "connect to Clipline Cloud first".to_string())?;
    let token = read_credential(&token_target)?;
    let client = connected_client(&cloud, &token)?;

    match bounded_cloud_get_clip(&client, &token, &remote_clip_id).await {
        Ok(clip) => {
            let mut updated = record;
            apply_remote_clip_to_record(&mut updated, &clip);
            persist_record(&state, &updated)?;
            Ok(CloudClipStatusSyncResult {
                path: request.path,
                record: Some(updated),
                removed: false,
            })
        }
        Err(error) if cloud_error_is_not_found(&error) => match missing_remote_sync_action(&record)
        {
            MissingRemoteSyncAction::Keep => Ok(CloudClipStatusSyncResult {
                path: request.path,
                record: Some(record),
                removed: false,
            }),
            MissingRemoteSyncAction::ConfirmMissing => {
                let mut updated = record;
                mark_remote_not_found_once(&mut updated);
                persist_record(&state, &updated)?;
                Ok(CloudClipStatusSyncResult {
                    path: request.path,
                    record: Some(updated),
                    removed: false,
                })
            }
            MissingRemoteSyncAction::Remove => {
                state.update_cloud(|cloud| {
                    remove_upload_record(cloud, &record);
                })?;
                Ok(CloudClipStatusSyncResult {
                    path: request.path,
                    record: None,
                    removed: true,
                })
            }
        },
        Err(error) => Err(cloud_error(error)),
    }
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

fn reconcile_cloud_credential_cleanup(state: &RuntimeState) -> Result<(), String> {
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

#[tauri::command]
pub async fn upload_clip_to_cloud<R: Runtime>(
    app: AppHandle<R>,
    state: tauri::State<'_, RuntimeState>,
    storage: tauri::State<'_, StorageSettings>,
    request: UploadClipCommandRequest,
) -> Result<CloudUploadResult, String> {
    let target = validate_clip_path(&storage, &request.path)?;
    let settings = state.settings();
    let cloud = settings.cloud.clone();

    let meta = std::fs::metadata(&target).map_err(|e| format!("read clip metadata: {e}"))?;
    if meta.len() == 0 {
        return Err("clip file is empty".into());
    }
    let markers = crate::util::read_markers_raw(&target);
    let payload = upload_payload_for_audio_selection_from_path(
        &target,
        markers.as_ref(),
        request.audio_track_ids.as_deref(),
    )
    .await?;
    let payload_meta = tokio::fs::metadata(payload.path())
        .await
        .map_err(|e| format!("read upload payload metadata: {e}"))?;
    let payload_size = payload_meta.len();
    let checksum = crate::cloud_upload::sha256_file(payload.path())
        .await
        .map_err(cloud_error)?;
    let local_clip_id = local_clip_id(&target, &meta, &checksum)?;
    if let Some(record) = existing_uploaded_record(&cloud, Some(&local_clip_id), &request.path) {
        return Ok(CloudUploadResult {
            record,
            clip: None,
            local_deleted: false,
        });
    }
    let upload_generation = app
        .state::<crate::desktop::ProducerGenerations>()
        .next_cloud_upload()?;
    let ui_sink = app
        .state::<crate::desktop::tauri_sink::TauriUiEventSink>()
        .inner()
        .clone();
    let token_target = cloud
        .credential_target
        .clone()
        .ok_or_else(|| "connect to Clipline Cloud first".to_string())?;
    let token = read_credential(&token_target)?;
    let client = connected_client(&cloud, &token)?;
    let visibility = request
        .visibility
        .as_deref()
        .map(normalize_cloud_visibility)
        .unwrap_or_else(|| cloud.default_visibility.clone());
    let description = normalize_upload_description(request.description.as_deref());
    let mut record = CloudUploadRecord {
        local_clip_id: local_clip_id.clone(),
        // Store the path exactly as `list_clips` emits it (non-canonical), so the
        // UI can pair this record to its clip row by string equality. `target` is
        // the canonicalized form (`\\?\D:\…` on Windows) and is used only for I/O.
        path: request.path.clone(),
        remote_clip_id: None,
        remote_url: None,
        visibility: visibility.clone(),
        upload_status: existing_retry_status(&cloud, &local_clip_id, &request.path),
        error: None,
        updated_at_unix: unix_now(),
    };
    persist_record(&state, &record)?;
    emit_upload_progress(&ui_sink, upload_generation, &record, 0, payload_size, None);

    let upload_request = create_upload_request(UploadRequestInput {
        path: &target,
        meta: &meta,
        file_size_bytes: payload_size,
        duration_ms: clip_duration_ms_file(payload.path(), markers.as_ref()),
        checksum: &checksum,
        visibility: &visibility,
        markers: markers.as_ref(),
        client_clip_id: &local_clip_id,
        title: request.title.as_deref(),
    })?;
    let progress_path = request.path.clone();
    let upload_result = crate::cloud_upload::upload_mp4_file_with_progress(
        &client,
        &token,
        &upload_request,
        description.as_deref(),
        payload.path(),
        |progress| {
            let status = if progress.status == "completed" {
                "processing"
            } else {
                "uploading"
            };
            let event = CloudUploadProgressEvent {
                local_clip_id: local_clip_id.clone(),
                path: progress_path.clone(),
                upload_status: status.to_string(),
                received_size_bytes: progress.received_size_bytes,
                file_size_bytes: progress.file_size_bytes,
                remote_clip_id: Some(progress.clip_id.clone()),
                remote_url: None,
                error: None,
            };
            let _ = ui_sink.try_publish(UiEvent::CloudUploadProgress {
                account: CloudAccountScope::INITIAL,
                generation: upload_generation,
                progress: event,
            });
        },
    )
    .await;

    let progress = match upload_result {
        Ok(progress) => progress,
        Err(error) => {
            record.upload_status = "failed".to_string();
            record.error = Some(cloud_error(error));
            record.updated_at_unix = unix_now();
            persist_record(&state, &record)?;
            emit_upload_progress(
                &ui_sink,
                upload_generation,
                &record,
                0,
                payload_size,
                record.error.clone(),
            );
            return Ok(CloudUploadResult {
                record,
                clip: None,
                local_deleted: false,
            });
        }
    };

    record.remote_clip_id = Some(progress.clip_id.clone());
    record.remote_url = None;
    record.upload_status = "processing".to_string();
    record.error = None;
    record.updated_at_unix = unix_now();
    persist_record(&state, &record)?;
    emit_upload_progress(
        &ui_sink,
        upload_generation,
        &record,
        progress.received_size_bytes,
        progress.file_size_bytes,
        None,
    );

    let clip = match wait_for_ready_clip(&client, &token, &progress.clip_id).await {
        Ok(ReadyClipOutcome::Ready(clip)) => clip,
        Ok(ReadyClipOutcome::Failed(clip)) => {
            apply_remote_clip_to_record(&mut record, &clip);
            record.upload_status = "failed".to_string();
            record.error = Some(
                "cloud upload completed, but cloud media processing failed; the local clip was preserved"
                    .to_string(),
            );
            record.updated_at_unix = unix_now();
            persist_post_upload_record(
                &ui_sink,
                upload_generation,
                &state,
                &record,
                progress.file_size_bytes,
            )?;
            return Ok(CloudUploadResult {
                record,
                clip: None,
                local_deleted: false,
            });
        }
        Ok(ReadyClipOutcome::TimedOut) => {
            mark_ready_timeout(&mut record);
            persist_post_upload_record(
                &ui_sink,
                upload_generation,
                &state,
                &record,
                progress.file_size_bytes,
            )?;
            return Ok(CloudUploadResult {
                record,
                clip: None,
                local_deleted: false,
            });
        }
        Err(error) => {
            mark_post_upload_problem(
                &mut record,
                format!(
                    "cloud upload completed, but checking cloud processing failed: {}; the local clip was preserved",
                    cloud_error(error)
                ),
            );
            persist_post_upload_record(
                &ui_sink,
                upload_generation,
                &state,
                &record,
                progress.file_size_bytes,
            )?;
            return Ok(CloudUploadResult {
                record,
                clip: None,
                local_deleted: false,
            });
        }
    };

    let clip = if visibility == "private" {
        clip
    } else {
        match update_cloud_clip_visibility(&client, &token, &clip.id, &visibility).await {
            Ok(updated) if updated.status == "ready" => updated,
            Ok(updated) => {
                apply_remote_clip_to_record(&mut record, &updated);
                mark_post_upload_problem(
                    &mut record,
                    format!(
                        "cloud upload completed, but visibility update returned status {:?}; the local clip was preserved",
                        updated.status
                    ),
                );
                persist_post_upload_record(
                    &ui_sink,
                    upload_generation,
                    &state,
                    &record,
                    progress.file_size_bytes,
                )?;
                return Ok(CloudUploadResult {
                    record,
                    clip: None,
                    local_deleted: false,
                });
            }
            Err(error) => {
                mark_post_upload_problem(
                    &mut record,
                    format!(
                        "cloud upload completed, but updating visibility failed: {}; the local clip was preserved",
                        cloud_error(error)
                    ),
                );
                persist_post_upload_record(
                    &ui_sink,
                    upload_generation,
                    &state,
                    &record,
                    progress.file_size_bytes,
                )?;
                return Ok(CloudUploadResult {
                    record,
                    clip: None,
                    local_deleted: false,
                });
            }
        }
    };

    apply_remote_clip_to_record(&mut record, &clip);
    persist_post_upload_record(
        &ui_sink,
        upload_generation,
        &state,
        &record,
        progress.file_size_bytes,
    )?;

    if cloud.delete_local_after_upload {
        if let Err(error) = verify_ready_cloud_media(&cloud, &token, &clip.id).await {
            mark_post_upload_problem(
                &mut record,
                format!(
                    "cloud reported the upload ready, but its media could not be verified: {error}; the local clip was preserved"
                ),
            );
            persist_post_upload_record(
                &ui_sink,
                upload_generation,
                &state,
                &record,
                progress.file_size_bytes,
            )?;
            return Ok(CloudUploadResult {
                record,
                clip: Some(clip),
                local_deleted: false,
            });
        }
        let cleanup_result = delete_uploaded_local_files(&target);
        let local_deleted = cleanup_result.is_ok() || matches!(target.try_exists(), Ok(false));
        if let Err(error) = cleanup_result {
            record.error = Some(format!(
                "cloud upload is ready, but local cleanup failed: {error}"
            ));
            record.updated_at_unix = unix_now();
            persist_post_upload_record(
                &ui_sink,
                upload_generation,
                &state,
                &record,
                progress.file_size_bytes,
            )?;
        }
        return Ok(CloudUploadResult {
            record,
            clip: Some(clip),
            local_deleted,
        });
    }

    Ok(CloudUploadResult {
        record,
        clip: Some(clip),
        local_deleted: false,
    })
}

fn connection_status(cloud: &CloudSettings) -> CloudConnectionStatus {
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

fn connected_client(cloud: &CloudSettings, token: &str) -> Result<CloudClient, String> {
    if !cloud.connected() {
        return Err("connect to Clipline Cloud first".into());
    }
    let base_url =
        clipline_cloud_api::validate_cloud_host(&cloud.host_url, true).map_err(cloud_error)?;
    Ok(CloudClient::with_device_token(base_url, token))
}

fn cloud_request(
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

fn cloud_clip_request(
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

async fn bounded_cloud_json<T: DeserializeOwned>(
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

async fn bounded_cloud_get_clip(
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

async fn update_cloud_clip_visibility(
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

struct UploadRequestInput<'a> {
    path: &'a Path,
    meta: &'a std::fs::Metadata,
    file_size_bytes: u64,
    duration_ms: Option<i64>,
    checksum: &'a str,
    visibility: &'a str,
    markers: Option<&'a ClipMarkers>,
    client_clip_id: &'a str,
    title: Option<&'a str>,
}

fn create_upload_request(input: UploadRequestInput<'_>) -> Result<CreateUploadRequest, String> {
    let game = read_clip_game(input.path, input.markers);
    Ok(CreateUploadRequest {
        client_clip_id: Some(input.client_clip_id.to_string()),
        title: upload_title(input.title, input.path),
        description: None,
        game_name: game.as_ref().map(|game| game.name.clone()),
        game_id: game.as_ref().map(|game| game.id.clone()),
        game_executable: None,
        source_type: Some(source_type(input.path)),
        recorded_at: input.meta.modified().ok().map(DateTime::<Utc>::from),
        duration_ms: input.duration_ms,
        file_size_bytes: input.file_size_bytes,
        checksum_sha256: input.checksum.to_string(),
        container: "mp4".to_string(),
        video_codec: None,
        audio_codec: None,
        width: None,
        height: None,
        fps: None,
        visibility: Some(input.visibility.to_string()),
        markers: None,
    })
}

fn upload_title(title: Option<&str>, path: &Path) -> String {
    title
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .unwrap_or_else(|| crate::library::clip_title_for_path(path))
}

fn normalize_upload_description(description: Option<&str>) -> Option<String> {
    description
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum UploadAudioSelectionPlan {
    Original,
    Remux(Vec<u32>),
    Mix(Vec<u32>),
}

struct UploadPayload {
    path: PathBuf,
    owned: bool,
}

impl UploadPayload {
    fn original(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
            owned: false,
        }
    }

    fn owned(path: PathBuf) -> Self {
        Self { path, owned: true }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for UploadPayload {
    fn drop(&mut self) {
        if self.owned {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

async fn upload_payload_for_audio_selection_from_path(
    source_path: &Path,
    markers: Option<&ClipMarkers>,
    selected_audio_track_ids: Option<&[String]>,
) -> Result<UploadPayload, String> {
    let markers_with_audio = selected_audio_track_ids.and_then(|_| {
        crate::util::markers_with_inferred_audio_tracks(source_path, markers.cloned())
    });
    let selection_markers = markers_with_audio.as_ref().or(markers);
    match upload_audio_selection_plan(selection_markers, selected_audio_track_ids)? {
        UploadAudioSelectionPlan::Original => Ok(UploadPayload::original(source_path)),
        UploadAudioSelectionPlan::Remux(selected_indices) => {
            let target = reserve_upload_payload_path(source_path)?;
            let payload = UploadPayload::owned(target.clone());
            let source = source_path.to_path_buf();
            tokio::task::spawn_blocking(move || {
                clipline_mp4::remux_with_selected_audio_tracks_file(
                    &source,
                    &target,
                    &selected_indices,
                )
            })
            .await
            .map_err(|e| format!("audio remux task failed: {e}"))?
            .map_err(|e| e.to_string())?;
            Ok(payload)
        }
        UploadAudioSelectionPlan::Mix(selected_indices) => {
            let target = reserve_upload_payload_path(source_path)?;
            let payload = UploadPayload::owned(target.clone());
            let source = source_path.to_path_buf();
            tokio::task::spawn_blocking(move || {
                clipline_mp4::remux_with_mixed_audio_track_file(&source, &target, &selected_indices)
            })
            .await
            .map_err(|e| format!("audio mix task failed: {e}"))?
            .map_err(|e| e.to_string())?;
            Ok(payload)
        }
    }
}

fn reserve_upload_payload_path(source: &Path) -> Result<PathBuf, String> {
    let file_name = source
        .file_name()
        .ok_or_else(|| "clip path must include a file name".to_string())?;
    let parent = source
        .parent()
        .ok_or_else(|| "clip path must include a parent directory".to_string())?;
    prune_abandoned_upload_payloads(parent);
    for _ in 0..128 {
        let suffix = UPLOAD_PAYLOAD_TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut name = file_name.to_os_string();
        name.push(format!(
            ".clipline-upload-{}-{suffix}.tmp",
            std::process::id()
        ));
        let path = source.with_file_name(name);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => {
                drop(file);
                return Ok(path);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("reserve upload payload: {error}")),
        }
    }
    Err("could not reserve a unique upload payload path".into())
}

fn prune_abandoned_upload_payloads(directory: &Path) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        let is_upload_temp = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.contains(".clipline-upload-") && name.ends_with(".tmp"));
        let abandoned = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age >= UPLOAD_PAYLOAD_MAX_AGE);
        if is_upload_temp && abandoned {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(test)]
fn upload_bytes_for_audio_selection(
    source_bytes: Vec<u8>,
    markers: Option<&ClipMarkers>,
    selected_audio_track_ids: Option<&[String]>,
) -> Result<Vec<u8>, String> {
    match upload_audio_selection_plan(markers, selected_audio_track_ids)? {
        UploadAudioSelectionPlan::Original => Ok(source_bytes),
        UploadAudioSelectionPlan::Remux(selected_indices) => {
            clipline_mp4::remux_with_selected_audio_tracks(&source_bytes, &selected_indices)
                .map_err(|e| e.to_string())
        }
        UploadAudioSelectionPlan::Mix(selected_indices) => {
            clipline_mp4::remux_with_mixed_audio_track(&source_bytes, &selected_indices)
                .map_err(|e| e.to_string())
        }
    }
}

fn upload_audio_selection_plan(
    markers: Option<&ClipMarkers>,
    selected_audio_track_ids: Option<&[String]>,
) -> Result<UploadAudioSelectionPlan, String> {
    let Some(selected_audio_track_ids) = selected_audio_track_ids else {
        return Ok(UploadAudioSelectionPlan::Original);
    };
    let tracks = markers.map(|m| m.audio_tracks.as_slice()).unwrap_or(&[]);
    if tracks.is_empty() {
        if selected_audio_track_ids.is_empty() {
            return Ok(UploadAudioSelectionPlan::Remux(Vec::new()));
        }
        return Err("this clip has no selectable audio track metadata".into());
    }

    let selected_indices =
        crate::util::selected_audio_track_indices(markers.unwrap(), selected_audio_track_ids)?;
    if selected_indices.len() > 1 {
        Ok(UploadAudioSelectionPlan::Mix(selected_indices))
    } else {
        Ok(UploadAudioSelectionPlan::Remux(selected_indices))
    }
}

fn read_clip_game(path: &Path, markers: Option<&ClipMarkers>) -> Option<crate::library::ClipGame> {
    path.parent()
        .and_then(|dir| std::fs::read_to_string(dir.join("clipline-session.json")).ok())
        .and_then(|json| serde_json::from_str::<crate::library::ClipGame>(&json).ok())
        .or_else(|| markers.and_then(game_from_markers))
}

fn game_from_markers(markers: &ClipMarkers) -> Option<crate::library::ClipGame> {
    let game_id = markers.markers.first()?.event.game_id;
    let id = crate::game_plugins::plugin_id_for_game_id(game_id);
    Some(crate::library::ClipGame {
        id: id.to_string(),
        name: crate::game_plugins::display_name_for_game_id(game_id).to_string(),
    })
}

fn clip_duration_ms_file(path: &Path, markers: Option<&ClipMarkers>) -> Option<i64> {
    clipline_mp4::movie_duration_s_file(path)
        .ok()
        .flatten()
        .or_else(|| markers.map(|markers| markers.duration_s))
        .map(|seconds| (seconds * 1000.0).round())
        .filter(|value| value.is_finite() && *value >= 0.0)
        .map(|value| value as i64)
}

fn source_type(path: &Path) -> String {
    crate::library::clip_kind_for_path(path)
}

fn local_clip_id(path: &Path, meta: &std::fs::Metadata, checksum: &str) -> Result<String, String> {
    let canonical = path
        .canonicalize()
        .map_err(|e| format!("resolve clip path: {e}"))?;
    let modified = meta
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs())
        .unwrap_or_default();
    let payload = format!(
        "clipline-local-v1\0{}\0{}\0{}\0{}",
        canonical.display(),
        meta.len(),
        modified,
        checksum
    );
    Ok(format!("clipline-local-{}", sha256_hex(payload.as_bytes())))
}

fn existing_retry_status(cloud: &CloudSettings, local_clip_id: &str, path: &str) -> String {
    let existing = cloud.uploads.get(local_clip_id).or_else(|| {
        cloud
            .uploads
            .values()
            .filter(|record| clip_paths_equal(&record.path, path))
            .max_by_key(|record| record.updated_at_unix)
    });
    match existing.map(|record| record.upload_status.as_str()) {
        Some("failed") => "retrying".to_string(),
        Some("uploading") | Some("queued") | Some("processing") => "retrying".to_string(),
        _ => "queued".to_string(),
    }
}

fn windows_clip_path_key(path: &str) -> Option<String> {
    let mut normalized = path.trim().replace('/', "\\");
    let lower = normalized.to_ascii_lowercase();
    if lower.starts_with(r"\\?\unc\") {
        normalized = format!(r"\\{}", &normalized[8..]);
    } else if lower.starts_with(r"\\?\") {
        normalized = normalized[4..].to_string();
    }
    let bytes = normalized.as_bytes();
    let drive_path =
        bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'\\';
    if !drive_path && !normalized.starts_with(r"\\") {
        return None;
    }
    Some(normalized.to_ascii_lowercase())
}

fn clip_paths_equal(left: &str, right: &str) -> bool {
    if left == right {
        return true;
    }
    matches!(
        (windows_clip_path_key(left), windows_clip_path_key(right)),
        (Some(left), Some(right)) if left == right
    )
}

fn existing_uploaded_record(
    cloud: &CloudSettings,
    local_clip_id: Option<&str>,
    path: &str,
) -> Option<CloudUploadRecord> {
    let uploaded = |record: &&CloudUploadRecord| {
        record.remote_clip_id.is_some() && record.upload_status.starts_with("uploaded_")
    };
    if let Some(local_clip_id) = local_clip_id {
        return cloud.uploads.get(local_clip_id).filter(uploaded).cloned();
    }
    cloud
        .uploads
        .values()
        .filter(uploaded)
        .filter(|record| clip_paths_equal(&record.path, path))
        .max_by_key(|record| record.updated_at_unix)
        .cloned()
}

fn cloud_record_for_path(cloud: &CloudSettings, path: &str) -> Option<CloudUploadRecord> {
    cloud
        .uploads
        .values()
        .filter(|record| clip_paths_equal(&record.path, path))
        .max_by_key(|record| record.updated_at_unix)
        .cloned()
}

fn replace_upload_record(cloud: &mut CloudSettings, record: CloudUploadRecord) {
    cloud.uploads.retain(|key, existing| {
        key == &record.local_clip_id || !clip_paths_equal(&existing.path, &record.path)
    });
    cloud.uploads.insert(record.local_clip_id.clone(), record);
}

fn remove_upload_record(cloud: &mut CloudSettings, record: &CloudUploadRecord) {
    cloud.uploads.retain(|key, existing| {
        key != &record.local_clip_id && !clip_paths_equal(&existing.path, &record.path)
    });
}

fn persist_record(state: &RuntimeState, record: &CloudUploadRecord) -> Result<(), String> {
    state.update_cloud(|cloud| {
        replace_upload_record(cloud, record.clone());
    })?;
    Ok(())
}

fn mark_ready_timeout(record: &mut CloudUploadRecord) {
    record.upload_status = "uploaded_processing".to_string();
    record.error = Some(format!(
        "cloud upload completed, but cloud processing did not become ready within {} seconds; the local clip was preserved and a public share link will remain unavailable until a later status refresh",
        READY_POLL_ATTEMPTS as u64 * READY_POLL_DELAY.as_secs()
    ));
    record.updated_at_unix = unix_now();
}

fn mark_post_upload_problem(record: &mut CloudUploadRecord, message: String) {
    record.upload_status = "uploaded_processing".to_string();
    record.error = Some(message);
    record.updated_at_unix = unix_now();
}

fn persist_post_upload_record(
    sink: &dyn UiEventSink,
    generation: Generation,
    state: &RuntimeState,
    record: &CloudUploadRecord,
    file_size_bytes: u64,
) -> Result<(), String> {
    persist_record(state, record)?;
    emit_upload_progress(
        sink,
        generation,
        record,
        file_size_bytes,
        file_size_bytes,
        record.error.clone(),
    );
    Ok(())
}

fn apply_remote_clip_to_record(record: &mut CloudUploadRecord, clip: &ClipDetailResponse) {
    record.visibility = clip.visibility.clone();
    record.remote_clip_id = Some(clip.id.clone());
    record.remote_url = if clip.visibility == "private" {
        None
    } else {
        clip.public_url.clone()
    };
    record.upload_status = upload_status_for_remote_clip(clip);
    record.error = None;
    record.updated_at_unix = unix_now();
}

fn upload_status_for_remote_clip(clip: &ClipDetailResponse) -> String {
    if clip.status != "ready" {
        "uploaded_processing".to_string()
    } else if clip.visibility == "private" {
        "uploaded_private".to_string()
    } else {
        "uploaded_public".to_string()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MissingRemoteSyncAction {
    Keep,
    ConfirmMissing,
    Remove,
}

fn missing_remote_sync_action(record: &CloudUploadRecord) -> MissingRemoteSyncAction {
    if !record.upload_status.starts_with("uploaded_")
        || record.upload_status == "uploaded_processing"
    {
        return MissingRemoteSyncAction::Keep;
    }
    if record.error.as_deref() == Some(REMOTE_NOT_FOUND_SYNC_MARKER) {
        MissingRemoteSyncAction::Remove
    } else {
        MissingRemoteSyncAction::ConfirmMissing
    }
}

fn mark_remote_not_found_once(record: &mut CloudUploadRecord) {
    record.error = Some(REMOTE_NOT_FOUND_SYNC_MARKER.to_string());
    record.updated_at_unix = unix_now();
}

fn delete_uploaded_local_files(target: &Path) -> std::io::Result<()> {
    std::fs::remove_file(target).map_err(|error| {
        std::io::Error::new(
            error.kind(),
            format!("delete uploaded local clip {target:?}: {error}"),
        )
    })?;
    // Sidecars may not exist — ignore missing-file errors.
    let mut first_error = None;
    for sidecar in crate::library::clip_sidecar_paths(target) {
        if let Err(error) = std::fs::remove_file(&sidecar) {
            if error.kind() != std::io::ErrorKind::NotFound && first_error.is_none() {
                first_error = Some(std::io::Error::new(
                    error.kind(),
                    format!("delete uploaded clip sidecar {sidecar:?}: {error}"),
                ));
            }
        }
    }
    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

fn emit_upload_progress(
    sink: &dyn UiEventSink,
    generation: Generation,
    record: &CloudUploadRecord,
    received_size_bytes: u64,
    file_size_bytes: u64,
    error: Option<String>,
) {
    let _ = sink.try_publish(UiEvent::CloudUploadProgress {
        account: CloudAccountScope::INITIAL,
        generation,
        progress: CloudUploadProgressEvent {
            local_clip_id: record.local_clip_id.clone(),
            path: record.path.clone(),
            upload_status: record.upload_status.clone(),
            received_size_bytes,
            file_size_bytes,
            remote_clip_id: record.remote_clip_id.clone(),
            remote_url: record.remote_url.clone(),
            error,
        },
    });
}

async fn wait_for_ready_clip(
    client: &CloudClient,
    token: &str,
    clip_id: &str,
) -> Result<ReadyClipOutcome, CloudApiError> {
    wait_for_ready_clip_with_policy(
        client,
        token,
        clip_id,
        READY_POLL_ATTEMPTS,
        READY_POLL_DELAY,
    )
    .await
}

async fn wait_for_ready_clip_with_policy(
    client: &CloudClient,
    token: &str,
    clip_id: &str,
    attempts: usize,
    delay: Duration,
) -> Result<ReadyClipOutcome, CloudApiError> {
    for attempt in 0..attempts {
        match bounded_cloud_get_clip(client, token, clip_id).await {
            Ok(clip) if clip.status == "ready" => return Ok(ReadyClipOutcome::Ready(clip)),
            Ok(clip) if clip.status == "failed" => return Ok(ReadyClipOutcome::Failed(clip)),
            Ok(_)
            | Err(CloudApiError::Api {
                status: reqwest::StatusCode::NOT_FOUND,
                ..
            }) => {}
            Err(error) => return Err(error),
        }
        if attempt + 1 < attempts && !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
    }
    Ok(ReadyClipOutcome::TimedOut)
}

async fn verify_ready_cloud_media(
    cloud: &CloudSettings,
    token: &str,
    remote_clip_id: &str,
) -> Result<(), String> {
    let url = cloud_clip_asset_url(cloud, remote_clip_id, "media")?;
    let client = reqwest::Client::builder()
        .connect_timeout(READY_MEDIA_PROBE_CONNECT_TIMEOUT)
        .timeout(READY_MEDIA_PROBE_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| format!("create media verification client: {error}"))?;
    let mut response = client
        .get(url)
        .bearer_auth(token)
        .header(reqwest::header::RANGE, "bytes=0-0")
        .header(reqwest::header::ACCEPT_ENCODING, "identity")
        .send()
        .await
        .map_err(|error| format!("request ready cloud media: {error}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "ready cloud media returned HTTP {}",
            response.status()
        ));
    }
    let first_chunk = response
        .chunk()
        .await
        .map_err(|error| format!("read ready cloud media: {error}"))?;
    if first_chunk.as_ref().is_none_or(|bytes| bytes.is_empty()) {
        return Err("ready cloud media returned no bytes".to_string());
    }
    Ok(())
}

fn credential_target(host_url: &str, user_id: &str) -> String {
    clipline_settings::cloud::cloud_credential_target(host_url, user_id)
}

fn write_credential(target: &str, username: &str, token: &str) -> Result<(), String> {
    CLOUD_CREDENTIALS
        .write(target, username, token)
        .map_err(|error| error.to_string())
}

fn read_credential(target: &str) -> Result<String, String> {
    CLOUD_CREDENTIALS
        .read(target)
        .map_err(|error| error.to_string())
}

fn delete_credential_if_present(target: &str) -> Result<(), String> {
    CLOUD_CREDENTIALS
        .delete_if_present(target)
        .map_err(|error| error.to_string())
}

fn cloud_error(error: CloudApiError) -> String {
    error.to_string()
}

fn cloud_error_is_not_found(error: &CloudApiError) -> bool {
    matches!(error, CloudApiError::Api { status, .. } if status.as_u16() == 404)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clipline_events::ClipAudioTrack;
    use clipline_mp4::{
        AudioTrackConfig, FragSample, HybridMp4Writer, TrackConfig, VideoTrackConfig,
    };
    use clipline_test_utils::TestDir;
    use httpmock::prelude::*;
    use std::io::Cursor;

    #[test]
    fn credential_target_includes_server_and_user() {
        assert_eq!(
            credential_target("https://clips.example.com", "user_1"),
            "Clipline Cloud:https://clips.example.com:user_1"
        );
    }

    #[test]
    fn cached_asset_dto_hides_windows_verbatim_prefixes() {
        assert_eq!(
            cloud_cache_display_path(Path::new(r"\\?\C:\Videos\Clipline\clip.mp4")),
            r"C:\Videos\Clipline\clip.mp4"
        );
        assert_eq!(
            cloud_cache_display_path(Path::new(r"\\?\UNC\nas\clips\clip.mp4")),
            r"\\nas\clips\clip.mp4"
        );
    }

    #[test]
    fn upload_progress_omits_absent_share_url() {
        let event = CloudUploadProgressEvent {
            local_clip_id: "local-1".into(),
            path: "D:\\Videos\\clip.mp4".into(),
            upload_status: "processing".into(),
            received_size_bytes: 10,
            file_size_bytes: 20,
            remote_clip_id: Some("remote-1".into()),
            remote_url: None,
            error: None,
        };

        let serialized = serde_json::to_value(event).expect("serialize upload progress");

        assert!(
            serialized.get("remote_url").is_none(),
            "an absent share URL must not erase a previously refreshed URL"
        );
    }

    #[test]
    fn cloud_upload_result_serializes_confirmed_local_deletion() {
        let result = CloudUploadResult {
            record: upload_record("local", "clip.mp4", "uploaded_private", 1),
            clip: None,
            local_deleted: true,
        };

        let value = serde_json::to_value(result).unwrap();
        assert_eq!(value["local_deleted"], true);
    }

    #[test]
    fn source_type_falls_back_to_replay() {
        assert_eq!(source_type(Path::new("clipline-2026-06-16.mp4")), "replay");
        assert_eq!(source_type(Path::new("full-session.mp4")), "replay");
        assert_eq!(source_type(Path::new("ranked-trim.mp4")), "replay");
        assert_eq!(source_type(Path::new("session_1781377615.mp4")), "session");
        assert_eq!(
            source_type(Path::new("clip_1_trim_001000_002000.mp4")),
            "trim"
        );
    }

    #[test]
    fn upload_metadata_uses_clip_title_and_kind_sidecar() {
        let dir = TestDir::new("clipline-cloud", "clip-metadata-upload");
        let clip = dir.path().join("Ranked win.mp4");
        std::fs::write(&clip, b"mp4").unwrap();
        std::fs::write(
            clip.with_extension("clipline.json"),
            r#"{"title":"Ranked win vs Lux","kind":"session"}"#,
        )
        .unwrap();

        assert_eq!(upload_title(None, &clip), "Ranked win vs Lux");
        assert_eq!(source_type(&clip), "session");
    }

    #[test]
    fn upload_audio_selection_plan_mixes_multiple_selected_tracks() {
        let markers = audio_markers();
        let selected = vec!["output".to_string(), "microphone".to_string()];

        assert_eq!(
            upload_audio_selection_plan(Some(&markers), Some(&selected)).unwrap(),
            UploadAudioSelectionPlan::Mix(vec![0, 1])
        );
    }

    #[test]
    fn upload_audio_selection_remuxes_only_selected_track() {
        let source = two_audio_mp4();
        let markers = audio_markers();
        let selected = vec!["microphone".to_string()];

        let out =
            upload_bytes_for_audio_selection(source, Some(&markers), Some(&selected)).unwrap();

        assert!(out.windows(6).any(|w| w == b"V00000"));
        assert!(!out.windows(6).any(|w| w == b"A00000"));
        assert!(out.windows(6).any(|w| w == b"B00000"));
    }

    #[test]
    fn upload_audio_selection_rejects_unknown_track_id() {
        let source = two_audio_mp4();
        let markers = audio_markers();
        let selected = vec!["discord".to_string()];

        let err = upload_bytes_for_audio_selection(source, Some(&markers), Some(&selected))
            .expect_err("unknown track");

        assert!(err.contains("unknown audio track"), "{err}");
    }

    #[test]
    fn owned_upload_payload_is_removed_but_original_is_preserved() {
        let dir = TestDir::new("clipline-cloud", "upload-payload-ownership");
        let original = dir.path().join("original.mp4");
        let temporary = dir.path().join("temporary.mp4");
        std::fs::write(&original, b"original").unwrap();
        std::fs::write(&temporary, b"temporary").unwrap();

        drop(UploadPayload::original(&original));
        drop(UploadPayload::owned(temporary.clone()));

        assert!(original.exists());
        assert!(!temporary.exists());
    }

    #[tokio::test]
    async fn selected_audio_upload_uses_and_cleans_file_backed_payload() {
        let dir = TestDir::new("clipline-cloud", "selected-upload-payload");
        let source = dir.path().join("source.mp4");
        std::fs::write(&source, two_audio_mp4()).unwrap();
        let markers = audio_markers();
        let selected = vec!["microphone".to_string()];

        let payload =
            upload_payload_for_audio_selection_from_path(&source, Some(&markers), Some(&selected))
                .await
                .unwrap();
        let payload_path = payload.path().to_path_buf();
        let payload_bytes = std::fs::read(&payload_path).unwrap();

        assert_ne!(payload_path, source);
        assert!(payload_bytes.windows(6).any(|window| window == b"V00000"));
        assert!(!payload_bytes.windows(6).any(|window| window == b"A00000"));
        assert!(payload_bytes.windows(6).any(|window| window == b"B00000"));
        drop(payload);
        assert!(!payload_path.exists());
        assert!(source.exists());
    }

    #[test]
    fn abandoned_upload_payload_prune_is_scoped_and_age_gated() {
        let dir = TestDir::new("clipline-cloud", "upload-payload-prune");
        let abandoned = dir.path().join("clip.mp4.clipline-upload-1-1.tmp");
        let active = dir.path().join("clip.mp4.clipline-upload-1-2.tmp");
        let unrelated = dir.path().join("editor.tmp");
        for path in [&abandoned, &active, &unrelated] {
            std::fs::write(path, b"temp").unwrap();
        }
        std::fs::File::options()
            .write(true)
            .open(&abandoned)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(1))
            .unwrap();

        prune_abandoned_upload_payloads(dir.path());

        assert!(!abandoned.exists());
        assert!(active.exists());
        assert!(unrelated.exists());
    }

    #[test]
    fn upload_record_supersedes_older_record_for_same_path() {
        let mut cloud = CloudSettings::default();
        cloud.uploads.insert(
            "old".into(),
            upload_record("old", "D:\\Videos\\clip.mp4", "failed", 10),
        );
        cloud.uploads.insert(
            "other".into(),
            upload_record("other", "D:\\Videos\\other.mp4", "uploaded_public", 11),
        );

        let newer = upload_record("new", "D:\\Videos\\clip.mp4", "queued", 12);
        replace_upload_record(&mut cloud, newer.clone());

        assert!(!cloud.uploads.contains_key("old"));
        assert_eq!(cloud.uploads.get("new"), Some(&newer));
        assert_eq!(
            cloud
                .uploads
                .get("other")
                .map(|record| record.path.as_str()),
            Some("D:\\Videos\\other.mp4")
        );
    }

    #[test]
    fn existing_retry_status_uses_same_path_when_audio_selection_changed() {
        let mut cloud = CloudSettings::default();
        cloud.uploads.insert(
            "old".into(),
            upload_record("old", "D:\\Videos\\clip.mp4", "failed", 10),
        );

        assert_eq!(
            existing_retry_status(&cloud, "new", "D:\\Videos\\clip.mp4"),
            "retrying"
        );
        assert_eq!(
            existing_retry_status(&cloud, "new", "D:\\Videos\\other.mp4"),
            "queued"
        );
    }

    #[test]
    fn legacy_windows_canonical_paths_match_library_paths() {
        assert!(clip_paths_equal(
            r"\\?\D:\Videos\Clipline\clip.mp4",
            r"D:\Videos\Clipline\clip.mp4"
        ));
        assert!(clip_paths_equal(
            r"d:/videos/clipline/CLIP.mp4",
            r"D:\Videos\Clipline\clip.mp4"
        ));
        assert!(!clip_paths_equal("/Clips/clip.mp4", "/clips/clip.mp4"));
    }

    #[test]
    fn uploaded_record_lookup_blocks_legacy_path_reupload() {
        let mut cloud = CloudSettings::default();
        let mut record = upload_record(
            "legacy",
            r"\\?\D:\Videos\Clipline\clip.mp4",
            "uploaded_public",
            10,
        );
        record.remote_clip_id = Some("remote-1".into());
        record.remote_url = Some("https://clips.example.com/c/c_existing".into());
        cloud.uploads.insert("legacy".into(), record.clone());

        assert_eq!(
            existing_uploaded_record(&cloud, None, r"D:\Videos\Clipline\clip.mp4"),
            Some(record.clone())
        );
        assert_eq!(
            existing_uploaded_record(
                &cloud,
                Some("different-payload-hash"),
                r"D:\Videos\Clipline\clip.mp4"
            ),
            None,
            "a changed payload at the same path must not reuse an older upload"
        );
    }

    #[test]
    fn uploaded_private_record_without_share_url_blocks_reupload() {
        let mut cloud = CloudSettings::default();
        let mut record = upload_record(
            "private-local",
            r"D:\Videos\Clipline\private.mp4",
            "uploaded_private",
            10,
        );
        record.remote_clip_id = Some("private-remote".into());
        cloud.uploads.insert("private-local".into(), record.clone());

        assert_eq!(
            existing_uploaded_record(
                &cloud,
                Some("private-local"),
                r"D:\Videos\Clipline\private.mp4"
            ),
            Some(record)
        );
    }

    #[test]
    fn ready_timeout_keeps_remote_identity_without_fabricating_share_url() {
        let mut record = upload_record("local", "D:\\Videos\\clip.mp4", "processing", 10);
        record.remote_clip_id = Some("remote-1".into());

        mark_ready_timeout(&mut record);

        assert_eq!(record.upload_status, "uploaded_processing");
        assert_eq!(record.remote_clip_id.as_deref(), Some("remote-1"));
        assert_eq!(record.remote_url, None);
        assert!(
            record
                .error
                .as_deref()
                .is_some_and(|error| error.contains("processing") && !error.contains("retry the upload")),
            "timeout should explain that cloud processing is still pending without forcing a reupload"
        );
    }

    #[tokio::test]
    async fn readiness_poll_does_not_accept_processing_as_ready() {
        let server = MockServer::start();
        let response = clip_detail("remote-1", "private", "processing", None);
        let request = server.mock(|when, then| {
            when.method(GET).path("/api/v1/clips/remote-1");
            then.status(200)
                .header("content-type", "application/json")
                .json_body_obj(&response);
        });

        let outcome = wait_for_ready_clip_with_policy(
            &test_cloud_client(&server),
            "token",
            "remote-1",
            3,
            Duration::ZERO,
        )
        .await
        .unwrap();

        assert!(matches!(outcome, ReadyClipOutcome::TimedOut));
        request.assert_hits(3);
    }

    #[tokio::test]
    async fn readiness_poll_treats_remote_processing_failure_as_terminal() {
        let server = MockServer::start();
        let response = clip_detail("remote-1", "private", "failed", None);
        let request = server.mock(|when, then| {
            when.method(GET).path("/api/v1/clips/remote-1");
            then.status(200)
                .header("content-type", "application/json")
                .json_body_obj(&response);
        });

        let outcome = wait_for_ready_clip_with_policy(
            &test_cloud_client(&server),
            "token",
            "remote-1",
            3,
            Duration::ZERO,
        )
        .await
        .unwrap();

        assert!(matches!(outcome, ReadyClipOutcome::Failed(clip) if clip.status == "failed"));
        request.assert_hits(1);
    }

    #[tokio::test]
    async fn readiness_poll_returns_only_an_explicitly_ready_clip() {
        let server = MockServer::start();
        let response = clip_detail("remote-1", "private", "ready", None);
        let request = server.mock(|when, then| {
            when.method(GET).path("/api/v1/clips/remote-1");
            then.status(200)
                .header("content-type", "application/json")
                .json_body_obj(&response);
        });

        let outcome = wait_for_ready_clip_with_policy(
            &test_cloud_client(&server),
            "token",
            "remote-1",
            3,
            Duration::ZERO,
        )
        .await
        .unwrap();

        assert!(matches!(outcome, ReadyClipOutcome::Ready(clip) if clip.status == "ready"));
        request.assert_hits(1);
    }

    #[tokio::test]
    async fn ready_media_probe_requires_retrievable_nonempty_content() {
        let server = MockServer::start();
        let media = server.mock(|when, then| {
            when.method(GET)
                .path("/api/v1/clips/remote-1/media")
                .header("authorization", "Bearer token")
                .header("range", "bytes=0-0");
            then.status(206).body("x");
        });
        let cloud = CloudSettings {
            host_url: server.base_url(),
            ..CloudSettings::default()
        };

        verify_ready_cloud_media(&cloud, "token", "remote-1")
            .await
            .unwrap();

        media.assert_hits(1);
    }

    #[tokio::test]
    async fn ready_media_probe_rejects_empty_and_failed_responses() {
        let empty_server = MockServer::start();
        empty_server.mock(|when, then| {
            when.method(GET).path("/api/v1/clips/remote-1/media");
            then.status(206);
        });
        let empty_cloud = CloudSettings {
            host_url: empty_server.base_url(),
            ..CloudSettings::default()
        };
        let empty_error = verify_ready_cloud_media(&empty_cloud, "token", "remote-1")
            .await
            .expect_err("empty media is not durable");
        assert!(empty_error.contains("no bytes"), "{empty_error}");

        let failed_server = MockServer::start();
        failed_server.mock(|when, then| {
            when.method(GET).path("/api/v1/clips/remote-1/media");
            then.status(404);
        });
        let failed_cloud = CloudSettings {
            host_url: failed_server.base_url(),
            ..CloudSettings::default()
        };
        let failed_error = verify_ready_cloud_media(&failed_cloud, "token", "remote-1")
            .await
            .expect_err("missing media is not durable");
        assert!(failed_error.contains("404"), "{failed_error}");
    }

    #[test]
    fn post_upload_problem_keeps_remote_identity_for_reconciliation() {
        let mut record = upload_record("local", "D:\\Videos\\clip.mp4", "processing", 10);
        record.remote_clip_id = Some("remote-1".into());
        record.remote_url = Some("https://clips.example.com/c/c_existing".into());

        mark_post_upload_problem(&mut record, "visibility update failed".into());

        assert_eq!(record.upload_status, "uploaded_processing");
        assert_eq!(record.remote_clip_id.as_deref(), Some("remote-1"));
        assert_eq!(
            record.remote_url.as_deref(),
            Some("https://clips.example.com/c/c_existing")
        );
        assert_eq!(record.error.as_deref(), Some("visibility update failed"));
    }

    #[test]
    fn cloud_clip_detail_updates_record_visibility_status_and_url() {
        let mut record = upload_record("local", "D:\\Videos\\clip.mp4", "uploaded_public", 10);
        record.remote_clip_id = Some("remote-1".into());
        record.remote_url = Some("https://clips.example.com/old".into());

        apply_remote_clip_to_record(
            &mut record,
            &clip_detail(
                "remote-1",
                "unlisted",
                "ready",
                Some("https://share.example.com/c/1"),
            ),
        );

        assert_eq!(record.visibility, "unlisted");
        assert_eq!(record.upload_status, "uploaded_public");
        assert_eq!(
            record.remote_url.as_deref(),
            Some("https://share.example.com/c/1")
        );
        assert!(record.error.is_none());

        apply_remote_clip_to_record(
            &mut record,
            &clip_detail("remote-1", "private", "ready", None),
        );

        assert_eq!(record.visibility, "private");
        assert_eq!(record.upload_status, "uploaded_private");
        assert_eq!(
            record.remote_url, None,
            "private clip detail must clear a previously saved public share URL"
        );
    }

    #[tokio::test]
    async fn visibility_update_refreshes_canonical_public_url() {
        let server = MockServer::start();
        let stale_update = clip_detail("remote-1", "public", "ready", None);
        let refreshed = clip_detail(
            "remote-1",
            "public",
            "ready",
            Some("https://clips.example.com/c/c_share"),
        );
        let update = server.mock(|when, then| {
            when.method(POST)
                .path("/api/v1/clips/remote-1/visibility")
                .json_body_obj(&UpdateVisibilityRequest {
                    visibility: "public".to_string(),
                });
            then.status(200)
                .header("content-type", "application/json")
                .json_body_obj(&stale_update);
        });
        let refresh = server.mock(|when, then| {
            when.method(GET).path("/api/v1/clips/remote-1");
            then.status(200)
                .header("content-type", "application/json")
                .json_body_obj(&refreshed);
        });

        let clip = update_cloud_clip_visibility(
            &test_cloud_client(&server),
            "token",
            "remote-1",
            "public",
        )
        .await
        .expect("update and refresh visibility");

        assert_eq!(
            clip.public_url.as_deref(),
            Some("https://clips.example.com/c/c_share")
        );
        update.assert();
        refresh.assert();
    }

    #[tokio::test]
    async fn visibility_update_preserves_post_detail_if_refresh_fails() {
        let server = MockServer::start();
        let updated = clip_detail(
            "remote-1",
            "unlisted",
            "ready",
            Some("https://clips.example.com/c/c_post_fallback"),
        );
        let update = server.mock(|when, then| {
            when.method(POST).path("/api/v1/clips/remote-1/visibility");
            then.status(200)
                .header("content-type", "application/json")
                .json_body_obj(&updated);
        });
        let refresh = server.mock(|when, then| {
            when.method(GET).path("/api/v1/clips/remote-1");
            then.status(503).body("try again later");
        });

        let clip = update_cloud_clip_visibility(
            &test_cloud_client(&server),
            "token",
            "remote-1",
            "unlisted",
        )
        .await
        .expect("successful visibility update remains successful");

        assert_eq!(
            clip.public_url.as_deref(),
            Some("https://clips.example.com/c/c_post_fallback")
        );
        update.assert();
        refresh.assert();
    }

    #[tokio::test]
    async fn visibility_update_keeps_url_less_success_recoverable_if_refresh_fails() {
        let server = MockServer::start();
        let updated = clip_detail("remote-1", "public", "ready", None);
        let update = server.mock(|when, then| {
            when.method(POST).path("/api/v1/clips/remote-1/visibility");
            then.status(200)
                .header("content-type", "application/json")
                .json_body_obj(&updated);
        });
        let refresh = server.mock(|when, then| {
            when.method(GET).path("/api/v1/clips/remote-1");
            then.status(503).body("try again later");
        });

        let error = update_cloud_clip_visibility(
            &test_cloud_client(&server),
            "token",
            "remote-1",
            "public",
        )
        .await
        .expect_err("a URL-less public update must remain recoverable");

        assert!(
            error
                .to_string()
                .contains("refreshing the canonical public URL failed"),
            "{error}"
        );
        update.assert();
        refresh.assert();
    }

    #[test]
    fn cloud_clip_asset_url_uses_api_host_and_safe_clip_ids() {
        let cloud = CloudSettings {
            host_url: "https://clips.example.com/base".into(),
            ..CloudSettings::default()
        };
        let url = cloud_clip_asset_url(&cloud, "remote-1_ABC", "media").expect("asset URL");
        assert_eq!(
            url.as_str(),
            "https://clips.example.com/base/api/v1/clips/remote-1_ABC/media"
        );
        assert!(cloud_clip_asset_url(&cloud, "../escape", "media").is_err());
        assert!(cloud_clip_asset_url(&cloud, "remote/escape", "thumbnail").is_err());
    }

    #[test]
    fn legacy_cloud_cache_migration_moves_only_regular_namespace_directories() {
        let dir = TestDir::new("clipline-cloud", "cloud-cache-migration");
        let legacy = dir.path().join("roaming-cloud-cache");
        let local = dir.path().join("local-cloud-cache");
        let namespace = legacy.join("abcdef0123456789");
        std::fs::create_dir_all(&namespace).unwrap();
        std::fs::write(namespace.join("clip.mp4"), b"clip").unwrap();
        std::fs::write(legacy.join("unrelated.txt"), b"leave me").unwrap();
        let external = dir.path().join("external");
        std::fs::create_dir_all(&external).unwrap();
        std::fs::write(external.join("outside.mp4"), b"outside").unwrap();
        let linked_namespace = legacy.join("1111111111111111");
        let linked = std::os::windows::fs::symlink_dir(&external, &linked_namespace).is_ok();

        migrate_legacy_cloud_cache(&legacy, &local).unwrap();

        assert!(local.join("abcdef0123456789").join("clip.mp4").exists());
        assert!(legacy.join("unrelated.txt").exists());
        if linked {
            assert!(external.join("outside.mp4").exists());
            assert!(!local.join("1111111111111111").exists());
        }
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

    #[test]
    fn cloud_user_avatar_data_url_preserves_legacy_shape() {
        assert_eq!(
            cloud_user_avatar_data_url("image/png", b"\x01\x02\x03"),
            "data:image/png;base64,AQID"
        );
    }

    #[test]
    fn missing_remote_clip_keeps_unconfirmed_and_processing_records() {
        assert_eq!(
            missing_remote_sync_action(&upload_record(
                "local",
                "D:\\Videos\\clip.mp4",
                "uploaded_public",
                10
            )),
            MissingRemoteSyncAction::ConfirmMissing
        );
        assert_eq!(
            missing_remote_sync_action(&upload_record(
                "local",
                "D:\\Videos\\clip.mp4",
                "uploaded_processing",
                10
            )),
            MissingRemoteSyncAction::Keep
        );
        assert_eq!(
            missing_remote_sync_action(&upload_record(
                "local",
                "D:\\Videos\\clip.mp4",
                "processing",
                10
            )),
            MissingRemoteSyncAction::Keep
        );
    }

    #[test]
    fn missing_remote_clip_requires_confirmation_before_removing_finalized_record() {
        let mut record = upload_record("local", "D:\\Videos\\clip.mp4", "uploaded_public", 10);

        assert_eq!(
            missing_remote_sync_action(&record),
            MissingRemoteSyncAction::ConfirmMissing
        );

        mark_remote_not_found_once(&mut record);

        assert_eq!(
            missing_remote_sync_action(&record),
            MissingRemoteSyncAction::Remove
        );
    }

    #[test]
    fn delete_uploaded_local_files_removes_poster_sidecar() {
        let dir = test_dir("cloud-delete");
        let clip = dir.join("clip.mp4");
        let markers = clip.with_extension("markers.json");
        let metadata = clip.with_extension("clipline.json");
        let pending_osu = clip.with_extension("osu-enrichment.json");
        let poster = clipline_library::poster_path(&clip);
        std::fs::write(&clip, b"mp4").unwrap();
        std::fs::write(&markers, b"{}").unwrap();
        std::fs::write(&metadata, b"{}").unwrap();
        std::fs::write(&pending_osu, b"{}").unwrap();
        std::fs::write(&poster, b"jpg").unwrap();

        delete_uploaded_local_files(&clip).unwrap();

        assert!(!clip.exists());
        assert!(!markers.exists());
        assert!(!metadata.exists());
        assert!(!pending_osu.exists());
        assert!(!poster.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn local_cleanup_preserves_sidecars_when_primary_deletion_fails() {
        let dir = TestDir::new("clipline-cloud", "delete-primary-first");
        let clip = dir.path().join("clip.mp4");
        let markers = clip.with_extension("markers.json");
        std::fs::create_dir(&clip).unwrap();
        std::fs::write(&markers, b"{}").unwrap();

        delete_uploaded_local_files(&clip).expect_err("a directory is not a removable MP4 file");

        assert!(clip.exists());
        assert!(markers.exists());
    }

    #[test]
    fn local_cleanup_reports_sidecar_failure_after_primary_deletion() {
        let dir = TestDir::new("clipline-cloud", "delete-sidecar-error");
        let clip = dir.path().join("clip.mp4");
        let markers = clip.with_extension("markers.json");
        std::fs::write(&clip, b"mp4").unwrap();
        std::fs::create_dir(&markers).unwrap();

        let error = delete_uploaded_local_files(&clip).expect_err("sidecar directory must fail");

        assert!(!clip.exists(), "primary deletion happens before sidecars");
        assert!(markers.exists());
        assert!(error.to_string().contains("sidecar"), "{error}");
    }

    fn audio_markers() -> ClipMarkers {
        ClipMarkers {
            recording_start_s: 0.0,
            duration_s: 1.0,
            player_summary: None,
            audio_tracks: vec![
                ClipAudioTrack {
                    id: "output".into(),
                    track_index: 0,
                    label: "Output Audio".into(),
                    kind: Some("output".into()),
                },
                ClipAudioTrack {
                    id: "microphone".into(),
                    track_index: 1,
                    label: "Microphone".into(),
                    kind: Some("microphone".into()),
                },
            ],
            plays: Vec::new(),
            markers: Vec::new(),
        }
    }

    fn two_audio_mp4() -> Vec<u8> {
        let tracks = vec![
            TrackConfig::Video(VideoTrackConfig::h264(
                128,
                72,
                90_000,
                vec![0x67, 0x64, 0x00, 0x0A, 0xAC],
                vec![0x68, 0xEE, 0x38, 0x80],
            )),
            TrackConfig::Audio(AudioTrackConfig {
                channels: 2,
                sample_rate: 48_000,
                pre_skip: 312,
            }),
            TrackConfig::Audio(AudioTrackConfig {
                channels: 2,
                sample_rate: 48_000,
                pre_skip: 312,
            }),
        ];
        let mut writer = HybridMp4Writer::new_multi(Cursor::new(Vec::new()), tracks).unwrap();
        let video: Vec<_> = (0..10)
            .map(|i| FragSample {
                data: format!("V{i:05}").into_bytes(),
                duration: 9_000,
                is_sync: i == 0,
            })
            .collect();
        let output = audio_samples("A");
        let mic = audio_samples("B");
        writer
            .write_fragment_multi(&[&video, &output, &mic])
            .unwrap();
        writer.finalize().unwrap().into_inner()
    }

    fn audio_samples(prefix: &str) -> Vec<FragSample> {
        (0..50)
            .map(|i| FragSample {
                data: format!("{prefix}{i:05}").into_bytes(),
                duration: 960,
                is_sync: true,
            })
            .collect()
    }

    fn upload_record(
        local_clip_id: &str,
        path: &str,
        upload_status: &str,
        updated_at_unix: u64,
    ) -> CloudUploadRecord {
        CloudUploadRecord {
            local_clip_id: local_clip_id.into(),
            path: path.into(),
            remote_clip_id: None,
            remote_url: None,
            visibility: "private".into(),
            upload_status: upload_status.into(),
            error: None,
            updated_at_unix,
        }
    }

    fn clip_detail(
        id: &str,
        visibility: &str,
        status: &str,
        public_url: Option<&str>,
    ) -> ClipDetailResponse {
        let now = Utc::now();
        ClipDetailResponse {
            id: id.into(),
            client_clip_id: Some("local".into()),
            title: "Clip".into(),
            description: None,
            game_name: None,
            game_id: None,
            game_executable: None,
            source_type: Some("replay".into()),
            recorded_at: None,
            uploaded_at: Some(now),
            duration_ms: None,
            file_size_bytes: None,
            width: None,
            height: None,
            fps: None,
            container: Some("mp4".into()),
            video_codec: None,
            audio_codec: None,
            checksum_sha256: None,
            visibility: visibility.into(),
            status: status.into(),
            public_share_id: None,
            public_url: public_url.map(str::to_string),
            view_count: 0,
            markers: Vec::new(),
            created_at: now,
            updated_at: now,
        }
    }

    fn test_cloud_client(server: &MockServer) -> CloudClient {
        CloudClient::with_device_token(server.base_url().parse().unwrap(), "token")
    }

    fn test_dir(name: &str) -> std::path::PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "clipline-cloud-{name}-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
