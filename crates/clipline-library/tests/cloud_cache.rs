use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
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
    let lease = cache.accept_media(&account, cached).unwrap();
    assert!(lease.path().exists());
    assert_eq!(cache.playback_lease_count(), 1);
    drop(lease);
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
    let lease = cache.accept_media(&account, cached).unwrap();
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
