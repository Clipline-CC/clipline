use std::collections::HashMap;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use chrono::Utc;
use clipline_library::ports::CloudCredential;
use clipline_library::protocol::{
    sha256_hex, CloudApiBase, CreateUploadRequest, UploadProgressResponse,
};
use clipline_library::{
    client_clip_id_for_payload, ActiveFileRegistry, CloudAccountGeneration, CloudAccountKey,
    LocalClipId, LocalLibraryRepository, OwnedUploadTemp, PreparedUploadPayload, ReadyUpload,
    StandardRepositoryFileSystem, UploadAccountFence, UploadAccountOwner, UploadCancellation,
    UploadDeletePermit, UploadDeletionPort, UploadEventPort, UploadEventPortError,
    UploadGeneration, UploadIntent, UploadJobOutcome, UploadPhase, UploadPreparationPort,
    UploadRecord, UploadRecordCursor, UploadRecordError, UploadRecordErrorKind, UploadRecordPort,
    UploadRemoteOutcome, UploadRemotePort, UploadRequestError, UploadService, UploadServiceEvent,
    UploadStartError, UploadStartRequest, UploadTransportPort, UploadWorkError, ValidatedClipPath,
    MAX_CLIP_DETAIL_AUDIO_TRACKS, MAX_CLIP_DETAIL_FIELD_BYTES, MAX_UPLOAD_DESCRIPTION_UTF16,
    MAX_UPLOAD_TITLE_UTF16,
};
use clipline_test_utils::TestDir;

#[derive(Default)]
struct FakeAccountFence {
    current: Mutex<Option<UploadAccountOwner>>,
    checks: AtomicUsize,
}

impl FakeAccountFence {
    fn set(&self, owner: Option<UploadAccountOwner>) {
        *self.current.lock().unwrap() = owner;
    }
}

impl UploadAccountFence for FakeAccountFence {
    fn is_current(&self, owner: &UploadAccountOwner) -> bool {
        self.checks.fetch_add(1, Ordering::Relaxed);
        self.current.lock().unwrap().as_ref() == Some(owner)
    }
}

fn record_key(record: &UploadRecord) -> (String, u64, String) {
    (
        record.token.account_key.as_str().to_owned(),
        record.token.account_generation.get(),
        record.token.local_clip_id.as_str().to_owned(),
    )
}

#[derive(Default)]
struct FakeRecords {
    records: Mutex<HashMap<(String, u64, String), UploadRecordCursor>>,
    writes: AtomicUsize,
    generation: std::sync::atomic::AtomicU64,
    legacy_generations: Mutex<Vec<(clipline_library::ClipPathIdentity, u64)>>,
}

impl FakeRecords {
    fn current(&self, token: &clipline_library::DurableUploadToken) -> UploadRecordCursor {
        self.records
            .lock()
            .unwrap()
            .get(&(
                token.account_key.as_str().to_owned(),
                token.account_generation.get(),
                token.local_clip_id.as_str().to_owned(),
            ))
            .unwrap()
            .clone()
    }
}

impl UploadRecordPort for FakeRecords {
    fn allocate_generation(
        &self,
        _owner: &UploadAccountOwner,
        _local_clip_id: &LocalClipId,
        source: &ValidatedClipPath,
    ) -> Result<UploadGeneration, UploadRecordError> {
        let legacy_floor = self
            .legacy_generations
            .lock()
            .unwrap()
            .iter()
            .filter(|(path, _)| path == source.comparison_identity())
            .map(|(_, generation)| *generation)
            .max()
            .unwrap_or(0);
        let generation = self
            .generation
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                value.max(legacy_floor).checked_add(1)
            })
            .map_err(|_| {
                UploadRecordError::new(
                    UploadRecordErrorKind::Persistence,
                    "upload generation exhausted",
                )
            })?;
        Ok(UploadGeneration::new(generation.max(legacy_floor) + 1))
    }

    fn load(
        &self,
        owner: &UploadAccountOwner,
        local_clip_id: &LocalClipId,
    ) -> Result<Option<UploadRecordCursor>, UploadRecordError> {
        Ok(self
            .records
            .lock()
            .unwrap()
            .get(&(
                owner.account_key.as_str().to_owned(),
                owner.account_generation.get(),
                local_clip_id.as_str().to_owned(),
            ))
            .cloned())
    }

    fn admit(&self, record: UploadRecord) -> Result<UploadRecordCursor, UploadRecordError> {
        let mut records = self.records.lock().unwrap();
        let key = record_key(&record);
        if let Some(current) = records.get(&key) {
            if !current.record.phase.is_terminal() {
                return Err(UploadRecordError::new(
                    UploadRecordErrorKind::Contended,
                    "active record",
                ));
            }
        }
        let revision = records.get(&key).map_or(1, |current| current.revision + 1);
        let cursor = UploadRecordCursor { revision, record };
        records.insert(key, cursor.clone());
        self.writes.fetch_add(1, Ordering::Relaxed);
        Ok(cursor)
    }

    fn compare_exchange(
        &self,
        expected: &UploadRecordCursor,
        replacement: UploadRecord,
    ) -> Result<UploadRecordCursor, UploadRecordError> {
        let mut records = self.records.lock().unwrap();
        let key = record_key(&expected.record);
        if record_key(&replacement) != key
            || records.get(&key) != Some(expected)
            || replacement.token != expected.record.token
        {
            return Err(UploadRecordError::new(
                UploadRecordErrorKind::Superseded,
                "record changed",
            ));
        }
        let cursor = UploadRecordCursor {
            revision: expected.revision + 1,
            record: replacement,
        };
        records.insert(key, cursor.clone());
        self.writes.fetch_add(1, Ordering::Relaxed);
        Ok(cursor)
    }
}

#[derive(Default)]
struct FakeEvents {
    events: Mutex<Vec<UploadServiceEvent>>,
}

impl UploadEventPort for FakeEvents {
    fn try_publish(&self, event: UploadServiceEvent) -> Result<(), UploadEventPortError> {
        self.events.lock().unwrap().push(event);
        Ok(())
    }
}

struct FakePreparation {
    registry: ActiveFileRegistry,
    calls: AtomicUsize,
    block: AtomicBool,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    owned_temp: AtomicBool,
    temp_path: Mutex<Option<std::path::PathBuf>>,
}

impl FakePreparation {
    fn new(registry: ActiveFileRegistry) -> Self {
        Self {
            registry,
            calls: AtomicUsize::new(0),
            block: AtomicBool::new(false),
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
            owned_temp: AtomicBool::new(false),
            temp_path: Mutex::new(None),
        }
    }

    async fn wait_entered(&self) {
        while self.calls.load(Ordering::Acquire) == 0 {
            self.entered.notified().await;
        }
    }

    fn unblock(&self) {
        self.block.store(false, Ordering::Release);
        self.release.notify_waiters();
    }
}

impl UploadPreparationPort for FakePreparation {
    fn prepare<'a>(
        &'a self,
        source: &'a clipline_library::UploadSourceLease,
        intent: &'a UploadIntent,
        cancellation: &'a UploadCancellation,
    ) -> clipline_library::UploadFuture<'a, Result<PreparedUploadPayload, UploadWorkError>> {
        Box::pin(async move {
            assert!(self.registry.is_current(source.token()));
            self.calls.fetch_add(1, Ordering::AcqRel);
            self.entered.notify_waiters();
            if self.block.load(Ordering::Acquire) {
                tokio::select! {
                    () = self.release.notified() => {},
                    () = cancellation.cancelled() => return Err(UploadWorkError::Canceled),
                }
            }
            cancellation
                .check()
                .map_err(|_| UploadWorkError::Canceled)?;

            let bytes = if self.owned_temp.load(Ordering::Acquire) {
                b"selected audio payload".to_vec()
            } else {
                std::fs::read(source.canonical_path())
                    .map_err(|error| UploadWorkError::failed(error.to_string()))?
            };
            let checksum = sha256_hex(&bytes);
            let client_clip_id =
                client_clip_id_for_payload(&source.token().local_clip_id, &checksum)
                    .map_err(|error| UploadWorkError::failed(error.to_string()))?;
            let request = create_request(
                intent,
                bytes.len() as u64,
                checksum,
                client_clip_id.as_str(),
            );
            if self.owned_temp.load(Ordering::Acquire) {
                let mut temp = OwnedUploadTemp::create_near(source.canonical_path())
                    .map_err(|error| UploadWorkError::failed(error.to_string()))?;
                temp.file_mut()
                    .map_err(|error| UploadWorkError::failed(error.to_string()))?
                    .write_all(&bytes)
                    .map_err(|error| UploadWorkError::failed(error.to_string()))?;
                *self.temp_path.lock().unwrap() = Some(temp.path().to_path_buf());
                PreparedUploadPayload::owned(
                    temp,
                    request,
                    intent.description.clone(),
                    client_clip_id,
                    source.token(),
                )
            } else {
                PreparedUploadPayload::original(
                    source,
                    request,
                    intent.description.clone(),
                    client_clip_id,
                )
            }
        })
    }
}

fn create_request(
    intent: &UploadIntent,
    size: u64,
    checksum: String,
    client_clip_id: &str,
) -> CreateUploadRequest {
    CreateUploadRequest {
        client_clip_id: Some(client_clip_id.into()),
        title: intent.title.clone().unwrap_or_else(|| "clip".into()),
        description: None,
        game_name: None,
        game_id: None,
        game_executable: None,
        source_type: Some("manual".into()),
        recorded_at: None,
        duration_ms: Some(1_000),
        file_size_bytes: size,
        checksum_sha256: checksum,
        container: "mp4".into(),
        video_codec: Some("h264".into()),
        audio_codec: Some("opus".into()),
        width: Some(640),
        height: Some(360),
        fps: Some(60.0),
        visibility: Some(intent.visibility.clone()),
        markers: None,
    }
}

struct FakeTransport {
    calls: AtomicUsize,
    block: AtomicBool,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    remote_clip_id: Mutex<String>,
}

impl Default for FakeTransport {
    fn default() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            block: AtomicBool::new(false),
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
            remote_clip_id: Mutex::new("remote-1".into()),
        }
    }
}

impl FakeTransport {
    async fn wait_entered(&self) {
        while self.calls.load(Ordering::Acquire) == 0 {
            self.entered.notified().await;
        }
    }

    fn unblock(&self) {
        self.block.store(false, Ordering::Release);
        self.release.notify_waiters();
    }
}

impl UploadTransportPort for FakeTransport {
    fn upload<'a>(
        &'a self,
        _endpoint: &'a clipline_library::UploadEndpoint,
        payload: &'a PreparedUploadPayload,
        cancellation: &'a UploadCancellation,
        on_progress: &'a mut (dyn FnMut(&UploadProgressResponse) + Send),
    ) -> clipline_library::UploadFuture<'a, Result<UploadProgressResponse, UploadWorkError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::AcqRel);
            self.entered.notify_waiters();
            let remote_clip_id = self.remote_clip_id.lock().unwrap().clone();
            let first = progress(
                payload.request().file_size_bytes,
                1,
                "uploading",
                &remote_clip_id,
            );
            on_progress(&first);
            if self.block.load(Ordering::Acquire) {
                tokio::select! {
                    () = self.release.notified() => {},
                    () = cancellation.cancelled() => return Err(UploadWorkError::Canceled),
                }
            }
            cancellation
                .check()
                .map_err(|_| UploadWorkError::Canceled)?;
            let done = progress(
                payload.request().file_size_bytes,
                payload.request().file_size_bytes,
                "completed",
                &remote_clip_id,
            );
            on_progress(&done);
            Ok(done)
        })
    }
}

fn progress(
    size: u64,
    received: u64,
    status: &str,
    remote_clip_id: &str,
) -> UploadProgressResponse {
    UploadProgressResponse {
        upload_id: "upload-1".into(),
        clip_id: remote_clip_id.into(),
        mode: "single_put".into(),
        status: status.into(),
        file_size_bytes: size,
        part_size_bytes: size,
        received_size_bytes: received.min(size),
        total_parts: 1,
        received_part_count: u16::from(received >= size),
        missing_part_count: u16::from(received < size),
        next_part_number: (received < size).then_some(1),
        progress_basis_points: (received.min(size) * 10_000)
            .checked_div(size)
            .map_or(0, |basis_points| u16::try_from(basis_points).unwrap()),
        failure_reason: None,
        recovery_action: None,
        expires_at: Utc::now(),
        received_parts: Vec::new(),
        missing_parts: Vec::new(),
    }
}

struct FakeRemote {
    wait_block: AtomicBool,
    wait_entered: tokio::sync::Notify,
    wait_release: tokio::sync::Notify,
    probe_block: AtomicBool,
    probe_entered: tokio::sync::Notify,
    probe_release: tokio::sync::Notify,
    probe_fail: AtomicBool,
}

impl Default for FakeRemote {
    fn default() -> Self {
        Self {
            wait_block: AtomicBool::new(false),
            wait_entered: tokio::sync::Notify::new(),
            wait_release: tokio::sync::Notify::new(),
            probe_block: AtomicBool::new(false),
            probe_entered: tokio::sync::Notify::new(),
            probe_release: tokio::sync::Notify::new(),
            probe_fail: AtomicBool::new(false),
        }
    }
}

impl UploadRemotePort for FakeRemote {
    fn wait_until_ready<'a>(
        &'a self,
        _endpoint: &'a clipline_library::UploadEndpoint,
        remote_clip_id: &'a str,
        visibility: &'a str,
        cancellation: &'a UploadCancellation,
    ) -> clipline_library::UploadFuture<'a, Result<UploadRemoteOutcome, UploadWorkError>> {
        Box::pin(async move {
            self.wait_entered.notify_waiters();
            if self.wait_block.load(Ordering::Acquire) {
                tokio::select! {
                    () = self.wait_release.notified() => {},
                    () = cancellation.cancelled() => return Err(UploadWorkError::Canceled),
                }
            }
            Ok(UploadRemoteOutcome::Ready(ReadyUpload {
                remote_clip_id: remote_clip_id.into(),
                visibility: visibility.into(),
                remote_url: (visibility != "private").then(|| "https://example.test/c/1".into()),
            }))
        })
    }

    fn probe_media<'a>(
        &'a self,
        _endpoint: &'a clipline_library::UploadEndpoint,
        _remote_clip_id: &'a str,
        cancellation: &'a UploadCancellation,
    ) -> clipline_library::UploadFuture<'a, Result<(), UploadWorkError>> {
        Box::pin(async move {
            self.probe_entered.notify_waiters();
            if self.probe_block.load(Ordering::Acquire) {
                tokio::select! {
                    () = self.probe_release.notified() => {},
                    () = cancellation.cancelled() => return Err(UploadWorkError::Canceled),
                }
            }
            if self.probe_fail.load(Ordering::Acquire) {
                return Err(UploadWorkError::failed("injected media probe failure"));
            }
            Ok(())
        })
    }
}

struct FakeDeletion {
    calls: AtomicUsize,
    fail: AtomicBool,
}

impl Default for FakeDeletion {
    fn default() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            fail: AtomicBool::new(false),
        }
    }
}

impl UploadDeletionPort for FakeDeletion {
    fn delete_local(&self, permit: &UploadDeletePermit) -> Result<(), UploadWorkError> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        if self.fail.load(Ordering::Acquire) {
            return Err(UploadWorkError::failed(
                "injected repository cleanup failure",
            ));
        }
        permit
            .delete_source_if_current()
            .map_err(|error| UploadWorkError::failed(error.to_string()))
    }
}

struct Harness {
    _directory: TestDir,
    registry: ActiveFileRegistry,
    source: ValidatedClipPath,
    accounts: Arc<FakeAccountFence>,
    preparation: Arc<FakePreparation>,
    transport: Arc<FakeTransport>,
    remote: Arc<FakeRemote>,
    deletion: Arc<FakeDeletion>,
    records: Arc<FakeRecords>,
    events: Arc<FakeEvents>,
    service: UploadService,
}

impl Harness {
    fn new(name: &str) -> Self {
        let directory = TestDir::new("clipline-upload-service", name);
        let root = directory.path().join("media");
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("clip.mp4");
        std::fs::write(&path, b"original clip payload").unwrap();
        let registry = ActiveFileRegistry::new();
        let repository = LocalLibraryRepository::with_seams(
            &root,
            Arc::new(StandardRepositoryFileSystem),
            Arc::new(registry.clone()),
        )
        .unwrap();
        let source = repository
            .validate_clip_path(&path.to_string_lossy())
            .unwrap();
        let accounts = Arc::new(FakeAccountFence::default());
        accounts.set(Some(owner("account-a", 1)));
        let preparation = Arc::new(FakePreparation::new(registry.clone()));
        let transport = Arc::new(FakeTransport::default());
        let remote = Arc::new(FakeRemote::default());
        let deletion = Arc::new(FakeDeletion::default());
        let records = Arc::new(FakeRecords::default());
        let events = Arc::new(FakeEvents::default());
        let service = UploadService::new(
            registry.clone(),
            accounts.clone(),
            preparation.clone(),
            transport.clone(),
            remote.clone(),
            deletion.clone(),
            records.clone(),
            events.clone(),
        );
        Self {
            _directory: directory,
            registry,
            source,
            accounts,
            preparation,
            transport,
            remote,
            deletion,
            records,
            events,
            service,
        }
    }

    fn request(&self, delete: bool) -> UploadStartRequest {
        UploadStartRequest {
            endpoint: clipline_library::UploadEndpoint::new(
                owner("account-a", 1),
                CloudApiBase::parse("http://127.0.0.1:1", true).unwrap(),
                CloudCredential::new("secret"),
            ),
            source: self.source.clone(),
            intent: UploadIntent {
                delete_local_after_upload: delete,
                ..UploadIntent::default()
            },
        }
    }
}

fn owner(key: &str, generation: u64) -> UploadAccountOwner {
    UploadAccountOwner::new(
        CloudAccountKey::new(key).unwrap(),
        CloudAccountGeneration::new(generation),
    )
}

#[tokio::test]
async fn invalid_intents_are_rejected_before_account_generation_lease_record_or_event_work() {
    let harness = Harness::new("invalid-before-work");
    let invalid = vec![
        UploadIntent {
            visibility: "friends".into(),
            ..UploadIntent::default()
        },
        UploadIntent {
            title: Some("😀".repeat(MAX_UPLOAD_TITLE_UTF16 / 2 + 1)),
            ..UploadIntent::default()
        },
        UploadIntent {
            description: Some("😀".repeat(MAX_UPLOAD_DESCRIPTION_UTF16 / 2 + 1)),
            ..UploadIntent::default()
        },
        UploadIntent {
            audio_track_ids: Some(
                (0..=MAX_CLIP_DETAIL_AUDIO_TRACKS)
                    .map(|index| format!("track-{index}"))
                    .collect(),
            ),
            ..UploadIntent::default()
        },
        UploadIntent {
            audio_track_ids: Some(vec![" ".into()]),
            ..UploadIntent::default()
        },
        UploadIntent {
            audio_track_ids: Some(vec!["same".into(), "same".into()]),
            ..UploadIntent::default()
        },
        UploadIntent {
            audio_track_ids: Some(vec!["x".repeat(MAX_CLIP_DETAIL_FIELD_BYTES + 1)]),
            ..UploadIntent::default()
        },
    ];

    for intent in invalid {
        let mut request = harness.request(false);
        request.intent = intent;
        assert!(matches!(
            harness.service.start(request).unwrap_err(),
            UploadStartError::InvalidRequest(_)
        ));
    }

    assert_eq!(harness.accounts.checks.load(Ordering::Acquire), 0);
    assert_eq!(harness.records.generation.load(Ordering::Acquire), 0);
    assert_eq!(harness.records.writes.load(Ordering::Acquire), 0);
    assert_eq!(harness.preparation.calls.load(Ordering::Acquire), 0);
    assert_eq!(harness.transport.calls.load(Ordering::Acquire), 0);
    assert_eq!(harness.deletion.calls.load(Ordering::Acquire), 0);
    assert!(harness.events.events.lock().unwrap().is_empty());
    assert!(!harness
        .registry
        .is_identity_active(harness.source.file_identity()));
    assert_eq!(harness.service.active_count(), 0);
}

#[test]
fn intent_limits_use_utf16_and_pin_path_and_audio_boundaries() {
    let mut intent = UploadIntent {
        title: Some("😀".repeat(MAX_UPLOAD_TITLE_UTF16 / 2)),
        description: Some("😀".repeat(MAX_UPLOAD_DESCRIPTION_UTF16 / 2)),
        audio_track_ids: Some(
            (0..MAX_CLIP_DETAIL_AUDIO_TRACKS)
                .map(|index| format!("track-{index}"))
                .collect(),
        ),
        ..UploadIntent::default()
    };
    assert!(intent.validate_for_path("clip.mp4").is_ok());

    intent.title = Some("😀".repeat(MAX_UPLOAD_TITLE_UTF16 / 2 + 1));
    assert_eq!(
        intent.validate_for_path("clip.mp4").unwrap_err(),
        UploadRequestError::TitleTooLong {
            actual: MAX_UPLOAD_TITLE_UTF16 + 2,
            maximum: MAX_UPLOAD_TITLE_UTF16,
        }
    );
    intent.title = None;
    assert_eq!(
        intent
            .validate_for_path(&"x".repeat(clipline_settings::MAX_CLOUD_UPLOAD_PATH_BYTES + 1))
            .unwrap_err(),
        UploadRequestError::DisplayPathTooLong {
            actual: clipline_settings::MAX_CLOUD_UPLOAD_PATH_BYTES + 1,
            maximum: clipline_settings::MAX_CLOUD_UPLOAD_PATH_BYTES,
        }
    );
}

#[tokio::test]
async fn source_lease_is_owned_before_preparation_and_through_completion() {
    let harness = Harness::new("lease-before-prepare");
    harness.preparation.block.store(true, Ordering::Release);
    let handle = harness.service.start(harness.request(false)).unwrap();
    harness.preparation.wait_entered().await;

    assert!(harness.registry.is_current(handle.token()));
    assert_eq!(harness.service.active_count(), 1);
    harness.preparation.unblock();

    assert_eq!(handle.wait().await.outcome, UploadJobOutcome::Completed);
    harness.service.wait_idle().await;
    assert!(!harness
        .registry
        .is_identity_active(harness.source.file_identity()));
}

#[tokio::test]
async fn duplicate_admission_is_rejected_before_second_preparation() {
    let harness = Harness::new("duplicate-before-work");
    harness.preparation.block.store(true, Ordering::Release);
    let first = harness.service.start(harness.request(false)).unwrap();
    harness.preparation.wait_entered().await;

    let error = harness.service.start(harness.request(false)).unwrap_err();
    assert!(matches!(error, UploadStartError::AlreadyActive(token) if token == *first.token()));
    assert_eq!(harness.preparation.calls.load(Ordering::Acquire), 1);

    harness.preparation.unblock();
    assert_eq!(first.wait().await.outcome, UploadJobOutcome::Completed);
}

#[tokio::test]
async fn dropping_the_caller_does_not_cancel_window_independent_work() {
    let harness = Harness::new("caller-drop");
    harness.transport.block.store(true, Ordering::Release);
    let handle = harness.service.start(harness.request(false)).unwrap();
    let token = handle.token().clone();
    drop(handle);
    harness.transport.wait_entered().await;

    assert_eq!(
        harness.records.current(&token).record.phase,
        UploadPhase::Uploading
    );
    harness.transport.unblock();
    harness.service.wait_idle().await;
    assert_eq!(
        harness.records.current(&token).record.phase,
        UploadPhase::Completed
    );
}

#[tokio::test]
async fn remote_identity_is_a_durable_barrier_before_transport_continues() {
    let harness = Harness::new("remote-id-barrier");
    harness.transport.block.store(true, Ordering::Release);
    let handle = harness.service.start(harness.request(false)).unwrap();
    harness.transport.wait_entered().await;

    let record = harness.records.current(handle.token()).record;
    assert_eq!(record.phase, UploadPhase::Uploading);
    assert_eq!(record.remote_clip_id.as_deref(), Some("remote-1"));
    assert!(harness
        .events
        .events
        .lock()
        .unwrap()
        .iter()
        .any(|event| event.record.remote_clip_id.as_deref() == Some("remote-1")));

    harness.transport.unblock();
    assert_eq!(handle.wait().await.outcome, UploadJobOutcome::Completed);
}

#[tokio::test]
async fn oversized_transport_identity_never_crosses_record_or_event_boundaries() {
    let harness = Harness::new("oversized-progress-identity");
    *harness.transport.remote_clip_id.lock().unwrap() =
        "x".repeat(clipline_settings::MAX_CLOUD_UPLOAD_ID_BYTES + 1);
    let handle = harness.service.start(harness.request(false)).unwrap();
    let token = handle.token().clone();

    let completion = handle.wait().await;
    assert_eq!(completion.outcome, UploadJobOutcome::Abandoned);
    let current = harness.records.current(&token).record;
    assert_eq!(current.phase, UploadPhase::Uploading);
    assert_eq!(current.remote_clip_id, None);
    assert!(harness.events.events.lock().unwrap().iter().all(|event| {
        event
            .record
            .remote_clip_id
            .as_ref()
            .is_none_or(|id| id.len() <= clipline_settings::MAX_CLOUD_UPLOAD_ID_BYTES)
    }));
}

#[tokio::test]
async fn status_sync_rejects_oversized_state_before_cas_or_event() {
    let harness = Harness::new("oversized-status-sync");
    let handle = harness.service.start(harness.request(false)).unwrap();
    let token = handle.token().clone();
    assert_eq!(handle.wait().await.outcome, UploadJobOutcome::Completed);
    let expected = harness.records.current(&token);
    let before_events = harness.events.events.lock().unwrap().clone();
    let mut replacement = expected.record.clone();
    replacement.error = Some("x".repeat(clipline_settings::MAX_CLOUD_UPLOAD_ERROR_BYTES + 1));

    let error = harness
        .service
        .commit_status_sync(&expected, replacement)
        .unwrap_err();
    assert_eq!(error.kind(), UploadRecordErrorKind::Persistence);
    assert_eq!(harness.records.current(&token), expected);
    assert_eq!(*harness.events.events.lock().unwrap(), before_events);
}

#[tokio::test]
async fn cancellation_removes_owned_payload_and_preserves_original() {
    let harness = Harness::new("cancel-cleanup");
    harness
        .preparation
        .owned_temp
        .store(true, Ordering::Release);
    harness.transport.block.store(true, Ordering::Release);
    let handle = harness.service.start(harness.request(false)).unwrap();
    harness.transport.wait_entered().await;
    let token = handle.token().clone();
    let temp = harness
        .preparation
        .temp_path
        .lock()
        .unwrap()
        .clone()
        .unwrap();
    assert!(temp.exists());

    assert!(harness.service.cancel(&token));
    assert_eq!(handle.wait().await.outcome, UploadJobOutcome::Canceled);
    harness.service.wait_idle().await;
    assert!(!temp.exists());
    assert!(harness.source.canonical_path().exists());
    assert_eq!(harness.deletion.calls.load(Ordering::Acquire), 0);
    assert_eq!(
        harness.records.current(&token).record.phase,
        UploadPhase::Canceled
    );
}

#[tokio::test]
async fn cancellation_after_server_completion_persists_pending_remote_and_preserves_local() {
    let harness = Harness::new("cancel-after-server");
    harness.remote.wait_block.store(true, Ordering::Release);
    let handle = harness.service.start(harness.request(true)).unwrap();
    let token = handle.token().clone();
    loop {
        if harness.records.current(&token).record.phase == UploadPhase::Processing {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(harness.service.cancel(&token));

    let completion = handle.wait().await;
    assert_eq!(completion.outcome, UploadJobOutcome::Abandoned);
    assert_eq!(completion.record.unwrap().phase, UploadPhase::Abandoned);
    assert_eq!(
        harness.records.current(&token).record.phase,
        UploadPhase::Abandoned
    );
    assert!(harness.source.canonical_path().exists());
}

#[tokio::test]
async fn delete_local_runs_only_after_ready_media_probe() {
    let harness = Harness::new("delete-after-probe");
    harness.remote.probe_block.store(true, Ordering::Release);
    let handle = harness.service.start(harness.request(true)).unwrap();
    let token = handle.token().clone();
    loop {
        if harness.records.current(&token).record.phase == UploadPhase::Verifying {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(harness.source.canonical_path().exists());

    harness.remote.probe_block.store(false, Ordering::Release);
    harness.remote.probe_release.notify_waiters();
    assert_eq!(handle.wait().await.outcome, UploadJobOutcome::Completed);
    let completed = harness.records.current(&token).record;
    assert_eq!(completed.phase, UploadPhase::Completed);
    assert!(completed.local_deleted);
    assert_eq!(harness.deletion.calls.load(Ordering::Acquire), 1);
    assert!(!harness.source.canonical_path().exists());
}

#[tokio::test]
async fn repository_cleanup_failure_preserves_local_and_completes_remote_state() {
    let harness = Harness::new("delete-failure");
    harness.deletion.fail.store(true, Ordering::Release);
    let handle = harness.service.start(harness.request(true)).unwrap();
    let token = handle.token().clone();

    assert_eq!(handle.wait().await.outcome, UploadJobOutcome::Completed);
    let completed = harness.records.current(&token).record;
    assert_eq!(completed.phase, UploadPhase::Completed);
    assert!(!completed.local_deleted);
    assert!(completed
        .error
        .unwrap()
        .contains("repository cleanup failure"));
    assert!(harness.source.canonical_path().exists());
    assert_eq!(harness.deletion.calls.load(Ordering::Acquire), 1);
}

#[tokio::test]
async fn failed_media_probe_never_invokes_repository_deletion() {
    let harness = Harness::new("probe-failure");
    harness.remote.probe_fail.store(true, Ordering::Release);
    let handle = harness.service.start(harness.request(true)).unwrap();
    let token = handle.token().clone();

    let completion = handle.wait().await;
    assert_eq!(completion.outcome, UploadJobOutcome::Completed);
    let completed = completion.record.unwrap();
    assert_eq!(completed.phase, UploadPhase::Completed);
    assert!(!completed.local_deleted);
    assert!(completed.error.unwrap().contains("media probe failure"));
    assert_eq!(harness.deletion.calls.load(Ordering::Acquire), 0);
    assert!(harness.source.canonical_path().exists());
    assert_eq!(
        harness.records.current(&token).record.phase,
        UploadPhase::Completed
    );
}

#[tokio::test]
async fn durable_generation_allocator_survives_service_recreation() {
    let harness = Harness::new("generation-restart");
    let first = harness.service.start(harness.request(false)).unwrap();
    let first_generation = first.token().upload_generation;
    assert_eq!(first.wait().await.outcome, UploadJobOutcome::Completed);
    harness.service.wait_idle().await;

    let restarted = UploadService::new(
        harness.registry.clone(),
        Arc::new(FakeAccountFence {
            current: Mutex::new(Some(owner("account-a", 1))),
            checks: AtomicUsize::new(0),
        }),
        harness.preparation.clone(),
        harness.transport.clone(),
        harness.remote.clone(),
        harness.deletion.clone(),
        harness.records.clone(),
        harness.events.clone(),
    );
    let second = restarted.start(harness.request(false)).unwrap();
    assert!(second.token().upload_generation > first_generation);
    assert_eq!(second.wait().await.outcome, UploadJobOutcome::Completed);
}

#[tokio::test]
async fn generation_allocation_includes_a_path_equivalent_legacy_alias() {
    let harness = Harness::new("legacy-alias-generation");
    harness
        .records
        .legacy_generations
        .lock()
        .unwrap()
        .push((harness.source.comparison_identity().clone(), 41));
    assert!(harness.records.records.lock().unwrap().is_empty());

    let handle = harness.service.start(harness.request(false)).unwrap();
    assert_eq!(handle.token().upload_generation, UploadGeneration::new(42));
    assert_eq!(handle.wait().await.outcome, UploadJobOutcome::Completed);
}

#[tokio::test]
async fn all_persisted_statuses_remain_in_the_settings_schema() {
    let harness = Harness::new("schema-statuses");
    let handle = harness.service.start(harness.request(true)).unwrap();
    assert_eq!(handle.wait().await.outcome, UploadJobOutcome::Completed);
    let allowed = [
        "queued",
        "uploading",
        "processing",
        "uploaded_processing",
        "uploaded_private",
        "uploaded_public",
        "failed",
        "retrying",
        "canceled",
    ];
    for event in harness.events.events.lock().unwrap().iter() {
        assert!(
            allowed.contains(&event.record.upload_status.as_str()),
            "unexpected durable status {}",
            event.record.upload_status
        );
    }
}

#[tokio::test]
async fn stale_cancel_cannot_cancel_a_newer_retry() {
    let harness = Harness::new("stale-cancel");
    let first = harness.service.start(harness.request(false)).unwrap();
    let stale = first.token().clone();
    assert_eq!(first.wait().await.outcome, UploadJobOutcome::Completed);
    harness.service.wait_idle().await;

    harness.preparation.block.store(true, Ordering::Release);
    let second = harness.service.start(harness.request(false)).unwrap();
    while harness.preparation.calls.load(Ordering::Acquire) < 2 {
        harness.preparation.entered.notified().await;
    }
    assert!(second.token().upload_generation > stale.upload_generation);
    assert!(!harness.service.cancel(&stale));
    harness.preparation.unblock();
    assert_eq!(second.wait().await.outcome, UploadJobOutcome::Completed);
}

#[test]
fn active_job_bound_matches_the_bounded_desktop_summary_contract() {
    assert_eq!(clipline_library::MAX_ACTIVE_UPLOAD_JOBS, 16);
}
