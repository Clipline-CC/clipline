//! Process-owned Clipline Cloud runtime for the native shell.

use std::collections::{BTreeMap, HashMap};
#[cfg(windows)]
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[cfg(windows)]
use clipline_library::cache::AvailableSpacePort;
use clipline_library::cache::{
    CloudAssetRequest, CloudCache, CloudCacheError, CloudCancellation, CloudMediaLease,
};
use clipline_library::cache_identity::{CloudAccountFence, CloudAssetKey, CloudAssetKind};
use clipline_library::cloud::protocol::CloudApiBase;
#[cfg(windows)]
use clipline_library::cloud::settings::{
    cloud_cache_account_fence_from_service_account, SettingsAccountPublicationGuard,
    SettingsCloudAccountPort,
};
#[cfg(windows)]
use clipline_library::http::{ReqwestAssetDownload, ReqwestCloudTransport};
#[cfg(windows)]
use clipline_library::ports::PortError;
use clipline_library::ports::{
    CloudAccountPort, CloudCancellationFuture, CloudCredential, CloudCredentialPort,
    CloudRequestFence, CloudTransport,
};
use clipline_library::{
    CatalogCloudPreferences, CatalogEffect, CatalogItemIdentity, CatalogOperationOwner,
    CatalogResult, CatalogUploadVisibility, CloudCatalogOwner, CloudListQuery, CloudMediaLeaseId,
    CloudReviewMediaOwner, CloudReviewMediaRequest, CloudService, CloudServiceError,
    CloudThumbnailDescriptor, CloudThumbnailOwner, CloudThumbnailRequest, CloudWorkToken,
    ExpectedResultOwner, ForegroundGeneration, PosterStatus, PreparedCloudReviewMedia,
    UploadAccountOwner, UploadEndpoint, WindowAttachmentGeneration, WindowWorkToken,
    MAX_CATALOG_PAGE_ROWS, MAX_CATALOG_STRING_BYTES, MAX_FOREGROUND_MESSAGE_BYTES,
};
#[cfg(windows)]
use clipline_settings::SettingsStore;
#[cfg(windows)]
use sha2::{Digest as _, Sha256};

use crate::catalog::{CatalogEffectExecutor, CatalogEffectHandler, OwnedCatalogResult};
use crate::cloud_profile::{
    bounded_cloud_profile_message, decode_bounded_avatar, CloudAvatarOutcome, CloudProfileOutcome,
    CloudProfileRequestPort, CloudRailSeed, CloudRailWork,
};
use crate::cloud_thumbnail::{
    decode_cached_cloud_thumbnail, CloudThumbnailDecodeOutcome, CloudThumbnailDecodePort,
};
use crate::cloud_upload::NativeUploadRuntime;

const CLOUD_RUNTIME_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_NATIVE_CLOUD_MEDIA_LEASES: usize = 4;

/// Reviewed platform boundary for Cloud browser and public-link actions.
/// Tests inject a recorder; production uses only safe `clipline-shell` wrappers.
pub trait NativeCloudPlatformPort: Send + Sync + 'static {
    fn open_browser(&self, url: &str, context: &str) -> Result<(), String>;
    fn copy_text(&self, text: &str, context: &str) -> Result<(), String>;
}

struct UnavailableCloudPlatform;

impl NativeCloudPlatformPort for UnavailableCloudPlatform {
    fn open_browser(&self, _url: &str, _context: &str) -> Result<(), String> {
        Err("native Cloud browser integration is unavailable".to_owned())
    }

    fn copy_text(&self, _text: &str, _context: &str) -> Result<(), String> {
        Err("native Cloud clipboard integration is unavailable".to_owned())
    }
}

#[cfg(windows)]
struct WindowsCloudPlatform;

#[cfg(windows)]
impl NativeCloudPlatformPort for WindowsCloudPlatform {
    fn open_browser(&self, url: &str, context: &str) -> Result<(), String> {
        clipline_shell::windows::shell_execute::open_browser_url(url, context)
            .map_err(|error| error.to_string())
    }

    fn copy_text(&self, text: &str, _context: &str) -> Result<(), String> {
        clipline_shell::windows::clipboard::copy_text_to_clipboard(text, 0)
            .map_err(|error| error.to_string())
    }
}

#[derive(Clone)]
struct WindowOwner {
    attachment: WindowAttachmentGeneration,
    foreground: ForegroundGeneration,
    cloud: Option<CloudCatalogOwner>,
}

struct CurrentPageRequest {
    token: CloudWorkToken,
    cancellation: Arc<RequestCancellation>,
}

struct CurrentAssetRequest<Owner> {
    owner: Owner,
    cancellation: Arc<AssetCancellation>,
}

#[derive(Default)]
struct FenceState {
    window: Option<WindowOwner>,
    page: Option<CurrentPageRequest>,
    thumbnails: HashMap<CloudThumbnailDescriptor, CurrentAssetRequest<CloudThumbnailOwner>>,
    media: Option<CurrentAssetRequest<CloudReviewMediaOwner>>,
}

#[derive(Default)]
struct FenceRegistry {
    state: Mutex<FenceState>,
}

impl FenceRegistry {
    fn attach(
        &self,
        attachment: WindowAttachmentGeneration,
        foreground: ForegroundGeneration,
        cloud: Option<CloudCatalogOwner>,
    ) -> Result<(), String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "native Cloud fence lock is unavailable".to_owned())?;
        cancel_all(&mut state);
        state.window = Some(WindowOwner {
            attachment,
            foreground,
            cloud,
        });
        Ok(())
    }

    fn detach(&self) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        cancel_all(&mut state);
        state.window = None;
    }

    fn begin_page(self: &Arc<Self>, token: &CloudWorkToken) -> Result<ExactCloudFence, String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "native Cloud fence lock is unavailable".to_owned())?;
        require_current_window(&state, token)?;
        cancel_page(&mut state);
        let cancellation = Arc::new(RequestCancellation::default());
        state.page = Some(CurrentPageRequest {
            token: token.clone(),
            cancellation: Arc::clone(&cancellation),
        });
        Ok(ExactCloudFence {
            registry: Arc::clone(self),
            token: token.clone(),
            cancellation,
        })
    }

    fn begin_thumbnail(
        self: &Arc<Self>,
        owner: &CloudThumbnailOwner,
    ) -> Result<ExactAssetFence<CloudThumbnailOwner>, String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "native Cloud fence lock is unavailable".to_owned())?;
        require_current_window(&state, &owner.token)?;
        if let Some(previous) = state.thumbnails.remove(&owner.descriptor) {
            previous.cancellation.cancel();
        } else if state.thumbnails.len() >= MAX_CATALOG_PAGE_ROWS {
            return Err(format!(
                "native Cloud thumbnail request capacity is full ({MAX_CATALOG_PAGE_ROWS})"
            ));
        }
        let cancellation = Arc::new(AssetCancellation::default());
        state.thumbnails.insert(
            owner.descriptor.clone(),
            CurrentAssetRequest {
                owner: owner.clone(),
                cancellation: Arc::clone(&cancellation),
            },
        );
        Ok(ExactAssetFence {
            registry: Arc::clone(self),
            owner: owner.clone(),
            cancellation,
            lane: AssetLane::Thumbnail,
            finish: finish_thumbnail,
        })
    }

    fn begin_media(
        self: &Arc<Self>,
        owner: &CloudReviewMediaOwner,
    ) -> Result<ExactAssetFence<CloudReviewMediaOwner>, String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "native Cloud fence lock is unavailable".to_owned())?;
        require_current_window(&state, &owner.token)?;
        cancel_media_request(&mut state);
        let cancellation = Arc::new(AssetCancellation::default());
        state.media = Some(CurrentAssetRequest {
            owner: owner.clone(),
            cancellation: Arc::clone(&cancellation),
        });
        Ok(ExactAssetFence {
            registry: Arc::clone(self),
            owner: owner.clone(),
            cancellation,
            lane: AssetLane::Media,
            finish: finish_media,
        })
    }

    fn cancel_media(&self, owner: &CloudReviewMediaOwner) -> Result<bool, String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "native Cloud fence lock is unavailable".to_owned())?;
        let exact = state
            .media
            .as_ref()
            .is_some_and(|request| request.owner == *owner);
        if exact {
            cancel_media_request(&mut state);
        }
        Ok(exact)
    }

    fn is_page_current(
        &self,
        token: &CloudWorkToken,
        cancellation: &Arc<RequestCancellation>,
    ) -> bool {
        if cancellation.is_canceled() {
            return false;
        }
        self.state.lock().ok().is_some_and(|state| {
            state.page.as_ref().is_some_and(|request| {
                request.token == *token && Arc::ptr_eq(&request.cancellation, cancellation)
            })
        })
    }

    fn is_window_current(&self, token: &CloudWorkToken) -> bool {
        self.state
            .lock()
            .ok()
            .is_some_and(|state| require_current_window(&state, token).is_ok())
    }

    fn run_if_window_current<T>(
        &self,
        token: &CloudWorkToken,
        operation: impl FnOnce() -> Result<T, String>,
    ) -> Result<Option<T>, String> {
        let state = self
            .state
            .lock()
            .map_err(|_| "native Cloud fence lock is unavailable".to_owned())?;
        if require_current_window(&state, token).is_err() {
            return Ok(None);
        }
        operation().map(Some)
    }

    fn is_thumbnail_current(
        &self,
        owner: &CloudThumbnailOwner,
        cancellation: &Arc<AssetCancellation>,
    ) -> bool {
        if cancellation.is_canceled() {
            return false;
        }
        self.state.lock().ok().is_some_and(|state| {
            state
                .thumbnails
                .get(&owner.descriptor)
                .is_some_and(|request| {
                    request.owner == *owner && Arc::ptr_eq(&request.cancellation, cancellation)
                })
        })
    }

    fn is_media_current(
        &self,
        owner: &CloudReviewMediaOwner,
        cancellation: &Arc<AssetCancellation>,
    ) -> bool {
        if cancellation.is_canceled() {
            return false;
        }
        self.state.lock().ok().is_some_and(|state| {
            state.media.as_ref().is_some_and(|request| {
                request.owner == *owner && Arc::ptr_eq(&request.cancellation, cancellation)
            })
        })
    }

    fn finish_thumbnail(&self, owner: &CloudThumbnailOwner, cancellation: &Arc<AssetCancellation>) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        let exact = state
            .thumbnails
            .get(&owner.descriptor)
            .is_some_and(|request| {
                request.owner == *owner && Arc::ptr_eq(&request.cancellation, cancellation)
            });
        if exact {
            state.thumbnails.remove(&owner.descriptor);
        }
    }

    fn finish_media(&self, owner: &CloudReviewMediaOwner, cancellation: &Arc<AssetCancellation>) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        let exact = state.media.as_ref().is_some_and(|request| {
            request.owner == *owner && Arc::ptr_eq(&request.cancellation, cancellation)
        });
        if exact {
            state.media = None;
        }
    }
}

fn finish_thumbnail(
    registry: &FenceRegistry,
    owner: &CloudThumbnailOwner,
    cancellation: &Arc<AssetCancellation>,
) {
    registry.finish_thumbnail(owner, cancellation);
}

fn finish_media(
    registry: &FenceRegistry,
    owner: &CloudReviewMediaOwner,
    cancellation: &Arc<AssetCancellation>,
) {
    registry.finish_media(owner, cancellation);
}

fn require_current_window(state: &FenceState, token: &CloudWorkToken) -> Result<(), String> {
    let current = state.window.as_ref().is_some_and(|window| {
        window.attachment == token.window.attachment
            && window.foreground == token.window.foreground
            && window.cloud.as_ref().is_some_and(|owner| {
                owner.account_key == token.account_key
                    && owner.account_generation == token.account_generation
            })
    });
    current
        .then_some(())
        .ok_or_else(|| "native Cloud work belongs to a stale window or account".to_owned())
}

fn cancel_page(state: &mut FenceState) {
    if let Some(request) = state.page.take() {
        request.cancellation.cancel();
    }
}

fn cancel_media_request(state: &mut FenceState) {
    if let Some(request) = state.media.take() {
        request.cancellation.cancel();
    }
}

fn cancel_all(state: &mut FenceState) {
    cancel_page(state);
    cancel_media_request(state);
    for (_, request) in state.thumbnails.drain() {
        request.cancellation.cancel();
    }
}

#[derive(Default)]
struct AssetCancellation {
    cache: CloudCancellation,
    registry: Mutex<()>,
}

impl AssetCancellation {
    fn cancel(&self) {
        let _registry = self
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.cache.cancel();
    }

    fn is_canceled(&self) -> bool {
        use clipline_library::cache::CancellationProbe as _;
        self.cache.is_cancelled()
    }

    fn register_if_current<T>(
        &self,
        operation: impl FnOnce() -> Result<T, String>,
    ) -> Result<T, String> {
        let _registry = self
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.is_canceled() {
            return Err("native Cloud asset work was canceled".to_owned());
        }
        operation()
    }
}

#[derive(Clone)]
enum AssetLane {
    Thumbnail,
    Media,
}

struct ExactAssetFence<Owner> {
    registry: Arc<FenceRegistry>,
    owner: Owner,
    cancellation: Arc<AssetCancellation>,
    lane: AssetLane,
    finish: fn(&FenceRegistry, &Owner, &Arc<AssetCancellation>),
}

impl<Owner> ExactAssetFence<Owner> {
    fn cancellation(&self) -> &CloudCancellation {
        &self.cancellation.cache
    }

    fn register_if_current<T>(
        &self,
        operation: impl FnOnce() -> Result<T, String>,
    ) -> Result<T, String> {
        self.cancellation.register_if_current(operation)
    }
}

impl ExactAssetFence<CloudThumbnailOwner> {
    fn is_current(&self) -> bool {
        debug_assert!(matches!(self.lane, AssetLane::Thumbnail));
        self.registry
            .is_thumbnail_current(&self.owner, &self.cancellation)
    }
}

impl ExactAssetFence<CloudReviewMediaOwner> {
    fn is_current(&self) -> bool {
        debug_assert!(matches!(self.lane, AssetLane::Media));
        self.registry
            .is_media_current(&self.owner, &self.cancellation)
    }
}

impl<Owner> Drop for ExactAssetFence<Owner> {
    fn drop(&mut self) {
        (self.finish)(&self.registry, &self.owner, &self.cancellation);
    }
}

#[derive(Default)]
struct RequestCancellation {
    canceled: AtomicBool,
    notify: tokio::sync::Notify,
}

impl RequestCancellation {
    fn cancel(&self) {
        if !self.canceled.swap(true, Ordering::AcqRel) {
            self.notify.notify_waiters();
        }
    }

    fn is_canceled(&self) -> bool {
        self.canceled.load(Ordering::Acquire)
    }

    async fn cancelled(&self) {
        loop {
            let notified = self.notify.notified();
            if self.is_canceled() {
                return;
            }
            notified.await;
        }
    }
}

struct ExactCloudFence {
    registry: Arc<FenceRegistry>,
    token: CloudWorkToken,
    cancellation: Arc<RequestCancellation>,
}

impl CloudRequestFence for ExactCloudFence {
    fn is_current(&self, token: &CloudWorkToken) -> bool {
        self.token == *token && self.registry.is_page_current(token, &self.cancellation)
    }

    fn cancelled<'a>(&'a self, token: &'a CloudWorkToken) -> CloudCancellationFuture<'a> {
        Box::pin(async move {
            if self.token != *token {
                return;
            }
            self.cancellation.cancelled().await;
        })
    }
}

struct CurrentWindowCloudFence {
    registry: Arc<FenceRegistry>,
    token: CloudWorkToken,
}

impl CloudRequestFence for CurrentWindowCloudFence {
    fn is_current(&self, token: &CloudWorkToken) -> bool {
        self.token == *token && self.registry.is_window_current(token)
    }

    fn cancelled<'a>(&'a self, token: &'a CloudWorkToken) -> CloudCancellationFuture<'a> {
        Box::pin(async move {
            if self.token != *token || !self.registry.is_window_current(token) {
                return;
            }
            std::future::pending::<()>().await;
        })
    }
}

/// Exact cache instance and account publication fence used by one native Cloud
/// asset request. Providers retain cache clients process-wide while window
/// attachment and request cancellation remain owned by [`FenceRegistry`].
#[derive(Clone)]
pub struct NativeCloudCacheContext {
    cache: Arc<CloudCache>,
    account: CloudAccountFence,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NativeCloudCacheProviderError {
    #[error("native Cloud cache account is stale")]
    StaleAccount,
    #[error("native Cloud cache provider failed: {0}")]
    Failed(String),
}

impl NativeCloudCacheContext {
    #[must_use]
    pub fn new(cache: Arc<CloudCache>, account: CloudAccountFence) -> Self {
        Self { cache, account }
    }

    #[must_use]
    pub fn cache(&self) -> &Arc<CloudCache> {
        &self.cache
    }

    #[must_use]
    pub const fn account(&self) -> &CloudAccountFence {
        &self.account
    }
}

/// Injectable source of an account-fenced Cloud cache. Tests supply bounded
/// fake transports; the Windows adapter constructs the shipping rustls client.
pub trait NativeCloudCacheProvider: Send + Sync {
    fn cache_for(
        &self,
        token: &CloudWorkToken,
    ) -> Result<NativeCloudCacheContext, NativeCloudCacheProviderError>;
}

struct UnavailableCloudCacheProvider;

impl NativeCloudCacheProvider for UnavailableCloudCacheProvider {
    fn cache_for(
        &self,
        _token: &CloudWorkToken,
    ) -> Result<NativeCloudCacheContext, NativeCloudCacheProviderError> {
        Err(NativeCloudCacheProviderError::Failed(
            "native Cloud asset cache is unavailable".to_owned(),
        ))
    }
}

#[cfg(windows)]
struct WindowsAvailableSpace;

#[cfg(windows)]
impl AvailableSpacePort for WindowsAvailableSpace {
    fn available_bytes(&self, cache_root: &Path) -> Result<u64, CloudCacheError> {
        clipline_shell::windows::filesystem::available_space_bytes(cache_root)
            .map_err(|error| CloudCacheError::Io(error.to_string()))
    }
}

#[cfg(windows)]
pub struct WindowsNativeCloudCacheProvider {
    store: SettingsStore,
    accounts: SettingsCloudAccountPort,
    credentials: Arc<dyn CloudCredentialPort>,
    current: Mutex<Option<WindowsCloudCacheSlot>>,
}

#[cfg(windows)]
struct WindowsCloudCacheSlot {
    account: CloudAccountFence,
    credential_fingerprint: [u8; 32],
    cache: Arc<CloudCache>,
}

#[cfg(windows)]
impl WindowsNativeCloudCacheProvider {
    pub fn new(store: SettingsStore, credentials: Arc<dyn CloudCredentialPort>) -> Self {
        Self {
            accounts: SettingsCloudAccountPort::new(store.clone()),
            store,
            credentials,
            current: Mutex::new(None),
        }
    }
}

#[cfg(windows)]
impl NativeCloudCacheProvider for WindowsNativeCloudCacheProvider {
    fn cache_for(
        &self,
        token: &CloudWorkToken,
    ) -> Result<NativeCloudCacheContext, NativeCloudCacheProviderError> {
        let account = self
            .accounts
            .snapshot()
            .map_err(|error| NativeCloudCacheProviderError::Failed(error.to_string()))?;
        if !account.snapshot.connected
            || account.snapshot.account_key != token.account_key
            || account.snapshot.generation != token.account_generation
        {
            return Err(NativeCloudCacheProviderError::StaleAccount);
        }
        let fence = cloud_cache_account_fence_from_service_account(&account)
            .map_err(|error| NativeCloudCacheProviderError::Failed(error.to_string()))?;
        let target = account.credential_target.as_deref().ok_or_else(|| {
            NativeCloudCacheProviderError::Failed(
                "native Cloud credential target is unavailable".to_owned(),
            )
        })?;
        let mut current = self.current.lock().map_err(|_| {
            NativeCloudCacheProviderError::Failed(
                "native Cloud cache provider lock is unavailable".to_owned(),
            )
        })?;
        // Credential rotation can preserve every durable account field and
        // generation, so reading and fingerprinting precedes cache reuse. The
        // slot lock spans that read: otherwise an older concurrent read could
        // overwrite a cache already installed for a newer secret.
        let credential = self
            .credentials
            .read(target)
            .map_err(|error| NativeCloudCacheProviderError::Failed(error.to_string()))?;
        let credential_fingerprint: [u8; 32] = Sha256::digest(credential.expose()).into();
        if let Some(existing) = current.as_ref() {
            if existing.account == fence
                && existing.credential_fingerprint == credential_fingerprint
            {
                return Ok(NativeCloudCacheContext::new(
                    Arc::clone(&existing.cache),
                    fence,
                ));
            }
        }
        let download = ReqwestAssetDownload::new(&account.snapshot.host_url, credential.expose())
            .map_err(|error| NativeCloudCacheProviderError::Failed(error.to_string()))?;
        let root = self.store.profile().local_cache_dir().join("cloud-cache");
        let cache = Arc::new(
            CloudCache::open(
                root,
                Arc::new(download),
                Arc::new(WindowsAvailableSpace),
                Arc::new(SettingsAccountPublicationGuard::new(self.store.clone())),
            )
            .map_err(|error| NativeCloudCacheProviderError::Failed(error.to_string()))?,
        );
        *current = Some(WindowsCloudCacheSlot {
            account: fence.clone(),
            credential_fingerprint,
            cache: Arc::clone(&cache),
        });
        Ok(NativeCloudCacheContext::new(cache, fence))
    }
}

struct NativeCloudMediaEntry {
    owner: CloudReviewMediaOwner,
    path: String,
    lease: CloudMediaLease,
}

struct NativeCloudMediaState {
    next_id: Option<u64>,
    entries: BTreeMap<CloudMediaLeaseId, NativeCloudMediaEntry>,
}

impl Default for NativeCloudMediaState {
    fn default() -> Self {
        Self {
            next_id: Some(1),
            entries: BTreeMap::new(),
        }
    }
}

/// Process-owned bridge between cache acceptance and the live player. The
/// opaque lease is non-cloneable and leaves this registry only after an exact
/// owner, id, and path match.
#[derive(Clone)]
pub struct NativeCloudMediaRegistry {
    state: Arc<Mutex<NativeCloudMediaState>>,
    fences: Arc<FenceRegistry>,
}

impl NativeCloudMediaRegistry {
    fn new(fences: Arc<FenceRegistry>) -> Self {
        Self {
            state: Arc::new(Mutex::new(NativeCloudMediaState::default())),
            fences,
        }
    }

    fn register(
        &self,
        owner: CloudReviewMediaOwner,
        lease: CloudMediaLease,
    ) -> Result<PreparedCloudReviewMedia, String> {
        owner.validate_bounds().map_err(|error| error.to_string())?;
        let path = lease.path().display().to_string();
        let mut state = self
            .state
            .lock()
            .map_err(|_| "native Cloud media lease registry is unavailable".to_owned())?;
        if state.entries.len() >= MAX_NATIVE_CLOUD_MEDIA_LEASES {
            return Err(format!(
                "native Cloud media lease capacity is full ({MAX_NATIVE_CLOUD_MEDIA_LEASES})"
            ));
        }
        let raw_id = state
            .next_id
            .ok_or_else(|| "native Cloud media lease id space is exhausted".to_owned())?;
        let lease_id = CloudMediaLeaseId::new(raw_id).map_err(|error| error.to_string())?;
        let prepared = PreparedCloudReviewMedia::new(path.clone(), lease_id)
            .map_err(|error| error.to_string())?;
        state.next_id = raw_id.checked_add(1);
        state
            .entries
            .insert(lease_id, NativeCloudMediaEntry { owner, path, lease });
        Ok(prepared)
    }

    /// Cancel only the exact pending request and release any result that raced
    /// publication. A wrong owner cannot terminate or release newer work.
    pub fn cancel_media(&self, owner: &CloudReviewMediaOwner) -> Result<bool, String> {
        owner.validate_bounds().map_err(|error| error.to_string())?;
        let canceled = self.fences.cancel_media(owner)?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| "native Cloud media lease registry is unavailable".to_owned())?;
        state.entries.retain(|_, entry| entry.owner != *owner);
        Ok(canceled)
    }

    /// Idempotently release one prepared lease. Unknown and duplicate ids are
    /// already unowned, so they are successful no-ops.
    pub fn release_media(&self, lease_id: CloudMediaLeaseId) -> Result<(), String> {
        self.state
            .lock()
            .map_err(|_| "native Cloud media lease registry is unavailable".to_owned())?
            .entries
            .remove(&lease_id);
        Ok(())
    }

    /// Transfer the non-cloneable cache lease into the live player only when
    /// every controller-echoed value matches the retained entry.
    pub fn take_media(
        &self,
        owner: &CloudReviewMediaOwner,
        prepared: &PreparedCloudReviewMedia,
    ) -> Result<CloudMediaLease, String> {
        owner.validate_bounds().map_err(|error| error.to_string())?;
        prepared
            .validate_bounds()
            .map_err(|error| error.to_string())?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| "native Cloud media lease registry is unavailable".to_owned())?;
        let entry = state
            .entries
            .get(&prepared.lease_id)
            .ok_or_else(|| "native Cloud media lease is no longer available".to_owned())?;
        if entry.owner != *owner || entry.path != prepared.path {
            return Err("native Cloud media lease owner or path is stale".to_owned());
        }
        Ok(state
            .entries
            .remove(&prepared.lease_id)
            .expect("validated native Cloud media entry must remain present")
            .lease)
    }

    #[must_use]
    pub fn lease_count(&self) -> usize {
        self.state.lock().map_or(0, |state| state.entries.len())
    }

    fn clear(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.entries.clear();
        }
    }
}

struct CloudShared {
    accounts: Arc<dyn CloudAccountPort>,
    credentials: Arc<dyn CloudCredentialPort>,
    service: CloudService,
    fences: Arc<FenceRegistry>,
    cache_provider: Arc<dyn NativeCloudCacheProvider>,
    platform: Arc<dyn NativeCloudPlatformPort>,
    media: NativeCloudMediaRegistry,
    handle: tokio::runtime::Handle,
}

/// Long-lived Cloud owner. Window destruction only cancels its current fence;
/// HTTP clients, account state, and the Tokio runtime survive in tray mode.
pub struct NativeCloudRuntime {
    shared: Arc<CloudShared>,
    runtime: Option<tokio::runtime::Runtime>,
}

impl NativeCloudRuntime {
    #[cfg(windows)]
    pub fn open(store: SettingsStore) -> Result<Self, String> {
        let accounts = Arc::new(SettingsCloudAccountPort::new(store.clone()));
        let credentials: Arc<dyn CloudCredentialPort> = Arc::new(WindowsCloudCredentialPort);
        let transport = Arc::new(ReqwestCloudTransport::new().map_err(|error| error.to_string())?);
        let cache_provider = Arc::new(WindowsNativeCloudCacheProvider::new(
            store,
            Arc::clone(&credentials),
        ));
        Self::with_transport_cache_and_platform(
            accounts,
            credentials,
            transport,
            cache_provider,
            Arc::new(WindowsCloudPlatform),
        )
    }

    pub fn with_transport(
        accounts: Arc<dyn CloudAccountPort>,
        credentials: Arc<dyn CloudCredentialPort>,
        transport: Arc<dyn CloudTransport>,
    ) -> Result<Self, String> {
        Self::with_transport_and_cache_provider(
            accounts,
            credentials,
            transport,
            Arc::new(UnavailableCloudCacheProvider),
        )
    }

    pub fn with_transport_and_platform(
        accounts: Arc<dyn CloudAccountPort>,
        credentials: Arc<dyn CloudCredentialPort>,
        transport: Arc<dyn CloudTransport>,
        platform: Arc<dyn NativeCloudPlatformPort>,
    ) -> Result<Self, String> {
        Self::with_transport_cache_and_platform(
            accounts,
            credentials,
            transport,
            Arc::new(UnavailableCloudCacheProvider),
            platform,
        )
    }

    pub fn with_transport_and_cache_provider(
        accounts: Arc<dyn CloudAccountPort>,
        credentials: Arc<dyn CloudCredentialPort>,
        transport: Arc<dyn CloudTransport>,
        cache_provider: Arc<dyn NativeCloudCacheProvider>,
    ) -> Result<Self, String> {
        Self::with_transport_cache_and_platform(
            accounts,
            credentials,
            transport,
            cache_provider,
            Arc::new(UnavailableCloudPlatform),
        )
    }

    pub fn with_transport_cache_and_platform(
        accounts: Arc<dyn CloudAccountPort>,
        credentials: Arc<dyn CloudCredentialPort>,
        transport: Arc<dyn CloudTransport>,
        cache_provider: Arc<dyn NativeCloudCacheProvider>,
        platform: Arc<dyn NativeCloudPlatformPort>,
    ) -> Result<Self, String> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("clipline-cloud")
            .build()
            .map_err(|error| format!("start native Cloud runtime: {error}"))?;
        let service = CloudService::new(Arc::clone(&accounts), Arc::clone(&credentials), transport);
        let fences = Arc::new(FenceRegistry::default());
        let media = NativeCloudMediaRegistry::new(Arc::clone(&fences));
        let shared = Arc::new(CloudShared {
            accounts,
            credentials,
            service,
            fences,
            cache_provider,
            platform,
            media,
            handle: runtime.handle().clone(),
        });
        Ok(Self {
            shared,
            runtime: Some(runtime),
        })
    }

    pub fn account_context(
        &self,
    ) -> Result<(Option<CloudCatalogOwner>, CatalogCloudPreferences), String> {
        let account = self
            .shared
            .accounts
            .snapshot()
            .map_err(|error| error.to_string())?;
        let preferences = CatalogCloudPreferences {
            default_visibility: match account.snapshot.default_visibility.as_str() {
                "public" => CatalogUploadVisibility::Public,
                "unlisted" => CatalogUploadVisibility::Unlisted,
                _ => CatalogUploadVisibility::Private,
            },
            delete_local_after_upload: account.snapshot.delete_local_after_upload,
        };
        let owner = account.snapshot.connected.then_some(CloudCatalogOwner {
            account_key: account.snapshot.account_key,
            account_generation: account.snapshot.generation,
        });
        Ok((owner, preferences))
    }

    /// Rebuild credential-bearing upload authority from the current durable
    /// account and reject any catalog owner captured before replacement.
    pub fn upload_endpoint(&self, owner: &CloudCatalogOwner) -> Result<UploadEndpoint, String> {
        upload_endpoint_from_shared(&self.shared, owner)
    }

    #[must_use]
    pub fn runtime_handle(&self) -> tokio::runtime::Handle {
        self.shared.handle.clone()
    }

    pub fn effect_handler_with_uploads(
        &self,
        local: Arc<dyn CatalogEffectHandler>,
        uploads: Arc<NativeUploadRuntime>,
    ) -> Arc<dyn CatalogEffectHandler> {
        Arc::new(NativeCloudEffectHandler {
            cloud: Arc::clone(&self.shared),
            local,
            uploads: Some(uploads),
        })
    }

    pub fn profile_seed(&self, window: WindowWorkToken) -> Result<Option<CloudRailSeed>, String> {
        let account = self
            .shared
            .accounts
            .snapshot()
            .map_err(|error| error.to_string())?;
        if !account.snapshot.connected || account.credential_target.is_none() {
            return Ok(None);
        }
        let name = account
            .snapshot
            .display_name
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .or_else(|| {
                account
                    .snapshot
                    .username
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
            })
            .or_else(|| {
                account
                    .snapshot
                    .user_id
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
            })
            .unwrap_or("Cloud")
            .to_owned();
        CloudRailSeed::new(
            CloudWorkToken {
                window,
                account_key: account.snapshot.account_key,
                account_generation: account.snapshot.generation,
            },
            name,
        )
        .map(Some)
        .map_err(|error| error.to_string())
    }

    pub fn attach(
        &self,
        attachment: WindowAttachmentGeneration,
        foreground: ForegroundGeneration,
        owner: Option<CloudCatalogOwner>,
    ) -> Result<(), String> {
        self.shared.fences.attach(attachment, foreground, owner)
    }

    pub fn detach(&self) {
        self.shared.fences.detach();
    }

    pub fn effect_handler(
        &self,
        local: Arc<dyn CatalogEffectHandler>,
    ) -> Arc<dyn CatalogEffectHandler> {
        Arc::new(NativeCloudEffectHandler {
            cloud: Arc::clone(&self.shared),
            local,
            uploads: None,
        })
    }

    /// Cloneable process-owned media registry for the shell's guaranteed
    /// inline cancel/release/open path. It remains valid across window rebuilds.
    #[must_use]
    pub fn media_registry(&self) -> NativeCloudMediaRegistry {
        self.shared.media.clone()
    }

    /// Process-owned decoder input. The returned port reopens the exact cache
    /// hit under a fresh transient pin, so no path-only `PosterStatus` can
    /// become image authority.
    #[must_use]
    pub fn thumbnail_decoder(&self) -> Arc<dyn CloudThumbnailDecodePort> {
        Arc::new(NativeCloudThumbnailDecoder {
            cache_provider: Arc::clone(&self.shared.cache_provider),
        })
    }

    #[must_use]
    pub fn profile_port(&self) -> Arc<dyn CloudProfileRequestPort> {
        Arc::new(NativeCloudProfilePort {
            cloud: Arc::clone(&self.shared),
        })
    }

    pub fn shutdown(mut self) -> Result<(), String> {
        self.detach();
        self.shared.media.clear();
        let runtime = self
            .runtime
            .take()
            .ok_or_else(|| "native Cloud runtime is already shut down".to_owned())?;
        runtime.shutdown_timeout(CLOUD_RUNTIME_SHUTDOWN_TIMEOUT);
        Ok(())
    }
}

fn upload_endpoint_from_shared(
    cloud: &CloudShared,
    owner: &CloudCatalogOwner,
) -> Result<UploadEndpoint, String> {
    let account = cloud
        .accounts
        .snapshot()
        .map_err(|error| error.to_string())?;
    if !account.snapshot.connected
        || account.snapshot.account_key != owner.account_key
        || account.snapshot.generation != owner.account_generation
    {
        return Err("native upload belongs to a replaced Cloud account".into());
    }
    let target = account
        .credential_target
        .as_deref()
        .ok_or_else(|| "native Cloud credential target is unavailable".to_owned())?;
    let credential = cloud
        .credentials
        .read(target)
        .map_err(|error| error.to_string())?;
    let api =
        CloudApiBase::parse(&account.snapshot.host_url, true).map_err(|error| error.to_string())?;
    Ok(UploadEndpoint::new(
        UploadAccountOwner::new(owner.account_key.clone(), owner.account_generation),
        api,
        credential,
    ))
}

struct NativeCloudThumbnailDecoder {
    cache_provider: Arc<dyn NativeCloudCacheProvider>,
}

struct NativeCloudProfilePort {
    cloud: Arc<CloudShared>,
}

struct ExactCloudRailFence<'a> {
    registry: Arc<FenceRegistry>,
    work: &'a CloudRailWork,
}

impl CloudRequestFence for ExactCloudRailFence<'_> {
    fn is_current(&self, token: &CloudWorkToken) -> bool {
        self.work.token == *token
            && !self.work.is_cancelled()
            && self.registry.is_window_current(token)
    }

    fn cancelled<'a>(&'a self, token: &'a CloudWorkToken) -> CloudCancellationFuture<'a> {
        Box::pin(async move {
            if self.work.token != *token || !self.registry.is_window_current(token) {
                return;
            }
            self.work.cancellation().cancelled().await;
        })
    }
}

impl CloudProfileRequestPort for NativeCloudProfilePort {
    fn profile(&self, work: &CloudRailWork) -> CloudProfileOutcome {
        let fence = ExactCloudRailFence {
            registry: Arc::clone(&self.cloud.fences),
            work,
        };
        match self
            .cloud
            .handle
            .block_on(self.cloud.service.profile(work.token.clone(), &fence))
        {
            Ok(profile) if fence.is_current(&profile.token) => {
                CloudProfileOutcome::Ready(profile.value)
            }
            Ok(_) | Err(CloudServiceError::StaleWork | CloudServiceError::AccountChanged) => {
                CloudProfileOutcome::Stale
            }
            Err(error) => {
                CloudProfileOutcome::Failed(bounded_cloud_profile_message(error.to_string()))
            }
        }
    }

    fn avatar(&self, work: &CloudRailWork) -> CloudAvatarOutcome {
        let fence = ExactCloudRailFence {
            registry: Arc::clone(&self.cloud.fences),
            work,
        };
        match self
            .cloud
            .handle
            .block_on(self.cloud.service.avatar(work.token.clone(), &fence))
        {
            Ok(avatar) if fence.is_current(&avatar.token) => match avatar.value {
                Some(avatar) => match decode_bounded_avatar(&avatar) {
                    Ok(pixels) => CloudAvatarOutcome::Ready(pixels),
                    Err(error) => CloudAvatarOutcome::Failed(error.to_string()),
                },
                None => CloudAvatarOutcome::Missing,
            },
            Ok(_) | Err(CloudServiceError::StaleWork | CloudServiceError::AccountChanged) => {
                CloudAvatarOutcome::Stale
            }
            Err(error) => {
                CloudAvatarOutcome::Failed(bounded_cloud_profile_message(error.to_string()))
            }
        }
    }
}

impl CloudThumbnailDecodePort for NativeCloudThumbnailDecoder {
    fn decode(
        &self,
        owner: &CloudThumbnailOwner,
        cancellation: &CloudCancellation,
    ) -> CloudThumbnailDecodeOutcome {
        if owner.validate_bounds().is_err() {
            return CloudThumbnailDecodeOutcome::Failed(
                "Cloud thumbnail owner is invalid".to_owned(),
            );
        }
        let context = match self.cache_provider.cache_for(&owner.token) {
            Ok(context) => context,
            Err(NativeCloudCacheProviderError::StaleAccount) => {
                return CloudThumbnailDecodeOutcome::Stale
            }
            Err(NativeCloudCacheProviderError::Failed(error)) => {
                return CloudThumbnailDecodeOutcome::Failed(error)
            }
        };
        if context.account.account_key != owner.token.account_key
            || context.account.account_generation != owner.token.account_generation
        {
            return CloudThumbnailDecodeOutcome::Stale;
        }
        let asset = match cloud_asset_key(
            &owner.descriptor.item,
            CloudAssetKind::Thumbnail,
            owner.descriptor.version,
        ) {
            Ok(asset) => asset,
            Err(error) => return CloudThumbnailDecodeOutcome::Failed(error),
        };
        let cached = match context.cache.get(
            CloudAssetRequest {
                account: context.account.clone(),
                asset,
                expected_size_bytes: None,
            },
            cancellation,
        ) {
            Ok(Some(cached)) => cached,
            Ok(None) => return CloudThumbnailDecodeOutcome::Missing,
            Err(CloudCacheError::Canceled | CloudCacheError::StaleAccount) => {
                return CloudThumbnailDecodeOutcome::Stale
            }
            Err(error) => return CloudThumbnailDecodeOutcome::Failed(error.to_string()),
        };
        match decode_cached_cloud_thumbnail(&cached) {
            Ok(pixels) => CloudThumbnailDecodeOutcome::Ready { pixels },
            Err(error) if error.invalidates_cache() => {
                let message = error.to_string();
                match context
                    .cache
                    .invalidate_thumbnail(&context.account, cached, cancellation)
                {
                    Ok(()) => CloudThumbnailDecodeOutcome::Failed(message),
                    Err(CloudCacheError::Canceled | CloudCacheError::StaleAccount) => {
                        CloudThumbnailDecodeOutcome::Stale
                    }
                    Err(invalidation) => CloudThumbnailDecodeOutcome::Failed(format!(
                        "{message}; exact cache invalidation failed: {invalidation}"
                    )),
                }
            }
            Err(error) => CloudThumbnailDecodeOutcome::Failed(error.to_string()),
        }
    }
}

/// Ordered process-lifetime owner for the Cloud runtime and the catalog
/// workers that may be blocked inside it. Both normal quit and exceptional
/// shell drop use the same idempotent shutdown path.
pub struct CatalogCloudLifetime {
    cloud: Option<NativeCloudRuntime>,
    executor: Option<CatalogEffectExecutor>,
}

impl CatalogCloudLifetime {
    #[must_use]
    pub fn new(cloud: Option<NativeCloudRuntime>, executor: CatalogEffectExecutor) -> Self {
        Self {
            cloud,
            executor: Some(executor),
        }
    }

    #[must_use]
    pub fn cloud(&self) -> Option<&NativeCloudRuntime> {
        self.cloud.as_ref()
    }

    #[must_use]
    pub fn executor(&self) -> Option<&CatalogEffectExecutor> {
        self.executor.as_ref()
    }

    pub fn shutdown(&mut self) -> Result<(), String> {
        if let Some(cloud) = self.cloud.as_ref() {
            cloud.detach();
        }
        let mut first_error = None;
        if let Some(executor) = self.executor.take() {
            if let Err(error) = executor.shutdown() {
                first_error = Some(error);
            }
        }
        if let Some(cloud) = self.cloud.take() {
            if let Err(error) = cloud.shutdown() {
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    pub fn shutdown_executor(&mut self) -> Result<(), String> {
        self.cloud.as_ref().map(NativeCloudRuntime::detach);
        self.executor
            .take()
            .map_or(Ok(()), CatalogEffectExecutor::shutdown)
    }

    pub fn shutdown_cloud(&mut self) -> Result<(), String> {
        self.cloud
            .take()
            .map_or(Ok(()), NativeCloudRuntime::shutdown)
    }
}

impl Drop for CatalogCloudLifetime {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

struct NativeCloudEffectHandler {
    cloud: Arc<CloudShared>,
    local: Arc<dyn CatalogEffectHandler>,
    uploads: Option<Arc<NativeUploadRuntime>>,
}

impl CatalogEffectHandler for NativeCloudEffectHandler {
    fn execute(&self, effect: CatalogEffect) -> Result<Option<OwnedCatalogResult>, String> {
        effect
            .validate_bounds()
            .map_err(|error| error.to_string())?;
        match effect {
            CatalogEffect::RefreshCloud {
                token,
                revision,
                page,
                query,
            } => self.refresh_cloud(token, revision, page, query),
            CatalogEffect::LoadCloudThumbnail { request } => self.load_thumbnail(request),
            CatalogEffect::PrepareCloudReviewMedia { request } => self.prepare_media(request),
            CatalogEffect::CancelCloudReviewMedia { owner } => {
                self.cloud.media.cancel_media(&owner)?;
                Ok(None)
            }
            CatalogEffect::ReleaseCloudReviewMedia { lease_id } => {
                self.cloud.media.release_media(lease_id)?;
                Ok(None)
            }
            CatalogEffect::OpenInBrowser { token, item } => self.open_cloud_clip_page(token, item),
            CatalogEffect::OpenCloudProfile { token } => self.open_cloud_profile(token),
            CatalogEffect::CopyPublicLink {
                token,
                item: _,
                url,
            } => self.copy_public_link(token, url),
            CatalogEffect::StartUpload {
                token,
                owner,
                target,
                options,
            } => self.start_upload(token, owner, target, options),
            other => self.local.execute(other),
        }
    }
}

impl NativeCloudEffectHandler {
    fn start_upload(
        &self,
        token: WindowWorkToken,
        owner: CloudCatalogOwner,
        target: clipline_library::ResolvedLocalClip,
        options: clipline_library::CatalogUploadOptions,
    ) -> Result<Option<OwnedCatalogResult>, String> {
        let Some(uploads) = self.uploads.as_ref() else {
            return Err("native Cloud upload service is unavailable".into());
        };
        let work = CloudWorkToken {
            window: token,
            account_key: owner.account_key.clone(),
            account_generation: owner.account_generation,
        };
        if !self.cloud.fences.is_window_current(&work) {
            return Ok(None);
        }
        let result = upload_endpoint_from_shared(&self.cloud, &owner)
            .and_then(|endpoint| uploads.start(&self.cloud.handle, endpoint, &target, options));
        match result {
            Ok(_) => Ok(None),
            Err(_error) if !self.cloud.fences.is_window_current(&work) => Ok(None),
            Err(error) => Ok(Some(foreground_feedback(
                token,
                format!("Start Cloud upload: {error}"),
            ))),
        }
    }
    fn open_cloud_profile(
        &self,
        token: CloudWorkToken,
    ) -> Result<Option<OwnedCatalogResult>, String> {
        let fence = CurrentWindowCloudFence {
            registry: Arc::clone(&self.cloud.fences),
            token: token.clone(),
        };
        let effect = match self
            .cloud
            .service
            .open_profile_effect(token.clone(), &fence)
        {
            Ok(effect) => effect.value,
            Err(CloudServiceError::StaleWork | CloudServiceError::AccountChanged) => {
                return Ok(None);
            }
            Err(error) => {
                return Ok(Some(foreground_feedback(
                    token.window,
                    format!("Open Cloud profile: {error}"),
                )));
            }
        };
        match self.cloud.fences.run_if_window_current(&token, || {
            self.cloud
                .platform
                .open_browser(&effect.url, &effect.context)
        }) {
            Ok(Some(())) | Ok(None) => Ok(None),
            Err(error) => Ok(Some(foreground_feedback(
                token.window,
                format!("Open Cloud profile: {error}"),
            ))),
        }
    }

    fn open_cloud_clip_page(
        &self,
        token: CloudWorkToken,
        item: CatalogItemIdentity,
    ) -> Result<Option<OwnedCatalogResult>, String> {
        let CatalogItemIdentity::Cloud { remote_clip_id, .. } = item else {
            return Err("native Cloud browser effect has a local identity".to_owned());
        };
        let fence = CurrentWindowCloudFence {
            registry: Arc::clone(&self.cloud.fences),
            token: token.clone(),
        };
        let effect = match self.cloud.service.open_clip_effect(
            token.clone(),
            &fence,
            remote_clip_id.as_str(),
        ) {
            Ok(effect) => effect.value,
            Err(CloudServiceError::StaleWork | CloudServiceError::AccountChanged) => {
                return Ok(None);
            }
            Err(error) => {
                return Ok(Some(foreground_feedback(
                    token.window,
                    format!("Open Cloud clip page: {error}"),
                )));
            }
        };
        match self.cloud.fences.run_if_window_current(&token, || {
            self.cloud
                .platform
                .open_browser(&effect.url, &effect.context)
        }) {
            Ok(Some(())) | Ok(None) => Ok(None),
            Err(error) => Ok(Some(foreground_feedback(
                token.window,
                format!("Open Cloud clip page: {error}"),
            ))),
        }
    }

    fn copy_public_link(
        &self,
        token: CloudWorkToken,
        url: String,
    ) -> Result<Option<OwnedCatalogResult>, String> {
        match self.cloud.fences.run_if_window_current(&token, || {
            self.cloud
                .platform
                .copy_text(&url, "copy Cloud public link")
        }) {
            Ok(Some(())) => Ok(Some(foreground_feedback(
                token.window,
                "Cloud public link copied".to_owned(),
            ))),
            Ok(None) => Ok(None),
            Err(error) => Ok(Some(foreground_feedback(
                token.window,
                format!("Copy Cloud public link: {error}"),
            ))),
        }
    }

    fn refresh_cloud(
        &self,
        token: CloudWorkToken,
        revision: clipline_library::CatalogRevision,
        page: clipline_library::CloudPageNumber,
        query: String,
    ) -> Result<Option<OwnedCatalogResult>, String> {
        let account = self
            .cloud
            .accounts
            .snapshot()
            .map_err(|error| error.to_string())?;
        if account.snapshot.account_key != token.account_key
            || account.snapshot.generation != token.account_generation
        {
            return Err("native Cloud work belongs to a replaced account".into());
        }
        let fence = self.cloud.fences.begin_page(&token)?;
        let query = query.trim();
        let request = CloudListQuery {
            query: (!query.is_empty()).then(|| query.to_owned()),
            ..CloudListQuery::default()
        };
        let result = self.cloud.handle.block_on(self.cloud.service.list_page(
            token.clone(),
            &fence,
            revision,
            page,
            request,
        ));
        match result {
            Ok(completion) => Ok(Some(OwnedCatalogResult {
                result: CatalogResult::CloudPage(completion),
                expected: ExpectedResultOwner::Cloud(token),
            })),
            Err(CloudServiceError::StaleWork) => Ok(None),
            Err(error) => {
                let owner = CatalogOperationOwner::CloudRefresh {
                    token,
                    revision,
                    page,
                };
                Ok(Some(OwnedCatalogResult {
                    result: CatalogResult::OperationFailed {
                        owner: owner.clone(),
                        message: bounded_message(error.to_string()),
                    },
                    expected: ExpectedResultOwner::Operation(owner),
                }))
            }
        }
    }

    fn load_thumbnail(
        &self,
        request: CloudThumbnailRequest,
    ) -> Result<Option<OwnedCatalogResult>, String> {
        let owner = request.owner;
        let fence = self.cloud.fences.begin_thumbnail(&owner)?;
        let context = match self.cache_context(&owner.token) {
            Ok(context) => context,
            Err(NativeCloudCacheProviderError::StaleAccount) => return Ok(None),
            Err(NativeCloudCacheProviderError::Failed(error)) => {
                return self.thumbnail_failure(owner, error, &fence)
            }
        };
        let asset = cloud_asset_key(
            &owner.descriptor.item,
            CloudAssetKind::Thumbnail,
            owner.descriptor.version,
        )?;
        let cache_request = CloudAssetRequest {
            account: context.account.clone(),
            asset,
            expected_size_bytes: None,
        };
        let result = context.cache.get(cache_request, fence.cancellation());
        if !fence.is_current() {
            return Ok(None);
        }
        match result {
            Ok(Some(cached)) => Ok(Some(OwnedCatalogResult {
                result: CatalogResult::CloudThumbnail {
                    owner: owner.clone(),
                    status: PosterStatus::Ready {
                        path: cached.path().display().to_string(),
                    },
                },
                expected: ExpectedResultOwner::CloudThumbnail(owner),
            })),
            Ok(None) => Ok(Some(OwnedCatalogResult {
                result: CatalogResult::CloudThumbnail {
                    owner: owner.clone(),
                    status: PosterStatus::Missing,
                },
                expected: ExpectedResultOwner::CloudThumbnail(owner),
            })),
            Err(CloudCacheError::Canceled | CloudCacheError::StaleAccount) => Ok(None),
            Err(error) => self.thumbnail_failure(owner, error.to_string(), &fence),
        }
    }

    fn thumbnail_failure(
        &self,
        owner: CloudThumbnailOwner,
        error: String,
        fence: &ExactAssetFence<CloudThumbnailOwner>,
    ) -> Result<Option<OwnedCatalogResult>, String> {
        if !fence.is_current() {
            return Ok(None);
        }
        Ok(Some(OwnedCatalogResult {
            result: CatalogResult::CloudThumbnail {
                owner: owner.clone(),
                status: PosterStatus::Failed {
                    message: bounded_thumbnail_message(error),
                },
            },
            expected: ExpectedResultOwner::CloudThumbnail(owner),
        }))
    }

    fn prepare_media(
        &self,
        request: CloudReviewMediaRequest,
    ) -> Result<Option<OwnedCatalogResult>, String> {
        let owner = request.owner;
        let fence = self.cloud.fences.begin_media(&owner)?;
        let operation = CatalogOperationOwner::CloudReviewMedia {
            owner: owner.clone(),
        };
        let context = match self.cache_context(&owner.token) {
            Ok(context) => context,
            Err(NativeCloudCacheProviderError::StaleAccount) => return Ok(None),
            Err(NativeCloudCacheProviderError::Failed(error)) => {
                return self.media_failure(operation, error, &fence)
            }
        };
        let asset = cloud_asset_key(&owner.item, CloudAssetKind::Media, request.version)?;
        let cache_request = CloudAssetRequest {
            account: context.account.clone(),
            asset,
            expected_size_bytes: request.expected_size_bytes,
        };
        let cached = match context.cache.get(cache_request, fence.cancellation()) {
            Ok(Some(cached)) => cached,
            Ok(None) => {
                return self.media_failure(
                    operation,
                    "Cloud review media is no longer available".to_owned(),
                    &fence,
                );
            }
            Err(CloudCacheError::Canceled | CloudCacheError::StaleAccount) => return Ok(None),
            Err(error) => return self.media_failure(operation, error.to_string(), &fence),
        };
        if !fence.is_current() {
            return Ok(None);
        }
        let lease = match context
            .cache
            .accept_media(&context.account, cached, fence.cancellation())
        {
            Ok(lease) => lease,
            Err(CloudCacheError::Canceled | CloudCacheError::StaleAccount) => return Ok(None),
            Err(error) => return self.media_failure(operation, error.to_string(), &fence),
        };
        if !fence.is_current() {
            return Ok(None);
        }
        let prepared =
            match fence.register_if_current(|| self.cloud.media.register(owner.clone(), lease)) {
                Ok(prepared) => prepared,
                Err(_error) if !fence.is_current() => return Ok(None),
                Err(error) => return self.media_failure(operation, error, &fence),
            };
        if !fence.is_current() {
            self.cloud.media.release_media(prepared.lease_id)?;
            return Ok(None);
        }
        Ok(Some(OwnedCatalogResult {
            result: CatalogResult::CloudReviewMediaPrepared {
                owner: owner.clone(),
                media: prepared,
            },
            expected: ExpectedResultOwner::CloudReviewMedia(owner),
        }))
    }

    fn media_failure(
        &self,
        owner: CatalogOperationOwner,
        error: String,
        fence: &ExactAssetFence<CloudReviewMediaOwner>,
    ) -> Result<Option<OwnedCatalogResult>, String> {
        if !fence.is_current() {
            return Ok(None);
        }
        Ok(Some(OwnedCatalogResult {
            result: CatalogResult::OperationFailed {
                owner: owner.clone(),
                message: bounded_message(error),
            },
            expected: ExpectedResultOwner::Operation(owner),
        }))
    }

    fn cache_context(
        &self,
        token: &CloudWorkToken,
    ) -> Result<NativeCloudCacheContext, NativeCloudCacheProviderError> {
        let context = self.cloud.cache_provider.cache_for(token)?;
        if context.account.account_key != token.account_key
            || context.account.account_generation != token.account_generation
        {
            return Err(NativeCloudCacheProviderError::StaleAccount);
        }
        Ok(context)
    }
}

fn foreground_feedback(token: WindowWorkToken, message: String) -> OwnedCatalogResult {
    OwnedCatalogResult {
        result: CatalogResult::ForegroundFeedback {
            token,
            message: bounded_message(message),
        },
        expected: ExpectedResultOwner::Window(token),
    }
}

fn cloud_asset_key(
    item: &CatalogItemIdentity,
    kind: CloudAssetKind,
    version: u64,
) -> Result<CloudAssetKey, String> {
    let CatalogItemIdentity::Cloud { remote_clip_id, .. } = item else {
        return Err("native Cloud asset identity is not remote".to_owned());
    };
    CloudAssetKey::new(remote_clip_id.as_str(), kind, version).map_err(|error| error.to_string())
}

fn bounded_message(mut message: String) -> String {
    truncate_message(&mut message, MAX_FOREGROUND_MESSAGE_BYTES);
    message
}

fn bounded_thumbnail_message(mut message: String) -> String {
    truncate_message(&mut message, MAX_CATALOG_STRING_BYTES);
    message
}

fn truncate_message(message: &mut String, maximum: usize) {
    if message.len() <= maximum {
        return;
    }
    let mut end = maximum;
    while end != 0 && !message.is_char_boundary(end) {
        end -= 1;
    }
    message.truncate(end);
}

#[cfg(windows)]
struct WindowsCloudCredentialPort;

#[cfg(windows)]
impl CloudCredentialPort for WindowsCloudCredentialPort {
    fn read(&self, target: &str) -> Result<CloudCredential, PortError> {
        clipline_shell::windows::credential::CredentialStore::new("cloud token")
            .read(target)
            .map(CloudCredential::new)
            .map_err(|error| PortError::new(error.to_string()))
    }
}
