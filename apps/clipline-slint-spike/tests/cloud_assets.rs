use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use clipline_library::cache::{
    AccountPublicationGuard, AvailableSpacePort, CancellationProbe, CloudAssetRequest, CloudCache,
    CloudCacheError, DownloadPort, DownloadReceipt, DownloadSink, DownloadStatus,
};
use clipline_library::cache_identity::{
    CloudAccountFence, CloudAssetKey, CloudAssetKind, CloudCacheNamespace,
};
use clipline_library::ports::{
    AvatarTransportResult, CloudAccountPort, CloudCredential, CloudCredentialPort,
    CloudProfilePatch, CloudRequestFence, CloudTransport, CloudTransportFuture, PortError,
};
use clipline_library::{
    CatalogEffect, CatalogItemIdentity, CatalogOperationOwner, CatalogResult,
    CloudAccountGeneration, CloudAccountKey, CloudAccountSnapshot, CloudCatalogOwner,
    CloudListTransportRequest, CloudListTransportResponse, CloudMediaLeaseId,
    CloudProfileTransport, CloudReviewMediaOwner, CloudReviewMediaRequest, CloudServiceAccount,
    CloudThumbnailDescriptor, CloudThumbnailOwner, CloudThumbnailRequest, CloudWorkToken,
    ExpectedResultOwner, ForegroundGeneration, PosterStatus, PreparedCloudReviewMedia,
    RemoteClipId, RequestGeneration, WindowAttachmentGeneration, WindowWorkToken,
    MAX_CATALOG_STRING_BYTES, MAX_FOREGROUND_MESSAGE_BYTES,
};
#[cfg(windows)]
use clipline_playback::windows::{session_channel, SessionUpdate, SessionUpdatePayload};
#[cfg(windows)]
use clipline_playback::{
    PipelineToken, PlaybackCommand, PlaybackPhase, PlaybackSnapshot, PlaybackTime, WorkGeneration,
};
#[cfg(windows)]
use clipline_settings::{AppSettings, SettingsProfile, SettingsStore};
use clipline_slint_spike::catalog::{CatalogEffectHandler, OwnedCatalogResult};
#[cfg(windows)]
use clipline_slint_spike::cloud::WindowsNativeCloudCacheProvider;
use clipline_slint_spike::cloud::{
    NativeCloudCacheContext, NativeCloudCacheProvider, NativeCloudCacheProviderError,
    NativeCloudMediaRegistry, NativeCloudRuntime,
};
use clipline_slint_spike::cloud_thumbnail::CloudThumbnailDecodeOutcome;
#[cfg(windows)]
use clipline_slint_spike::live::{
    LiveMediaCommandPort, LiveMediaRequestToken, ValidatedLiveMediaSource,
};
use clipline_test_utils::TestDir;

#[derive(Clone)]
struct FakeAccount(CloudServiceAccount);

impl CloudAccountPort for FakeAccount {
    fn snapshot(&self) -> Result<CloudServiceAccount, PortError> {
        Ok(self.0.clone())
    }

    fn apply_profile(
        &self,
        _expected_key: &CloudAccountKey,
        _expected_generation: CloudAccountGeneration,
        _patch: CloudProfilePatch,
    ) -> Result<CloudServiceAccount, PortError> {
        Err(PortError::new("profile is outside native asset tests"))
    }
}

struct FakeCredential;

impl CloudCredentialPort for FakeCredential {
    fn read(&self, _target: &str) -> Result<CloudCredential, PortError> {
        Ok(CloudCredential::new("test-only-secret"))
    }
}

struct RejectTransport;

impl CloudTransport for RejectTransport {
    fn list<'a>(
        &'a self,
        _account: &'a CloudServiceAccount,
        _credential: &'a CloudCredential,
        _request: &'a CloudListTransportRequest,
        _cancellation: &'a dyn CloudRequestFence,
        _token: &'a CloudWorkToken,
    ) -> CloudTransportFuture<'a, CloudListTransportResponse> {
        Box::pin(async { Err(PortError::new("Cloud list must not run in asset tests")) })
    }

    fn profile<'a>(
        &'a self,
        _account: &'a CloudServiceAccount,
        _credential: &'a CloudCredential,
        _cancellation: &'a dyn CloudRequestFence,
        _token: &'a CloudWorkToken,
    ) -> CloudTransportFuture<'a, CloudProfileTransport> {
        Box::pin(async { Err(PortError::new("Cloud profile must not run in asset tests")) })
    }

    fn avatar<'a>(
        &'a self,
        _account: &'a CloudServiceAccount,
        _credential: &'a CloudCredential,
        _etag: Option<&'a str>,
        _cancellation: &'a dyn CloudRequestFence,
        _token: &'a CloudWorkToken,
    ) -> CloudTransportFuture<'a, AvatarTransportResult> {
        Box::pin(async { Err(PortError::new("Cloud avatar must not run in asset tests")) })
    }
}

struct RejectLocal;

impl CatalogEffectHandler for RejectLocal {
    fn execute(&self, effect: CatalogEffect) -> Result<Option<OwnedCatalogResult>, String> {
        Err(format!("unexpected local effect: {effect:?}"))
    }
}

#[derive(Clone)]
enum AssetResponse {
    Found(Vec<u8>),
    Missing,
    Failed(String),
    Block(mpsc::Sender<(CloudAssetKind, String, u64)>),
}

struct FakeDownload {
    default: AssetResponse,
    responses: Mutex<HashMap<(String, CloudAssetKind, u64), AssetResponse>>,
    requests: Mutex<Vec<CloudAssetKey>>,
}

impl FakeDownload {
    fn new(default: AssetResponse) -> Self {
        Self {
            default,
            responses: Mutex::new(HashMap::new()),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn respond(&self, remote: &str, kind: CloudAssetKind, version: u64, response: AssetResponse) {
        self.responses
            .lock()
            .unwrap()
            .insert((remote.to_owned(), kind, version), response);
    }
}

impl DownloadPort for FakeDownload {
    fn download(
        &self,
        request: &CloudAssetRequest,
        sink: &mut DownloadSink<'_>,
        cancellation: &dyn CancellationProbe,
    ) -> Result<DownloadReceipt, CloudCacheError> {
        self.requests.lock().unwrap().push(request.asset.clone());
        let response = self
            .responses
            .lock()
            .unwrap()
            .get(&(
                request.asset.remote_clip_id().to_owned(),
                request.asset.kind(),
                request.asset.version(),
            ))
            .cloned()
            .unwrap_or_else(|| self.default.clone());
        match response {
            AssetResponse::Found(bytes) => {
                sink.write_chunk(&bytes)?;
                Ok(DownloadReceipt {
                    status: DownloadStatus::Found,
                    advertised_size_bytes: Some(bytes.len() as u64),
                })
            }
            AssetResponse::Missing => Ok(DownloadReceipt {
                status: DownloadStatus::Missing,
                advertised_size_bytes: None,
            }),
            AssetResponse::Failed(message) => Err(CloudCacheError::Download(message)),
            AssetResponse::Block(started) => {
                let _ = started.send((
                    request.asset.kind(),
                    request.asset.remote_clip_id().to_owned(),
                    request.asset.version(),
                ));
                while !cancellation.is_cancelled() {
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(CloudCacheError::Canceled)
            }
        }
    }
}

struct FakeSpace;

impl AvailableSpacePort for FakeSpace {
    fn available_bytes(&self, _cache_root: &Path) -> Result<u64, CloudCacheError> {
        Ok(u64::MAX)
    }
}

struct ExactGuard(CloudAccountFence);

impl AccountPublicationGuard for ExactGuard {
    fn is_current(&self, account: &CloudAccountFence) -> bool {
        account == &self.0
    }

    fn publish_if_current(
        &self,
        account: &CloudAccountFence,
        publication: &mut dyn FnMut() -> Result<(), CloudCacheError>,
    ) -> Result<(), CloudCacheError> {
        if !self.is_current(account) {
            return Err(CloudCacheError::StaleAccount);
        }
        publication()
    }
}

struct FakeProvider {
    context: NativeCloudCacheContext,
    calls: AtomicUsize,
    stale: AtomicBool,
}

impl NativeCloudCacheProvider for FakeProvider {
    fn cache_for(
        &self,
        _token: &CloudWorkToken,
    ) -> Result<NativeCloudCacheContext, NativeCloudCacheProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.stale.load(Ordering::SeqCst) {
            return Err(NativeCloudCacheProviderError::StaleAccount);
        }
        Ok(self.context.clone())
    }
}

struct AssetHarness {
    _directory: TestDir,
    runtime: NativeCloudRuntime,
    handler: Arc<dyn CatalogEffectHandler>,
    media: NativeCloudMediaRegistry,
    provider: Arc<FakeProvider>,
    download: Arc<FakeDownload>,
    cache: Arc<CloudCache>,
}

fn account_key() -> CloudAccountKey {
    CloudAccountKey::new("account-a").unwrap()
}

fn catalog_owner() -> CloudCatalogOwner {
    CloudCatalogOwner {
        account_key: account_key(),
        account_generation: CloudAccountGeneration::new(7),
    }
}

fn token(request: u64) -> CloudWorkToken {
    CloudWorkToken {
        window: WindowWorkToken {
            attachment: WindowAttachmentGeneration::new(3),
            foreground: ForegroundGeneration::new(5),
            request: RequestGeneration::new(request),
        },
        account_key: account_key(),
        account_generation: CloudAccountGeneration::new(7),
    }
}

fn service_account() -> CloudServiceAccount {
    CloudServiceAccount {
        snapshot: CloudAccountSnapshot {
            account_key: account_key(),
            generation: CloudAccountGeneration::new(7),
            connected: true,
            host_url: "https://clips.example".into(),
            public_url: Some("https://clips.example".into()),
            username: Some("user".into()),
            display_name: Some("User".into()),
            user_id: Some("user-1".into()),
            default_visibility: "private".into(),
            delete_local_after_upload: false,
            auto_upload_rules: false,
        },
        credential_target: Some("credential-a".into()),
        local_paths_by_clip_id: BTreeMap::new(),
    }
}

fn account_fence() -> CloudAccountFence {
    CloudAccountFence {
        account_key: account_key(),
        account_generation: CloudAccountGeneration::new(7),
        cache_namespace: CloudCacheNamespace::new("0123456789abcdef").unwrap(),
    }
}

fn make_harness(name: &str, download: Arc<FakeDownload>) -> AssetHarness {
    let directory = TestDir::new("slint-cloud-assets", name);
    let account = account_fence();
    let cache = Arc::new(
        CloudCache::open(
            directory.path().join("cache"),
            download.clone(),
            Arc::new(FakeSpace),
            Arc::new(ExactGuard(account.clone())),
        )
        .unwrap(),
    );
    let provider = Arc::new(FakeProvider {
        context: NativeCloudCacheContext::new(cache.clone(), account),
        calls: AtomicUsize::new(0),
        stale: AtomicBool::new(false),
    });
    let runtime = NativeCloudRuntime::with_transport_and_cache_provider(
        Arc::new(FakeAccount(service_account())),
        Arc::new(FakeCredential),
        Arc::new(RejectTransport),
        provider.clone(),
    )
    .unwrap();
    runtime
        .attach(
            WindowAttachmentGeneration::new(3),
            ForegroundGeneration::new(5),
            Some(catalog_owner()),
        )
        .unwrap();
    let media = runtime.media_registry();
    let handler = runtime.effect_handler(Arc::new(RejectLocal));
    AssetHarness {
        _directory: directory,
        runtime,
        handler,
        media,
        provider,
        download,
        cache,
    }
}

fn cloud_item(remote: &str, owner: &CloudWorkToken) -> CatalogItemIdentity {
    CatalogItemIdentity::Cloud {
        account_key: owner.account_key.clone(),
        account_generation: owner.account_generation,
        remote_clip_id: RemoteClipId::new(remote).unwrap(),
    }
}

fn thumbnail_effect(remote: &str, version: u64, request: u64) -> CatalogEffect {
    let token = token(request);
    let descriptor = CloudThumbnailDescriptor::new(cloud_item(remote, &token), version).unwrap();
    let owner = CloudThumbnailOwner::new(token, descriptor).unwrap();
    CatalogEffect::LoadCloudThumbnail {
        request: CloudThumbnailRequest::new(owner).unwrap(),
    }
}

fn jpeg_bytes(width: u32, height: u32) -> Vec<u8> {
    let pixels = vec![83_u8; (u64::from(width) * u64::from(height) * 3) as usize];
    let mut encoded = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut encoded, 80)
        .encode(&pixels, width, height, image::ExtendedColorType::Rgb8)
        .unwrap();
    encoded
}

fn media_owner(remote: &str, request: u64) -> CloudReviewMediaOwner {
    let token = token(request);
    CloudReviewMediaOwner::new(token.clone(), cloud_item(remote, &token)).unwrap()
}

fn media_effect(remote: &str, version: u64, request: u64) -> CatalogEffect {
    CatalogEffect::PrepareCloudReviewMedia {
        request: CloudReviewMediaRequest::new(media_owner(remote, request), version, Some(4))
            .unwrap(),
    }
}

fn prepared(result: OwnedCatalogResult) -> (CloudReviewMediaOwner, PreparedCloudReviewMedia) {
    let ExpectedResultOwner::CloudReviewMedia(expected) = result.expected else {
        panic!("expected Cloud review owner");
    };
    let CatalogResult::CloudReviewMediaPrepared { owner, media } = result.result else {
        panic!("expected prepared Cloud review media");
    };
    assert_eq!(owner, expected);
    (owner, media)
}

#[test]
fn stale_window_and_account_are_rejected_before_provider_or_download() {
    let download = Arc::new(FakeDownload::new(AssetResponse::Found(vec![1, 2, 3])));
    let harness = make_harness("stale-before-provider", download);

    let CatalogEffect::LoadCloudThumbnail { mut request } = thumbnail_effect("stale-window", 1, 1)
    else {
        unreachable!();
    };
    request.owner.token.window.foreground = ForegroundGeneration::new(99);
    assert!(harness
        .handler
        .execute(CatalogEffect::LoadCloudThumbnail { request })
        .is_err());

    let mut stale = token(2);
    stale.account_generation = CloudAccountGeneration::new(8);
    let item = cloud_item("stale-account", &stale);
    let owner =
        CloudThumbnailOwner::new(stale, CloudThumbnailDescriptor::new(item, 2).unwrap()).unwrap();
    assert!(harness
        .handler
        .execute(CatalogEffect::LoadCloudThumbnail {
            request: CloudThumbnailRequest::new(owner).unwrap(),
        })
        .is_err());
    assert_eq!(harness.provider.calls.load(Ordering::SeqCst), 0);
    assert!(harness.download.requests.lock().unwrap().is_empty());
    harness.runtime.shutdown().unwrap();
}

#[test]
fn provider_time_account_replacement_is_silently_dropped_as_stale_work() {
    let download = Arc::new(FakeDownload::new(AssetResponse::Found(vec![1, 2, 3])));
    let harness = make_harness("provider-stale-account", download);
    harness.provider.stale.store(true, Ordering::SeqCst);

    assert!(harness
        .handler
        .execute(thumbnail_effect("stale-thumbnail", 1, 1))
        .unwrap()
        .is_none());
    assert!(harness
        .handler
        .execute(media_effect("stale-media", 2, 2))
        .unwrap()
        .is_none());
    assert_eq!(harness.provider.calls.load(Ordering::SeqCst), 2);
    assert!(harness.download.requests.lock().unwrap().is_empty());
    assert_eq!(harness.media.lease_count(), 0);
    harness.runtime.shutdown().unwrap();
}

#[test]
fn thumbnail_uses_exact_remote_version_and_maps_ready_missing_and_bounded_failure() {
    let download = Arc::new(FakeDownload::new(AssetResponse::Found(vec![
        0xff, 0xd8, 0xff, 0xd9,
    ])));
    download.respond(
        "missing",
        CloudAssetKind::Thumbnail,
        22,
        AssetResponse::Missing,
    );
    download.respond(
        "failed",
        CloudAssetKind::Thumbnail,
        33,
        AssetResponse::Failed("x".repeat(MAX_FOREGROUND_MESSAGE_BYTES + 100)),
    );
    let harness = make_harness("thumbnail-mapping", download);

    let ready = harness
        .handler
        .execute(thumbnail_effect("ready", 11, 1))
        .unwrap()
        .unwrap();
    assert!(matches!(
        ready.result,
        CatalogResult::CloudThumbnail {
            status: PosterStatus::Ready { .. },
            ..
        }
    ));
    let missing = harness
        .handler
        .execute(thumbnail_effect("missing", 22, 2))
        .unwrap()
        .unwrap();
    assert!(matches!(
        missing.result,
        CatalogResult::CloudThumbnail {
            status: PosterStatus::Missing,
            ..
        }
    ));
    let failed = harness
        .handler
        .execute(thumbnail_effect("failed", 33, 3))
        .unwrap()
        .unwrap();
    let CatalogResult::CloudThumbnail {
        status: PosterStatus::Failed { message },
        ..
    } = failed.result
    else {
        panic!("expected failed thumbnail");
    };
    assert_eq!(message.len(), MAX_CATALOG_STRING_BYTES);

    let requests = harness.download.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0].remote_clip_id(), "ready");
    assert_eq!(requests[0].kind(), CloudAssetKind::Thumbnail);
    assert_eq!(requests[0].version(), 11);
    assert_eq!(requests[1].remote_clip_id(), "missing");
    assert_eq!(requests[1].version(), 22);
    assert_eq!(requests[2].remote_clip_id(), "failed");
    assert_eq!(requests[2].version(), 33);
    drop(requests);
    harness.runtime.shutdown().unwrap();
}

#[test]
fn native_decoder_regets_the_exact_cache_hit_and_invalidates_corrupt_owned_bytes() {
    let valid = jpeg_bytes(8, 6);
    let download = Arc::new(FakeDownload::new(AssetResponse::Found(valid)));
    download.respond(
        "corrupt",
        CloudAssetKind::Thumbnail,
        22,
        AssetResponse::Found(b"not a jpeg".to_vec()),
    );
    let harness = make_harness("thumbnail-native-decode", download);
    let decoder = harness.runtime.thumbnail_decoder();

    let CatalogEffect::LoadCloudThumbnail { request } = thumbnail_effect("valid", 11, 1) else {
        unreachable!();
    };
    let valid_owner = request.owner.clone();
    harness
        .handler
        .execute(CatalogEffect::LoadCloudThumbnail { request })
        .unwrap()
        .unwrap();
    assert!(matches!(
        decoder.decode(
            &valid_owner,
            &clipline_library::cache::CloudCancellation::default()
        ),
        CloudThumbnailDecodeOutcome::Ready { .. }
    ));
    assert_eq!(
        harness.download.requests.lock().unwrap().len(),
        1,
        "decoder must reacquire a cache hit rather than trust a path or redownload"
    );

    let CatalogEffect::LoadCloudThumbnail { request } = thumbnail_effect("corrupt", 22, 2) else {
        unreachable!();
    };
    let corrupt_owner = request.owner.clone();
    let cached = harness
        .handler
        .execute(CatalogEffect::LoadCloudThumbnail { request })
        .unwrap()
        .unwrap();
    let CatalogResult::CloudThumbnail {
        status: PosterStatus::Ready { path },
        ..
    } = cached.result
    else {
        panic!("corrupt encoded bytes are cache-ready until the bounded decoder runs");
    };
    assert!(Path::new(&path).exists());
    assert!(matches!(
        decoder.decode(
            &corrupt_owner,
            &clipline_library::cache::CloudCancellation::default()
        ),
        CloudThumbnailDecodeOutcome::Failed(_)
    ));
    assert!(
        !Path::new(&path).exists(),
        "decoder rejection must remove only its exact owned cache entry"
    );
    drop(decoder);
    harness.runtime.shutdown().unwrap();
}

#[test]
fn completed_thumbnail_lanes_do_not_exhaust_the_next_page_capacity() {
    let download = Arc::new(FakeDownload::new(AssetResponse::Found(vec![0xff, 0xd8])));
    let harness = make_harness("thumbnail-lane-reuse", download);
    for index in 0..60_u64 {
        let result = harness
            .handler
            .execute(thumbnail_effect(
                &format!("page-one-{index}"),
                index,
                index + 1,
            ))
            .unwrap()
            .unwrap();
        assert!(matches!(
            result.result,
            CatalogResult::CloudThumbnail {
                status: PosterStatus::Ready { .. },
                ..
            }
        ));
    }
    let next_page = harness
        .handler
        .execute(thumbnail_effect("page-two-first", 61, 61))
        .unwrap()
        .unwrap();
    assert!(matches!(
        next_page.result,
        CatalogResult::CloudThumbnail {
            status: PosterStatus::Ready { .. },
            ..
        }
    ));
    harness.runtime.shutdown().unwrap();
}

#[test]
fn thumbnail_and_media_lanes_run_independently_and_detach_cancels_both() {
    let (started_tx, started_rx) = mpsc::channel();
    let download = Arc::new(FakeDownload::new(AssetResponse::Block(started_tx)));
    let harness = make_harness("independent-lanes", download);
    let handler = harness.handler.clone();
    let (finished_tx, finished_rx) = mpsc::channel();
    let thumbnail = std::thread::spawn({
        let finished_tx = finished_tx.clone();
        move || {
            let result = handler.execute(thumbnail_effect("thumb-block", 1, 1));
            let _ = finished_tx.send(result);
        }
    });
    assert_eq!(
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap().0,
        CloudAssetKind::Thumbnail
    );
    let handler = harness.handler.clone();
    let media = std::thread::spawn(move || {
        let result = handler.execute(media_effect("media-block", 2, 2));
        let _ = finished_tx.send(result);
    });
    assert_eq!(
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap().0,
        CloudAssetKind::Media
    );
    assert!(matches!(
        finished_rx.try_recv(),
        Err(mpsc::TryRecvError::Empty)
    ));

    harness.runtime.detach();
    for _ in 0..2 {
        assert!(finished_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap()
            .is_none());
    }
    thumbnail.join().unwrap();
    media.join().unwrap();
    harness.runtime.shutdown().unwrap();
}

#[test]
fn exact_cancel_does_not_terminate_wrong_media_owner() {
    let (started_tx, started_rx) = mpsc::channel();
    let download = Arc::new(FakeDownload::new(AssetResponse::Block(started_tx)));
    let harness = make_harness("exact-media-cancel", download);
    let exact = media_owner("exact", 1);
    let handler = harness.handler.clone();
    let (finished_tx, finished_rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let result = handler.execute(CatalogEffect::PrepareCloudReviewMedia {
            request: CloudReviewMediaRequest::new(exact, 1, Some(4)).unwrap(),
        });
        let _ = finished_tx.send(result);
    });
    started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(!harness
        .media
        .cancel_media(&media_owner("wrong", 2))
        .unwrap());
    assert!(matches!(
        finished_rx.try_recv(),
        Err(mpsc::TryRecvError::Empty)
    ));
    assert!(harness
        .media
        .cancel_media(&media_owner("exact", 1))
        .unwrap());
    assert!(finished_rx
        .recv_timeout(Duration::from_secs(1))
        .unwrap()
        .unwrap()
        .is_none());
    worker.join().unwrap();
    harness.runtime.shutdown().unwrap();
}

#[test]
fn media_lease_transfers_only_for_exact_owner_id_and_path() {
    let download = Arc::new(FakeDownload::new(AssetResponse::Found(vec![1, 2, 3, 4])));
    let harness = make_harness("media-transfer", download);
    let completion = harness
        .handler
        .execute(media_effect("review", 41, 1))
        .unwrap()
        .unwrap();
    let (owner, media) = prepared(completion);
    assert_eq!(harness.media.lease_count(), 1);
    assert_eq!(harness.cache.playback_lease_count(), 1);

    assert!(harness
        .media
        .take_media(&media_owner("wrong", 2), &media)
        .is_err());
    let wrong_path = PreparedCloudReviewMedia::new("wrong.mp4", media.lease_id).unwrap();
    assert!(harness.media.take_media(&owner, &wrong_path).is_err());
    let wrong_id = PreparedCloudReviewMedia::new(
        media.path.clone(),
        CloudMediaLeaseId::new(media.lease_id.get() + 100).unwrap(),
    )
    .unwrap();
    assert!(harness.media.take_media(&owner, &wrong_id).is_err());
    assert_eq!(harness.media.lease_count(), 1);

    let lease = harness.media.take_media(&owner, &media).unwrap();
    assert_eq!(lease.path().display().to_string(), media.path);
    assert_eq!(harness.media.lease_count(), 0);
    assert_eq!(harness.cache.playback_lease_count(), 1);
    drop(lease);
    assert_eq!(harness.cache.playback_lease_count(), 0);
    harness.runtime.shutdown().unwrap();
}

#[test]
fn duplicate_release_is_idempotent_and_frees_playback_protection() {
    let download = Arc::new(FakeDownload::new(AssetResponse::Found(vec![1, 2, 3, 4])));
    let harness = make_harness("duplicate-release", download);
    let (_, media) = prepared(
        harness
            .handler
            .execute(media_effect("release", 1, 1))
            .unwrap()
            .unwrap(),
    );
    assert_eq!(harness.cache.playback_lease_count(), 1);
    harness.media.release_media(media.lease_id).unwrap();
    harness.media.release_media(media.lease_id).unwrap();
    assert_eq!(harness.media.lease_count(), 0);
    assert_eq!(harness.cache.playback_lease_count(), 0);
    harness.runtime.shutdown().unwrap();
}

#[test]
fn media_registry_capacity_drops_rejected_lease_and_recovers_after_release() {
    let download = Arc::new(FakeDownload::new(AssetResponse::Found(vec![1, 2, 3, 4])));
    let harness = make_harness("registry-capacity", download);
    let mut prepared_media = Vec::new();
    for index in 0..4_u64 {
        let (_, media) = prepared(
            harness
                .handler
                .execute(media_effect(
                    &format!("capacity-{index}"),
                    index + 1,
                    index + 1,
                ))
                .unwrap()
                .unwrap(),
        );
        prepared_media.push(media);
    }
    assert_eq!(harness.media.lease_count(), 4);
    assert_eq!(harness.cache.playback_lease_count(), 4);

    let rejected = harness
        .handler
        .execute(media_effect("capacity-rejected", 10, 10))
        .unwrap()
        .unwrap();
    assert!(matches!(
        rejected.result,
        CatalogResult::OperationFailed {
            owner: CatalogOperationOwner::CloudReviewMedia { .. },
            ..
        }
    ));
    assert_eq!(harness.media.lease_count(), 4);
    assert_eq!(harness.cache.playback_lease_count(), 4);

    harness
        .media
        .release_media(prepared_media[0].lease_id)
        .unwrap();
    let (_, replacement) = prepared(
        harness
            .handler
            .execute(media_effect("capacity-recovered", 11, 11))
            .unwrap()
            .unwrap(),
    );
    assert_eq!(harness.media.lease_count(), 4);
    for media in prepared_media.into_iter().skip(1) {
        harness.media.release_media(media.lease_id).unwrap();
    }
    harness.media.release_media(replacement.lease_id).unwrap();
    assert_eq!(harness.cache.playback_lease_count(), 0);
    harness.runtime.shutdown().unwrap();
}

#[test]
fn shutdown_cancels_lanes_and_drops_all_untransferred_leases() {
    let download = Arc::new(FakeDownload::new(AssetResponse::Found(vec![1, 2, 3, 4])));
    let harness = make_harness("shutdown-leases", download);
    let (owner, media) = prepared(
        harness
            .handler
            .execute(media_effect("shutdown", 1, 1))
            .unwrap()
            .unwrap(),
    );
    assert_eq!(harness.media.lease_count(), 1);
    assert_eq!(harness.cache.playback_lease_count(), 1);
    harness.runtime.shutdown().unwrap();
    assert_eq!(harness.media.lease_count(), 0);
    assert_eq!(harness.cache.playback_lease_count(), 0);
    assert!(harness.media.take_media(&owner, &media).is_err());
}

#[cfg(windows)]
fn playback_snapshot(
    open: u64,
    phase: PlaybackPhase,
    path: Option<std::path::PathBuf>,
) -> SessionUpdate {
    let generation = WorkGeneration::new(open, 0);
    SessionUpdate {
        sequence: open,
        token: PipelineToken::new(generation, 0),
        payload: SessionUpdatePayload::Snapshot(PlaybackSnapshot {
            phase,
            generation,
            path,
            position: PlaybackTime::new(0, 1).unwrap(),
            duration: None,
            audio_track_indices: Vec::new(),
            volume: 1.0,
            rate: 1.0,
            playing_intent: false,
        }),
    }
}

#[cfg(windows)]
#[test]
fn exact_registry_handoff_keeps_cache_pin_through_playback_ready_and_close() {
    let download = Arc::new(FakeDownload::new(AssetResponse::Found(vec![1, 2, 3, 4])));
    let harness = make_harness("live-media-handoff", download);
    let (owner, prepared) = prepared(
        harness
            .handler
            .execute(media_effect("live-handoff", 1, 1))
            .unwrap()
            .unwrap(),
    );
    let expected_path = std::path::PathBuf::from(&prepared.path);
    let lease = harness.media.take_media(&owner, &prepared).unwrap();
    let source = ValidatedLiveMediaSource::cached_cloud(lease);
    assert_eq!(source.path(), expected_path);
    let (client, mut playback) = session_channel();
    let port = LiveMediaCommandPort::new_dynamic(Arc::new(client));
    port.open(LiveMediaRequestToken::new(1).unwrap(), source)
        .unwrap();
    assert_eq!(
        playback.try_recv_command().unwrap().command,
        PlaybackCommand::Open {
            path: expected_path.clone()
        }
    );
    assert_eq!(harness.cache.playback_lease_count(), 1);

    port.accept_session_update(&playback_snapshot(0, PlaybackPhase::Closed, None));
    port.accept_session_update(&playback_snapshot(
        1,
        PlaybackPhase::Paused,
        Some(expected_path),
    ));
    assert_eq!(harness.cache.playback_lease_count(), 1);
    assert!(port.close().unwrap());
    assert_eq!(
        playback.try_recv_command().unwrap().command,
        PlaybackCommand::Close
    );
    port.accept_session_update(&playback_snapshot(2, PlaybackPhase::Closed, None));
    assert_eq!(harness.cache.playback_lease_count(), 0);
    harness.runtime.shutdown().unwrap();
}

#[cfg(windows)]
struct RotatingCredential(Mutex<String>);

#[cfg(windows)]
impl CloudCredentialPort for RotatingCredential {
    fn read(&self, _target: &str) -> Result<CloudCredential, PortError> {
        Ok(CloudCredential::new(self.0.lock().unwrap().clone()))
    }
}

#[cfg(windows)]
struct RacingCredential {
    calls: AtomicUsize,
    first_entered: (Mutex<bool>, std::sync::Condvar),
    release_first: (Mutex<bool>, std::sync::Condvar),
}

#[cfg(windows)]
impl RacingCredential {
    fn wait_for_first(&self) {
        let entered = self.first_entered.0.lock().unwrap();
        let (entered, timeout) = self
            .first_entered
            .1
            .wait_timeout_while(entered, Duration::from_secs(5), |entered| !*entered)
            .unwrap();
        assert!(
            *entered && !timeout.timed_out(),
            "first credential read did not start"
        );
    }

    fn release_first(&self) {
        *self.release_first.0.lock().unwrap() = true;
        self.release_first.1.notify_all();
    }
}

#[cfg(windows)]
impl CloudCredentialPort for RacingCredential {
    fn read(&self, _target: &str) -> Result<CloudCredential, PortError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            *self.first_entered.0.lock().unwrap() = true;
            self.first_entered.1.notify_all();
            let release = self.release_first.0.lock().unwrap();
            let (release, timeout) = self
                .release_first
                .1
                .wait_timeout_while(release, Duration::from_secs(5), |release| !*release)
                .unwrap();
            assert!(
                *release && !timeout.timed_out(),
                "first credential read was not released"
            );
            return Ok(CloudCredential::new("first-secret"));
        }
        Ok(CloudCredential::new("rotated-secret"))
    }
}

#[cfg(windows)]
#[test]
fn production_cache_provider_reuses_exact_secret_and_rebuilds_after_rotation() {
    use clipline_library::cloud::settings::SettingsCloudAccountPort;

    let directory = TestDir::new("slint-cloud-assets", "credential-rotation");
    let profile = SettingsProfile::isolated(directory.path());
    let mut settings = AppSettings {
        media_dir: profile.default_media_dir().display().to_string(),
        ..AppSettings::default()
    };
    settings.cloud.host_url = "https://clips.example".into();
    settings.cloud.connected_user_id = Some("user-1".into());
    settings.cloud.connected_username = Some("user".into());
    settings.cloud.credential_target = Some("credential-a".into());
    settings.save_to(profile.settings_path()).unwrap();
    let store = SettingsStore::open(profile);
    let account = SettingsCloudAccountPort::new(store.clone())
        .snapshot()
        .unwrap();
    let token = CloudWorkToken {
        window: WindowWorkToken {
            attachment: WindowAttachmentGeneration::new(1),
            foreground: ForegroundGeneration::new(1),
            request: RequestGeneration::new(1),
        },
        account_key: account.snapshot.account_key,
        account_generation: account.snapshot.generation,
    };
    let credential = Arc::new(RotatingCredential(Mutex::new("first-secret".into())));
    let provider = WindowsNativeCloudCacheProvider::new(store, credential.clone());
    let first = provider.cache_for(&token).unwrap();
    let same = provider.cache_for(&token).unwrap();
    assert!(Arc::ptr_eq(first.cache(), same.cache()));

    *credential.0.lock().unwrap() = "rotated-secret".into();
    let rotated = provider.cache_for(&token).unwrap();
    assert!(!Arc::ptr_eq(first.cache(), rotated.cache()));
    assert_eq!(first.account(), rotated.account());
}

#[cfg(windows)]
#[test]
fn older_credential_read_cannot_overwrite_a_newer_cache_slot() {
    use clipline_library::cloud::settings::SettingsCloudAccountPort;

    let directory = TestDir::new("slint-cloud-assets", "credential-rotation-race");
    let profile = SettingsProfile::isolated(directory.path());
    let mut settings = AppSettings {
        media_dir: profile.default_media_dir().display().to_string(),
        ..AppSettings::default()
    };
    settings.cloud.host_url = "https://clips.example".into();
    settings.cloud.connected_user_id = Some("user-1".into());
    settings.cloud.connected_username = Some("user".into());
    settings.cloud.credential_target = Some("credential-a".into());
    settings.save_to(profile.settings_path()).unwrap();
    let store = SettingsStore::open(profile);
    let account = SettingsCloudAccountPort::new(store.clone())
        .snapshot()
        .unwrap();
    let token = CloudWorkToken {
        window: WindowWorkToken {
            attachment: WindowAttachmentGeneration::new(1),
            foreground: ForegroundGeneration::new(1),
            request: RequestGeneration::new(1),
        },
        account_key: account.snapshot.account_key,
        account_generation: account.snapshot.generation,
    };
    let credential = Arc::new(RacingCredential {
        calls: AtomicUsize::new(0),
        first_entered: (Mutex::new(false), std::sync::Condvar::new()),
        release_first: (Mutex::new(false), std::sync::Condvar::new()),
    });
    let provider = Arc::new(WindowsNativeCloudCacheProvider::new(
        store,
        credential.clone(),
    ));

    let first = std::thread::spawn({
        let provider = Arc::clone(&provider);
        let token = token.clone();
        move || provider.cache_for(&token).unwrap()
    });
    credential.wait_for_first();
    let (second_tx, second_rx) = mpsc::channel();
    let second_worker = std::thread::spawn({
        let provider = Arc::clone(&provider);
        let token = token.clone();
        move || second_tx.send(provider.cache_for(&token).unwrap()).unwrap()
    });

    let early_second = second_rx.recv_timeout(Duration::from_millis(100)).ok();
    credential.release_first();
    let second =
        early_second.unwrap_or_else(|| second_rx.recv_timeout(Duration::from_secs(5)).unwrap());
    first.join().unwrap();
    second_worker.join().unwrap();

    let current = provider.cache_for(&token).unwrap();
    assert!(
        Arc::ptr_eq(second.cache(), current.cache()),
        "an older credential read overwrote the cache slot installed for the rotated secret"
    );
}
