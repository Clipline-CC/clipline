use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, SystemTime};

use clipline_library::cache::*;
use clipline_library::cache_identity::*;
use clipline_library::{CloudAccountGeneration, CloudAccountKey};
use clipline_test_utils::TestDir;

fn account(name: &str, generation: u64, namespace: &str) -> CloudAccountFence {
    CloudAccountFence {
        account_key: CloudAccountKey::new(name).unwrap(),
        account_generation: CloudAccountGeneration::new(generation),
        cache_namespace: CloudCacheNamespace::new(namespace).unwrap(),
    }
}

fn request(account: &CloudAccountFence, id: &str, kind: CloudAssetKind) -> CloudAssetRequest {
    CloudAssetRequest {
        account: account.clone(),
        asset: CloudAssetKey::new(id, kind, 7).unwrap(),
        expected_size_bytes: Some(13),
    }
}

struct AccountGate(Mutex<CloudAccountFence>);

impl AccountGate {
    fn set(&self, account: CloudAccountFence) {
        *self.0.lock().unwrap() = account;
    }
}

impl AccountPublicationGuard for AccountGate {
    fn is_current(&self, account: &CloudAccountFence) -> bool {
        self.0.lock().unwrap().eq(account)
    }

    fn publish_if_current(
        &self,
        account: &CloudAccountFence,
        publication: &mut dyn FnMut() -> Result<(), CloudCacheError>,
    ) -> Result<(), CloudCacheError> {
        let current = self.0.lock().unwrap();
        if current.ne(account) {
            return Err(CloudCacheError::StaleAccount);
        }
        publication()
    }
}

struct PermissiveGate;

impl AccountPublicationGuard for PermissiveGate {
    fn is_current(&self, _account: &CloudAccountFence) -> bool {
        true
    }

    fn publish_if_current(
        &self,
        _account: &CloudAccountFence,
        publication: &mut dyn FnMut() -> Result<(), CloudCacheError>,
    ) -> Result<(), CloudCacheError> {
        publication()
    }
}

struct FixedSpace(Mutex<u64>);

impl FixedSpace {
    fn ample() -> Self {
        Self(Mutex::new(u64::MAX))
    }

    fn set(&self, bytes: u64) {
        *self.0.lock().unwrap() = bytes;
    }
}

impl AvailableSpacePort for FixedSpace {
    fn available_bytes(&self, _cache_root: &Path) -> Result<u64, CloudCacheError> {
        Ok(*self.0.lock().unwrap())
    }
}

struct FakeDownload {
    bytes: Vec<u8>,
    delay: Duration,
    calls: AtomicUsize,
    active: AtomicUsize,
    max_active: AtomicUsize,
}

impl FakeDownload {
    fn new(bytes: &[u8]) -> Self {
        Self {
            bytes: bytes.to_vec(),
            delay: Duration::ZERO,
            calls: AtomicUsize::new(0),
            active: AtomicUsize::new(0),
            max_active: AtomicUsize::new(0),
        }
    }

    fn delayed(bytes: &[u8], delay: Duration) -> Self {
        Self {
            delay,
            ..Self::new(bytes)
        }
    }
}

impl DownloadPort for FakeDownload {
    fn download(
        &self,
        _request: &CloudAssetRequest,
        sink: &mut DownloadSink<'_>,
        cancellation: &dyn CancellationProbe,
    ) -> Result<DownloadReceipt, CloudCacheError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_active.fetch_max(active, Ordering::SeqCst);
        struct Active<'a>(&'a AtomicUsize);
        impl Drop for Active<'_> {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::SeqCst);
            }
        }
        let _active = Active(&self.active);
        if !self.delay.is_zero() {
            thread::sleep(self.delay);
        }
        if cancellation.is_cancelled() {
            return Err(CloudCacheError::Canceled);
        }
        sink.write_chunk(&self.bytes)?;
        Ok(DownloadReceipt {
            status: DownloadStatus::Found,
            advertised_size_bytes: Some(self.bytes.len() as u64),
        })
    }
}

struct BlockingDownload {
    entered: (Mutex<bool>, Condvar),
    release: (Mutex<bool>, Condvar),
}

impl BlockingDownload {
    fn new() -> Self {
        Self {
            entered: (Mutex::new(false), Condvar::new()),
            release: (Mutex::new(false), Condvar::new()),
        }
    }

    fn wait_until_entered(&self) {
        let entered = self.entered.0.lock().unwrap();
        let (entered, timeout) = self
            .entered
            .1
            .wait_timeout_while(entered, Duration::from_secs(5), |entered| !*entered)
            .unwrap();
        assert!(*entered && !timeout.timed_out(), "download did not start");
    }

    fn release(&self) {
        *self.release.0.lock().unwrap() = true;
        self.release.1.notify_all();
    }
}

impl DownloadPort for BlockingDownload {
    fn download(
        &self,
        _request: &CloudAssetRequest,
        sink: &mut DownloadSink<'_>,
        _cancellation: &dyn CancellationProbe,
    ) -> Result<DownloadReceipt, CloudCacheError> {
        sink.write_chunk(b"payload")?;
        *self.entered.0.lock().unwrap() = true;
        self.entered.1.notify_all();
        let release = self.release.0.lock().unwrap();
        let (release, timeout) = self
            .release
            .1
            .wait_timeout_while(release, Duration::from_secs(5), |release| !*release)
            .unwrap();
        assert!(
            *release && !timeout.timed_out(),
            "download was not released"
        );
        Ok(DownloadReceipt {
            status: DownloadStatus::Found,
            advertised_size_bytes: Some(7),
        })
    }
}

struct AfterPublicationGate {
    account: CloudAccountFence,
    calls: AtomicUsize,
    entered: (Mutex<bool>, Condvar),
    release: (Mutex<bool>, Condvar),
}

impl AfterPublicationGate {
    fn wait_until_publication_returns(&self) {
        let entered = self.entered.0.lock().unwrap();
        let (entered, timeout) = self
            .entered
            .1
            .wait_timeout_while(entered, Duration::from_secs(5), |entered| !*entered)
            .unwrap();
        assert!(
            *entered && !timeout.timed_out(),
            "publication did not complete"
        );
    }

    fn release(&self) {
        *self.release.0.lock().unwrap() = true;
        self.release.1.notify_all();
    }
}

impl AccountPublicationGuard for AfterPublicationGate {
    fn is_current(&self, account: &CloudAccountFence) -> bool {
        account == &self.account
    }

    fn publish_if_current(
        &self,
        account: &CloudAccountFence,
        publication: &mut dyn FnMut() -> Result<(), CloudCacheError>,
    ) -> Result<(), CloudCacheError> {
        if account != &self.account {
            return Err(CloudCacheError::StaleAccount);
        }
        publication()?;
        if self.calls.fetch_add(1, Ordering::SeqCst) + 1 == 2 {
            *self.entered.0.lock().unwrap() = true;
            self.entered.1.notify_all();
            let release = self.release.0.lock().unwrap();
            let (release, timeout) = self
                .release
                .1
                .wait_timeout_while(release, Duration::from_secs(5), |release| !*release)
                .unwrap();
            assert!(
                *release && !timeout.timed_out(),
                "publication was not released"
            );
        }
        Ok(())
    }
}

fn open_cache(
    root: &Path,
    download: Arc<dyn DownloadPort>,
    space: Arc<dyn AvailableSpacePort>,
    gate: Arc<dyn AccountPublicationGuard>,
) -> Arc<CloudCache> {
    Arc::new(CloudCache::open(root, download, space, gate).unwrap())
}

#[test]
fn same_account_generation_and_asset_is_single_flight() {
    let dir = TestDir::new("clipline-library", "cloud-single-flight");
    let account = account("account-a", 1, "aaaaaaaaaaaaaaaa");
    let download = Arc::new(FakeDownload::delayed(
        b"asset payload",
        Duration::from_millis(80),
    ));
    let cache = open_cache(
        dir.path(),
        download.clone(),
        Arc::new(FixedSpace::ample()),
        Arc::new(AccountGate(Mutex::new(account.clone()))),
    );
    let mut workers = Vec::new();
    for _ in 0..24 {
        let cache = Arc::clone(&cache);
        let request = request(&account, "remote-1", CloudAssetKind::Thumbnail);
        workers.push(thread::spawn(move || {
            cache
                .get(request, &CloudCancellation::default())
                .unwrap()
                .unwrap()
        }));
    }
    let assets: Vec<_> = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect();
    assert_eq!(download.calls.load(Ordering::SeqCst), 1);
    assert!(assets
        .windows(2)
        .all(|pair| pair[0].path() == pair[1].path() && pair[0].identity() == pair[1].identity()));
}

#[test]
fn invalidated_thumbnail_is_removed_by_identity_and_downloaded_again() {
    let dir = TestDir::new("clipline-library", "cloud-thumbnail-invalidate");
    let account = account("account-a", 1, "aaaabbbbccccdddd");
    let download = Arc::new(FakeDownload::new(b"corrupt jpeg"));
    let cache = open_cache(
        dir.path(),
        download.clone(),
        Arc::new(FixedSpace::ample()),
        Arc::new(AccountGate(Mutex::new(account.clone()))),
    );
    let request = request(&account, "remote-1", CloudAssetKind::Thumbnail);
    let first = cache
        .get(request.clone(), &CloudCancellation::default())
        .unwrap()
        .unwrap();
    let path = first.path().to_path_buf();
    let marker = path.with_file_name(request.asset.marker_name());

    cache
        .invalidate_thumbnail(&account, first, &CloudCancellation::default())
        .unwrap();

    assert!(!path.exists());
    assert!(!marker.exists());
    let replacement = cache
        .get(request, &CloudCancellation::default())
        .unwrap()
        .unwrap();
    assert_eq!(replacement.path(), path);
    assert_eq!(download.calls.load(Ordering::SeqCst), 2);
}

#[test]
fn thumbnail_invalidation_is_account_identity_and_pin_fenced() {
    let dir = TestDir::new("clipline-library", "cloud-thumbnail-invalidate-fences");
    let current = account("account-a", 1, "1111222233334444");
    let cache = open_cache(
        dir.path(),
        Arc::new(FakeDownload::new(b"corrupt jpeg")),
        Arc::new(FixedSpace::ample()),
        Arc::new(AccountGate(Mutex::new(current.clone()))),
    );
    let asset = cache
        .get(
            request(&current, "remote-1", CloudAssetKind::Thumbnail),
            &CloudCancellation::default(),
        )
        .unwrap()
        .unwrap();
    let path = asset.path().to_path_buf();
    let retained = asset.clone();

    let error = cache
        .invalidate_thumbnail(&current, asset, &CloudCancellation::default())
        .unwrap_err();
    assert!(matches!(error, CloudCacheError::InvalidAsset(_)));
    assert!(path.exists());

    let stale = account("account-a", 2, "1111222233334444");
    let error = cache
        .invalidate_thumbnail(&stale, retained.clone(), &CloudCancellation::default())
        .unwrap_err();
    assert_eq!(error, CloudCacheError::StaleAccount);
    assert!(path.exists());

    cache
        .invalidate_thumbnail(&current, retained, &CloudCancellation::default())
        .unwrap();
    assert!(!path.exists());
}

#[test]
fn thumbnail_invalidation_preserves_a_foreign_replacement() {
    let dir = TestDir::new("clipline-library", "cloud-thumbnail-invalidate-replacement");
    let current = account("account-a", 1, "1111222233335555");
    let cache = open_cache(
        dir.path(),
        Arc::new(FakeDownload::new(b"corrupt jpeg")),
        Arc::new(FixedSpace::ample()),
        Arc::new(AccountGate(Mutex::new(current.clone()))),
    );
    let cached = cache
        .get(
            request(&current, "remote-1", CloudAssetKind::Thumbnail),
            &CloudCancellation::default(),
        )
        .unwrap()
        .unwrap();
    let path = cached.path().to_path_buf();
    std::fs::remove_file(&path).unwrap();
    std::fs::write(&path, b"foreign replacement").unwrap();

    assert!(matches!(
        cache.invalidate_thumbnail(&current, cached, &CloudCancellation::default()),
        Err(CloudCacheError::Io(_))
    ));
    assert_eq!(std::fs::read(path).unwrap(), b"foreign replacement");
}

#[test]
fn canceled_thumbnail_invalidation_preserves_the_exact_cache_pair() {
    let dir = TestDir::new("clipline-library", "cloud-thumbnail-invalidate-canceled");
    let current = account("account-a", 1, "1111222233336666");
    let cache = open_cache(
        dir.path(),
        Arc::new(FakeDownload::new(b"corrupt jpeg")),
        Arc::new(FixedSpace::ample()),
        Arc::new(AccountGate(Mutex::new(current.clone()))),
    );
    let asset_request = request(&current, "remote-1", CloudAssetKind::Thumbnail);
    let cached = cache
        .get(asset_request.clone(), &CloudCancellation::default())
        .unwrap()
        .unwrap();
    let path = cached.path().to_path_buf();
    let marker = path.with_file_name(asset_request.asset.marker_name());
    let cancellation = CloudCancellation::default();
    cancellation.cancel();

    assert_eq!(
        cache
            .invalidate_thumbnail(&current, cached, &cancellation)
            .unwrap_err(),
        CloudCacheError::Canceled
    );
    assert!(path.exists());
    assert!(marker.exists());
}

#[test]
fn hard_download_limit_is_shared_across_cache_adapters() {
    let dir = TestDir::new("clipline-library", "cloud-hard-limit");
    let account = account("account-a", 1, "bbbbbbbbbbbbbbbb");
    let download = Arc::new(FakeDownload::delayed(b"payload", Duration::from_millis(70)));
    let gate = Arc::new(AccountGate(Mutex::new(account.clone())));
    let first = open_cache(
        dir.path(),
        download.clone(),
        Arc::new(FixedSpace::ample()),
        gate.clone(),
    );
    let second = open_cache(
        dir.path(),
        download.clone(),
        Arc::new(FixedSpace::ample()),
        gate,
    );
    let mut workers = Vec::new();
    for index in 0..12 {
        let cache = if index % 2 == 0 {
            Arc::clone(&first)
        } else {
            Arc::clone(&second)
        };
        let request = request(
            &account,
            &format!("remote-{index}"),
            CloudAssetKind::Thumbnail,
        );
        workers.push(thread::spawn(move || {
            cache.get(request, &CloudCancellation::default()).unwrap()
        }));
    }
    for worker in workers {
        worker.join().unwrap();
    }
    assert!(download.max_active.load(Ordering::SeqCst) <= 4);
    assert_eq!(download.calls.load(Ordering::SeqCst), 12);
}

#[test]
fn account_generation_is_part_of_the_flight_key() {
    let dir = TestDir::new("clipline-library", "cloud-generation-flight");
    let first_account = account("account-a", 1, "cccccccccccccccc");
    let next_account = account("account-a", 2, "dddddddddddddddd");
    let download = Arc::new(FakeDownload::delayed(b"payload", Duration::from_millis(50)));
    let cache = open_cache(
        dir.path(),
        download.clone(),
        Arc::new(FixedSpace::ample()),
        Arc::new(PermissiveGate),
    );
    let first = {
        let cache = Arc::clone(&cache);
        let request = request(&first_account, "remote", CloudAssetKind::Thumbnail);
        thread::spawn(move || cache.get(request, &CloudCancellation::default()).unwrap())
    };
    let second = {
        let cache = Arc::clone(&cache);
        let request = request(&next_account, "remote", CloudAssetKind::Thumbnail);
        thread::spawn(move || cache.get(request, &CloudCancellation::default()).unwrap())
    };
    first.join().unwrap();
    second.join().unwrap();
    assert_eq!(download.calls.load(Ordering::SeqCst), 2);
}

#[test]
fn marker_is_required_for_a_cache_hit() {
    let dir = TestDir::new("clipline-library", "cloud-marker-required");
    let account = account("account-a", 1, "eeeeeeeeeeeeeeee");
    let namespace = dir.path().join(account.cache_namespace.as_str());
    std::fs::create_dir_all(&namespace).unwrap();
    let asset = request(&account, "remote", CloudAssetKind::Thumbnail).asset;
    std::fs::write(namespace.join(asset.file_name()), b"orphan").unwrap();
    let download = Arc::new(FakeDownload::new(b"replacement"));
    let cache = open_cache(
        dir.path(),
        download.clone(),
        Arc::new(FixedSpace::ample()),
        Arc::new(AccountGate(Mutex::new(account.clone()))),
    );
    let first = cache
        .get(
            CloudAssetRequest {
                account: account.clone(),
                asset: asset.clone(),
                expected_size_bytes: None,
            },
            &CloudCancellation::default(),
        )
        .unwrap()
        .unwrap();
    drop(first);
    let second = cache
        .get(
            CloudAssetRequest {
                account,
                asset,
                expected_size_bytes: None,
            },
            &CloudCancellation::default(),
        )
        .unwrap()
        .unwrap();
    assert_eq!(second.bytes(), b"replacement".len() as u64);
    assert_eq!(download.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn stale_account_drops_temp_and_publishes_nothing() {
    let dir = TestDir::new("clipline-library", "cloud-stale-publish");
    let original_account = account("account-a", 1, "ffffffffffffffff");
    let replacement = account("account-b", 2, "0123456789abcdef");
    let gate = Arc::new(AccountGate(Mutex::new(original_account.clone())));
    let download = Arc::new(FakeDownload::delayed(
        b"payload",
        Duration::from_millis(100),
    ));
    let cache = open_cache(
        dir.path(),
        download.clone(),
        Arc::new(FixedSpace::ample()),
        gate.clone(),
    );
    let worker = {
        let cache = Arc::clone(&cache);
        let request = request(&original_account, "remote", CloudAssetKind::Media);
        thread::spawn(move || cache.get(request, &CloudCancellation::default()))
    };
    while download.calls.load(Ordering::SeqCst) == 0 {
        thread::yield_now();
    }
    gate.set(replacement);
    assert!(matches!(
        worker.join().unwrap(),
        Err(CloudCacheError::StaleAccount)
    ));
    let namespace = dir.path().join(original_account.cache_namespace.as_str());
    let names: Vec<_> = std::fs::read_dir(namespace)
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name())
        .collect();
    assert!(names.is_empty(), "stale work left cache files: {names:?}");
    assert_eq!(cache.playback_lease_count(), 0);
}

#[test]
fn media_lease_is_acquired_only_on_accept_and_released_on_drop() {
    let dir = TestDir::new("clipline-library", "cloud-media-lease");
    let account = account("account-a", 1, "1111111111111111");
    let cache = open_cache(
        dir.path(),
        Arc::new(FakeDownload::new(b"media")),
        Arc::new(FixedSpace::ample()),
        Arc::new(AccountGate(Mutex::new(account.clone()))),
    );
    let cached = cache
        .get(
            request(&account, "remote", CloudAssetKind::Media),
            &CloudCancellation::default(),
        )
        .unwrap()
        .unwrap();
    assert_eq!(cache.playback_lease_count(), 0);
    let lease = cache
        .accept_media(&account, cached, &CloudCancellation::default())
        .unwrap();
    assert!(lease.path().exists());
    assert_eq!(cache.playback_lease_count(), 1);
    drop(lease);
    assert_eq!(cache.playback_lease_count(), 0);
}

#[test]
fn cancellation_before_media_acceptance_creates_no_playback_lease() {
    let dir = TestDir::new("clipline-library", "cloud-media-accept-canceled");
    let account = account("account-a", 1, "1111111111112222");
    let cache = open_cache(
        dir.path(),
        Arc::new(FakeDownload::new(b"media")),
        Arc::new(FixedSpace::ample()),
        Arc::new(AccountGate(Mutex::new(account.clone()))),
    );
    let cached = cache
        .get(
            request(&account, "remote", CloudAssetKind::Media),
            &CloudCancellation::default(),
        )
        .unwrap()
        .unwrap();
    let cancellation = CloudCancellation::default();
    cancellation.cancel();

    assert!(matches!(
        cache.accept_media(&account, cached, &cancellation),
        Err(CloudCacheError::Canceled)
    ));
    assert_eq!(cache.playback_lease_count(), 0);
}

#[test]
fn exact_cap_is_accepted_and_the_next_byte_cleans_the_temp() {
    let dir = TestDir::new("clipline-library", "cloud-cap");
    let account = account("account-a", 1, "2222222222222222");
    let bytes = vec![7_u8; CLOUD_THUMBNAIL_MAX_BYTES as usize + 1];
    let cache = open_cache(
        dir.path(),
        Arc::new(FakeDownload::new(&bytes)),
        Arc::new(FixedSpace::ample()),
        Arc::new(AccountGate(Mutex::new(account.clone()))),
    );
    let error = cache
        .get(
            request(&account, "remote", CloudAssetKind::Thumbnail),
            &CloudCancellation::default(),
        )
        .unwrap_err();
    assert_eq!(
        error,
        CloudCacheError::TooLarge {
            limit: CLOUD_THUMBNAIL_MAX_BYTES
        }
    );
    let namespace = dir.path().join(account.cache_namespace.as_str());
    assert_eq!(std::fs::read_dir(namespace).unwrap().count(), 0);
}

#[test]
fn protected_media_blocks_eviction_until_the_lease_drops() {
    let dir = TestDir::new("clipline-library", "cloud-protected-lru");
    let account = account("account-a", 1, "3333333333333333");
    let space = Arc::new(FixedSpace::ample());
    let cache = open_cache(
        dir.path(),
        Arc::new(FakeDownload::new(b"media-payload")),
        space.clone(),
        Arc::new(AccountGate(Mutex::new(account.clone()))),
    );
    let cached = cache
        .get(
            request(&account, "remote", CloudAssetKind::Media),
            &CloudCancellation::default(),
        )
        .unwrap()
        .unwrap();
    let path = cached.path().to_path_buf();
    let lease = cache
        .accept_media(&account, cached, &CloudCancellation::default())
        .unwrap();
    space.set(CLOUD_CACHE_FREE_SPACE_FLOOR_BYTES - 1);
    assert!(matches!(
        cache.prune(0),
        Err(CloudCacheError::InsufficientSpace { .. })
    ));
    assert!(path.exists());
    drop(lease);
    cache.prune(0).unwrap();
    assert!(!path.exists());
}

#[test]
fn independently_opened_cache_shares_playback_protection_for_the_same_root() {
    let dir = TestDir::new("clipline-library", "cloud-shared-root-protection");
    let account = account("account-a", 1, "3333333333333334");
    let space = Arc::new(FixedSpace::ample());
    let first = open_cache(
        dir.path(),
        Arc::new(FakeDownload::new(b"media-payload")),
        space.clone(),
        Arc::new(AccountGate(Mutex::new(account.clone()))),
    );
    let cached = first
        .get(
            request(&account, "remote", CloudAssetKind::Media),
            &CloudCancellation::default(),
        )
        .unwrap()
        .unwrap();
    let path = cached.path().to_path_buf();
    let lease = first
        .accept_media(&account, cached, &CloudCancellation::default())
        .unwrap();

    let second = open_cache(
        dir.path(),
        Arc::new(FakeDownload::new(b"replacement-transport")),
        space.clone(),
        Arc::new(AccountGate(Mutex::new(account))),
    );
    space.set(CLOUD_CACHE_FREE_SPACE_FLOOR_BYTES - 1);
    assert!(matches!(
        second.prune(0),
        Err(CloudCacheError::InsufficientSpace { .. })
    ));
    assert!(
        path.exists(),
        "a cache reopened for credential rotation evicted live playback media"
    );

    drop(lease);
    second.prune(0).unwrap();
    assert!(!path.exists());
}

#[test]
fn foreign_files_and_nested_directories_are_not_cache_owned() {
    let dir = TestDir::new("clipline-library", "cloud-foreign-files");
    let account = account("account-a", 1, "4444444444444444");
    let namespace = dir.path().join(account.cache_namespace.as_str());
    let nested = namespace.join("nested");
    std::fs::create_dir_all(&nested).unwrap();
    let foreign = namespace.join("notes.txt");
    let nested_file = nested.join("remote-media-1.mp4");
    std::fs::write(&foreign, b"foreign").unwrap();
    std::fs::write(&nested_file, b"foreign nested").unwrap();
    let cache = open_cache(
        dir.path(),
        Arc::new(FakeDownload::new(b"unused")),
        Arc::new(FixedSpace::ample()),
        Arc::new(AccountGate(Mutex::new(account))),
    );
    cache.prune(0).unwrap();
    assert!(foreign.exists());
    assert!(nested_file.exists());
}

#[test]
fn stale_owned_temp_is_removed_but_unrelated_tmp_survives() {
    let dir = TestDir::new("clipline-library", "cloud-temp-age");
    let account = account("account-a", 1, "5555555555555555");
    let namespace = dir.path().join(account.cache_namespace.as_str());
    std::fs::create_dir_all(&namespace).unwrap();
    let stale = namespace.join("remote-media-1.mp4.123.4.tmp");
    let unrelated = namespace.join("editor.tmp");
    std::fs::write(&stale, b"partial").unwrap();
    std::fs::write(&unrelated, b"keep").unwrap();
    std::fs::File::options()
        .write(true)
        .open(&stale)
        .unwrap()
        .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1))
        .unwrap();
    let cache = open_cache(
        dir.path(),
        Arc::new(FakeDownload::new(b"unused")),
        Arc::new(FixedSpace::ample()),
        Arc::new(AccountGate(Mutex::new(account))),
    );
    cache.prune(0).unwrap();
    assert!(!stale.exists());
    assert!(unrelated.exists());
}

#[test]
fn account_publication_guard_linearizes_the_publish_closure() {
    struct BarrierGate {
        current: Mutex<CloudAccountFence>,
        entered: (Mutex<bool>, Condvar),
    }
    impl AccountPublicationGuard for BarrierGate {
        fn is_current(&self, account: &CloudAccountFence) -> bool {
            self.current.lock().unwrap().eq(account)
        }
        fn publish_if_current(
            &self,
            account: &CloudAccountFence,
            publication: &mut dyn FnMut() -> Result<(), CloudCacheError>,
        ) -> Result<(), CloudCacheError> {
            let current = self.current.lock().unwrap();
            if current.ne(account) {
                return Err(CloudCacheError::StaleAccount);
            }
            *self.entered.0.lock().unwrap() = true;
            self.entered.1.notify_all();
            publication()
        }
    }

    let dir = TestDir::new("clipline-library", "cloud-publication-guard");
    let account = account("account-a", 1, "6666666666666666");
    let gate = Arc::new(BarrierGate {
        current: Mutex::new(account.clone()),
        entered: (Mutex::new(false), Condvar::new()),
    });
    let cache = open_cache(
        dir.path(),
        Arc::new(FakeDownload::new(b"payload")),
        Arc::new(FixedSpace::ample()),
        gate.clone(),
    );
    cache
        .get(
            request(&account, "remote", CloudAssetKind::Thumbnail),
            &CloudCancellation::default(),
        )
        .unwrap();
    assert!(*gate.entered.0.lock().unwrap());
}

#[test]
fn result_is_rejected_when_request_fence_changes_after_publication() {
    struct PostPublicationInvalidationGate {
        account: CloudAccountFence,
        publications: AtomicUsize,
    }

    impl AccountPublicationGuard for PostPublicationInvalidationGate {
        fn is_current(&self, account: &CloudAccountFence) -> bool {
            account == &self.account && self.publications.load(Ordering::SeqCst) < 2
        }

        fn publish_if_current(
            &self,
            account: &CloudAccountFence,
            publication: &mut dyn FnMut() -> Result<(), CloudCacheError>,
        ) -> Result<(), CloudCacheError> {
            if !self.is_current(account) {
                return Err(CloudCacheError::StaleAccount);
            }
            publication()?;
            self.publications.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    let dir = TestDir::new("clipline-library", "cloud-post-publication-fence");
    let account = account("account-a", 1, "6789abcdef012345");
    let gate = Arc::new(PostPublicationInvalidationGate {
        account: account.clone(),
        publications: AtomicUsize::new(0),
    });
    let cache = open_cache(
        dir.path(),
        Arc::new(FakeDownload::new(b"payload")),
        Arc::new(FixedSpace::ample()),
        gate,
    );

    assert!(matches!(
        cache.get(
            request(&account, "remote", CloudAssetKind::Thumbnail),
            &CloudCancellation::default(),
        ),
        Err(CloudCacheError::StaleAccount)
    ));
}

#[test]
fn per_asset_policy_is_derived_instead_of_caller_selected() {
    let policies: HashMap<_, _> = [
        (CloudAssetKind::Thumbnail, CLOUD_THUMBNAIL_MAX_BYTES),
        (CloudAssetKind::Media, CLOUD_MEDIA_SIZE_SLACK_BYTES + 2 * 13),
    ]
    .into_iter()
    .collect();
    let account = account("account-a", 1, "7777777777777777");
    for (kind, limit) in policies {
        assert_eq!(request(&account, "remote", kind).hard_limit_bytes(), limit);
    }
    let mut unknown = request(&account, "unknown", CloudAssetKind::Media);
    unknown.expected_size_bytes = None;
    assert_eq!(unknown.hard_limit_bytes(), CLOUD_MEDIA_MAX_BYTES);
    unknown.expected_size_bytes = Some(u64::MAX);
    assert_eq!(unknown.hard_limit_bytes(), CLOUD_MEDIA_MAX_BYTES);
}

#[test]
fn media_reserves_the_full_permitted_body_before_downloading() {
    let dir = TestDir::new("clipline-library", "cloud-full-body-reservation");
    let account = account("account-a", 1, "8888888888888888");
    let request = request(&account, "remote", CloudAssetKind::Media);
    let download = Arc::new(FakeDownload::new(b"payload"));
    let space = Arc::new(FixedSpace(Mutex::new(
        CLOUD_CACHE_FREE_SPACE_FLOOR_BYTES + request.hard_limit_bytes() - 1,
    )));
    let cache = open_cache(
        dir.path(),
        download.clone(),
        space,
        Arc::new(AccountGate(Mutex::new(account))),
    );
    assert!(matches!(
        cache.get(request, &CloudCancellation::default()),
        Err(CloudCacheError::InsufficientSpace { .. })
    ));
    assert_eq!(download.calls.load(Ordering::SeqCst), 0);
}

#[test]
fn active_owned_temp_is_protected_from_age_cleanup() {
    let dir = TestDir::new("clipline-library", "cloud-active-temp");
    let account = account("account-a", 1, "9999999999999999");
    let download = Arc::new(BlockingDownload::new());
    let cache = open_cache(
        dir.path(),
        download.clone(),
        Arc::new(FixedSpace::ample()),
        Arc::new(AccountGate(Mutex::new(account.clone()))),
    );
    let worker = {
        let cache = Arc::clone(&cache);
        let request = request(&account, "remote", CloudAssetKind::Thumbnail);
        thread::spawn(move || cache.get(request, &CloudCancellation::default()))
    };
    download.wait_until_entered();
    let namespace = dir.path().join(account.cache_namespace.as_str());
    let temp = std::fs::read_dir(&namespace)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.extension().is_some_and(|extension| extension == "tmp"))
        .expect("active temp should exist");
    std::fs::File::options()
        .write(true)
        .open(&temp)
        .unwrap()
        .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1))
        .unwrap();
    cache.prune(0).unwrap();
    assert!(temp.exists(), "active temp was removed by age cleanup");
    download.release();
    worker.join().unwrap().unwrap();
}

#[test]
fn cancellation_before_publication_removes_the_owned_temp_and_publishes_nothing() {
    let dir = TestDir::new("clipline-library", "cloud-cancel-before-publish");
    let account = account("account-a", 1, "9999999999999998");
    let download = Arc::new(BlockingDownload::new());
    let cache = open_cache(
        dir.path(),
        download.clone(),
        Arc::new(FixedSpace::ample()),
        Arc::new(AccountGate(Mutex::new(account.clone()))),
    );
    let cancellation = CloudCancellation::default();
    let worker = {
        let cache = Arc::clone(&cache);
        let cancellation = cancellation.clone();
        let request = request(&account, "remote", CloudAssetKind::Thumbnail);
        thread::spawn(move || cache.get(request, &cancellation))
    };
    download.wait_until_entered();
    cancellation.cancel();
    download.release();

    assert_eq!(
        worker.join().unwrap().unwrap_err(),
        CloudCacheError::Canceled
    );
    let namespace = dir.path().join(account.cache_namespace.as_str());
    assert_eq!(std::fs::read_dir(namespace).unwrap().count(), 0);
}

#[test]
fn published_asset_is_pinned_before_account_guard_releases() {
    let dir = TestDir::new("clipline-library", "cloud-publish-pin");
    let account = account("account-a", 1, "aaaaaaaaaaaaaaa1");
    let gate = Arc::new(AfterPublicationGate {
        account: account.clone(),
        calls: AtomicUsize::new(0),
        entered: (Mutex::new(false), Condvar::new()),
        release: (Mutex::new(false), Condvar::new()),
    });
    let space = Arc::new(FixedSpace::ample());
    let cache = open_cache(
        dir.path(),
        Arc::new(FakeDownload::new(b"payload")),
        space.clone(),
        gate.clone(),
    );
    let worker = {
        let cache = Arc::clone(&cache);
        let request = request(&account, "remote", CloudAssetKind::Thumbnail);
        thread::spawn(move || cache.get(request, &CloudCancellation::default()))
    };
    gate.wait_until_publication_returns();
    let asset_path = dir.path().join(account.cache_namespace.as_str()).join(
        request(&account, "remote", CloudAssetKind::Thumbnail)
            .asset
            .file_name(),
    );
    space.set(CLOUD_CACHE_FREE_SPACE_FLOOR_BYTES - 1);
    assert!(matches!(
        cache.prune(0),
        Err(CloudCacheError::InsufficientSpace { .. })
    ));
    assert!(
        asset_path.exists(),
        "prune crossed the publish-to-pin boundary"
    );
    gate.release();
    worker.join().unwrap().unwrap();
}

#[test]
fn cancellation_racing_publication_is_linearized_after_the_cache_pair() {
    let dir = TestDir::new("clipline-library", "cloud-cancel-publication-race");
    let account = account("account-a", 1, "aaaaaaaaaaaaaaa2");
    let gate = Arc::new(AfterPublicationGate {
        account: account.clone(),
        calls: AtomicUsize::new(0),
        entered: (Mutex::new(false), Condvar::new()),
        release: (Mutex::new(false), Condvar::new()),
    });
    let cache = open_cache(
        dir.path(),
        Arc::new(FakeDownload::new(b"payload")),
        Arc::new(FixedSpace::ample()),
        gate.clone(),
    );
    let cancellation = CloudCancellation::default();
    let worker = {
        let cache = Arc::clone(&cache);
        let cancellation = cancellation.clone();
        let request = request(&account, "remote", CloudAssetKind::Thumbnail);
        thread::spawn(move || cache.get(request, &cancellation))
    };
    gate.wait_until_publication_returns();

    let cancel_returned = Arc::new(AtomicBool::new(false));
    let cancel_worker = {
        let cancellation = cancellation.clone();
        let cancel_returned = Arc::clone(&cancel_returned);
        thread::spawn(move || {
            cancellation.cancel();
            cancel_returned.store(true, Ordering::Release);
        })
    };
    thread::sleep(Duration::from_millis(30));
    assert!(
        !cancel_returned.load(Ordering::Acquire),
        "cancel returned before the in-progress publication linearized"
    );

    gate.release();
    cancel_worker.join().unwrap();
    assert!(cancel_returned.load(Ordering::Acquire));
    assert_eq!(
        worker.join().unwrap().unwrap_err(),
        CloudCacheError::Canceled
    );

    let asset = request(&account, "remote", CloudAssetKind::Thumbnail).asset;
    let namespace = dir.path().join(account.cache_namespace.as_str());
    assert!(namespace.join(asset.file_name()).exists());
    assert!(namespace.join(asset.marker_name()).exists());
}

#[test]
fn recognized_namespace_scan_errors_fail_closed() {
    let dir = TestDir::new("clipline-library", "cloud-scan-fail-closed");
    std::fs::write(dir.path().join("bbbbbbbbbbbbbbbb"), b"not a directory").unwrap();
    let cache = open_cache(
        dir.path(),
        Arc::new(FakeDownload::new(b"unused")),
        Arc::new(FixedSpace::ample()),
        Arc::new(PermissiveGate),
    );
    assert!(matches!(cache.prune(0), Err(CloudCacheError::Io(_))));
}

#[test]
fn orphan_marker_bytes_remain_owned_and_evictable() {
    let dir = TestDir::new("clipline-library", "cloud-orphan-marker");
    let account = account("account-a", 1, "ccccccccccccccc1");
    let namespace = dir.path().join(account.cache_namespace.as_str());
    std::fs::create_dir_all(&namespace).unwrap();
    let marker = request(&account, "remote", CloudAssetKind::Thumbnail)
        .asset
        .marker_name();
    let marker_path = namespace.join(marker);
    std::fs::write(&marker_path, b"marker").unwrap();
    let cache = open_cache(
        dir.path(),
        Arc::new(FakeDownload::new(b"unused")),
        Arc::new(FixedSpace(Mutex::new(
            CLOUD_CACHE_FREE_SPACE_FLOOR_BYTES - 1,
        ))),
        Arc::new(AccountGate(Mutex::new(account))),
    );
    let report = cache.prune(0).unwrap();
    assert_eq!(report.freed_bytes, 6);
    assert!(!marker_path.exists());
}
