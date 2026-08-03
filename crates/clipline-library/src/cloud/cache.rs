//! Bounded, account-fenced Cloud asset cache.
//!
//! The cache is synchronous by design: adapters run it on their blocking
//! executor while their [`DownloadPort`] streams network chunks into the
//! bounded sink. All destructive filesystem operations are made through a
//! retained [`clipline_shell::DirectoryAuthority`] and an exact file identity.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clipline_shell::{opened_file_identity, DirectoryAuthority, FileIdentity};

use super::cache_identity::{
    CloudAccountFence, CloudAssetKey, CloudAssetKind, CloudCacheNamespace,
};

pub const CLOUD_THUMBNAIL_MAX_BYTES: u64 = 10 * 1024 * 1024;
pub const CLOUD_MEDIA_MAX_BYTES: u64 = 4 * 1024 * 1024 * 1024;
pub const CLOUD_MEDIA_SIZE_SLACK_BYTES: u64 = 64 * 1024 * 1024;
pub const CLOUD_CACHE_QUOTA_BYTES: u64 = 10 * 1024 * 1024 * 1024;
pub const CLOUD_CACHE_FREE_SPACE_FLOOR_BYTES: u64 = 2 * 1024 * 1024 * 1024;
pub const CLOUD_CACHE_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);
pub const CLOUD_CACHE_TEMP_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);
pub const MAX_CONCURRENT_CLOUD_ASSET_DOWNLOADS: usize = 4;
pub const MAX_PENDING_CLOUD_ASSET_FLIGHTS: usize = 64;
pub const MAX_CLOUD_CACHE_SCAN_ENTRIES: usize = 100_000;
pub const MAX_CLOUD_CACHE_MARKER_BYTES: u64 = 64;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloudAssetRequest {
    pub account: CloudAccountFence,
    pub asset: CloudAssetKey,
    pub expected_size_bytes: Option<u64>,
}

impl CloudAssetRequest {
    #[must_use]
    pub fn hard_limit_bytes(&self) -> u64 {
        match self.asset.kind() {
            CloudAssetKind::Thumbnail => CLOUD_THUMBNAIL_MAX_BYTES,
            CloudAssetKind::Media => self
                .expected_size_bytes
                .filter(|size| *size > 0)
                .map(|size| {
                    size.saturating_mul(2)
                        .saturating_add(CLOUD_MEDIA_SIZE_SLACK_BYTES)
                })
                .unwrap_or(CLOUD_MEDIA_MAX_BYTES)
                .min(CLOUD_MEDIA_MAX_BYTES),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadStatus {
    Found,
    Missing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DownloadReceipt {
    pub status: DownloadStatus,
    pub advertised_size_bytes: Option<u64>,
}

/// Cooperative cancellation checked by transport implementations between chunks.
pub trait CancellationProbe: Send + Sync {
    fn is_cancelled(&self) -> bool;
}

#[derive(Debug, Default)]
struct CloudCancellationState {
    canceled: AtomicBool,
    publication: Mutex<()>,
}

#[derive(Debug, Clone, Default)]
pub struct CloudCancellation(Arc<CloudCancellationState>);

impl CloudCancellation {
    pub fn cancel(&self) {
        let _publication = self
            .0
            .publication
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.0.canceled.store(true, Ordering::Release);
    }

    fn run_if_current<T>(
        &self,
        operation: impl FnOnce() -> Result<T, CloudCacheError>,
    ) -> Result<T, CloudCacheError> {
        let _publication = self
            .0
            .publication
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.is_cancelled() {
            return Err(CloudCacheError::Canceled);
        }
        operation()
    }
}

impl CancellationProbe for CloudCancellation {
    fn is_cancelled(&self) -> bool {
        self.0.canceled.load(Ordering::Acquire)
    }
}

/// Bounded file sink supplied to a download transport.
pub struct DownloadSink<'a> {
    file: &'a mut File,
    written: u64,
    limit: u64,
}

impl DownloadSink<'_> {
    pub fn write_chunk(&mut self, bytes: &[u8]) -> Result<(), CloudCacheError> {
        let written = self
            .written
            .checked_add(bytes.len() as u64)
            .ok_or(CloudCacheError::TooLarge { limit: self.limit })?;
        if written > self.limit {
            return Err(CloudCacheError::TooLarge { limit: self.limit });
        }
        self.file
            .write_all(bytes)
            .map_err(|error| CloudCacheError::Io(error.to_string()))?;
        self.written = written;
        Ok(())
    }

    #[must_use]
    pub const fn written(&self) -> u64 {
        self.written
    }
}

pub trait DownloadPort: Send + Sync {
    fn download(
        &self,
        request: &CloudAssetRequest,
        sink: &mut DownloadSink<'_>,
        cancellation: &dyn CancellationProbe,
    ) -> Result<DownloadReceipt, CloudCacheError>;
}

pub trait AvailableSpacePort: Send + Sync {
    fn available_bytes(&self, cache_root: &Path) -> Result<u64, CloudCacheError>;
}

/// Serializes the final publication with account replacement.
///
/// Implementations must hold the same account-state exclusion used by connect,
/// disconnect, host, user, and credential changes while `publication` runs.
pub trait AccountPublicationGuard: Send + Sync {
    fn is_current(&self, account: &CloudAccountFence) -> bool;

    fn publish_if_current(
        &self,
        account: &CloudAccountFence,
        publication: &mut dyn FnMut() -> Result<(), CloudCacheError>,
    ) -> Result<(), CloudCacheError>;
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CloudCacheError {
    #[error("cloud account changed while asset work was in flight")]
    StaleAccount,
    #[error("cloud asset work was canceled")]
    Canceled,
    #[error("cloud asset flight capacity is full")]
    Capacity,
    #[error("cloud asset is too large (limit {limit} bytes)")]
    TooLarge { limit: u64 },
    #[error("cloud cache cannot reserve {requested} bytes without evicting protected data")]
    InsufficientSpace { requested: u64 },
    #[error("cloud cache contains too many owned entries")]
    ScanLimit,
    #[error("cloud cache asset is invalid: {0}")]
    InvalidAsset(String),
    #[error("cloud cache I/O failed: {0}")]
    Io(String),
    #[error("cloud download failed: {0}")]
    Download(String),
    #[error("cloud cache internal state failed: {0}")]
    Internal(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct FlightKey {
    account: CloudAccountFence,
    asset: CloudAssetKey,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ProtectionKey {
    path: PathBuf,
    identity: FileIdentity,
}

#[derive(Debug, Default)]
struct ProtectionCounts {
    transient: usize,
    playback: usize,
}

#[derive(Debug, Default)]
struct AccountingState {
    protection: HashMap<ProtectionKey, ProtectionCounts>,
}

struct Flight {
    result: Mutex<Option<Result<Option<CachedCloudAsset>, CloudCacheError>>>,
    ready: Condvar,
}

impl Flight {
    fn new() -> Self {
        Self {
            result: Mutex::new(None),
            ready: Condvar::new(),
        }
    }

    fn publish(&self, result: Result<Option<CachedCloudAsset>, CloudCacheError>) {
        let mut slot = self
            .result
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *slot = Some(result);
        self.ready.notify_all();
    }

    fn wait(
        &self,
        cancellation: &CloudCancellation,
    ) -> Result<Option<CachedCloudAsset>, CloudCacheError> {
        let mut slot = self
            .result
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            if let Some(result) = slot.as_ref() {
                return cancellation.run_if_current(|| result.clone());
            }
            if cancellation.is_cancelled() {
                return Err(CloudCacheError::Canceled);
            }
            let waited = self
                .ready
                .wait_timeout(slot, Duration::from_millis(20))
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            slot = waited.0;
        }
    }
}

#[derive(Default)]
struct RuntimeState {
    flights: Mutex<HashMap<FlightKey, Arc<Flight>>>,
    accounting: Mutex<AccountingState>,
}

impl RuntimeState {
    fn add_transient(self: &Arc<Self>, path: PathBuf, identity: FileIdentity) -> TransientCachePin {
        let key = ProtectionKey { path, identity };
        let mut accounting = self
            .accounting
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let counts = accounting.protection.entry(key.clone()).or_default();
        counts.transient = counts.transient.saturating_add(1);
        drop(accounting);
        TransientCachePin {
            runtime: Arc::clone(self),
            key,
            armed: true,
        }
    }

    fn release(&self, key: &ProtectionKey, playback: bool) {
        let mut accounting = self
            .accounting
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(counts) = accounting.protection.get_mut(key) else {
            return;
        };
        if playback {
            counts.playback = counts.playback.saturating_sub(1);
        } else {
            counts.transient = counts.transient.saturating_sub(1);
        }
        if counts.transient == 0 && counts.playback == 0 {
            accounting.protection.remove(key);
        }
    }
}

struct TransientCachePin {
    runtime: Arc<RuntimeState>,
    key: ProtectionKey,
    armed: bool,
}

impl Clone for TransientCachePin {
    fn clone(&self) -> Self {
        let mut accounting = self
            .runtime
            .accounting
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let counts = accounting.protection.entry(self.key.clone()).or_default();
        counts.transient = counts.transient.saturating_add(1);
        drop(accounting);
        Self {
            runtime: Arc::clone(&self.runtime),
            key: self.key.clone(),
            armed: true,
        }
    }
}

impl Drop for TransientCachePin {
    fn drop(&mut self) {
        if self.armed {
            self.runtime.release(&self.key, false);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CachedAssetMetadata {
    account: CloudAccountFence,
    asset: CloudAssetKey,
    path: PathBuf,
    identity: FileIdentity,
    bytes: u64,
}

pub struct CachedCloudAsset {
    metadata: CachedAssetMetadata,
    pin: TransientCachePin,
}

impl Clone for CachedCloudAsset {
    fn clone(&self) -> Self {
        Self {
            metadata: self.metadata.clone(),
            pin: self.pin.clone(),
        }
    }
}

impl std::fmt::Debug for CachedCloudAsset {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.metadata.fmt(formatter)
    }
}

impl CachedCloudAsset {
    #[must_use]
    pub fn account(&self) -> &CloudAccountFence {
        &self.metadata.account
    }

    #[must_use]
    pub fn asset(&self) -> &CloudAssetKey {
        &self.metadata.asset
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.metadata.path
    }

    #[must_use]
    pub const fn identity(&self) -> FileIdentity {
        self.metadata.identity
    }

    #[must_use]
    pub const fn bytes(&self) -> u64 {
        self.metadata.bytes
    }
}

struct PlaybackProtection {
    runtime: Arc<RuntimeState>,
    key: ProtectionKey,
}

impl Drop for PlaybackProtection {
    fn drop(&mut self) {
        self.runtime.release(&self.key, true);
    }
}

/// Scoped protection transferred into the accepted player Open command.
///
/// This type is deliberately not `Clone`: another consumer must perform a new
/// exact-identity acceptance and receives its own protection count.
pub struct CloudMediaLease {
    metadata: CachedAssetMetadata,
    _protection: PlaybackProtection,
}

impl CloudMediaLease {
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.metadata.path
    }

    #[must_use]
    pub const fn identity(&self) -> FileIdentity {
        self.metadata.identity
    }

    #[must_use]
    pub fn asset(&self) -> &CloudAssetKey {
        &self.metadata.asset
    }
}

struct PermitPool {
    available: Mutex<usize>,
    changed: Condvar,
}

impl PermitPool {
    fn global() -> &'static Self {
        static POOL: OnceLock<PermitPool> = OnceLock::new();
        POOL.get_or_init(|| PermitPool {
            available: Mutex::new(MAX_CONCURRENT_CLOUD_ASSET_DOWNLOADS),
            changed: Condvar::new(),
        })
    }

    fn acquire(
        &'static self,
        cancellation: &dyn CancellationProbe,
    ) -> Result<Permit, CloudCacheError> {
        let mut available = self
            .available
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while *available == 0 {
            if cancellation.is_cancelled() {
                return Err(CloudCacheError::Canceled);
            }
            available = self
                .changed
                .wait_timeout(available, Duration::from_millis(20))
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
        *available -= 1;
        Ok(Permit(self))
    }
}

struct Permit(&'static PermitPool);

impl Drop for Permit {
    fn drop(&mut self) {
        let mut available = self
            .0
            .available
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *available = available.saturating_add(1);
        self.0.changed.notify_one();
    }
}

// Cache adapters may be rebuilt after credential rotation while an older
// adapter's playback lease is still alive. Sharing accounting by canonical
// root prevents the replacement adapter from evicting that protected file.
fn runtime_for_root(root: &Path) -> Arc<RuntimeState> {
    type RuntimeRegistry = Vec<(PathBuf, Weak<RuntimeState>)>;
    static RUNTIMES: OnceLock<Mutex<RuntimeRegistry>> = OnceLock::new();
    let mut runtimes = RUNTIMES
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    runtimes.retain(|(_, runtime)| runtime.strong_count() > 0);
    if let Some(runtime) = runtimes
        .iter()
        .find_map(|(path, runtime)| (path == root).then(|| runtime.upgrade()).flatten())
    {
        return runtime;
    }
    let runtime = Arc::new(RuntimeState::default());
    runtimes.push((root.to_path_buf(), Arc::downgrade(&runtime)));
    runtime
}

pub struct CloudCache {
    root: PathBuf,
    _root_authority: Arc<DirectoryAuthority>,
    runtime: Arc<RuntimeState>,
    download: Arc<dyn DownloadPort>,
    available_space: Arc<dyn AvailableSpacePort>,
    account_guard: Arc<dyn AccountPublicationGuard>,
}

impl CloudCache {
    pub fn open(
        root: impl AsRef<Path>,
        download: Arc<dyn DownloadPort>,
        available_space: Arc<dyn AvailableSpacePort>,
        account_guard: Arc<dyn AccountPublicationGuard>,
    ) -> Result<Self, CloudCacheError> {
        std::fs::create_dir_all(root.as_ref())
            .map_err(|error| CloudCacheError::Io(error.to_string()))?;
        let root = root
            .as_ref()
            .canonicalize()
            .map_err(|error| CloudCacheError::Io(error.to_string()))?;
        let root_authority = Arc::new(
            DirectoryAuthority::open(&root)
                .map_err(|error| CloudCacheError::Io(error.to_string()))?,
        );
        Ok(Self {
            runtime: runtime_for_root(&root),
            root,
            _root_authority: root_authority,
            download,
            available_space,
            account_guard,
        })
    }

    pub fn get(
        &self,
        request: CloudAssetRequest,
        cancellation: &CloudCancellation,
    ) -> Result<Option<CachedCloudAsset>, CloudCacheError> {
        if !self.account_guard.is_current(&request.account) {
            return Err(CloudCacheError::StaleAccount);
        }
        let key = FlightKey {
            account: request.account.clone(),
            asset: request.asset.clone(),
        };
        let (flight, leader) = {
            let mut flights = self
                .runtime
                .flights
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(flight) = flights.get(&key) {
                (Arc::clone(flight), false)
            } else {
                if flights.len() >= MAX_PENDING_CLOUD_ASSET_FLIGHTS {
                    return Err(CloudCacheError::Capacity);
                }
                let flight = Arc::new(Flight::new());
                flights.insert(key.clone(), Arc::clone(&flight));
                (flight, true)
            }
        };

        if !leader {
            let result = flight.wait(cancellation)?;
            if cancellation.is_cancelled() {
                return Err(CloudCacheError::Canceled);
            }
            if !self.account_guard.is_current(&request.account) {
                return Err(CloudCacheError::StaleAccount);
            }
            return Ok(result);
        }

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.get_as_leader(&request, cancellation)
        }))
        .unwrap_or_else(|_| Err(CloudCacheError::Internal("download worker panicked".into())));
        // Cloning a successful result acquires another transient cache pin.
        // Linearize that acquisition with cancellation just like the original
        // cache hit/publication, so cancel() returning means no follower pin
        // can appear afterward.
        let shared_result = cancellation.run_if_current(|| result.clone());
        flight.publish(shared_result);
        let mut flights = self
            .runtime
            .flights
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if flights
            .get(&key)
            .is_some_and(|current| Arc::ptr_eq(current, &flight))
        {
            flights.remove(&key);
        }
        if cancellation.is_cancelled() {
            return Err(CloudCacheError::Canceled);
        }
        if !self.account_guard.is_current(&request.account) {
            return Err(CloudCacheError::StaleAccount);
        }
        result
    }

    pub fn accept_media(
        &self,
        current: &CloudAccountFence,
        mut cached: CachedCloudAsset,
        cancellation: &CloudCancellation,
    ) -> Result<CloudMediaLease, CloudCacheError> {
        if cached.metadata.asset.kind() != CloudAssetKind::Media {
            return Err(CloudCacheError::InvalidAsset(
                "only cached Cloud media can acquire a playback lease".into(),
            ));
        }
        if &cached.metadata.account != current {
            return Err(CloudCacheError::StaleAccount);
        }
        let authority = self.namespace_authority(current)?;
        let key = cached.pin.key.clone();
        let runtime = Arc::clone(&cached.pin.runtime);
        cancellation.run_if_current(|| {
            self.account_guard.publish_if_current(current, &mut || {
                let name = cached.metadata.asset.file_name();
                let current_identity = authority
                    .regular_file_identity(&name)
                    .map_err(|error| CloudCacheError::Io(error.to_string()))?
                    .ok_or_else(|| {
                        CloudCacheError::InvalidAsset("cached media disappeared".into())
                    })?;
                if current_identity != cached.metadata.identity {
                    return Err(CloudCacheError::InvalidAsset(
                        "cached media identity changed before Open".into(),
                    ));
                }
                let mut accounting = runtime
                    .accounting
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let counts = accounting.protection.get_mut(&key).ok_or_else(|| {
                    CloudCacheError::Internal("transient cache protection disappeared".into())
                })?;
                if counts.transient == 0 {
                    return Err(CloudCacheError::Internal(
                        "transient cache protection was already released".into(),
                    ));
                }
                counts.transient -= 1;
                counts.playback = counts.playback.saturating_add(1);
                Ok(())
            })
        })?;
        cached.pin.armed = false;
        Ok(CloudMediaLease {
            metadata: cached.metadata.clone(),
            _protection: PlaybackProtection { runtime, key },
        })
    }

    /// Remove one thumbnail that passed the cache marker checks but failed the
    /// bounded image decoder. The transient pin is consumed only after the
    /// exact cached identity is removed, so a foreign replacement at the same
    /// path is preserved and another caller's pin prevents invalidation.
    pub fn invalidate_thumbnail(
        &self,
        current: &CloudAccountFence,
        mut cached: CachedCloudAsset,
        cancellation: &CloudCancellation,
    ) -> Result<(), CloudCacheError> {
        if cached.metadata.asset.kind() != CloudAssetKind::Thumbnail {
            return Err(CloudCacheError::InvalidAsset(
                "only cached Cloud thumbnails can be invalidated after decode".into(),
            ));
        }
        if &cached.metadata.account != current {
            return Err(CloudCacheError::StaleAccount);
        }
        let authority = self.namespace_authority(current)?;
        let asset_name = cached.metadata.asset.file_name();
        let marker_name = cached.metadata.asset.marker_name();
        if cached.metadata.path != authority.display_path().join(&asset_name) {
            return Err(CloudCacheError::InvalidAsset(
                "cached thumbnail path does not match its account namespace".into(),
            ));
        }
        cancellation.run_if_current(|| {
            self.account_guard.publish_if_current(current, &mut || {
                let mut accounting = self
                    .runtime
                    .accounting
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let counts = accounting.protection.get(&cached.pin.key).ok_or_else(|| {
                    CloudCacheError::Internal(
                        "cached thumbnail transient protection disappeared".into(),
                    )
                })?;
                if counts.transient != 1 || counts.playback != 0 {
                    return Err(CloudCacheError::InvalidAsset(
                        "cached thumbnail is still protected by another consumer".into(),
                    ));
                }
                let marker_identity = authority
                    .regular_file_identity(&marker_name)
                    .map_err(|error| CloudCacheError::Io(error.to_string()))?;
                authority
                    .remove_file_if_identity(&asset_name, cached.metadata.identity)
                    .map_err(|error| {
                        CloudCacheError::Io(format!(
                            "remove decoder-rejected Cloud thumbnail: {error}"
                        ))
                    })?;

                // The exact asset is gone, so consume this method's transient pin
                // even if marker cleanup reports an error. Leaving the logical pin
                // armed would retain a protection entry for a nonexistent file.
                accounting.protection.remove(&cached.pin.key);
                cached.pin.armed = false;
                if let Some(marker_identity) = marker_identity {
                    authority
                        .remove_file_if_identity(&marker_name, marker_identity)
                        .map_err(|error| {
                            CloudCacheError::Io(format!(
                                "remove decoder-rejected Cloud thumbnail marker: {error}"
                            ))
                        })?;
                }
                Ok(())
            })
        })
    }

    #[must_use]
    pub fn playback_lease_count(&self) -> usize {
        self.runtime
            .accounting
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .protection
            .values()
            .map(|counts| counts.playback)
            .sum()
    }

    fn get_as_leader(
        &self,
        request: &CloudAssetRequest,
        cancellation: &CloudCancellation,
    ) -> Result<Option<CachedCloudAsset>, CloudCacheError> {
        let authority = self.namespace_authority(&request.account)?;
        if let Some(hit) = self.cache_hit(&authority, request, cancellation)? {
            return Ok(Some(hit));
        }
        let _permit = PermitPool::global().acquire(cancellation)?;
        if cancellation.is_cancelled() {
            return Err(CloudCacheError::Canceled);
        }
        if !self.account_guard.is_current(&request.account) {
            return Err(CloudCacheError::StaleAccount);
        }
        self.download_and_publish(authority, request, cancellation)
    }

    fn namespace_authority(
        &self,
        account: &CloudAccountFence,
    ) -> Result<Arc<DirectoryAuthority>, CloudCacheError> {
        let path = self.root.join(account.cache_namespace.as_str());
        match std::fs::create_dir(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(CloudCacheError::Io(error.to_string())),
        }
        DirectoryAuthority::open(&path)
            .map(Arc::new)
            .map_err(|error| CloudCacheError::Io(error.to_string()))
    }

    fn cache_hit(
        &self,
        authority: &Arc<DirectoryAuthority>,
        request: &CloudAssetRequest,
        cancellation: &CloudCancellation,
    ) -> Result<Option<CachedCloudAsset>, CloudCacheError> {
        let mut hit = None;
        cancellation.run_if_current(|| {
            self.account_guard
                .publish_if_current(&request.account, &mut || {
                    let mut accounting = self
                        .runtime
                        .accounting
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let Some(completed) = completed_asset(authority, request)? else {
                        return Ok(());
                    };
                    touch_completed(&completed)?;
                    hit = Some(self.pin_completed(&mut accounting, request, completed));
                    Ok(())
                })
        })?;
        Ok(hit)
    }

    fn reserve_before_download(&self, reservation: u64) -> Result<(), CloudCacheError> {
        let mut accounting = self
            .runtime
            .accounting
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.prune_locked(&mut accounting, reservation, reservation)?;
        Ok(())
    }

    fn download_and_publish(
        &self,
        authority: Arc<DirectoryAuthority>,
        request: &CloudAssetRequest,
        cancellation: &CloudCancellation,
    ) -> Result<Option<CachedCloudAsset>, CloudCacheError> {
        let reservation = request.hard_limit_bytes();
        self.reserve_before_download(reservation)?;

        let target_name = request.asset.file_name();
        let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let temp_name = OsString::from(format!(
            "{}.{}.{}.tmp",
            target_name.to_string_lossy(),
            std::process::id(),
            counter
        ));
        let mut file = authority
            .create_new_regular_file(&temp_name)
            .map_err(|error| CloudCacheError::Io(error.to_string()))?;
        let temp_identity =
            opened_file_identity(&file).map_err(|error| CloudCacheError::Io(error.to_string()))?;
        let mut temp = OwnedCacheTemp {
            authority: Arc::clone(&authority),
            _pin: Some(
                self.runtime
                    .add_transient(authority.display_path().join(&temp_name), temp_identity),
            ),
            name: temp_name,
            identity: temp_identity,
            armed: true,
        };
        let mut sink = DownloadSink {
            file: &mut file,
            written: 0,
            limit: request.hard_limit_bytes(),
        };
        let receipt = self.download.download(request, &mut sink, cancellation)?;
        let written = sink.written();
        if cancellation.is_cancelled() {
            return Err(CloudCacheError::Canceled);
        }
        if receipt
            .advertised_size_bytes
            .is_some_and(|size| size > request.hard_limit_bytes())
        {
            return Err(CloudCacheError::TooLarge {
                limit: request.hard_limit_bytes(),
            });
        }
        match receipt.status {
            DownloadStatus::Missing if written == 0 => return Ok(None),
            DownloadStatus::Missing => {
                return Err(CloudCacheError::InvalidAsset(
                    "missing response contained asset bytes".into(),
                ));
            }
            DownloadStatus::Found if written == 0 => {
                return Err(CloudCacheError::InvalidAsset(
                    "download returned an empty body".into(),
                ));
            }
            DownloadStatus::Found => {}
        }
        file.sync_all()
            .map_err(|error| CloudCacheError::Io(error.to_string()))?;
        drop(file);

        let mut published: Option<CachedCloudAsset> = None;
        cancellation.run_if_current(|| {
            self.account_guard
                .publish_if_current(&request.account, &mut || {
                    let mut accounting = self
                        .runtime
                        .accounting
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let completed = if let Some(existing) = completed_asset(&authority, request)? {
                        existing
                    } else {
                        self.prune_locked(&mut accounting, written, 0)?;
                        remove_invalid_pair_if_present(
                            &authority,
                            &request.asset,
                            &accounting.protection,
                        )?;
                        publish_temp_pair(
                            &authority,
                            &request.asset,
                            &mut temp,
                            written,
                            request.expected_size_bytes,
                        )?;
                        completed_asset(&authority, request)?.ok_or_else(|| {
                            CloudCacheError::Internal(
                                "published cache pair did not validate".into(),
                            )
                        })?
                    };
                    touch_completed(&completed)?;
                    published = Some(self.pin_completed(&mut accounting, request, completed));
                    Ok(())
                })
        })?;
        published.ok_or(CloudCacheError::StaleAccount).map(Some)
    }

    fn pin_completed(
        &self,
        accounting: &mut AccountingState,
        request: &CloudAssetRequest,
        completed: CompletedAsset,
    ) -> CachedCloudAsset {
        let key = ProtectionKey {
            path: completed.path.clone(),
            identity: completed.identity,
        };
        let counts = accounting.protection.entry(key.clone()).or_default();
        counts.transient = counts.transient.saturating_add(1);
        CachedCloudAsset {
            metadata: CachedAssetMetadata {
                account: request.account.clone(),
                asset: request.asset.clone(),
                path: completed.path,
                identity: completed.identity,
                bytes: completed.bytes,
            },
            pin: TransientCachePin {
                runtime: Arc::clone(&self.runtime),
                key,
                armed: true,
            },
        }
    }

    pub fn prune(&self, reservation: u64) -> Result<CloudCachePruneReport, CloudCacheError> {
        let mut accounting = self
            .runtime
            .accounting
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.prune_locked(&mut accounting, reservation, reservation)
    }

    fn prune_locked(
        &self,
        accounting: &mut AccountingState,
        additional_bytes: u64,
        free_space_reservation: u64,
    ) -> Result<CloudCachePruneReport, CloudCacheError> {
        let now = SystemTime::now();
        let mut entries = collect_owned_entries(&self.root, now, &accounting.protection)?;
        entries.sort_by_key(|entry| entry.modified);
        let total = entries
            .iter()
            .fold(0_u64, |sum, entry| sum.saturating_add(entry.total_bytes()));
        let available = self.available_space.available_bytes(&self.root)?;
        let required_quota = total
            .saturating_add(additional_bytes)
            .saturating_sub(CLOUD_CACHE_QUOTA_BYTES);
        let required_free = CLOUD_CACHE_FREE_SPACE_FLOOR_BYTES
            .saturating_add(free_space_reservation)
            .saturating_sub(available);
        let required = required_quota.max(required_free);
        let mut report = CloudCachePruneReport::default();

        for entry in entries {
            let protected = accounting.protection.get(&ProtectionKey {
                path: entry.path.clone(),
                identity: entry.identity,
            });
            if protected.is_some_and(|counts| counts.transient > 0 || counts.playback > 0) {
                continue;
            }
            let stale = now
                .duration_since(entry.modified)
                .ok()
                .is_some_and(|age| age >= CLOUD_CACHE_MAX_AGE);
            if !stale && report.freed_bytes >= required {
                continue;
            }
            if entry
                .authority
                .remove_file_if_identity(&entry.name, entry.identity)
                .is_err()
            {
                continue;
            }
            report.freed_bytes = report.freed_bytes.saturating_add(entry.asset_bytes);
            if let Some((marker_name, marker_identity)) = entry.marker {
                if entry
                    .authority
                    .remove_file_if_identity(&marker_name, marker_identity)
                    .is_ok()
                {
                    report.freed_bytes = report.freed_bytes.saturating_add(entry.marker_bytes);
                }
            }
            report.evicted_entries += 1;
        }
        report.remaining_bytes = total.saturating_sub(report.freed_bytes);
        let quota_ok =
            report.remaining_bytes.saturating_add(additional_bytes) <= CLOUD_CACHE_QUOTA_BYTES;
        let free_ok = available.saturating_add(report.freed_bytes)
            >= CLOUD_CACHE_FREE_SPACE_FLOOR_BYTES.saturating_add(free_space_reservation);
        if !quota_ok || !free_ok {
            return Err(CloudCacheError::InsufficientSpace {
                requested: additional_bytes,
            });
        }
        Ok(report)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CloudCachePruneReport {
    pub evicted_entries: usize,
    pub freed_bytes: u64,
    pub remaining_bytes: u64,
}

struct CompletedAsset {
    path: PathBuf,
    identity: FileIdentity,
    bytes: u64,
    marker_path: PathBuf,
    marker_identity: FileIdentity,
}

fn completed_asset(
    authority: &DirectoryAuthority,
    request: &CloudAssetRequest,
) -> Result<Option<CompletedAsset>, CloudCacheError> {
    let asset = &request.asset;
    let asset_name = asset.file_name();
    let marker_name = asset.marker_name();
    let Some(asset_identity) = authority
        .regular_file_identity(&asset_name)
        .map_err(|error| CloudCacheError::Io(error.to_string()))?
    else {
        return Ok(None);
    };
    let Some(marker_identity) = authority
        .regular_file_identity(&marker_name)
        .map_err(|error| CloudCacheError::Io(error.to_string()))?
    else {
        return Ok(None);
    };
    let asset_path = authority.display_path().join(&asset_name);
    let marker_path = authority.display_path().join(&marker_name);
    let file = clipline_shell::open_regular_file_nofollow(&asset_path)
        .map_err(|error| CloudCacheError::Io(error.to_string()))?;
    let marker = clipline_shell::open_regular_file_nofollow(&marker_path)
        .map_err(|error| CloudCacheError::Io(error.to_string()))?;
    if opened_file_identity(&file).map_err(|error| CloudCacheError::Io(error.to_string()))?
        != asset_identity
        || opened_file_identity(&marker).map_err(|error| CloudCacheError::Io(error.to_string()))?
            != marker_identity
    {
        return Ok(None);
    }
    let bytes = file
        .metadata()
        .map_err(|error| CloudCacheError::Io(error.to_string()))?
        .len();
    let marker_len = marker
        .metadata()
        .map_err(|error| CloudCacheError::Io(error.to_string()))?
        .len();
    if bytes == 0 || marker_len == 0 || marker_len > MAX_CLOUD_CACHE_MARKER_BYTES {
        return Ok(None);
    }
    let mut marker_bytes = Vec::with_capacity(marker_len as usize);
    marker
        .try_clone()
        .and_then(|copy| {
            copy.take(MAX_CLOUD_CACHE_MARKER_BYTES)
                .read_to_end(&mut marker_bytes)
        })
        .map_err(|error| CloudCacheError::Io(error.to_string()))?;
    let marker_record = std::str::from_utf8(&marker_bytes).ok().and_then(|value| {
        let mut fields = value.trim().split(':');
        let schema = fields.next()?;
        let version = fields.next()?.parse::<u64>().ok()?;
        let actual = fields.next()?.parse::<u64>().ok()?;
        let expected = fields.next()?.parse::<u64>().ok()?;
        (schema == "v1" && fields.next().is_none()).then_some((version, actual, expected))
    });
    let expected = request
        .expected_size_bytes
        .filter(|size| *size > 0)
        .unwrap_or(0);
    if marker_record != Some((asset.version(), bytes, expected)) {
        return Ok(None);
    }
    Ok(Some(CompletedAsset {
        path: asset_path,
        identity: asset_identity,
        bytes,
        marker_path,
        marker_identity,
    }))
}

fn touch_completed(completed: &CompletedAsset) -> Result<(), CloudCacheError> {
    let now = SystemTime::now();
    touch_if_identity(&completed.path, completed.identity, now)?;
    touch_if_identity(&completed.marker_path, completed.marker_identity, now)
}

fn touch_if_identity(
    path: &Path,
    expected: FileIdentity,
    modified: SystemTime,
) -> Result<(), CloudCacheError> {
    clipline_shell::set_regular_file_modified_if_identity(path, expected, modified)
        .map_err(|error| CloudCacheError::Io(format!("refresh cache recency: {error}")))
}

struct OwnedCacheTemp {
    authority: Arc<DirectoryAuthority>,
    _pin: Option<TransientCachePin>,
    name: OsString,
    identity: FileIdentity,
    armed: bool,
}

impl Drop for OwnedCacheTemp {
    fn drop(&mut self) {
        if self.armed {
            let _ = self
                .authority
                .remove_file_if_identity(&self.name, self.identity);
        }
    }
}

fn publish_temp_pair(
    authority: &Arc<DirectoryAuthority>,
    asset: &CloudAssetKey,
    temp: &mut OwnedCacheTemp,
    bytes: u64,
    expected_size_bytes: Option<u64>,
) -> Result<(), CloudCacheError> {
    let asset_name = asset.file_name();
    authority
        .rename_file_noreplace_if_identity(&temp.name, &asset_name, temp.identity)
        .map_err(|error| CloudCacheError::Io(format!("publish asset rename: {error}")))?;
    temp.armed = false;

    let marker_name = asset.marker_name();
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let marker_temp_name = OsString::from(format!(
        "{}.{}.{}.tmp",
        marker_name.to_string_lossy(),
        std::process::id(),
        counter
    ));
    let mut marker_file = authority
        .create_new_regular_file(&marker_temp_name)
        .map_err(|error| CloudCacheError::Io(format!("create marker temp: {error}")))?;
    let marker_identity = opened_file_identity(&marker_file)
        .map_err(|error| CloudCacheError::Io(error.to_string()))?;
    let mut marker_temp = OwnedCacheTemp {
        authority: Arc::clone(authority),
        // Publication holds the accounting lock for this temp's entire
        // lifetime, so no concurrent janitor can observe it as unprotected.
        _pin: None,
        name: marker_temp_name,
        identity: marker_identity,
        armed: true,
    };
    let marker_result = (|| {
        let marker = format!(
            "v1:{}:{}:{}",
            asset.version(),
            bytes,
            expected_size_bytes.filter(|size| *size > 0).unwrap_or(0)
        );
        marker_file
            .write_all(marker.as_bytes())
            .and_then(|()| marker_file.sync_all())
            .map_err(|error| CloudCacheError::Io(error.to_string()))?;
        drop(marker_file);
        authority
            .rename_file_noreplace_if_identity(
                &marker_temp.name,
                &marker_name,
                marker_temp.identity,
            )
            .map_err(|error| CloudCacheError::Io(error.to_string()))?;
        marker_temp.armed = false;
        Ok(())
    })();
    if let Err(error) = marker_result {
        let _ = authority.remove_file_if_identity(&asset_name, temp.identity);
        return Err(error);
    }
    Ok(())
}

fn remove_invalid_pair_if_present(
    authority: &DirectoryAuthority,
    asset: &CloudAssetKey,
    protected: &HashMap<ProtectionKey, ProtectionCounts>,
) -> Result<(), CloudCacheError> {
    for name in [asset.file_name(), asset.marker_name()] {
        let Some(identity) = authority
            .regular_file_identity(&name)
            .map_err(|error| CloudCacheError::Io(error.to_string()))?
        else {
            continue;
        };
        let path = authority.display_path().join(&name);
        if protected
            .get(&ProtectionKey { path, identity })
            .is_some_and(|counts| counts.transient > 0 || counts.playback > 0)
        {
            return Err(CloudCacheError::InsufficientSpace { requested: 0 });
        }
        authority
            .remove_file_if_identity(&name, identity)
            .map_err(|error| CloudCacheError::Io(format!("remove invalid cache pair: {error}")))?;
    }
    Ok(())
}

struct OwnedCacheEntry {
    authority: Arc<DirectoryAuthority>,
    name: OsString,
    path: PathBuf,
    identity: FileIdentity,
    marker: Option<(OsString, FileIdentity)>,
    asset_bytes: u64,
    marker_bytes: u64,
    modified: SystemTime,
}

impl OwnedCacheEntry {
    fn total_bytes(&self) -> u64 {
        self.asset_bytes.saturating_add(self.marker_bytes)
    }
}

fn collect_owned_entries(
    root: &Path,
    now: SystemTime,
    protected: &HashMap<ProtectionKey, ProtectionCounts>,
) -> Result<Vec<OwnedCacheEntry>, CloudCacheError> {
    let mut scanned = 0_usize;
    let mut entries = Vec::new();
    let namespaces =
        std::fs::read_dir(root).map_err(|error| CloudCacheError::Io(error.to_string()))?;
    for namespace in namespaces {
        let namespace = namespace.map_err(|error| CloudCacheError::Io(error.to_string()))?;
        scanned = scanned.checked_add(1).ok_or(CloudCacheError::ScanLimit)?;
        if scanned > MAX_CLOUD_CACHE_SCAN_ENTRIES {
            return Err(CloudCacheError::ScanLimit);
        }
        let Some(namespace_name) = namespace.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if CloudCacheNamespace::new(namespace_name).is_err() {
            continue;
        }
        let authority = Arc::new(
            DirectoryAuthority::open(&namespace.path())
                .map_err(|error| CloudCacheError::Io(error.to_string()))?,
        );
        let children = std::fs::read_dir(authority.display_path())
            .map_err(|error| CloudCacheError::Io(error.to_string()))?;
        for child in children {
            let child = child.map_err(|error| CloudCacheError::Io(error.to_string()))?;
            scanned = scanned.checked_add(1).ok_or(CloudCacheError::ScanLimit)?;
            if scanned > MAX_CLOUD_CACHE_SCAN_ENTRIES {
                return Err(CloudCacheError::ScanLimit);
            }
            let name = child.file_name();
            if CloudAssetKey::owns_temp_name(&name) {
                prune_stale_temp(&authority, &name, now, protected)?;
                continue;
            }
            if CloudAssetKey::owns_marker_name(&name) {
                let Some(name_text) = name.to_str() else {
                    continue;
                };
                let asset_name = OsStr::new(name_text.trim_end_matches(".ok"));
                if authority
                    .regular_file_identity(asset_name)
                    .map_err(|error| CloudCacheError::Io(error.to_string()))?
                    .is_some()
                {
                    continue;
                }
                let Some(identity) = authority
                    .regular_file_identity(&name)
                    .map_err(|error| CloudCacheError::Io(error.to_string()))?
                else {
                    continue;
                };
                let path = authority.display_path().join(&name);
                let file = clipline_shell::open_regular_file_nofollow(&path)
                    .map_err(|error| CloudCacheError::Io(error.to_string()))?;
                if opened_file_identity(&file)
                    .map_err(|error| CloudCacheError::Io(error.to_string()))?
                    != identity
                {
                    return Err(CloudCacheError::Io(
                        "orphan cache marker identity changed during scan".into(),
                    ));
                }
                let metadata = file
                    .metadata()
                    .map_err(|error| CloudCacheError::Io(error.to_string()))?;
                entries.push(OwnedCacheEntry {
                    authority: Arc::clone(&authority),
                    name,
                    path,
                    identity,
                    marker: None,
                    asset_bytes: metadata.len(),
                    marker_bytes: 0,
                    modified: metadata.modified().unwrap_or(UNIX_EPOCH),
                });
                continue;
            }
            if !CloudAssetKey::owns_asset_name(&name) {
                continue;
            }
            let Some(identity) = authority
                .regular_file_identity(&name)
                .map_err(|error| CloudCacheError::Io(error.to_string()))?
            else {
                continue;
            };
            let path = authority.display_path().join(&name);
            let file = clipline_shell::open_regular_file_nofollow(&path)
                .map_err(|error| CloudCacheError::Io(error.to_string()))?;
            if opened_file_identity(&file)
                .map_err(|error| CloudCacheError::Io(error.to_string()))?
                != identity
            {
                return Err(CloudCacheError::Io(
                    "cache asset identity changed during scan".into(),
                ));
            }
            let metadata = file
                .metadata()
                .map_err(|error| CloudCacheError::Io(error.to_string()))?;
            let marker_name = {
                let mut marker = name.clone();
                marker.push(".ok");
                marker
            };
            let marker = authority
                .regular_file_identity(&marker_name)
                .map_err(|error| CloudCacheError::Io(error.to_string()))?;
            let marker = marker
                .map(|identity| {
                    let marker_path = authority.display_path().join(&marker_name);
                    let marker_file = clipline_shell::open_regular_file_nofollow(&marker_path)
                        .map_err(|error| CloudCacheError::Io(error.to_string()))?;
                    if opened_file_identity(&marker_file)
                        .map_err(|error| CloudCacheError::Io(error.to_string()))?
                        != identity
                    {
                        return Err(CloudCacheError::Io(
                            "cache marker identity changed during scan".into(),
                        ));
                    }
                    let bytes = marker_file
                        .metadata()
                        .map_err(|error| CloudCacheError::Io(error.to_string()))?
                        .len();
                    Ok((identity, bytes))
                })
                .transpose()?;
            let marker_bytes = marker.map_or(0, |(_, bytes)| bytes);
            entries.push(OwnedCacheEntry {
                authority: Arc::clone(&authority),
                name,
                path,
                identity,
                marker: marker.map(|(identity, _)| (marker_name, identity)),
                asset_bytes: metadata.len(),
                marker_bytes,
                modified: metadata.modified().unwrap_or(UNIX_EPOCH),
            });
        }
    }
    Ok(entries)
}

fn prune_stale_temp(
    authority: &DirectoryAuthority,
    name: &OsStr,
    now: SystemTime,
    protected: &HashMap<ProtectionKey, ProtectionCounts>,
) -> Result<(), CloudCacheError> {
    let Some(identity) = authority
        .regular_file_identity(name)
        .map_err(|error| CloudCacheError::Io(error.to_string()))?
    else {
        return Ok(());
    };
    let path = authority.display_path().join(name);
    if protected
        .get(&ProtectionKey {
            path: path.clone(),
            identity,
        })
        .is_some_and(|counts| counts.transient > 0 || counts.playback > 0)
    {
        return Ok(());
    }
    let file = clipline_shell::open_regular_file_nofollow(&path)
        .map_err(|error| CloudCacheError::Io(error.to_string()))?;
    if opened_file_identity(&file).map_err(|error| CloudCacheError::Io(error.to_string()))?
        != identity
    {
        return Err(CloudCacheError::Io(
            "cache temp identity changed during cleanup".into(),
        ));
    }
    let modified = file
        .metadata()
        .map_err(|error| CloudCacheError::Io(error.to_string()))?
        .modified()
        .map_err(|error| CloudCacheError::Io(error.to_string()))?;
    let stale = now
        .duration_since(modified)
        .ok()
        .is_some_and(|age| age >= CLOUD_CACHE_TEMP_MAX_AGE);
    drop(file);
    if stale {
        authority
            .remove_file_if_identity(name, identity)
            .map_err(|error| CloudCacheError::Io(error.to_string()))?;
    }
    Ok(())
}
