//! Clipline Cloud desktop integration: connection state, OS credential storage,
//! and per-clip uploads through the first-party API client.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::UNIX_EPOCH;

use base64::{engine::general_purpose, Engine as _};
use clipline_desktop::{
    CloudAccountOwner, CloudAccountScope, UiEvent, UiEventSink, WindowLifecycleMode,
};
use clipline_library::cache::{
    AccountPublicationGuard, AvailableSpacePort, CloudAssetRequest as SharedCloudAssetRequest,
    CloudCache, CloudCacheError, CloudCancellation, CloudMediaLease,
};
use clipline_library::cache_identity::{
    CloudAccountFence, CloudAssetKey, CloudAssetKind, CloudCacheNamespace,
};
use clipline_library::http::{ReqwestAssetDownload, ReqwestCloudProtocol, ReqwestCloudTransport};
use clipline_library::ports::{
    CloudAccountPort, CloudCredential, CloudCredentialPort, CloudProfilePatch, CloudRequestFence,
    PortError,
};
use clipline_library::protocol::{
    ClipDetailResponse, CloudApiBase, CloudProtocolError, CreateDeviceTokenRequest,
};
use clipline_library::{
    account_key as shared_account_key, CloudAccountFields,
    CloudAccountGeneration as LibraryAccountGeneration, CloudAccountSnapshot, CloudBrowserEffect,
    CloudService, CloudServiceAccount, CloudUserProfile as SharedCloudUserProfile, CloudWorkToken,
    ForegroundGeneration, RequestGeneration, UploadCancellation, UploadEndpoint,
    UploadStatusSyncOutcome, WindowAttachmentGeneration, WindowWorkToken,
};
use clipline_shell::windows::credential::CredentialStore;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, Runtime, Wry};

use crate::app::{RuntimeState, WindowLifecycleState};
use crate::library::{validate_upload_source, StorageSettings};
use crate::settings::{normalize_cloud_visibility, CloudSettings, CloudUploadRecord};
use crate::util::unix_now;

const DEFAULT_DEVICE_NAME: &str = "Clipline Desktop";
const CLOUD_CREDENTIALS: CredentialStore = CredentialStore::new("cloud token");
static CLOUD_COMPAT_REQUEST_GENERATION: AtomicU64 = AtomicU64::new(0);
static CLOUD_MEDIA_LEASE_GENERATION: AtomicU64 = AtomicU64::new(0);
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

fn desktop_cloud_account_owner(
    cloud: &CloudSettings,
    generation: u64,
) -> Result<CloudAccountOwner, String> {
    let service =
        service_account_from_settings(cloud, generation).map_err(|error| error.to_string())?;
    CloudAccountOwner::new(
        service.snapshot.account_key.as_str(),
        CloudAccountScope::new(generation),
    )
    .map_err(|error| error.to_string())
}

fn publish_desktop_cloud_account<R: Runtime>(
    app: &AppHandle<R>,
    generation: u64,
    account: Option<CloudAccountOwner>,
) -> Result<(), String> {
    app.state::<crate::desktop::tauri_sink::TauriUiEventSink>()
        .inner()
        .try_publish(UiEvent::CloudAccountChanged {
            generation: CloudAccountScope::new(generation),
            account,
        })
        .map(|_| ())
        .map_err(|error| error.to_string())
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

#[derive(Debug, Serialize)]
pub struct CloudUploadResult {
    pub record: CloudUploadRecord,
    pub clip: Option<ClipDetailResponse>,
    pub local_deleted: bool,
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
        .accept_media(&account, cached, &CloudCancellation::default())
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
    uploads: tauri::State<'_, crate::cloud_upload::TauriUploadState>,
    request: SyncCloudClipStatusRequest,
) -> Result<CloudClipStatusSyncResult, String> {
    let snapshot = state.cloud_settings_snapshot()?;
    let Some(durable) = cloud_record_for_path(&snapshot.document.cloud, &request.path) else {
        return Ok(CloudClipStatusSyncResult {
            path: request.path,
            record: None,
            removed: false,
        });
    };
    let account =
        service_account_from_settings(&snapshot.document.cloud, snapshot.account_generation.get())
            .map_err(|error| error.to_string())?;
    let owner = clipline_library::UploadAccountOwner::new(
        account.snapshot.account_key,
        account.snapshot.generation,
    );
    let local_clip_id = clipline_library::LocalClipId::new(durable.local_clip_id.clone())
        .map_err(|error| error.to_string())?;
    if durable.remote_clip_id.is_none() {
        return Ok(CloudClipStatusSyncResult {
            path: request.path,
            record: Some(durable),
            removed: false,
        });
    }
    let credential_target = snapshot
        .document
        .cloud
        .credential_target
        .as_deref()
        .ok_or_else(|| "connect to Clipline Cloud first".to_string())?;
    let endpoint = UploadEndpoint::new(
        owner,
        CloudApiBase::parse(&snapshot.document.cloud.host_url, true)
            .map_err(cloud_protocol_error)?,
        CloudCredential::new(read_credential(credential_target)?),
    );
    let outcome = uploads
        .status()
        .sync(&endpoint, &local_clip_id, &UploadCancellation::default())
        .await
        .map_err(|error| error.to_string())?;
    match outcome {
        UploadStatusSyncOutcome::MissingRecord => Ok(CloudClipStatusSyncResult {
            path: request.path,
            record: None,
            // The command resolved a compatibility record above, but the
            // exact durable slot disappeared before status work began. Let
            // the JS expected-record fence remove only that stale mirror.
            removed: true,
        }),
        UploadStatusSyncOutcome::Unchanged(record) | UploadStatusSyncOutcome::Updated(record) => {
            Ok(CloudClipStatusSyncResult {
                path: request.path,
                record: Some(crate::cloud_upload::cloud_record_from_upload(&record)),
                removed: false,
            })
        }
        UploadStatusSyncOutcome::Removed { .. } => Ok(CloudClipStatusSyncResult {
            path: request.path,
            record: None,
            removed: true,
        }),
    }
}

#[tauri::command]
pub async fn cloud_connect(
    app: AppHandle<Wry>,
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

    let base_url = CloudApiBase::parse(request.host_url.trim(), request.plain_http_confirmed)
        .map_err(cloud_protocol_error)?;
    let protocol = ReqwestCloudProtocol::new(base_url.clone()).map_err(cloud_protocol_error)?;
    let discovery = protocol.discovery().await.map_err(cloud_protocol_error)?;
    let device_token = protocol
        .create_device_token(&CreateDeviceTokenRequest {
            username: request.username.trim().to_string(),
            password: request.password,
            name: device_name,
        })
        .await
        .map_err(cloud_protocol_error)?;
    let me = protocol
        .me(&device_token.token)
        .await
        .map_err(cloud_protocol_error)?;

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
    let (committed_cloud, generation) = state.cloud_settings_generation()?;
    let upload_account = service_account_from_settings(&committed_cloud, generation)
        .map_err(|error| error.to_string())?;
    app.state::<crate::cloud_upload::TauriUploadState>()
        .service()
        .account_changed(Some(&clipline_library::UploadAccountOwner::new(
            upload_account.snapshot.account_key,
            upload_account.snapshot.generation,
        )));
    publish_desktop_cloud_account(
        &app,
        generation,
        Some(desktop_cloud_account_owner(&committed_cloud, generation)?),
    )?;

    Ok(connection_status(&settings.cloud))
}

#[tauri::command]
pub fn cloud_disconnect(
    app: AppHandle<Wry>,
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
    let (_, generation) = state.cloud_settings_generation()?;
    app.state::<crate::cloud_upload::TauriUploadState>()
        .service()
        .account_changed(None);
    publish_desktop_cloud_account(&app, generation, None)?;
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
pub async fn upload_clip_to_cloud(
    state: tauri::State<'_, RuntimeState>,
    storage: tauri::State<'_, StorageSettings>,
    active_files: tauri::State<'_, clipline_library::ActiveFileRegistry>,
    uploads: tauri::State<'_, crate::cloud_upload::TauriUploadState>,
    request: UploadClipCommandRequest,
) -> Result<CloudUploadResult, String> {
    let snapshot = state.cloud_settings_snapshot()?;
    let account =
        service_account_from_settings(&snapshot.document.cloud, snapshot.account_generation.get())
            .map_err(|error| error.to_string())?;
    if !account.snapshot.connected {
        return Err("connect to Clipline Cloud first".into());
    }

    let source = validate_upload_source(&storage, active_files.inner(), &request.path)?;
    let local_clip_id = clipline_library::local_clip_id_for_source(source.file_identity());
    if let Some(record) = existing_uploaded_record(
        &snapshot.document.cloud,
        Some(local_clip_id.as_str()),
        &request.path,
    ) {
        return Ok(CloudUploadResult {
            record,
            clip: None,
            local_deleted: false,
        });
    }

    let credential_target = snapshot
        .document
        .cloud
        .credential_target
        .as_deref()
        .ok_or_else(|| "connect to Clipline Cloud first".to_string())?;
    let credential = CloudCredential::new(read_credential(credential_target)?);
    let api = CloudApiBase::parse(&snapshot.document.cloud.host_url, true)
        .map_err(cloud_protocol_error)?;
    let owner = clipline_library::UploadAccountOwner::new(
        account.snapshot.account_key,
        account.snapshot.generation,
    );
    let visibility = request
        .visibility
        .as_deref()
        .map(normalize_cloud_visibility)
        .unwrap_or_else(|| snapshot.document.cloud.default_visibility.clone());
    let intent = clipline_library::UploadIntent {
        title: request.title,
        description: request.description,
        visibility,
        audio_track_ids: request.audio_track_ids,
        delete_local_after_upload: snapshot.document.cloud.delete_local_after_upload,
    };
    let handle = uploads
        .service()
        .start(clipline_library::UploadStartRequest {
            endpoint: clipline_library::UploadEndpoint::new(owner, api, credential),
            source,
            intent,
        })
        .map_err(|error| error.to_string())?;
    let completion = handle.wait().await;
    let record = completion.record.ok_or_else(|| {
        format!(
            "cloud upload ownership changed before completion ({:?})",
            completion.outcome
        )
    })?;
    let local_deleted = record.local_deleted;
    Ok(CloudUploadResult {
        record: crate::cloud_upload::cloud_record_from_upload(&record),
        clip: None,
        local_deleted,
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

fn cloud_protocol_error(error: CloudProtocolError) -> String {
    error.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use clipline_test_utils::TestDir;

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

    fn upload_record(
        local_clip_id: &str,
        path: &str,
        upload_status: &str,
        updated_at_unix: u64,
    ) -> CloudUploadRecord {
        CloudUploadRecord {
            local_clip_id: local_clip_id.into(),
            client_clip_id: None,
            upload_generation: None,
            path: path.into(),
            remote_clip_id: None,
            remote_url: None,
            visibility: "private".into(),
            upload_status: upload_status.into(),
            error: None,
            updated_at_unix,
        }
    }
}
